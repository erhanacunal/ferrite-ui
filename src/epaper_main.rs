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
mod battery;
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

const MAX_CODE_SIZE: usize = 50 * 1024; // sanity check to avoid OOM when loading malformed program

// --- System (0x00–0x0F) ---
const SYS_UPTIME: u8 = 0x00; // () → milliseconds as i32 (same as millis())
const SYS_REBOOT: u8 = 0x01; // () → does not return
const SYS_REPAIR_SCREEN: u8 = 0x02; // () → 0=ok, -1=error
const SYS_EPD_POWER_OFF: u8 = 0x03; // () → 0
const SYS_EPD_POWER_ON: u8 = 0x04; // () → 0
const SYS_BATTERY_VOLTAGE: u8 = 0x05; // () → battery voltage in millivolts (e.g. 3750 = 3.75 V)

// --- WiFi (0x10–0x1F) ---
const SYS_WIFI_CONNECT: u8 = 0x10; // (ssid: str_id, pass: str_id) → 0=ok, -1=error
const SYS_WIFI_DISCONNECT: u8 = 0x11; // () → 0
const SYS_WIFI_STATUS: u8 = 0x12; // () → 0=idle, 1=connecting, 2=connected, 3=failed
const SYS_WIFI_RSSI: u8 = 0x13; // () → RSSI in dBm (negative i32), or 0 if not connected
const SYS_WIFI_IP: u8 = 0x14; // () → IPv4 as u32 big-endian (0 if not connected)

// --- Bluetooth (0x20–0x2F) ---
const SYS_BT_START: u8 = 0x20; // (name: str_id) → 0=ok, -1=error
const SYS_BT_STOP: u8 = 0x21; // () → 0
const SYS_BT_STATUS: u8 = 0x22; // () → 0=off, 1=advertising, 2=connected
const SYS_BT_SEND: u8 = 0x23; // (data: str_id) → bytes_sent, or -1 on error

// --- GPIO pin ownership ---

struct EpaperPins {
    _sd_sclk: Output<'static>,
    _sd_mosi: Output<'static>,
    _sd_miso: Input<'static>,
    _sd_cs: Output<'static>,
    _button_1: Input<'static>,
    // GPIO14 and ADC2 are consumed by battery::init() — not stored here.
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
    // System timer (SYSTIMER UNIT0, 16 MHz).
    systick::init(peripherals.SYSTIMER);
    rtc::epaper::init(peripherals.I2C0, peripherals.GPIO17, peripherals.GPIO18);
    crate::rtc::Rtc::init();
    battery::init(peripherals.GPIO14, peripherals.ADC2);
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

// === Error display ===

/// Draw error box on screen and send error via USART.
fn show_error(
    lcd: &mut Lcd,
    font: &Font,
    flash: &Flash,
    usart: &usart::Usart,
    code: u8,
    vm_pc: Option<u16>,
) {
    lcd.begin_frame();
    lcd.fill_rect(0, 0, lcd::WIDTH, lcd::HEIGHT, COLOR_WHITE);

    let bw: u16 = 460;
    let bh: u16 = 210;
    let bx: u16 = (lcd::WIDTH - bw) / 2;
    let by: u16 = (lcd::HEIGHT - bh) / 2;

    lcd.draw_rect(bx, by, bw, bh, COLOR_RED);
    lcd.fill_rect(bx + 2, by + 2, bw - 4, bh - 4, COLOR_WHITE);

    let title = b"ERROR";
    let tw = font.text_width(title);
    let tx = bx as i16 + (bw as i16 - tw as i16) / 2;
    let ty = by as i16 + 34;
    font.draw_str(lcd, flash, title, tx, ty, COLOR_RED, Some(COLOR_WHITE));

    let mut code_line = [0u8; 16];
    let code_len = format_error_code(code, &mut code_line);
    let cw = font.text_width(&code_line[..code_len]);
    let cx = bx as i16 + (bw as i16 - cw as i16) / 2;
    font.draw_str(
        lcd,
        flash,
        &code_line[..code_len],
        cx,
        ty + 30,
        COLOR_BLACK,
        Some(COLOR_WHITE),
    );

    let desc = error_description(code);
    let dw = font.text_width(desc);
    let dx = bx as i16 + (bw as i16 - dw as i16) / 2;
    font.draw_str(
        lcd,
        flash,
        desc,
        dx,
        ty + 60,
        COLOR_BLACK,
        Some(COLOR_WHITE),
    );

    if let Some(pc) = vm_pc {
        let mut pc_buf = [0u8; 12];
        let pc_len = format_hex_u16(b"PC: 0x", pc, &mut pc_buf);
        let pw = font.text_width(&pc_buf[..pc_len]);
        let px = bx as i16 + (bw as i16 - pw as i16) / 2;
        font.draw_str(
            lcd,
            flash,
            &pc_buf[..pc_len],
            px,
            ty + 90,
            COLOR_BLACK,
            Some(COLOR_WHITE),
        );
    }

    lcd.end_frame();
    lcd.flush_dirty();

    protocol::send_error(usart, code);
}

fn format_hex_u16(prefix: &[u8], val: u16, buf: &mut [u8; 12]) -> usize {
    let plen = prefix.len().min(6);
    buf[..plen].copy_from_slice(&prefix[..plen]);
    let hex = b"0123456789ABCDEF";
    buf[plen] = hex[((val >> 12) & 0xF) as usize];
    buf[plen + 1] = hex[((val >> 8) & 0xF) as usize];
    buf[plen + 2] = hex[((val >> 4) & 0xF) as usize];
    buf[plen + 3] = hex[(val & 0xF) as usize];
    plen + 4
}

fn format_error_code(code: u8, buf: &mut [u8; 16]) -> usize {
    let prefix = b"Code: ";
    buf[..6].copy_from_slice(prefix);
    if code >= 10 {
        buf[6] = b'0' + code / 10;
        buf[7] = b'0' + code % 10;
        8
    } else {
        buf[6] = b'0' + code;
        7
    }
}

fn error_description(code: u8) -> &'static [u8] {
    match code {
        2 => b"main program not found",
        5 => b"no file system in flash",
        6 => b"program execution error",
        7 => b"insufficient memory",
        _ => b"error",
    }
}

// --- Panic handler ---

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    if lcd::epaper::is_ready() {
        let lcd = Lcd::new();
        let font = Font::from_embedded(
            &embedded_font::GLYPHS,
            &embedded_font::CODEPOINTS,
            &embedded_font::BITMAP,
            embedded_font::Y_ADVANCE,
        );
        let flash = Flash::new();

        lcd.fill_rect(0, 0, lcd::WIDTH, lcd::HEIGHT, COLOR_WHITE);

        let bw: u16 = 520;
        let bh: u16 = 230;
        let bx: u16 = (lcd::WIDTH - bw) / 2;
        let by: u16 = (lcd::HEIGHT - bh) / 2;

        lcd.draw_rect(bx, by, bw, bh, COLOR_RED);
        lcd.fill_rect(bx + 2, by + 2, bw - 4, bh - 4, COLOR_WHITE);

        let title = b"PANIC";
        let tw = font.text_width(title);
        let tx = bx as i16 + (bw as i16 - tw as i16) / 2;
        font.draw_str(
            &lcd,
            &flash,
            title,
            tx,
            by as i16 + 42,
            COLOR_RED,
            Some(COLOR_WHITE),
        );

        if let Some(loc) = info.location() {
            let file_bytes = loc.file().as_bytes();
            let mut name_start = 0usize;
            for i in 0..file_bytes.len() {
                if file_bytes[i] == b'/' || file_bytes[i] == b'\\' {
                    name_start = i + 1;
                }
            }
            let short_name = &file_bytes[name_start..];

            let mut loc_buf = [0u8; 64];
            let mut pos = 0usize;
            let copy_len = short_name.len().min(48);
            loc_buf[..copy_len].copy_from_slice(&short_name[..copy_len]);
            pos += copy_len;
            loc_buf[pos] = b':';
            pos += 1;

            let mut line_buf = [0u8; 5];
            let line_len = format_u16(loc.line() as u16, &mut line_buf);
            loc_buf[pos..pos + line_len].copy_from_slice(&line_buf[..line_len]);
            pos += line_len;

            let loc_line = &loc_buf[..pos];
            let lw = font.text_width(loc_line);
            let lx = bx as i16 + (bw as i16 - lw as i16) / 2;
            font.draw_str(
                &lcd,
                &flash,
                loc_line,
                lx,
                by as i16 + 84,
                COLOR_BLACK,
                Some(COLOR_WHITE),
            );
        }

        let msg = b"Device halted";
        let mw = font.text_width(msg);
        let mx = bx as i16 + (bw as i16 - mw as i16) / 2;
        font.draw_str(
            &lcd,
            &flash,
            msg,
            mx,
            by as i16 + 138,
            COLOR_BLACK,
            Some(COLOR_WHITE),
        );

        let hint = b"Reset to recover";
        let hw = font.text_width(hint);
        let hx = bx as i16 + (bw as i16 - hw as i16) / 2;
        font.draw_str(
            &lcd,
            &flash,
            hint,
            hx,
            by as i16 + 168,
            COLOR_BLACK,
            Some(COLOR_WHITE),
        );

        lcd.flush_dirty();
    }

    loop {}
}

fn format_u16(val: u16, buf: &mut [u8; 5]) -> usize {
    if val == 0 {
        buf[0] = b'0';
        return 1;
    }

    let mut n = val;
    let mut pos = buf.len();
    while n > 0 {
        pos -= 1;
        buf[pos] = b'0' + (n % 10) as u8;
        n /= 10;
    }

    let len = buf.len() - pos;
    for i in 0..len {
        buf[i] = buf[pos + i];
    }
    len
}

// === Entry point ===

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_hal::main]
fn main() -> ! {
    let config = esp_hal::Config::default(); //.with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Early UART banner — confirms the binary is executing before any heap/FS ops.
    // UART0 is already configured at 115200 baud by the ROM bootloader.

    // 64 KB DRAM heap for small allocations (Vec metadata, Box, etc.).
    // PSRAM (8 MB) is added inside init_gpio() via psram_allocator! before
    // any large allocation (FRAMEBUFFER, LUT) is accessed.
    esp_alloc::heap_allocator!(size: 64 * 1024);

    // Configure GPIO pins; PSRAM heap and I8080/RMT are set up inside.
    let _pins = init_gpio(peripherals);

    // Allocate framebuffer (259 KB) and LUT (64 KB) from the PSRAM heap.
    // Must happen after init_gpio() so psram_allocator! has already run.
    lcd::epaper::alloc_buffers();

    // Clear any ghost image from the previous session before first content draw.
    lcd::epaper::clear();

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
    let usart = usart::Usart::init();

    // Embedded font (FreeMono 9pt, ROM-resident — never touches flash).
    ctx.fonts.add(Font::from_embedded(
        &embedded_font::GLYPHS,
        &embedded_font::CODEPOINTS,
        &embedded_font::BITMAP,
        embedded_font::Y_ADVANCE,
    ));

    // Map logical FS addresses into ferrite partition (compact vs padded `ferrite_fs.bin`).
    flash::epaper::probe_ferrite_fs_preamble();

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

    vm.syscall_fn = Some(|id, args, strpool| match id {
        SYS_UPTIME => Some(systick::millis() as i32),
        SYS_REBOOT => {
            unsafe {
                esp_restart();
            }
            Some(0) // not reached
        }
        SYS_REPAIR_SCREEN => {
            lcd::epaper::repair_screen();
            Some(0)
        }
        SYS_EPD_POWER_OFF => {
            lcd::epaper::power_off();
            Some(0)
        }
        SYS_EPD_POWER_ON => {
            lcd::epaper::power_on();
            Some(0)
        }
        SYS_BATTERY_VOLTAGE => battery::read_mv().or(Some(-1)),

        _ => None, // unknown id → VM Error
    });

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
        show_error(
            &mut ctx.lcd,
            ctx.fonts.embedded(),
            &ctx.flash,
            &usart,
            error_code,
            None,
        );
    } else if vm.render_mode != RenderMode::EPaper {
        render::render_all(&mut ctx, &vm);
        ctx.lcd.flush_dirty();
    }

    let mut touch = Touch::init();
    let mut protocol = Protocol::new();

    // ===================== MAIN LOOP =====================
    loop {
        // --- Modal resume ---
        if error_code == 0 {
            vm.try_resume_modal(&mut ctx);
        }

        // --- VM step ---
        if error_code == 0 {
            match vm.state {
                VmState::Running | VmState::Yielded => {
                    vm.state = VmState::Running;
                    vm.step(&mut ctx);

                    while vm.is_critical() && vm.state == VmState::Running {
                        vm.step(&mut ctx);
                    }

                    if vm.state == VmState::Error {
                        error_code = ERR_PROGRAM_ERROR;
                        show_error(
                            &mut ctx.lcd,
                            ctx.fonts.embedded(),
                            &ctx.flash,
                            &usart,
                            error_code,
                            Some(vm.pc()),
                        );
                    }
                }
                VmState::Waiting => {
                    if systick::millis().wrapping_sub(vm.wait_until) < 0x8000_0000 {
                        vm.state = VmState::Running;
                    }
                }
                _ => {}
            }
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

                    if error_code == 0 && vm.render_mode != RenderMode::EPaper {
                        render::render_all(&mut ctx, &vm);
                        ctx.lcd.flush_dirty();
                    } else if error_code != 0 {
                        show_error(
                            &mut ctx.lcd,
                            ctx.fonts.embedded(),
                            &ctx.flash,
                            &usart,
                            error_code,
                            None,
                        );
                    }
                }

                RxEvent::ProgramTooLarge => {
                    error_code = ERR_INSUFFICIENT_MEMORY;
                    show_error(
                        &mut ctx.lcd,
                        ctx.fonts.embedded(),
                        &ctx.flash,
                        &usart,
                        ERR_INSUFFICIENT_MEMORY,
                        None,
                    );
                }

                RxEvent::FsReady => {
                    protocol.free_program();
                    protocol.alloc_sector_buf();
                    error_code = 0;
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

                RxEvent::SetDateTime => {
                    let dt = protocol.datetime();
                    rtc::Rtc::init().set_time(&dt);
                    protocol::send_pong(&usart);
                }

                _ => {}
            }
        }

        // --- Button / touch poll ---
        if error_code == 0 {
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
        }

        // --- Paint callbacks ---
        if error_code == 0 && vm.has_code() {
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
        if error_code == 0 {
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
        }

        // --- Drain callback queue ---
        if error_code == 0 && vm.has_pending_callbacks() {
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
