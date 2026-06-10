//! Custom panic handler — displays error on LCD and sends via USART.
//!
//! Uses only stack memory and ROM data (no heap). Draws a red error box with a
//! "PANIC" message and the source location, and mirrors it over USART0.
//!
//! Concrete `Lcd`/`Flash`/`Gpio` come from this BSP; `Font`/`embedded_font`/
//! `types` come from the framework crate.

use core::panic::PanicInfo;

use crate::gpio::Gpio;
use ferrite_core::embedded_font;
use ferrite_core::font::Font;
use ferrite_core::types::{COLOR_BLACK, COLOR_RED, COLOR_WHITE};

const USART_BASE: u32 = 0x4001_3800;
const USART_STAT: u32 = USART_BASE;
const USART_DATA: u32 = USART_BASE + 0x04;
const STAT_TBE: u32 = 1 << 7;

/// Send a byte via USART0 (raw register access, no struct needed).
fn usart_byte(b: u8) {
    unsafe {
        while core::ptr::read_volatile(USART_STAT as *const u32) & STAT_TBE == 0 {}
        core::ptr::write_volatile(USART_DATA as *mut u32, b as u32);
    }
}

/// Send a byte slice via USART0.
fn usart_write(data: &[u8]) {
    for &b in data {
        usart_byte(b);
    }
}

/// Format u32 as decimal into a stack buffer. Returns the used length.
fn fmt_u32(val: u32, buf: &mut [u8; 10]) -> usize {
    if val == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut n = val;
    let mut i = 0usize;
    while n > 0 && i < 10 {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    let len = i;
    let half = len / 2;
    for j in 0..half {
        buf.swap(j, len - 1 - j);
    }
    len
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    cortex_m::interrupt::disable();

    let gpio = Gpio::init();
    let lcd = crate::lcd::new(gpio);

    let font = Font::from_embedded(
        &embedded_font::GLYPHS,
        &embedded_font::CODEPOINTS,
        &embedded_font::BITMAP,
        embedded_font::Y_ADVANCE,
        embedded_font::BPP,
    );

    let flash = crate::flash::new();

    // Reset FPGA to buffer 0 so our draws land on the displayed buffer.
    lcd.send_command(0x05);
    lcd.send_data(0);
    lcd.send_command(0x04);
    lcd.send_data(0);

    lcd.fill_rect(0, 0, 800, 480, COLOR_BLACK);

    let bx: u16 = 150;
    let by: u16 = 120;
    let bw: u16 = 500;
    let bh: u16 = 240;
    lcd.fill_rect(bx, by, bw, 2, COLOR_RED);
    lcd.fill_rect(bx, by + bh - 2, bw, 2, COLOR_RED);
    lcd.fill_rect(bx, by, 2, bh, COLOR_RED);
    lcd.fill_rect(bx + bw - 2, by, 2, bh, COLOR_RED);

    let title = b"PANIC";
    let tw = font.text_width(title);
    let tx = bx as i16 + (bw as i16 - tw as i16) / 2;
    font.draw_str(
        &lcd,
        &flash,
        title,
        tx,
        by as i16 + 40,
        COLOR_RED,
        Some(COLOR_BLACK),
    );

    usart_write(b"\r\nPANIC: ");

    if let Some(location) = info.location() {
        let file = location.file();
        let line = location.line();

        let file_bytes = file.as_bytes();
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

        let mut num_buf = [0u8; 10];
        let num_len = fmt_u32(line, &mut num_buf);
        loc_buf[pos..pos + num_len].copy_from_slice(&num_buf[..num_len]);
        pos += num_len;

        let loc = &loc_buf[..pos];
        let lw = font.text_width(loc);
        let lx = bx as i16 + (bw as i16 - lw as i16) / 2;
        font.draw_str(
            &lcd,
            &flash,
            loc,
            lx,
            by as i16 + 80,
            COLOR_WHITE,
            Some(COLOR_BLACK),
        );

        usart_write(file_bytes);
        usart_write(b":");
        usart_write(&num_buf[..num_len]);
    }

    usart_write(b"\r\n");

    let msg = b"Device halted";
    let mw = font.text_width(msg);
    let mx = bx as i16 + (bw as i16 - mw as i16) / 2;
    font.draw_str(
        &lcd,
        &flash,
        msg,
        mx,
        by as i16 + 140,
        0x8410,
        Some(COLOR_BLACK),
    );

    let hint = b"Reset to recover";
    let hw = font.text_width(hint);
    let hx = bx as i16 + (bw as i16 - hw as i16) / 2;
    font.draw_str(
        &lcd,
        &flash,
        hint,
        hx,
        by as i16 + 170,
        0x8410,
        Some(COLOR_BLACK),
    );

    loop {
        cortex_m::asm::wfi();
    }
}
