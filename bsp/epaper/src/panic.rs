//! Panic handler for the e-paper BSP — draws an error box on the EPD and halts.
//!
//! Names concrete `Lcd`/`Flash` backends, so it lives in the BSP rather than the
//! device-agnostic framework lib.

use ferrite_core::embedded_font;
use ferrite_core::font::Font;
use ferrite_core::types::{COLOR_BLACK, COLOR_RED, COLOR_WHITE};

use crate::lcd;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    if lcd::epaper::is_ready() {
        let lcd = lcd::new();
        let font = Font::from_embedded(
            &embedded_font::GLYPHS,
            &embedded_font::CODEPOINTS,
            &embedded_font::BITMAP,
            embedded_font::Y_ADVANCE,
            embedded_font::BPP,
        );
        let flash = crate::flash::new();

        let w = lcd::epaper::EPD_WIDTH;
        let h = lcd::epaper::EPD_HEIGHT;
        lcd.fill_rect(0, 0, w, h, COLOR_WHITE);

        let bw: u16 = 520;
        let bh: u16 = 230;
        let bx: u16 = (w - bw) / 2;
        let by: u16 = (h - bh) / 2;

        lcd.draw_rect(bx, by, bw, bh, COLOR_RED);
        lcd.fill_rect(bx + 2, by + 2, bw - 4, bh - 4, COLOR_WHITE);

        let title = b"PANIC";
        let tw = font.text_width(title);
        let tx = bx as i16 + (bw as i16 - tw as i16) / 2;
        font.draw_str(&lcd, &flash, title, tx, by as i16 + 42, COLOR_RED, Some(COLOR_WHITE));

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
            font.draw_str(&lcd, &flash, loc_line, lx, by as i16 + 84, COLOR_BLACK, Some(COLOR_WHITE));
        }

        let msg = b"Device halted";
        let mw = font.text_width(msg);
        let mx = bx as i16 + (bw as i16 - mw as i16) / 2;
        font.draw_str(&lcd, &flash, msg, mx, by as i16 + 138, COLOR_BLACK, Some(COLOR_WHITE));

        let hint = b"Reset to recover";
        let hw = font.text_width(hint);
        let hx = bx as i16 + (bw as i16 - hw as i16) / 2;
        font.draw_str(&lcd, &flash, hint, hx, by as i16 + 168, COLOR_BLACK, Some(COLOR_WHITE));

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
