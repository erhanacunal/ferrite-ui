#![no_std]
#![no_main]

use panic_halt as _;

mod backlight;
mod callback;
mod clip;
mod embedded_font;
mod flash;
mod font;
mod fs;
mod gpio;
mod image;
mod irq;
mod lcd;
mod page;
mod proto;
mod protocol;
mod render;
mod strpool;
mod systick;
mod touch;
mod types;
mod usart;
mod vm;
mod widget;

use callback::{CallbackMeta, NO_CALLBACK};
use cortex_m_rt::entry;
use flash::Flash;
use font::{Font, FontList};
use gpio::Gpio;
use image::ImageList;
use lcd::Lcd;
use page::PageManager;
use protocol::{Protocol, RxEvent};
use touch::Touch;
use types::{Size, COLOR_BLACK, COLOR_RED, COLOR_WHITE};
use usart::Usart;
use vm::{Vm, VmState};
use widget::WidgetTree;

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

// === Max program code size ===

const MAX_CODE_SIZE: usize = 8192;

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
const CFG0_SCS: u32 = 0x03;           // System clock switch [1:0]
const CFG0_SCSS: u32 = 0x0C;          // System clock switch status [3:2]
const CFG0_AHBPSC: u32 = 0xF0;        // AHB prescaler [7:4]
const CFG0_APB1PSC: u32 = 0x700;      // APB1 prescaler [10:8]
const CFG0_APB2PSC: u32 = 0x3800;     // APB2 prescaler [13:11]
const CFG0_ADCPSC: u32 = 0xC000;      // ADC prescaler [15:14]
const CFG0_PLLSEL: u32 = 1 << 16;
const CFG0_PREDV0: u32 = 1 << 17;
const CFG0_PLLMF: u32 = 0xF << 18;   // PLL multiplier [21:18]
const CFG0_USBDPSC: u32 = 0x3 << 22;
const CFG0_CKOUT0SEL: u32 = 0x7 << 24;
const CFG0_PLLMF_4: u32 = 1 << 27;   // PLL multiplier bit 4
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
        core::ptr::write_volatile(RCU_CTL as *mut u32, val & !(CTL_HXTALEN | CTL_CKMEN | CTL_PLLEN));

        // Reset CFG0: prescalers, clock source, PLL config
        let val = core::ptr::read_volatile(RCU_CFG0 as *const u32);
        core::ptr::write_volatile(
            RCU_CFG0 as *mut u32,
            val & !(CFG0_SCS | CFG0_AHBPSC | CFG0_APB1PSC | CFG0_APB2PSC
                | CFG0_ADCPSC | CFG0_ADCPSC_2 | CFG0_CKOUT0SEL
                | CFG0_PLLSEL | CFG0_PREDV0 | CFG0_PLLMF | CFG0_USBDPSC | CFG0_PLLMF_4),
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
        core::ptr::write_volatile(RCU_CFG0 as *mut u32, (val & !(CFG0_PLLMF | CFG0_PLLMF_4)) | PLL_MUL27);

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
        val &= !(0xF << 0);  // PA8:  PP output 50MHz (backlight off until timer init)
        val |= 0x3 << 0;
        val &= !(0xF << 4);  // PA9:  AF PP 50MHz (USART0 TX)
        val |= 0xB << 4;
        val &= !(0xF << 8);  // PA10: floating input (USART0 RX)
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
        core::ptr::write_volatile(
            (GPIOC + 0x10) as *mut u32,
            (1 << 0) | (1 << 1) | (1 << 13),
        );
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
fn show_error(lcd: &Lcd, font: &Font, flash: &Flash, usart: &Usart, code: u8) {
    lcd.fill_rect(0, 0, 800, 480, COLOR_BLACK);

    let bx: u16 = 200;
    let by: u16 = 160;
    let bw: u16 = 400;
    let bh: u16 = 160;

    lcd.fill_rect(bx, by, bw, bh, COLOR_RED);
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

    protocol::send_error(usart, code);
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
        99 => b"unknown error",
        _ => b"error",
    }
}

// === Callback helper ===

/// Run a callback function on the callback VM.
/// Resets the VM, sets PC to the function offset, and runs to completion.
fn run_callback(
    cb_vm: &mut Vm,
    code: &[u8],
    offset: u16,
    tree: &mut WidgetTree,
    lcd: &Lcd,
    flash: &Flash,
    fonts: &mut FontList,
    images: &mut ImageList,
    fs: Option<&fs::Fs>,
) {
    cb_vm.reset();
    cb_vm.set_pc(offset);
    cb_vm.run(code, tree, lcd, flash, fonts, images, fs);
}

// === Entry Point ===

#[entry]
fn main() -> ! {
    // --- System clock: IRC8M → PLL ×27 → 108MHz ---
    system_init();

    // --- SysTick: 1ms tick counter ---
    systick::init();

    // --- Peripheral init ---
    init_ports();

    let gpio = Gpio::init();
    let lcd = Lcd::new(gpio);
    let flash = Flash::init();
    let usart = Usart::init();
    let mut touch = Touch::init();
    let backlight = backlight::Backlight::init();

    // --- Font list (embedded font is always id=0) ---
    let mut fonts = FontList::new();
    fonts.add(Font::from_embedded(
        &embedded_font::GLYPHS,
        &embedded_font::BITMAP,
        embedded_font::FIRST,
        embedded_font::LAST,
        embedded_font::Y_ADVANCE,
    ));

    // --- Image list ---
    let mut images = ImageList::new();

    // === STARTUP SEQUENCE ===

    // 1. Backlight 50%
    backlight.set_brightness(100);

    // 2. Black screen
    lcd.fill_rect(0, 0, 800, 480, COLOR_BLACK);

    // 3. Splash text
    fonts.embedded().draw_str(
        &lcd,
        &flash,
        b"ferrite-ui",
        10,
        24,
        COLOR_WHITE,
        Some(COLOR_BLACK),
    );

    // 4. Flash filesystem
    let mut error_code: u8 = 0;

    let fs = match fs::Fs::mount(&flash) {
        Ok(f) => Some(f),
        Err(_) => {
            error_code = ERR_NO_FILESYSTEM;
            None
        }
    };

    // 5. Widget tree + root
    let mut tree = WidgetTree::new();
    let root = tree.alloc().unwrap();
    {
        let w = tree.get_mut(root);
        w.size = Size { w: 800, h: 480 };
        w.background_color = COLOR_BLACK;
    }
    tree.root = root;

    let mut pm = PageManager::new();
    let mut code_buf = [0u8; MAX_CODE_SIZE];
    let mut code_len: usize = 0;

    // 6. Load page_main (optional — program can run without pages)
    if error_code == 0 {
        let fs_ref = fs.as_ref().unwrap();
        pm.load_page(&mut tree, &lcd, fs_ref, &flash, b"page_main");
    }

    // 7. Load main program (if no error)
    if error_code == 0 {
        let fs_ref = fs.as_ref().unwrap();
        match fs_ref.find(&flash, b"main") {
            Some(entry) => {
                code_len = entry.size.min(MAX_CODE_SIZE as u32) as usize;
                fs_ref.read_resource(&flash, &entry, 0, &mut code_buf[..code_len]);
            }
            None => {
                error_code = ERR_PROGRAM_NOT_FOUND;
            }
        }
    }

    // 8. Load callback metadata
    let cb_meta = if error_code == 0 {
        if let Some(fs_ref) = fs.as_ref() {
            CallbackMeta::load(fs_ref, &flash, b"main").unwrap_or(CallbackMeta::new())
        } else {
            CallbackMeta::new()
        }
    } else {
        CallbackMeta::new()
    };

    // 9. Prepare VMs
    let mut vm = Vm::new();
    let mut cb_vm = Vm::new();

    if error_code == 0 && code_len > 0 {
        // Show first page (if any was loaded)
        if pm.count() > 0 {
            pm.show(0, &mut tree, &lcd, &flash, &fonts, &images);
        }

        // Run on_program_start callback
        if cb_meta.on_program_start != NO_CALLBACK {
            run_callback(
                &mut cb_vm,
                &code_buf[..code_len],
                cb_meta.on_program_start,
                &mut tree,
                &lcd,
                &flash,
                &mut fonts,
                &mut images,
                fs.as_ref(),
            );
        }

        // Full initial render
        render::render_all(&mut tree, &lcd, &flash, &fonts, &images);

        // Start main VM
        vm.state = VmState::Running;
    }

    // Show error if any
    if error_code != 0 {
        show_error(&lcd, fonts.embedded(), &flash, &usart, error_code);
    }

    // === MAIN LOOP ===

    let mut protocol = Protocol::new();

    loop {
        // --- VM step (only when Running or Yielded) ---
        match vm.state {
            VmState::Running | VmState::Yielded => {
                vm.state = VmState::Running;
                vm.step(
                    &code_buf[..code_len],
                    &mut tree,
                    &lcd,
                    &flash,
                    &mut fonts,
                    &mut images,
                    fs.as_ref(),
                );

                if vm.state == VmState::Error {
                    error_code = ERR_PROGRAM_ERROR;
                    show_error(&lcd, fonts.embedded(), &flash, &usart, error_code);
                }
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
            match protocol.feed(byte, &flash) {
                RxEvent::None => {}

                RxEvent::Ping => {
                    protocol::send_pong(&usart);
                }

                RxEvent::Restart => {
                    cortex_m::peripheral::SCB::sys_reset();
                }

                RxEvent::ProgramReady => {
                    let prog = protocol.program_code();
                    let new_len = prog.len().min(MAX_CODE_SIZE);
                    code_buf[..new_len].copy_from_slice(&prog[..new_len]);
                    code_len = new_len;

                    vm.reset();
                    vm.state = VmState::Running;
                    error_code = 0;
                }

                RxEvent::FsReady => {
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
                    // Fire on_user_message callback with message data as VM array
                    if cb_meta.on_user_message != NO_CALLBACK && code_len > 0 {
                        let msg = protocol.user_message();
                        cb_vm.reset();
                        if let Some(arr_id) = cb_vm.alloc_array_from(msg) {
                            cb_vm.set_pc(cb_meta.on_user_message);
                            cb_vm.push_arg(arr_id);
                            cb_vm.run(
                                &code_buf[..code_len],
                                &mut tree,
                                &lcd,
                                &flash,
                                &mut fonts,
                                &mut images,
                                fs.as_ref(),
                            );
                        }
                    }
                }
            }
        }

        // --- Touch handling (if no error) ---
        if error_code == 0 {
            if let Some(event) = touch.poll() {
                if event.kind == touch::TouchEventKind::Press {
                    let hit = touch::hit_test(&tree, event.x, event.y);
                    if hit.is_some() {
                        tree.get_mut(hit).flags |= widget::FLAG_PRESSED;
                        tree.mark_dirty(hit);
                        render::render_dirty(&mut tree, &lcd, &flash, &fonts, &images);
                    }
                } else if event.kind == touch::TouchEventKind::Release {
                    // Find pressed widget, release it, and fire on_click callback
                    let mut clicked_id = widget::WidgetId::NONE;
                    let mut clicked_func: u16 = 0;

                    let (dfs, count) = tree.dfs_order();
                    for i in 0..count {
                        let w = tree.get_mut(dfs[i]);
                        if w.flags & widget::FLAG_PRESSED != 0 {
                            w.flags &= !widget::FLAG_PRESSED;
                            // Check if release is still on the widget (click = press + release)
                            let abs = tree.absolute_rect(dfs[i]);
                            if abs.contains(event.x, event.y) {
                                clicked_id = dfs[i];
                                clicked_func = tree.get(dfs[i]).on_click;
                            }
                            tree.mark_dirty(dfs[i]);
                        }
                    }

                    render::render_dirty(&mut tree, &lcd, &flash, &fonts, &images);

                    // Fire on_click callback if defined
                    if clicked_id.is_some() && clicked_func > 0 && code_len > 0 {
                        if let Some((offset, _arg_count)) = cb_meta.find_func(clicked_func) {
                            cb_vm.reset();
                            cb_vm.set_pc(offset);
                            // Push widget_id as argument
                            cb_vm.push_arg(clicked_id.0 as i32);
                            cb_vm.run(
                                &code_buf[..code_len],
                                &mut tree,
                                &lcd,
                                &flash,
                                &mut fonts,
                                &mut images,
                                fs.as_ref(),
                            );
                        }
                    }

                    // Fire on_tap callback if defined (widget_id, packed x|y)
                    if clicked_id.is_some() && code_len > 0 {
                        let tap_func = tree.get(clicked_id).on_tap;
                        if tap_func > 0 {
                            if let Some((offset, _arg_count)) = cb_meta.find_func(tap_func) {
                                cb_vm.reset();
                                cb_vm.set_pc(offset);
                                cb_vm.push_arg(clicked_id.0 as i32);
                                let packed_xy = ((event.x as u32) << 16 | event.y as u32) as i32;
                                cb_vm.push_arg(packed_xy);
                                cb_vm.run(
                                    &code_buf[..code_len],
                                    &mut tree,
                                    &lcd,
                                    &flash,
                                    &mut fonts,
                                    &mut images,
                                    fs.as_ref(),
                                );
                            }
                        }
                    }
                }
            }
        }

        // --- Render any dirty widgets (from VM, callbacks, or other state changes) ---
        if error_code == 0 {
            // Collect widgets with on_paint before render clears dirty flags
            let mut paint_ids = [widget::WidgetId::NONE; 8];
            let mut paint_count: usize = 0;
            {
                let (dfs, count) = tree.dfs_order();
                for i in 0..count {
                    let w = tree.get(dfs[i]);
                    if w.is_dirty() && w.on_paint != 0 && paint_count < 8 {
                        paint_ids[paint_count] = dfs[i];
                        paint_count += 1;
                    }
                }
            }

            render::render_dirty(&mut tree, &lcd, &flash, &fonts, &images);

            // Fire on_paint callbacks after widget background is rendered
            if code_len > 0 {
                for i in 0..paint_count {
                    let id = paint_ids[i];
                    let paint_func = tree.get(id).on_paint;
                    if paint_func > 0 {
                        if let Some((offset, _arg_count)) = cb_meta.find_func(paint_func) {
                            cb_vm.reset();
                            cb_vm.set_pc(offset);
                            cb_vm.push_arg(id.0 as i32);
                            cb_vm.run(
                                &code_buf[..code_len],
                                &mut tree,
                                &lcd,
                                &flash,
                                &mut fonts,
                                &mut images,
                                fs.as_ref(),
                            );
                        }
                    }
                }
            }
        }
    }
}
