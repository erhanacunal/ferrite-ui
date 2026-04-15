/// Custom panic handler — displays error on LCD and sends via USART.
///
/// Uses only stack memory and ROM data (no heap).
/// Draws a red error box with "PANIC" message and source location.
/// Also sends the panic message via USART for serial debugging.

use core::panic::PanicInfo;

use crate::embedded_font;
use crate::flash::Flash;
use crate::font::Font;
use crate::gpio::Gpio;
use crate::lcd::Lcd;
use crate::types::{COLOR_BLACK, COLOR_RED, COLOR_WHITE};

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

/// Format u32 as decimal into a stack buffer. Returns the used slice.
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
    // Reverse
    let len = i;
    let half = len / 2;
    for j in 0..half {
        let tmp = buf[j];
        buf[j] = buf[len - 1 - j];
        buf[len - 1 - j] = tmp;
    }
    len
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Disable interrupts to prevent further issues
    cortex_m::interrupt::disable();

    // Construct LCD from raw GPIO (zero-sized, no state needed)
    let gpio = Gpio::init();
    let lcd = Lcd::new(gpio);

    // Construct embedded font (all data in ROM)
    let font = Font::from_embedded(
        &embedded_font::GLYPHS,
        &embedded_font::BITMAP,
        embedded_font::FIRST,
        embedded_font::LAST,
        embedded_font::Y_ADVANCE,
    );

    // Dummy flash (embedded font reads from ROM, never touches flash hardware)
    let flash = Flash::new();

    // Reset FPGA to buffer 0: select back buffer 0, then swap to front
    // This ensures our draws go to the displayed buffer regardless of
    // what state the double-buffer was in before the panic.
    lcd.send_command(0x05); // CMD_BACK_SELECT
    lcd.send_data(0);       // buffer 0
    lcd.send_command(0x04); // CMD_FRONT_SWAP
    lcd.send_data(0);       // display buffer 0

    lcd.fill_rect(0, 0, 800, 480, COLOR_BLACK);

    // Red border box
    let bx: u16 = 150;
    let by: u16 = 120;
    let bw: u16 = 500;
    let bh: u16 = 240;
    lcd.fill_rect(bx, by, bw, 2, COLOR_RED);
    lcd.fill_rect(bx, by + bh - 2, bw, 2, COLOR_RED);
    lcd.fill_rect(bx, by, 2, bh, COLOR_RED);
    lcd.fill_rect(bx + bw - 2, by, 2, bh, COLOR_RED);

    // "PANIC" title
    let title = b"PANIC";
    let tw = font.text_width(title);
    let tx = bx as i16 + (bw as i16 - tw as i16) / 2;
    font.draw_str(&lcd, &flash, title, tx, by as i16 + 40, COLOR_RED, Some(COLOR_BLACK));

    // Send via USART
    usart_write(b"\r\nPANIC: ");

    // Extract and display location
    if let Some(location) = info.location() {
        let file = location.file();
        let line = location.line();

        // File name — extract just the filename (after last / or \)
        let file_bytes = file.as_bytes();
        let mut name_start = 0usize;
        for i in 0..file_bytes.len() {
            if file_bytes[i] == b'/' || file_bytes[i] == b'\\' {
                name_start = i + 1;
            }
        }
        let short_name = &file_bytes[name_start..];

        // Format: "file.rs:123"
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
        font.draw_str(&lcd, &flash, loc, lx, by as i16 + 80, COLOR_WHITE, Some(COLOR_BLACK));

        // USART: full path
        usart_write(file_bytes);
        usart_write(b":");
        usart_write(&num_buf[..num_len]);
    }

    usart_write(b"\r\n");

    // "Device halted" message
    let msg = b"Device halted";
    let mw = font.text_width(msg);
    let mx = bx as i16 + (bw as i16 - mw as i16) / 2;
    font.draw_str(&lcd, &flash, msg, mx, by as i16 + 140, 0x8410, Some(COLOR_BLACK));

    // "Reset to recover" message
    let hint = b"Reset to recover";
    let hw = font.text_width(hint);
    let hx = bx as i16 + (bw as i16 - hw as i16) / 2;
    font.draw_str(&lcd, &flash, hint, hx, by as i16 + 170, 0x8410, Some(COLOR_BLACK));

    loop {
        cortex_m::asm::wfi();
    }
}
