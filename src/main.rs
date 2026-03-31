#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use ctx::Ctx;
use panic_halt as _;
mod backlight;
mod clip;
mod config;
mod ctx;
mod embedded_font;
mod fat;
mod flash;
mod font;
mod fs;
mod gpio;
mod heap;
mod image;
mod irq;
mod lcd;
mod page;
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

use cortex_m_rt::entry;
use flash::Flash;
use font::Font;
use gpio::Gpio;
use image::ImageList;
use lcd::Lcd;
use page::PageManager;
use protocol::{Protocol, RxEvent};
use strpool::StringPool;
use touch::Touch;
use types::{COLOR_BLACK, COLOR_RED, COLOR_WHITE, Size};
use usart::Usart;
use vm::{Vm, VmState, FunctionKind};

use crate::systick::delay_ms;

// === Hardware addresses (port init) ===

const RCU_APB2EN: u32 = 0x4002_1018;
const RCU_APB1EN: u32 = 0x4002_101C;
const RCU_APB1RST: u32 = 0x4002_1010;
const RCU_BDCTL: u32 = 0x4002_1020;
const AFIO_PCF0: u32 = 0x4001_0004;
const PMU_CTL: u32 = 0x4000_7000;
const FMC_WS: u32 = 0x4002_2000;

const GPIOA: u32 = 0x4001_0800;
const GPIOB: u32 = 0x4001_0C00;
const GPIOC: u32 = 0x4001_1000;
const GPIOD: u32 = 0x4001_1400;

const TIMER2: u32 = 0x4000_0400;

// === Error codes ===

#[allow(dead_code)]
const ERR_PAGE_NOT_FOUND: u8 = 1;
const ERR_PROGRAM_NOT_FOUND: u8 = 2;
#[allow(dead_code)]
const ERR_IMAGE_NOT_FOUND: u8 = 3;
#[allow(dead_code)]
const ERR_FONT_NOT_FOUND: u8 = 4;
const ERR_NO_FILESYSTEM: u8 = 5;
const ERR_PROGRAM_ERROR: u8 = 6;
const ERR_INSUFFICIENT_MEMORY: u8 = 7;

// === Max program code size ===

const MAX_CODE_SIZE: usize = 4096;

// === RCU registers ===

const RCU_BASE: u32 = 0x4002_1000;
const RCU_CTL: u32 = RCU_BASE + 0x00;
const RCU_CFG0: u32 = RCU_BASE + 0x04;
const RCU_INT: u32 = RCU_BASE + 0x08;

// RCU_CTL bits
const CTL_IRC8MEN: u32 = 1 << 0;
const CTL_IRC8MSTB: u32 = 1 << 1;
const CTL_HXTALEN: u32 = 1 << 16;
const CTL_HXTALBPS: u32 = 1 << 18;
const CTL_CKMEN: u32 = 1 << 19;
const CTL_PLLEN: u32 = 1 << 24;
const CTL_PLLSTB: u32 = 1 << 25;

// RCU_CFG0 bits/masks
const CFG0_SCS: u32 = 0x03; // System clock switch [1:0]
const CFG0_SCSS: u32 = 0x0C; // System clock switch status [3:2]
const CFG0_AHBPSC: u32 = 0xF0; // AHB prescaler [7:4]
const CFG0_APB1PSC: u32 = 0x700; // APB1 prescaler [10:8]
const CFG0_APB2PSC: u32 = 0x3800; // APB2 prescaler [13:11]
const CFG0_ADCPSC: u32 = 0xC000; // ADC prescaler [15:14]
const CFG0_PLLSEL: u32 = 1 << 16;
const CFG0_PREDV0: u32 = 1 << 17;
const CFG0_PLLMF: u32 = 0xF << 18; // PLL multiplier [21:18]
const CFG0_USBDPSC: u32 = 0x3 << 22;
const CFG0_CKOUT0SEL: u32 = 0x7 << 24;
const CFG0_PLLMF_4: u32 = 1 << 27; // PLL multiplier bit 4
const CFG0_ADCPSC_2: u32 = 1 << 28;

// PLL source = IRC8M/2, multiply by 27 → (4MHz × 27) = 108MHz
// PLLMF_4=1, PLLMF[3:0]=10 → extended multiplier = 10 + 17 = 27
const PLL_MUL27: u32 = CFG0_PLLMF_4 | (0xA << 18);

// APB1 = AHB/2 (max 54MHz for APB1)
const APB1_DIV2: u32 = 0x4 << 8;

// System clock source = PLL
const CKSYSSRC_PLL: u32 = 0x02;
// System clock switch status = PLL
const SCSS_PLL: u32 = 0x02 << 2;

const IRC8M_STARTUP_TIMEOUT: u32 = 0x0500;

/// System clock initialization: IRC8M → PLL × 27 → 108MHz SYSCLK.
/// Must be called before any peripheral init.
///
/// Clock tree after init:
///   IRC8M (8MHz) → /2 → PLL ×27 → SYSCLK (108MHz)
///   AHB  = SYSCLK / 1  = 108MHz
///   APB2 = AHB / 1     = 108MHz  (GPIO, SPI0, USART0, TIMER0)
///   APB1 = AHB / 2     = 54MHz   (TIMER2)
fn system_init() {
    unsafe {
        // --- Flash wait state = 2 (must be set BEFORE increasing clock) ---
        let val = core::ptr::read_volatile(FMC_WS as *const u32);
        core::ptr::write_volatile(FMC_WS as *mut u32, (val & !0x7) | 2);

        // --- Reset RCU to default state ---

        // Enable IRC8M
        let val = core::ptr::read_volatile(RCU_CTL as *const u32);
        core::ptr::write_volatile(RCU_CTL as *mut u32, val | CTL_IRC8MEN);

        // Wait for IRC8M stable
        while core::ptr::read_volatile(RCU_CTL as *const u32) & CTL_IRC8MSTB == 0 {}

        // Switch system clock to IRC8M (SCS = 00)
        let val = core::ptr::read_volatile(RCU_CFG0 as *const u32);
        core::ptr::write_volatile(RCU_CFG0 as *mut u32, val & !CFG0_SCS);

        // Reset HXTALEN, CKMEN, PLLEN
        let val = core::ptr::read_volatile(RCU_CTL as *const u32);
        core::ptr::write_volatile(
            RCU_CTL as *mut u32,
            val & !(CTL_HXTALEN | CTL_CKMEN | CTL_PLLEN),
        );

        // Reset CFG0: prescalers, clock source, PLL config
        let val = core::ptr::read_volatile(RCU_CFG0 as *const u32);
        core::ptr::write_volatile(
            RCU_CFG0 as *mut u32,
            val & !(CFG0_SCS
                | CFG0_AHBPSC
                | CFG0_APB1PSC
                | CFG0_APB2PSC
                | CFG0_ADCPSC
                | CFG0_ADCPSC_2
                | CFG0_CKOUT0SEL
                | CFG0_PLLSEL
                | CFG0_PREDV0
                | CFG0_PLLMF
                | CFG0_USBDPSC
                | CFG0_PLLMF_4),
        );

        // Reset HXTALBPS
        let val = core::ptr::read_volatile(RCU_CTL as *const u32);
        core::ptr::write_volatile(RCU_CTL as *mut u32, val & !CTL_HXTALBPS);

        // Disable all RCU interrupts
        core::ptr::write_volatile(RCU_INT as *mut u32, 0x009F_0000);

        // --- Configure 108MHz from IRC8M ---

        // Enable IRC8M (should already be enabled, but ensure)
        let val = core::ptr::read_volatile(RCU_CTL as *const u32);
        core::ptr::write_volatile(RCU_CTL as *mut u32, val | CTL_IRC8MEN);

        // Wait for IRC8M stable with timeout
        let mut timeout: u32 = 0;
        while core::ptr::read_volatile(RCU_CTL as *const u32) & CTL_IRC8MSTB == 0 {
            timeout += 1;
            if timeout >= IRC8M_STARTUP_TIMEOUT {
                // IRC8M failed — hang (should never happen)
                loop {
                    cortex_m::asm::nop();
                }
            }
        }

        // AHB = SYSCLK / 1 (bits already cleared above)
        // APB2 = AHB / 1 (bits already cleared above)
        // APB1 = AHB / 2
        let val = core::ptr::read_volatile(RCU_CFG0 as *const u32);
        core::ptr::write_volatile(RCU_CFG0 as *mut u32, val | APB1_DIV2);

        // PLL = IRC8M/2 × 27 = 108MHz (PLLSEL=0 selects IRC8M/2)
        let val = core::ptr::read_volatile(RCU_CFG0 as *const u32);
        core::ptr::write_volatile(
            RCU_CFG0 as *mut u32,
            (val & !(CFG0_PLLMF | CFG0_PLLMF_4)) | PLL_MUL27,
        );

        // Enable PLL
        let val = core::ptr::read_volatile(RCU_CTL as *const u32);
        core::ptr::write_volatile(RCU_CTL as *mut u32, val | CTL_PLLEN);

        // Wait for PLL stable
        while core::ptr::read_volatile(RCU_CTL as *const u32) & CTL_PLLSTB == 0 {}

        // Switch system clock to PLL
        let val = core::ptr::read_volatile(RCU_CFG0 as *const u32);
        core::ptr::write_volatile(RCU_CFG0 as *mut u32, (val & !CFG0_SCS) | CKSYSSRC_PLL);

        // Wait until PLL is selected as system clock
        while (core::ptr::read_volatile(RCU_CFG0 as *const u32) & CFG0_SCSS) != SCSS_PLL {}
    }
}

/// Initialize all GPIO ports, AFIO, TIMER2 PWM.
fn init_ports() {
    unsafe {
        // RCU: Peripheral clock enable
        // AFEN(0) | PAEN(2) | PBEN(3) | PCEN(4) | PDEN(5) | SPI0EN(12) | USART0EN(14)
        let val = core::ptr::read_volatile(RCU_APB2EN as *const u32);
        core::ptr::write_volatile(RCU_APB2EN as *mut u32, val | 0x503D);

        // JTAG disable, SWD enable: SWJ_CFG = 010
        let val = core::ptr::read_volatile(AFIO_PCF0 as *const u32);
        core::ptr::write_volatile(AFIO_PCF0 as *mut u32, (val & 0xF8FF_FFFF) | 0x0200_0000);

        // GPIOB: all pins push-pull output 50MHz (LCD 16-bit data bus)
        core::ptr::write_volatile((GPIOB + 0x00) as *mut u32, 0x3333_3333);
        core::ptr::write_volatile((GPIOB + 0x04) as *mut u32, 0x3333_3333);

        // GPIOA CTL0 (PA0-PA7)
        core::ptr::write_volatile((GPIOA + 0x00) as *mut u32, 0xB4B3_3334);

        // GPIOA CTL1 (PA8-PA15)
        let ctl1 = (GPIOA + 0x04) as *mut u32;
        let mut val = core::ptr::read_volatile(ctl1);
        val &= !(0xF << 0); // PA8:  PP output 50MHz (backlight off until timer init)
        val |= 0x3 << 0;
        val &= !(0xF << 4); // PA9:  AF PP 50MHz (USART0 TX)
        val |= 0xB << 4;
        val &= !(0xF << 8); // PA10: floating input (USART0 RX)
        val |= 0x4 << 8;
        val &= !(0xF << 12); // PA11: PP output 50MHz
        val |= 0x3 << 12;
        val &= !(0xF << 16); // PA12: PP output 50MHz (LCD CLK)
        val |= 0x3 << 16;
        val &= !(0xF << 28); // PA15: PP output 50MHz (LCD CMD/DATA)
        val |= 0x3 << 28;
        core::ptr::write_volatile(ctl1, val);

        // GPIOA initial pin states
        core::ptr::write_volatile(
            (GPIOA + 0x10) as *mut u32,
            (1 << 2) | (1 << 4) | (1 << 11) | (1 << 12),
        );
        core::ptr::write_volatile((GPIOA + 0x14) as *mut u32, (1 << 1) | (1 << 3));

        // GPIOC CTL0 (PC0-PC7)
        core::ptr::write_volatile((GPIOC + 0x00) as *mut u32, 0x4444_4477);

        // GPIOC CTL1 (PC8-PC15)
        let ctl1 = (GPIOC + 0x04) as *mut u32;
        let mut val = core::ptr::read_volatile(ctl1);
        val &= !(0xFF);
        val |= 0x44;
        val &= !(0xF << 12);
        val |= 0x3 << 12;
        val &= !(0xF << 16);
        val |= 0x3 << 16;
        val &= !(0xF << 20);
        val |= 0x3 << 20;
        val &= !(0xF << 24);
        val |= 0x4 << 24;
        core::ptr::write_volatile(ctl1, val);

        // GPIOC initial pin states
        core::ptr::write_volatile((GPIOC + 0x14) as *mut u32, 1 << 12);
        core::ptr::write_volatile((GPIOC + 0x10) as *mut u32, (1 << 0) | (1 << 1) | (1 << 13));
        core::ptr::write_volatile((GPIOC + 0x10) as *mut u32, 0x03FC_0000);

        // GPIOD: PD2 PP output 50MHz
        let ctl0 = (GPIOD + 0x00) as *mut u32;
        let mut val = core::ptr::read_volatile(ctl0);
        val &= !(0xF << 8);
        val |= 0x3 << 8;
        core::ptr::write_volatile(ctl0, val);

        // TIMER2 PWM: display brightness (full remap → PC6-PC9)
        let val = core::ptr::read_volatile(AFIO_PCF0 as *const u32);
        core::ptr::write_volatile(AFIO_PCF0 as *mut u32, val | 0x0C00);

        let val = core::ptr::read_volatile(RCU_APB1EN as *const u32);
        core::ptr::write_volatile(RCU_APB1EN as *mut u32, val | (1 << 1));
        let val = core::ptr::read_volatile(RCU_APB1RST as *const u32);
        core::ptr::write_volatile(RCU_APB1RST as *mut u32, val | (1 << 1));
        core::ptr::write_volatile(RCU_APB1RST as *mut u32, val & !(1 << 1));

        core::ptr::write_volatile((TIMER2 + 0x18) as *mut u32, 0x6868);
        core::ptr::write_volatile((TIMER2 + 0x1C) as *mut u32, 0x6868);
        core::ptr::write_volatile((TIMER2 + 0x20) as *mut u32, 0);
        core::ptr::write_volatile((TIMER2 + 0x28) as *mut u32, 0);
        core::ptr::write_volatile((TIMER2 + 0x00) as *mut u32, 1);

        // Backup domain: LXTAL disable
        let val = core::ptr::read_volatile(PMU_CTL as *const u32);
        core::ptr::write_volatile(PMU_CTL as *mut u32, val | (1 << 8));
        let val = core::ptr::read_volatile(RCU_BDCTL as *const u32);
        core::ptr::write_volatile(RCU_BDCTL as *mut u32, val & !1);
    }
}

// === Error display ===

/// Draw error box on screen and send error via USART.
fn show_error(lcd: &mut Lcd, font: &Font, flash: &Flash, usart: &Usart, code: u8, vm_pc: Option<u16>) {
    lcd.begin_frame();
    lcd.fill_rect(0, 0, 800, 480, COLOR_BLACK);

    let bx: u16 = 200;
    let by: u16 = 140;
    let bw: u16 = 400;
    let bh: u16 = 200;

    lcd.draw_rect(bx, by, bw, bh, COLOR_RED);
    lcd.fill_rect(bx + 2, by + 2, bw - 4, bh - 4, COLOR_BLACK);

    let title = b"ERROR";
    let tw = font.text_width(title);
    let tx = bx as i16 + (bw as i16 - tw as i16) / 2;
    let ty = by as i16 + 30;
    font.draw_str(lcd, flash, title, tx, ty, COLOR_RED, Some(COLOR_BLACK));

    let mut code_line = [0u8; 16];
    let code_len = format_error_code(code, &mut code_line);
    let cw = font.text_width(&code_line[..code_len]);
    let cx = bx as i16 + (bw as i16 - cw as i16) / 2;
    font.draw_str(
        lcd,
        flash,
        &code_line[..code_len],
        cx,
        ty + 28,
        COLOR_WHITE,
        Some(COLOR_BLACK),
    );

    let desc = error_description(code);
    let dw = font.text_width(desc);
    let dx = bx as i16 + (bw as i16 - dw as i16) / 2;
    font.draw_str(lcd, flash, desc, dx, ty + 56, 0xC618, Some(COLOR_BLACK));

    // Show VM program counter if available
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
            ty + 84,
            0xFD20,
            Some(COLOR_BLACK),
        );
    }
    lcd.end_frame();

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
        1 => b"page_main not found",
        2 => b"main program not found",
        3 => b"image not found",
        4 => b"font not found",
        5 => b"no file system in flash",
        6 => b"program execution error",
        7 => b"insufficient memory",
        99 => b"unknown error",
        _ => b"error",
    }
}

/// Convert touch X position to slider value (0-100) relative to widget rect.
fn touch_to_slider_value(abs: &types::Rect, touch_x: u16) -> i16 {
    let x = touch_x as i16;
    if x <= abs.x {
        0
    } else if x >= abs.x + abs.w as i16 {
        100
    } else {
        (((x - abs.x) as u32) * 100 / abs.w as u32) as i16
    }
}

// === Entry Point ===

#[entry]
fn main() -> ! {
    // --- System clock: IRC8M → PLL ×27 → 108MHz ---
    system_init();

    // --- Heap allocator init (must be before any Box/Vec) ---
    heap::init();

    // --- SysTick: 1ms tick counter ---
    systick::init();

    // --- Peripheral init ---
    init_ports();

    let gpio = Gpio::init();
    let usart = Usart::init();
    let mut touch = Touch::init();
    let backlight = backlight::Backlight::init();

    // --- Application context (heap allocated) ---
    let mut ctx = Box::new(Ctx {
        lcd: Lcd::new(gpio),
        flash: Flash::init(),
        tree: widget::WidgetTree::new(),
        fonts: font::FontList::new(),
        images: ImageList::new(),
        strpool: StringPool::new(),
        fs: None,
    });
    ctx.fonts.add(Font::from_embedded(
        &embedded_font::GLYPHS,
        &embedded_font::BITMAP,
        embedded_font::FIRST,
        embedded_font::LAST,
        embedded_font::Y_ADVANCE,
    ));

    // --- Config store (flash sector 0) ---
    let mut cfg = config::ConfigStore::mount_or_format(&ctx.flash);

    // Load touch calibration from config (if saved)
    let mut cal_buf = [0u8; 9];
    if let Some(len) = cfg.read(&ctx.flash, config::KEY_TOUCH_CAL, &mut cal_buf) {
        if len >= 9 {
            if let Some(cal) = touch::CalParams::from_bytes(&cal_buf) {
                touch.cal = cal;
            }
        }
    }

    // === STARTUP SEQUENCE ===

    // 1. Backlight

    delay_ms(500);

    // draw current buffer
    ctx.lcd.fill_rect(0, 0, 800, 480, COLOR_BLACK);
    
    ctx.lcd.begin_frame();
    ctx.lcd.fill_rect(0, 0, 800, 480, COLOR_BLACK);
    ctx.lcd.end_frame();

    backlight.set_brightness(100);

    // 4. Flash filesystem
    let mut error_code: u8 = 0;

    match fs::Fs::mount(&ctx.flash) {
        Ok(f) => ctx.fs = Some(f),
        Err(_) => error_code = ERR_NO_FILESYSTEM,
    }

    // 5. Widget tree root
    let root = ctx.tree.alloc().unwrap();
    {
        let w = ctx.tree.get_mut(root);
        w.size = Size { w: 800, h: 480 };
        w.background_color = COLOR_BLACK;
    }
    ctx.tree.root = root;

    let mut pm = PageManager::new();

    // 6. Load page_main (optional — program can run without pages)
    if error_code == 0 {
        pm.load_page(&mut ctx, b"page_main");
    }

    // 7. Single VM — heap allocated, owns code and function table
    let mut vm = Box::new(Vm::new());

    // 8. Load main program into VM (image header + opcodes)
    if error_code == 0 {
        if let Some(fs_ref) = ctx.fs.as_ref() {
            match fs_ref.find(&ctx.flash, b"main") {
                Some(entry) => {
                    if entry.flags & fs::RES_FLAG_FLASH_EXEC != 0 {
                        // Flash execution: VM reads opcodes from flash on demand
                        if !vm.load_flash(&ctx.flash, entry.offset, entry.size as usize) {
                            error_code = ERR_PROGRAM_ERROR;
                        }
                    } else {
                        // RAM execution: read full image into temp buffer, parse header
                        let img_len = entry.size.min(MAX_CODE_SIZE as u32) as usize;
                        let mut img_buf = alloc::vec![0u8; img_len];
                        fs_ref.read_resource(&ctx.flash, &entry, 0, &mut img_buf);
                        if !vm.load_ram(&img_buf) {
                            error_code = ERR_PROGRAM_ERROR;
                        }
                        // img_buf freed here — VM owns the opcodes via VmCode::Ram
                    }
                }
                None => {
                    error_code = ERR_PROGRAM_NOT_FOUND;
                }
            }
        }
    }

    if error_code == 0 && vm.has_code() {
        // Show first page (if any was loaded)
        if pm.count() > 0 {
            pm.show(0, &mut ctx);
        }

        // Run setup() function — must return 0 for success
        if let Some(entry) = vm.find_by_kind(FunctionKind::Setup) {
            let offset = entry.offset as u16;
            vm.run_callback(offset, &mut ctx);
            let result = vm.pop_result();
            if result != 0 {
                error_code = ERR_PROGRAM_ERROR;
            }
        }

        // Run on_program_start callback (if defined and setup succeeded)
        if error_code == 0 {
            if let Some(entry) = vm.find_by_kind(FunctionKind::OnProgramStart) {
                let offset = entry.offset as u16;
                vm.run_callback(offset, &mut ctx);
            }
        }

        // Full initial render
        if error_code == 0 {
            render::render_all(&mut ctx);

            // Start loop() function (compiler wraps body in while(1){...yield;})
            if let Some(entry) = vm.find_by_kind(FunctionKind::Loop) {
                vm.set_pc(entry.offset as u16);
                vm.state = VmState::Running;
            }
        }
    }

    // Show error if any
    if error_code != 0 {
        show_error(
            &mut ctx.lcd,
            ctx.fonts.embedded(),
            &ctx.flash,
            &usart,
            error_code,
            None,
        );
    }

    // === MAIN LOOP ===

    let mut protocol = Protocol::new();

    loop {
        // --- VM step (only when Running or Yielded) ---
        match vm.state {
            VmState::Running | VmState::Yielded => {
                vm.state = VmState::Running;
                vm.step(&mut ctx);

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

                // loop() is compiler-wrapped in while(1){...yield;} — never halts
            }
            VmState::Waiting => {
                // Non-blocking delay: check if target tick has passed
                if systick::millis().wrapping_sub(vm.wait_until) < 0x8000_0000 {
                    vm.state = VmState::Running;
                }
            }
            _ => {} // Halted, Error, Ready → skip
        }

        // --- USART message handling ---
        while let Some(byte) = usart::rx_read_byte() {
            match protocol.feed(byte, &ctx.flash) {
                RxEvent::None => {}

                RxEvent::Ping => {
                    protocol::send_pong(&usart);
                }

                RxEvent::Restart => {
                    cortex_m::peripheral::SCB::sys_reset();
                }

                RxEvent::MemInfo => {
                    let (free, _largest) = heap::stats();
                    protocol::send_meminfo(&usart, free as u32);
                }

                RxEvent::TouchCalibrate => {
                    let cal = touch::run_calibration(&mut touch, &ctx.lcd);
                    cfg.write(&ctx.flash, config::KEY_TOUCH_CAL, &cal.to_bytes());
                    protocol::send_touch_cal(&usart, &cal);
                    render::render_all(&mut ctx);
                }

                RxEvent::ProgramReady => {
                    // Clear previous state
                    ctx.tree.clear();
                    ctx.strpool.clear();
                    vm.reset();

                    // Re-create root widget
                    let root = ctx.tree.alloc().unwrap();
                    {
                        let w = ctx.tree.get_mut(root);
                        w.size = Size { w: 800, h: 480 };
                        w.background_color = COLOR_BLACK;
                    }
                    ctx.tree.root = root;

                    // Load new program (new image format)
                    let prog = protocol.program_code();
                    if !vm.load_ram(prog) {
                        // Fallback: treat as raw bytecode (no header)
                        vm.load_raw(prog);
                    }
                    protocol.free_program();

                    error_code = 0;

                    // Run setup() if present
                    if let Some(entry) = vm.find_by_kind(FunctionKind::Setup) {
                        let offset = entry.offset as u16;
                        vm.run_callback(offset, &mut ctx);
                        let result = vm.pop_result();
                        if result != 0 {
                            error_code = ERR_PROGRAM_ERROR;
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

                    // Start loop() if present and setup succeeded
                    if error_code == 0 {
                        if let Some(entry) = vm.find_by_kind(FunctionKind::Loop) {
                            vm.set_pc(entry.offset as u16);
                            vm.state = VmState::Running;
                        } else {
                            // No loop — just run opcodes from beginning (legacy compat)
                            vm.state = VmState::Running;
                        }
                    }
                }

                RxEvent::ProgramTooLarge => {
                    protocol::send_error(&usart, ERR_INSUFFICIENT_MEMORY);
                }

                RxEvent::FsReady => {
                    let vm_was_running = vm.state == VmState::Running
                        || vm.state == VmState::Waiting
                        || vm.state == VmState::Yielded;
                    if vm_was_running {
                        vm.state = VmState::Halted;
                    }
                    protocol::send_pong(&usart);
                }

                RxEvent::FsChunkDone => {
                    protocol::send_pong(&usart);
                }

                RxEvent::FsWriteComplete => {
                    protocol::send_pong(&usart);
                    usart.flush();
                    cortex_m::peripheral::SCB::sys_reset();
                }

                RxEvent::UserMessage => {
                    if vm.has_code() {
                        if let Some(entry) = vm.find_by_kind(FunctionKind::OnUserMessage) {
                            let offset = entry.offset as u16;
                            let msg = protocol.user_message();
                            if let Some(arr_id) = vm.alloc_array_from(msg) {
                                vm.enqueue_callback(offset, &[arr_id]);
                            }
                        }
                    }
                }
            }
        }

        // --- Touch handling (if no error) ---
        if error_code == 0 {
            if let Some(event) = touch.poll() {
                if event.kind == touch::TouchEventKind::Press {
                    let hit = touch::hit_test(&ctx.tree, event.x, event.y);
                    if hit.is_some() {
                        ctx.tree.get_mut(hit).flags |= widget::FLAG_PRESSED;

                        // Slider: update value from touch position
                        if ctx.tree.get(hit).kind == widget::KIND_SLIDER {
                            let abs = ctx.tree.absolute_rect(hit);
                            let new_val = touch_to_slider_value(&abs, event.x);
                            ctx.tree.get_mut(hit).value = new_val;
                            if vm.has_code() {
                                let on_click = ctx.tree.get(hit).on_click;
                                if on_click > 0 {
                                    if let Some(entry) = vm.find_func(on_click) {
                                        vm.enqueue_callback(
                                            entry.offset as u16,
                                            &[hit.0 as i32, new_val as i32],
                                        );
                                    }
                                }
                            }
                        }

                        ctx.tree.mark_dirty(hit);
                        render::render_dirty(&mut ctx);
                    }

                    if vm.has_code() {
                        if let Some(entry) = vm.find_by_kind(FunctionKind::OnTouchDown) {
                            vm.enqueue_callback(
                                entry.offset as u16,
                                &[event.x as i32, event.y as i32],
                            );
                        }
                    }
                } else if event.kind == touch::TouchEventKind::Hold {
                    // Slider drag: update pressed slider value
                    let dfs = ctx.tree.dfs_order();
                    for i in 0..dfs.len() {
                        let w = ctx.tree.get(dfs[i]);
                        if w.flags & widget::FLAG_PRESSED != 0
                            && w.kind == widget::KIND_SLIDER
                        {
                            let abs = ctx.tree.absolute_rect(dfs[i]);
                            let new_val = touch_to_slider_value(&abs, event.x);
                            ctx.tree.get_mut(dfs[i]).value = new_val;
                            ctx.tree.mark_dirty(dfs[i]);
                            render::render_dirty(&mut ctx);

                            if vm.has_code() {
                                let on_click = ctx.tree.get(dfs[i]).on_click;
                                if on_click > 0 {
                                    if let Some(entry) = vm.find_func(on_click) {
                                        vm.enqueue_callback(
                                            entry.offset as u16,
                                            &[dfs[i].0 as i32, new_val as i32],
                                        );
                                    }
                                }
                            }
                            break;
                        }
                    }

                    if vm.has_code() {
                        if let Some(entry) = vm.find_by_kind(FunctionKind::OnTouchMove) {
                            vm.enqueue_callback(
                                entry.offset as u16,
                                &[event.x as i32, event.y as i32],
                            );
                        }
                    }
                } else if event.kind == touch::TouchEventKind::Release {
                    let mut clicked_id = widget::WidgetId::NONE;
                    let mut clicked_func: u16 = 0;

                    let dfs = ctx.tree.dfs_order();
                    for i in 0..dfs.len() {
                        let w = ctx.tree.get_mut(dfs[i]);
                        if w.flags & widget::FLAG_PRESSED != 0 {
                            w.flags &= !widget::FLAG_PRESSED;
                            let abs = ctx.tree.absolute_rect(dfs[i]);
                            if abs.contains(event.x, event.y) {
                                clicked_id = dfs[i];
                                clicked_func = ctx.tree.get(dfs[i]).on_click;
                            }
                            ctx.tree.mark_dirty(dfs[i]);
                        }
                    }

                    render::render_dirty(&mut ctx);

                    // Enqueue on_click callback
                    if clicked_id.is_some() && clicked_func > 0 && vm.has_code() {
                        if let Some(entry) = vm.find_func(clicked_func) {
                            vm.enqueue_callback(
                                entry.offset as u16,
                                &[clicked_id.0 as i32],
                            );
                        }
                    }

                    // Enqueue on_tap callback (widget_id, packed x|y)
                    if clicked_id.is_some() && vm.has_code() {
                        let tap_func = ctx.tree.get(clicked_id).on_tap;
                        if tap_func > 0 {
                            if let Some(entry) = vm.find_func(tap_func) {
                                let packed_xy = ((event.x as u32) << 16 | event.y as u32) as i32;
                                vm.enqueue_callback(
                                    entry.offset as u16,
                                    &[clicked_id.0 as i32, packed_xy],
                                );
                            }
                        }
                    }

                    if vm.has_code() {
                        if let Some(entry) = vm.find_by_kind(FunctionKind::OnTouchUp) {
                            vm.enqueue_callback(entry.offset as u16, &[]);
                        }
                    }
                }
            }
        }

        // --- Render dirty widgets + enqueue on_paint callbacks ---
        if error_code == 0 {
            let mut paint_ids = [widget::WidgetId::NONE; 8];
            let mut paint_count: usize = 0;
            {
                let dfs = ctx.tree.dfs_order();
                for i in 0..dfs.len() {
                    let w = ctx.tree.get(dfs[i]);
                    if w.is_dirty() && w.on_paint != 0 && paint_count < 8 {
                        paint_ids[paint_count] = dfs[i];
                        paint_count += 1;
                    }
                }
            }

            render::render_dirty(&mut ctx);

            if vm.has_code() {
                for i in 0..paint_count {
                    let id = paint_ids[i];
                    let paint_func = ctx.tree.get(id).on_paint;
                    if paint_func > 0 {
                        if let Some(entry) = vm.find_func(paint_func) {
                            vm.enqueue_callback(
                                entry.offset as u16,
                                &[id.0 as i32],
                            );
                        }
                    }
                }
            }
        }

        // --- Drain callback queue (runs each to completion, FIFO order) ---
        if vm.has_pending_callbacks() {
            vm.drain_callbacks(&mut ctx);
        }
    }
}
