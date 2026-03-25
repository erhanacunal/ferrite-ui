#![no_std]
#![no_main]

use panic_halt as _;

mod backlight;
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
mod touch;
mod types;
mod usart;
mod vm;
mod widget;

use cortex_m_rt::entry;
use flash::Flash;
use font::{Font, FontList};
use image::ImageList;
use gpio::Gpio;
use lcd::Lcd;
use page::PageManager;
use protocol::{Protocol, RxEvent};
use touch::Touch;
use types::{Color, Edges, Offset, Size, COLOR_BLACK, COLOR_RED, COLOR_WHITE};
use usart::Usart;
use vm::{Vm, VmState};
use widget::WidgetTree;

// === Donanım adresleri (port init) ===

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

// === Hata kodları ===

const ERR_PAGE_NOT_FOUND: u8 = 1;
const ERR_PROGRAM_NOT_FOUND: u8 = 2;
#[allow(dead_code)]
const ERR_IMAGE_NOT_FOUND: u8 = 3;
#[allow(dead_code)]
const ERR_FONT_NOT_FOUND: u8 = 4;
const ERR_NO_FILESYSTEM: u8 = 5;
const ERR_PROGRAM_ERROR: u8 = 6;

// === Program kodu max boyutu ===

const MAX_CODE_SIZE: usize = 1024;

/// Tüm GPIO portlarını, AFIO, TIMER2 PWM ve flash wait state başlat.
fn init_ports() {
    unsafe {
        // --- Flash wait state = 2 (108MHz için gerekli) ---
        let val = core::ptr::read_volatile(FMC_WS as *const u32);
        core::ptr::write_volatile(FMC_WS as *mut u32, (val & !0x7) | 2);

        // --- RCU: Peripheral clock enable ---
        // AFEN(0) | PAEN(2) | PBEN(3) | PCEN(4) | PDEN(5) | SPI0EN(12) | USART0EN(14)
        let val = core::ptr::read_volatile(RCU_APB2EN as *const u32);
        core::ptr::write_volatile(RCU_APB2EN as *mut u32, val | 0x503D);

        // --- JTAG disable, SWD enable: SWJ_CFG = 010 ---
        let val = core::ptr::read_volatile(AFIO_PCF0 as *const u32);
        core::ptr::write_volatile(AFIO_PCF0 as *mut u32, (val & 0xF8FF_FFFF) | 0x0200_0000);

        // --- GPIOB: tüm pinler push-pull output 50MHz (LCD 16-bit data bus) ---
        core::ptr::write_volatile((GPIOB + 0x00) as *mut u32, 0x3333_3333);
        core::ptr::write_volatile((GPIOB + 0x04) as *mut u32, 0x3333_3333);

        // --- GPIOA CTL0 (PA0-PA7) ---
        core::ptr::write_volatile((GPIOA + 0x00) as *mut u32, 0xB4B3_3334);

        // --- GPIOA CTL1 (PA8-PA15) ---
        let ctl1 = (GPIOA + 0x04) as *mut u32;
        let mut val = core::ptr::read_volatile(ctl1);
        val &= !(0xF << 0);  // PA8:  AF PP 50MHz (TIMER0_CH0 — backlight PWM)
        val |= 0xB << 0;
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

        // --- GPIOA initial pin states ---
        core::ptr::write_volatile(
            (GPIOA + 0x10) as *mut u32,
            (1 << 2) | (1 << 4) | (1 << 11) | (1 << 12),
        );
        core::ptr::write_volatile((GPIOA + 0x14) as *mut u32, (1 << 1) | (1 << 3));

        // --- GPIOC CTL0 (PC0-PC7) ---
        core::ptr::write_volatile((GPIOC + 0x00) as *mut u32, 0x4444_4477);

        // --- GPIOC CTL1 (PC8-PC15) ---
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

        // --- GPIOC initial pin states ---
        core::ptr::write_volatile((GPIOC + 0x14) as *mut u32, 1 << 12);
        core::ptr::write_volatile(
            (GPIOC + 0x10) as *mut u32,
            (1 << 0) | (1 << 1) | (1 << 13),
        );
        core::ptr::write_volatile((GPIOC + 0x10) as *mut u32, 0x03FC_0000);

        // --- GPIOD: PD2 PP output 50MHz ---
        let ctl0 = (GPIOD + 0x00) as *mut u32;
        let mut val = core::ptr::read_volatile(ctl0);
        val &= !(0xF << 8);
        val |= 0x3 << 8;
        core::ptr::write_volatile(ctl0, val);

        // --- TIMER2 PWM: display brightness (full remap → PC6-PC9) ---
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

        // --- Backup domain: LXTAL disable ---
        let val = core::ptr::read_volatile(PMU_CTL as *const u32);
        core::ptr::write_volatile(PMU_CTL as *mut u32, val | (1 << 8));
        let val = core::ptr::read_volatile(RCU_BDCTL as *const u32);
        core::ptr::write_volatile(RCU_BDCTL as *mut u32, val & !1);
    }
}

// === Hata gösterim ===

/// Ekrana hata kutusu çiz ve USART'tan hata gönder.
fn show_error(lcd: &Lcd, font: &Font, flash: &Flash, usart: &Usart, code: u8) {
    // Ekranı siyaha boya
    lcd.fill_rect(0, 0, 800, 480, COLOR_BLACK);

    // Hata kutusu (ortalanmış)
    let bx: u16 = 200;
    let by: u16 = 160;
    let bw: u16 = 400;
    let bh: u16 = 160;

    // Kırmızı kenar
    lcd.fill_rect(bx, by, bw, bh, COLOR_RED);
    lcd.fill_rect(bx + 2, by + 2, bw - 4, bh - 4, COLOR_BLACK);

    // "ERROR" başlığı
    let title = b"ERROR";
    let tw = font.text_width(title);
    let tx = bx as i16 + (bw as i16 - tw as i16) / 2;
    let ty = by as i16 + 30;
    font.draw_str(lcd, flash, title, tx, ty, COLOR_RED, Some(COLOR_BLACK));

    // Hata kodu
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

    // Hata açıklaması
    let desc = error_description(code);
    let dw = font.text_width(desc);
    let dx = bx as i16 + (bw as i16 - dw as i16) / 2;
    font.draw_str(lcd, flash, desc, dx, ty + 56, 0xC618, Some(COLOR_BLACK));

    // USART'tan hata gönder
    protocol::send_error(usart, code);
}

/// Hata kodunu "Code: NN" formatına çevir
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

/// Hata kodu açıklaması
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

// === Entry Point ===

#[entry]
fn main() -> ! {
    // --- Çevre birimi başlatma ---
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

    // 1. Backlight %50
    backlight.set_brightness(50);

    // 2. Ekran siyah
    lcd.fill_rect(0, 0, 800, 480, COLOR_BLACK);

    // 3. Sol üstte "ferrite-ui" yazısı
    fonts.embedded().draw_str(
        &lcd,
        &flash,
        b"ferrite-ui",
        10,
        24,
        COLOR_WHITE,
        Some(COLOR_BLACK),
    );

    // 4. Flash dosya sistemi kontrolü
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

    // 6. page_main yükle (FS varsa)
    if error_code == 0 {
        let fs_ref = fs.as_ref().unwrap();
        if pm
            .load_page(&mut tree, &lcd, fs_ref, &flash, b"page_main")
            .is_none()
        {
            error_code = ERR_PAGE_NOT_FOUND;
        }
    }

    // 7. main program ara (hata yoksa)
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

    // 8. VM hazırla (hata yoksa)
    let mut vm = Vm::new();

    if error_code == 0 && code_len > 0 {
        // Sayfayı göster
        pm.show(0, &mut tree, &lcd, &flash, &fonts, &images);
        // VM Ready → Running olarak işaretle, main loop'ta step çalışacak
        vm.state = VmState::Running;
    }

    // Hata varsa ekrana göster
    if error_code != 0 {
        show_error(&lcd, fonts.embedded(), &flash, &usart, error_code);
    }

    // === ANA DÖNGÜ ===

    let mut protocol = Protocol::new();

    loop {
        // --- VM step (sadece Running veya Yielded ise) ---
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
            _ => {} // Halted, Error, Ready → skip
        }

        // --- USART mesaj işleme ---
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
                    // Yeni program yükle — code_buf'a kopyala, VM'i sıfırla
                    let prog = protocol.program_code();
                    let new_len = prog.len().min(MAX_CODE_SIZE);
                    code_buf[..new_len].copy_from_slice(&prog[..new_len]);
                    code_len = new_len;

                    vm.reset();
                    vm.state = VmState::Running;
                    error_code = 0;
                }

                RxEvent::FsWriteComplete => {
                    cortex_m::peripheral::SCB::sys_reset();
                }
            }
        }

        // --- Touch işleme (hata yoksa) ---
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
                    let (dfs, count) = tree.dfs_order();
                    for i in 0..count {
                        let w = tree.get_mut(dfs[i]);
                        if w.flags & widget::FLAG_PRESSED != 0 {
                            w.flags &= !widget::FLAG_PRESSED;
                            tree.mark_dirty(dfs[i]);
                        }
                    }
                    render::render_dirty(&mut tree, &lcd, &flash, &fonts, &images);
                }
            }
        }
    }
}
