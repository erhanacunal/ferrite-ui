//! Entry point for the LilyGo T5-ePaper-S3 (ESP32-S3R8 + ED047TC1 4.7" EPD).
//!
//! Build:
//!   cargo +esp build --no-default-features --features epaper \
//!         --bin epaper --target xtensa-esp32s3-none-elf --release
//!
//! Requires the Espressif Rust fork: `espup install` then `rustup toolchain use esp`.
//! `esp-hal` provides the ESP32-S3 startup/runtime.
//!
//! Pin assignment:
//!   EPD data bus : D0=GPIO8, D1-D7=GPIO1-7
//!   EPD control  : CKH=GPIO41, STH=GPIO40, CKV=GPIO38
//!   Config SR    : CFG_DATA=GPIO13, CFG_CLK=GPIO12, CFG_STR=GPIO0
//!   SD card      : SCLK=GPIO11, MOSI=GPIO15, MISO=GPIO16, CS=GPIO42
//!   UART0        : TXD=GPIO43, RXD=GPIO44 (configured by ROM bootloader)
//!   Button       : BUTTON_1=GPIO21 (active-low, pull-up)

#![no_std]
#![no_main]
#![feature(asm_experimental_arch)]
#![allow(dead_code, unused_imports, unsafe_op_in_unsafe_fn)]

extern crate alloc;
use esp_alloc as _; // activates esp_alloc as the global allocator

use alloc::boxed::Box;
use alloc::vec;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    peripherals::Peripherals,
};

// --- Module tree (mirrors src/main.rs; paths resolve relative to src/) ---

mod backlight;
mod clip;
mod config;
mod ctx;
mod embedded_font;
mod fat;
mod flash;
mod font;
mod fs;
mod heap;
mod image;
mod keyboard;
mod lcd;
mod proto;
mod protocol;
mod render;
mod rtc;
mod sdcard;
mod strpool;
mod systick;
mod touch;
mod types;
mod usart;
mod vm;
mod widget;

use ctx::Ctx;
use flash::Flash;
use font::Font;
use image::ImageList;
use lcd::Lcd;
use protocol::{Protocol, RxEvent};
use strpool::StringPool;
use touch::Touch;
use types::{COLOR_BLACK, COLOR_RED, COLOR_WHITE, Size};
use vm::{FunctionKind, RenderMode, Vm, VmState};

use crate::{rtc::DateTime, systick::delay_ms};

// --- Error codes (same as main.rs) ---

const ERR_NO_FILESYSTEM: u8 = 5;
const ERR_PROGRAM_ERROR: u8 = 6;
const ERR_PROGRAM_NOT_FOUND: u8 = 2;
const ERR_INSUFFICIENT_MEMORY: u8 = 7;

const MAX_CODE_SIZE: usize = 4096;

// --- GPIO pin ownership ---

struct EpaperPins {
    _sd_sclk: Output<'static>,
    _sd_mosi: Output<'static>,
    _sd_miso: Input<'static>,
    _sd_cs: Output<'static>,
    _button_1: Input<'static>,
}

fn init_gpio(peripherals: Peripherals) -> EpaperPins {
    // PSRAM heap — must be first so large allocations land in PSRAM.
    // Create PSRAM allocator (octal mode for the LilyGo T5 V2.3)
    let psram_config = esp_hal::psram::PsramConfig {
        mode: esp_hal::psram::PsramMode::OctalSpi,
        ..Default::default()
    };
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram, psram_config);

    let output = OutputConfig::default();
    let input_pu = InputConfig::default().with_pull(Pull::Up);

    // Initialise ED047TC1: I8080, RMT, and config shift-register in one call.
    usart::dbg(b"  epd hw init\r\n");
    lcd::epaper::init(lcd::epaper::EpdHwPins {
        cfg_data: peripherals.GPIO13,
        cfg_clk: peripherals.GPIO12,
        cfg_str: peripherals.GPIO0,
        data0: peripherals.GPIO8,
        data1: peripherals.GPIO1,
        data2: peripherals.GPIO2,
        data3: peripherals.GPIO3,
        data4: peripherals.GPIO4,
        data5: peripherals.GPIO5,
        data6: peripherals.GPIO6,
        data7: peripherals.GPIO7,
        lcd_dc: peripherals.GPIO40,
        lcd_wrx: peripherals.GPIO41,
        dma_ch: peripherals.DMA_CH0,
        lcd_cam: peripherals.LCD_CAM,
        rmt: peripherals.RMT,
    });
    usart::dbg(b"  epd hw ok\r\n");
    // System timer (SYSTIMER UNIT0, 16 MHz).
    systick::init(peripherals.SYSTIMER);
    rtc::epaper::init(peripherals.I2C0, peripherals.GPIO17, peripherals.GPIO18);
    let rtc_instance = crate::rtc::Rtc::init();
    let now = crate::rtc::DateTime {
        year: 26,
        month: 5,
        day: 6,
        weekday: 0,
        hour: 12,
        minute: 8,
        second: 0,
    };
    rtc_instance.set_time(&now);
    let check = rtc_instance.read_time();
    usart::dbg(b"  rtc time: ");
    usart::dbg_u16(check.year as u16);
    usart::dbg(b".");
    usart::dbg_u16(check.month as u16);
    usart::dbg(b".");
    usart::dbg_u16(check.day as u16);
    usart::dbg(b" ");
    usart::dbg_u16(check.hour as u16);
    usart::dbg(b":");
    usart::dbg_u16(check.minute as u16);
    usart::dbg(b":");
    usart::dbg_u16(check.second as u16);
    usart::dbg(b"\r\n");
    if check.year != now.year || check.month != now.month || check.day != now.day {
        rtc_instance.set_time(&now);
    }
    EpaperPins {
        _sd_sclk: Output::new(peripherals.GPIO11, Level::Low, output),
        _sd_mosi: Output::new(peripherals.GPIO15, Level::Low, output),
        _sd_miso: Input::new(peripherals.GPIO16, input_pu),
        _sd_cs: Output::new(peripherals.GPIO42, Level::High, output),
        _button_1: Input::new(peripherals.GPIO21, input_pu),
    }
}

// --- Software reset ---

/// Trigger an ESP32-S3 system reset via the RTC_CNTL_OPTIONS0_REG SW_SYS_RST bit.
#[allow(dead_code)]
unsafe fn esp_restart() -> ! {
    const RTC_CNTL_OPTIONS0: *mut u32 = 0x6000_8000u32 as *mut u32;
    let v = core::ptr::read_volatile(RTC_CNTL_OPTIONS0);
    core::ptr::write_volatile(RTC_CNTL_OPTIONS0, v | (1 << 31));
    loop {}
}

// --- Panic handler ---

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    usart::dbg(b"\r\nPANIC");
    if let Some(loc) = info.location() {
        usart::dbg(b" at ");
        usart::dbg(loc.file().as_bytes());
        usart::dbg(b":");
        usart::dbg_u16(loc.line() as u16);
    }
    usart::dbg(b"\r\n");
    usart::dbg(b"Message: ");
    loop {}
}

// === Entry point ===

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_hal::main]
fn main() -> ! {
    let config = esp_hal::Config::default(); //.with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Early UART banner — confirms the binary is executing before any heap/FS ops.
    // UART0 is already configured at 115200 baud by the ROM bootloader.
    usart::dbg(b"\r\nferrite-ui booting\r\n");

    // 64 KB DRAM heap for small allocations (Vec metadata, Box, etc.).
    // PSRAM (8 MB) is added inside init_gpio() via psram_allocator! before
    // any large allocation (FRAMEBUFFER, LUT) is accessed.
    usart::dbg(b"[1] heap alloc\r\n");
    esp_alloc::heap_allocator!(size: 64 * 1024);

    // Configure GPIO pins; PSRAM heap and I8080/RMT are set up inside.
    usart::dbg(b"[2] init_gpio\r\n");
    let _pins = init_gpio(peripherals);
    usart::dbg(b"[2] done\r\n");

    // Allocate framebuffer (259 KB) and LUT (64 KB) from the PSRAM heap.
    // Must happen after init_gpio() so psram_allocator! has already run.
    usart::dbg(b"[3] alloc_buffers\r\n");
    lcd::epaper::alloc_buffers();
    usart::dbg(b"[3] done\r\n");

    // Clear any ghost image from the previous session before first content draw.
    usart::dbg(b"[4] epd clear\r\n");
    lcd::epaper::clear();
    usart::dbg(b"[4] done\r\n");

    // Build context on the heap to avoid blowing the stack.
    let mut ctx = Box::new(Ctx {
        lcd: Lcd::new(),
        flash: Flash::new(),
        tree: widget::WidgetTree::new(),
        fonts: font::FontList::new(),
        images: ImageList::new(),
        strpool: StringPool::new(),
        fs: None,
        backlight: backlight::Backlight::init(),
        cursor_visible: false,
    });

    // Embedded font (FreeMono 9pt, ROM-resident — never touches flash).
    ctx.fonts.add(Font::from_embedded(
        &embedded_font::GLYPHS,
        &embedded_font::BITMAP,
        embedded_font::FIRST,
        embedded_font::LAST,
        embedded_font::Y_ADVANCE,
    ));

    // Mount FS from XIP-mapped internal flash.
    match fs::Fs::mount(&ctx.flash) {
        Ok(f) => ctx.fs = Some(f),
        Err(_) => {}
    }

    // Root widget (always widget 0).
    let root = ctx.tree.alloc().unwrap();
    {
        let w = ctx.tree.get_mut(root);
        w.size = Size {
            w: lcd::epaper::EPD_WIDTH,
            h: lcd::epaper::EPD_HEIGHT,
        };
        w.background_color = COLOR_WHITE;
    }
    ctx.tree.root = root;

    let mut vm = Box::new(Vm::new());
    let mut error_code: u8 = 0;

    // Load main program from FS.
    if let Some(ref fs) = ctx.fs {
        match fs.find(&ctx.flash, b"main") {
            Some(entry) => {
                if entry.flags & fs::RES_FLAG_FLASH_EXEC != 0 {
                    if !vm.load_flash(&ctx.flash, entry.offset, entry.size as usize) {
                        error_code = ERR_PROGRAM_ERROR;
                    }
                } else {
                    let img_len = entry.size.min(MAX_CODE_SIZE as u32) as usize;
                    let mut img_buf = vec![0u8; img_len];
                    fs.read_resource(&ctx.flash, &entry, 0, &mut img_buf);
                    if !vm.load_ram(&img_buf) {
                        error_code = ERR_PROGRAM_ERROR;
                    }
                }
                let wc = if vm.widget_count > 0 {
                    vm.widget_count as usize
                } else {
                    96
                };
                let ec = if vm.ext_count > 0 {
                    vm.ext_count as usize
                } else {
                    96
                };
                ctx.tree.reserve(wc, ec);
            }
            None => error_code = ERR_PROGRAM_NOT_FOUND,
        }
    } else {
        error_code = ERR_NO_FILESYSTEM;
    }

    // Run setup() if defined.
    if error_code == 0 && vm.has_code() {
        if let Some(entry) = vm.find_by_kind(FunctionKind::Setup) {
            vm.run_callback(entry.offset as u16, &mut ctx);
            if vm.pop_result() != 0 {
                error_code = ERR_PROGRAM_ERROR;
            }
        }
    }

    // Run on_program_start callback.
    if error_code == 0 && vm.has_code() {
        if let Some(entry) = vm.find_by_kind(FunctionKind::OnProgramStart) {
            vm.run_callback(entry.offset as u16, &mut ctx);
        }
    }

    // Start loop() — VM will yield on each iteration.
    if error_code == 0 && vm.has_code() {
        if let Some(entry) = vm.find_by_kind(FunctionKind::Loop) {
            vm.set_pc(entry.offset as u16);
            vm.state = VmState::Running;
        }
    }

    // Initial full render + EPD flush.
    // Always drive the EPD so hardware can be verified even when the FS is absent.
    if error_code != 0 {
        usart::dbg(b"\r\nBOOT ERROR: ");
        usart::dbg_u16(error_code as u16);
        usart::dbg(b"\r\n");
    }
    render::render_all(&mut ctx, &vm);
    ctx.lcd.flush_dirty();

    usart::dbg(b"EPD flush done\r\n");

    let mut touch = Touch::init();
    let mut protocol = Protocol::new();
    let usart = usart::Usart::init();

    // ===================== MAIN LOOP =====================
    loop {
        // --- Modal resume ---
        vm.try_resume_modal(&mut ctx);

        // --- VM step ---
        match vm.state {
            VmState::Running | VmState::Yielded => {
                vm.state = VmState::Running;
                vm.step(&mut ctx);

                while vm.is_critical() && vm.state == VmState::Running {
                    vm.step(&mut ctx);
                }

                if vm.state == VmState::Error {
                    usart::dbg(b"\r\nVM ERROR\r\n");
                }
            }
            VmState::Waiting => {
                if systick::millis().wrapping_sub(vm.wait_until) < 0x8000_0000 {
                    vm.state = VmState::Running;
                }
            }
            _ => {}
        }

        // --- USART protocol ---
        while let Some(byte) = usart::rx_read_byte() {
            match protocol.feed(byte, &ctx.flash) {
                RxEvent::None => {}

                RxEvent::Ping => {
                    protocol::send_pong(&usart);
                }

                RxEvent::Restart => unsafe {
                    esp_restart();
                },

                RxEvent::MemInfo => {
                    let (free, _) = heap::stats();
                    protocol::send_meminfo(&usart, free as u32);
                }

                RxEvent::StackInfo => {
                    protocol::send_stackinfo(&usart, 0, 0);
                }

                RxEvent::ProgramReady => {
                    ctx.tree.clear();
                    ctx.strpool.clear();
                    vm.reset();

                    let root = ctx.tree.alloc().unwrap();
                    {
                        let w = ctx.tree.get_mut(root);
                        w.size = Size {
                            w: lcd::epaper::EPD_WIDTH,
                            h: lcd::epaper::EPD_HEIGHT,
                        };
                        w.background_color = COLOR_BLACK;
                    }
                    ctx.tree.root = root;

                    let prog = protocol.program_code();
                    if !vm.load_ram(prog) {
                        vm.load_raw(prog);
                    }
                    let wc = if vm.widget_count > 0 {
                        vm.widget_count as usize
                    } else {
                        96
                    };
                    let ec = if vm.ext_count > 0 {
                        vm.ext_count as usize
                    } else {
                        96
                    };
                    ctx.tree.reserve(wc, ec);
                    protocol.free_program();

                    error_code = 0;

                    if let Some(entry) = vm.find_by_kind(FunctionKind::Setup) {
                        vm.run_callback(entry.offset as u16, &mut ctx);
                        if vm.pop_result() != 0 {
                            error_code = ERR_PROGRAM_ERROR;
                        }
                    }

                    if error_code == 0 {
                        if let Some(entry) = vm.find_by_kind(FunctionKind::Loop) {
                            vm.set_pc(entry.offset as u16);
                            vm.state = VmState::Running;
                        }
                    }

                    render::render_all(&mut ctx, &vm);
                    ctx.lcd.flush_dirty();
                }

                RxEvent::ProgramTooLarge => {
                    protocol::send_error(&usart, ERR_INSUFFICIENT_MEMORY);
                }

                RxEvent::FsReady => {
                    protocol.free_program();
                    protocol.alloc_sector_buf();
                    protocol::send_pong(&usart);
                }

                RxEvent::FsChunkDone => {
                    protocol::send_pong(&usart);
                }

                RxEvent::FsWriteComplete => {
                    protocol::send_pong(&usart);
                    delay_ms(50);
                    unsafe { esp_restart() };
                }

                _ => {}
            }
        }

        // --- Button / touch poll ---
        if let Some(event) = touch.poll() {
            use touch::TouchEventKind;
            match event.kind {
                TouchEventKind::Press => {
                    let hit = touch::hit_test(&mut ctx.tree, event.x, event.y);
                    if hit.is_some() {
                        // Mark pressed state.
                        let flags = ctx.tree.get(hit).flags;
                        ctx.tree.get_mut(hit).flags = flags | widget::FLAG_PRESSED;
                        ctx.tree.mark_dirty(hit);
                    }
                }
                TouchEventKind::Release => {
                    // Find the widget at last known position and fire on_click.
                    let hit = touch::hit_test(&mut ctx.tree, event.x, event.y);
                    if hit.is_some() {
                        let flags = ctx.tree.get(hit).flags;
                        ctx.tree.get_mut(hit).flags = flags & !widget::FLAG_PRESSED;
                        ctx.tree.mark_dirty(hit);

                        if vm.has_code() {
                            if let Some(entry) = vm.find_func(ctx.tree.on_click(hit)) {
                                vm.enqueue_callback(entry.offset as u16, &[hit.0 as i32]);
                            }
                        }
                    }
                }
                TouchEventKind::Hold => {}
            }
        }

        // --- Paint callbacks ---
        if vm.has_code() {
            let dfs = ctx.tree.dfs_order();
            let mut paint_count = 0usize;
            let mut paint_ids = [widget::WidgetId::NONE; 8];
            for i in 0..dfs.len() {
                let id = dfs[i];
                let on_paint = ctx.tree.on_paint(id);
                if ctx.tree.get(id).is_dirty() && on_paint != 0 && paint_count < 8 {
                    paint_ids[paint_count] = id;
                    paint_count += 1;
                }
            }
            for i in 0..paint_count {
                let id = paint_ids[i];
                let paint_func = ctx.tree.on_paint(id);
                if paint_func > 0 {
                    if let Some(entry) = vm.find_func(paint_func) {
                        vm.enqueue_callback(entry.offset as u16, &[id.0 as i32]);
                    }
                }
            }
        }

        // --- Render ---
        match vm.render_mode {
            RenderMode::Buffered => {
                if render::buffered_has_dirty(&mut ctx) {
                    ctx.lcd.begin_frame();
                    render::render_buffered_content(&mut ctx, &vm);
                    ctx.lcd.end_frame();
                    ctx.lcd.flush_dirty();
                }
            }
            RenderMode::Dirty => {
                render::render_dirty(&mut ctx, &vm);
                ctx.lcd.flush_dirty();
            }
            RenderMode::EPaper => {
                // EPD updates are driven exclusively by OP_W_RENDER (render() in
                // Ferrite code). Do nothing here to avoid premature dirty-flag
                // clearing and spurious ghost-only flushes.
            }
        }

        // --- Drain callback queue ---
        if vm.has_pending_callbacks() {
            vm.drain_callbacks(&mut ctx);
            match vm.render_mode {
                RenderMode::Buffered => {
                    if render::buffered_has_dirty(&mut ctx) {
                        ctx.lcd.begin_frame();
                        render::render_buffered_content(&mut ctx, &vm);
                        ctx.lcd.end_frame();
                        ctx.lcd.flush_dirty();
                    }
                }
                RenderMode::Dirty => {
                    render::render_dirty(&mut ctx, &vm);
                    ctx.lcd.flush_dirty();
                }
                RenderMode::EPaper => {}
            }
        }
    }
}
