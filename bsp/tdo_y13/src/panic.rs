//! Custom panic handler — displays error on LCD and sends via USART.
//!
//! Uses only stack memory and ROM data (no heap). Draws a red error box
//! with a "PANIC" message and the source location, and mirrors it over
//! UART2. Renders text using the raw embedded font data directly.

use core::panic::PanicInfo;

use ferrite_core::embedded_font;
use ferrite_core::types::{COLOR_BLACK, COLOR_RED, COLOR_WHITE};

// === Framebuffer access ===

const WIDTH: usize = 480;
const HEIGHT: usize = 480;

fn fb() -> &'static mut [u32] {
    // The DEBE framebuffer is ARGB8888 (one u32 per pixel), so index it as u32.
    let ptr = unsafe { crate::lcd::framebuffer_mut_ptr() as *mut u32 };
    unsafe { core::slice::from_raw_parts_mut(ptr, WIDTH * HEIGHT) }
}

fn fb_pixel(x: usize, y: usize, color: u16) {
    if x < WIDTH && y < HEIGHT {
        // Colors are RGB565; convert to the framebuffer's ARGB8888 word.
        fb()[y * WIDTH + x] = crate::lcd::rgb565_to_argb8888(color);
    }
}

fn fb_fill_rect(x: u16, y: u16, w: u16, h: u16, color: u16) {
    let x = (x as usize).min(WIDTH - 1);
    let y = (y as usize).min(HEIGHT - 1);
    let w = (w as usize).min(WIDTH - x);
    let h = (h as usize).min(HEIGHT - y);
    for row in 0..h {
        for col in 0..w {
            fb_pixel(x + col, y + row, color);
        }
    }
}

// === Embedded font rendering ===

const Y_ADVANCE: u8 = 15;

fn find_glyph(cp: u16) -> Option<usize> {
    embedded_font::CODEPOINTS.binary_search(&cp).ok()
}

fn draw_glyph(glyph: &ferrite_core::gfx_glyph::GfxGlyph, x: i16, y: i16, fg: u16, bg: Option<u16>) {
    let left = x + glyph.x_offset as i16;
    let top = y + (Y_ADVANCE as i16 - glyph.height as i16 - glyph.y_offset as i16);
    let bitmap = &embedded_font::BITMAP[glyph.bitmap_offset as usize..];

    let bpp = embedded_font::BPP;
    if bpp == 1 {
        for row in 0..glyph.height as usize {
            for col in 0..glyph.width as usize {
                let byte_idx = row * ((glyph.width as usize + 7) / 8) + col / 8;
                let bit = 7 - (col % 8);
                if byte_idx < bitmap.len() && (bitmap[byte_idx] >> bit) & 1 != 0 {
                    fb_pixel((left + col as i16) as usize, (top + row as i16) as usize, fg);
                } else if let Some(bg) = bg {
                    fb_pixel((left + col as i16) as usize, (top + row as i16) as usize, bg);
                }
            }
        }
    } else {
        for row in 0..glyph.height as usize {
            for col in 0..glyph.width as usize {
                let pixel_idx = row * glyph.width as usize + col;
                let byte_idx = pixel_idx / 4;
                let shift = 6 - (pixel_idx % 4) * 2;
                if byte_idx < bitmap.len() {
                    let val = (bitmap[byte_idx] >> shift) & 0x3;
                    if val != 0 {
                        fb_pixel((left + col as i16) as usize, (top + row as i16) as usize, fg);
                    } else if let Some(bg) = bg {
                        fb_pixel((left + col as i16) as usize, (top + row as i16) as usize, bg);
                    }
                }
            }
        }
    }
}

fn draw_str(text: &[u8], x: i16, y: i16, fg: u16, bg: Option<u16>) -> i16 {
    let mut cx = x;
    let mut pos = 0usize;
    while pos < text.len() {
        let byte = text[pos];
        let (cp, len): (u16, usize);
        if byte < 0x80 { cp = byte as u16; len = 1; }
        else if byte < 0xE0 {
            if pos + 1 >= text.len() { pos += 1; continue; }
            cp = ((byte as u16 & 0x1F) << 6) | (text[pos + 1] as u16 & 0x3F); len = 2;
        } else if byte < 0xF0 {
            if pos + 2 >= text.len() { pos += 1; continue; }
            cp = ((byte as u16 & 0x0F) << 12) | ((text[pos + 1] as u16 & 0x3F) << 6) | (text[pos + 2] as u16 & 0x3F); len = 3;
        } else { cp = 0x003F; len = 1; pos += 1; while pos < text.len() && text[pos] & 0xC0 == 0x80 { pos += 1; } continue; }
        pos += len;
        if let Some(idx) = find_glyph(cp) {
            let glyph = &embedded_font::GLYPHS[idx];
            draw_glyph(glyph, cx, y, fg, bg);
            cx += glyph.x_advance as i16;
        }
    }
    cx
}

fn text_width(text: &[u8]) -> i16 {
    let mut w = 0i16;
    let mut pos = 0usize;
    while pos < text.len() {
        let byte = text[pos];
        let (cp, len): (u16, usize);
        if byte < 0x80 { cp = byte as u16; len = 1; }
        else if byte < 0xE0 {
            if pos + 1 >= text.len() { pos += 1; continue; }
            cp = ((byte as u16 & 0x1F) << 6) | (text[pos + 1] as u16 & 0x3F); len = 2;
        } else if byte < 0xF0 {
            if pos + 2 >= text.len() { pos += 1; continue; }
            cp = ((byte as u16 & 0x0F) << 12) | ((text[pos + 1] as u16 & 0x3F) << 6) | (text[pos + 2] as u16 & 0x3F); len = 3;
        } else { cp = 0x003F; len = 1; pos += 1; while pos < text.len() && text[pos] & 0xC0 == 0x80 { pos += 1; } continue; }
        pos += len;
        if let Some(idx) = find_glyph(cp) {
            w += embedded_font::GLYPHS[idx].x_advance as i16;
        }
    }
    w
}

// === USART output ===

const UART2_BASE: u32 = 0x01C25800;
const USR_REG: u32 = UART2_BASE + 0x7C;
const USR_TX_NOT_FULL: u32 = 1 << 1;
const USART_DATA: u32 = UART2_BASE + 0x04;

fn usart_byte(b: u8) {
    unsafe {
        while ((USR_REG as *const u32).read_volatile() & USR_TX_NOT_FULL) == 0 {}
        (USART_DATA as *mut u32).write_volatile(b as u32);
    }
}

fn usart_write(data: &[u8]) {
    for &b in data { usart_byte(b); }
}

fn fmt_u32(val: u32, buf: &mut [u8; 10]) -> usize {
    if val == 0 { buf[0] = b'0'; return 1; }
    let mut n = val;
    let mut i = 0usize;
    while n > 0 && i < 10 { buf[i] = b'0' + (n % 10) as u8; n /= 10; i += 1; }
    let len = i;
    for j in 0..len/2 { buf.swap(j, len - 1 - j); }
    len
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // ARMv5TE (ARM926EJ-S) has no CPS instruction — mask IRQ/FIQ via CPSR (HAL helper).
    let _ = f1c100s::cpu::interrupt_disable();

    fb_fill_rect(0, 0, 480, 480, COLOR_BLACK);

    let bx: u16 = 40; let by: u16 = 80; let bw: u16 = 400; let bh: u16 = 240;
    fb_fill_rect(bx, by, bw, 2, COLOR_RED);
    fb_fill_rect(bx, by + bh - 2, bw, 2, COLOR_RED);
    fb_fill_rect(bx, by, 2, bh, COLOR_RED);
    fb_fill_rect(bx + bw - 2, by, 2, bh, COLOR_RED);

    let title = b"PANIC";
    let tw = text_width(title);
    draw_str(title, bx as i16 + (bw as i16 - tw) / 2, by as i16 + 40, COLOR_RED, Some(COLOR_BLACK));

    usart_write(b"\r\nPANIC: ");

    if let Some(location) = info.location() {
        let file = location.file(); let line = location.line();
        let file_bytes = file.as_bytes();
        let mut name_start = 0usize;
        for i in 0..file_bytes.len() {
            if file_bytes[i] == b'/' || file_bytes[i] == b'\\' { name_start = i + 1; }
        }
        let short_name = &file_bytes[name_start..];
        let mut loc_buf = [0u8; 64]; let mut pos = 0usize;
        let copy_len = short_name.len().min(48);
        loc_buf[..copy_len].copy_from_slice(&short_name[..copy_len]); pos += copy_len;
        loc_buf[pos] = b':'; pos += 1;
        let mut num_buf = [0u8; 10];
        let num_len = fmt_u32(line, &mut num_buf);
        loc_buf[pos..pos + num_len].copy_from_slice(&num_buf[..num_len]); pos += num_len;
        let loc = &loc_buf[..pos];
        let lw = text_width(loc);
        draw_str(loc, bx as i16 + (bw as i16 - lw) / 2, by as i16 + 80, COLOR_WHITE, Some(COLOR_BLACK));
        usart_write(file_bytes); usart_write(b":"); usart_write(&num_buf[..num_len]);
    }

    usart_write(b"\r\n");
    let msg = b"Device halted";
    let mw = text_width(msg);
    draw_str(msg, bx as i16 + (bw as i16 - mw) / 2, by as i16 + 140, 0x8410, Some(COLOR_BLACK));
    let hint = b"Reset to recover";
    let hw = text_width(hint);
    draw_str(hint, bx as i16 + (bw as i16 - hw) / 2, by as i16 + 170, 0x8410, Some(COLOR_BLACK));

    // ARMv5TE has no `wfi` mnemonic — HAL issues the CP15 wait-for-interrupt.
    loop { f1c100s::cpu::wfi(); }
}
