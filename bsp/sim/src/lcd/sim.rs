use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::vec;
use std::vec::Vec;

use super::{HEIGHT, LcdBackend, WIDTH};

/// Shared 32-bit ARGB framebuffer (0xAARRGGBB). The sim binary owns it too —
/// it pushes the buffer into minifb each frame.
pub type Framebuffer = Rc<RefCell<Vec<u32>>>;

pub fn new_framebuffer() -> Framebuffer {
    Rc::new(RefCell::new(vec![0u32; WIDTH as usize * HEIGHT as usize]))
}

pub struct SimLcd {
    fb: Framebuffer,
    cursor_x: Cell<u16>,
    cursor_y: Cell<u16>,
    rect_x_start: Cell<u16>,
    rect_x_end: Cell<u16>,
    rect_y_end: Cell<u16>,
    buf_index: Cell<u8>,
}

impl SimLcd {
    pub fn new(fb: Framebuffer) -> Self {
        Self {
            fb,
            cursor_x: Cell::new(0),
            cursor_y: Cell::new(0),
            rect_x_start: Cell::new(0),
            rect_x_end: Cell::new(0),
            rect_y_end: Cell::new(0),
            buf_index: Cell::new(0),
        }
    }
}

#[inline]
fn rgb565_to_argb(c: u16) -> u32 {
    let r = ((c >> 11) & 0x1F) as u32;
    let g = ((c >> 5) & 0x3F) as u32;
    let b = (c & 0x1F) as u32;
    let r8 = (r << 3) | (r >> 2);
    let g8 = (g << 2) | (g >> 4);
    let b8 = (b << 3) | (b >> 2);
    0xFF00_0000 | (r8 << 16) | (g8 << 8) | b8
}

/// Blend `src` (ARGB, opaque) over `dst` per 8-bit channel:
/// `out = (src*a + dst*(255-a) + 127) / 255`. Same math as the tdo_y13
/// DEBE backend, so alpha rendering is host-testable here.
#[inline]
fn blend_argb(dst: u32, src: u32, alpha: u8) -> u32 {
    let a = alpha as u32;
    let na = 255 - a;
    let r = (((src >> 16) & 0xFF) * a + ((dst >> 16) & 0xFF) * na + 127) / 255;
    let g = (((src >> 8) & 0xFF) * a + ((dst >> 8) & 0xFF) * na + 127) / 255;
    let b = ((src & 0xFF) * a + (dst & 0xFF) * na + 127) / 255;
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

impl LcdBackend for SimLcd {
    const WIDTH: u16 = 800;
    const HEIGHT: u16 = 480;

    // The sim deliberately supports alpha even though nextion.json says
    // caps.alpha = false: it is the host test vehicle for the tdo_y13
    // framebuffer path. Non-alpha behavior is covered by the defaults.
    const HAS_ALPHA: bool = true;
    fn begin_frame(&mut self) {
        self.buf_index.set(self.buf_index.get() ^ 1);
    }

    fn end_frame(&mut self) {
        // single-buffer sim — the window reads `fb` directly.
    }

    fn back_buf(&self) -> u8 {
        self.buf_index.get()
    }

    fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        if w == 0 || h == 0 {
            return;
        }
        let argb = rgb565_to_argb(color);
        let mut fb = self.fb.borrow_mut();
        let stride = WIDTH as usize;
        let x_end = (x + w).min(WIDTH);
        let y_end = (y + h).min(HEIGHT);
        for py in y..y_end {
            let row = py as usize * stride;
            for px in x..x_end {
                fb[row + px as usize] = argb;
            }
        }
    }

    fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16) {
        self.cursor_x.set(x);
        self.cursor_y.set(y);
        self.rect_x_start.set(x);
        self.rect_x_end.set(x + w);
        self.rect_y_end.set(y + h);
    }

    fn write_pixel(&self, color: u16) {
        let mut x = self.cursor_x.get();
        let mut y = self.cursor_y.get();
        let xe = self.rect_x_end.get();
        let ye = self.rect_y_end.get();
        if x < xe && y < ye && x < WIDTH && y < HEIGHT {
            let argb = rgb565_to_argb(color);
            self.fb.borrow_mut()[y as usize * WIDTH as usize + x as usize] = argb;
        }
        x += 1;
        if x >= xe {
            x = self.rect_x_start.get();
            y += 1;
        }
        self.cursor_x.set(x);
        self.cursor_y.set(y);
    }

    fn send_command(&self, _cmd: u16) {
        // FPGA raw command stream — no-op in the sim.
    }

    fn send_data(&self, _data: u16) {}

    fn draw_pixel(&self, x: u16, y: u16, color: u16) {
        if x < WIDTH && y < HEIGHT {
            self.fb.borrow_mut()[y as usize * WIDTH as usize + x as usize] = rgb565_to_argb(color);
        }
    }

    fn blend_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16, alpha: u8) {
        if alpha == 0 {
            return;
        }
        if alpha == 255 {
            self.fill_rect(x, y, w, h, color);
            return;
        }
        if w == 0 || h == 0 {
            return;
        }
        let src = rgb565_to_argb(color);
        // Borrow once for the whole rect, not per pixel.
        let mut fb = self.fb.borrow_mut();
        let stride = WIDTH as usize;
        let x_end = (x + w).min(WIDTH);
        let y_end = (y + h).min(HEIGHT);
        for py in y..y_end {
            let row = py as usize * stride;
            for px in x..x_end {
                let i = row + px as usize;
                fb[i] = blend_argb(fb[i], src, alpha);
            }
        }
    }

    fn blend_pixel(&self, x: u16, y: u16, color: u16, alpha: u8) {
        if alpha == 0 || x >= WIDTH || y >= HEIGHT {
            return;
        }
        let src = rgb565_to_argb(color);
        let mut fb = self.fb.borrow_mut();
        let i = y as usize * WIDTH as usize + x as usize;
        fb[i] = if alpha == 255 {
            src
        } else {
            blend_argb(fb[i], src, alpha)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHITE: u32 = 0xFFFF_FFFF;
    const BLACK: u32 = 0xFF00_0000;

    #[test]
    fn blend_endpoints() {
        assert_eq!(blend_argb(BLACK, WHITE, 255), WHITE);
        assert_eq!(blend_argb(BLACK, WHITE, 0), BLACK);
        assert_eq!(blend_argb(WHITE, BLACK, 0), WHITE);
    }

    #[test]
    fn blend_midpoint() {
        // 50% white over black: each channel (255*128 + 0*127 + 127)/255 = 128
        let mid = blend_argb(BLACK, WHITE, 128);
        assert_eq!(mid, 0xFF80_8080);
    }

    #[test]
    fn blend_rect_blends_against_existing() {
        let fb = new_framebuffer();
        let lcd = SimLcd::new(fb.clone());
        lcd.fill_rect(0, 0, 4, 4, 0x0000); // black
        lcd.blend_rect(1, 1, 2, 2, 0xFFFF, 128); // 50% white

        let buf = fb.borrow();
        assert_eq!(buf[0], BLACK); // outside the blend rect
        let inside = buf[1 * WIDTH as usize + 1];
        assert_eq!(inside, 0xFF80_8080);
    }

    #[test]
    fn blend_rect_alpha_255_is_opaque() {
        let fb = new_framebuffer();
        let lcd = SimLcd::new(fb.clone());
        lcd.fill_rect(0, 0, 2, 2, 0x0000);
        lcd.blend_rect(0, 0, 2, 2, 0xFFFF, 255);
        assert_eq!(fb.borrow()[0], WHITE);
    }

    #[test]
    fn blend_pixel_clips_to_screen() {
        let fb = new_framebuffer();
        let lcd = SimLcd::new(fb.clone());
        // Must not panic / write out of bounds.
        lcd.blend_pixel(WIDTH, HEIGHT, 0xFFFF, 128);
        lcd.blend_rect(WIDTH - 1, HEIGHT - 1, 10, 10, 0xFFFF, 128);
    }
}
