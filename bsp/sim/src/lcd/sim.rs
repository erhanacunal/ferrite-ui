use std::cell::{Cell, Ref, RefCell, RefMut};
use std::rc::Rc;
use std::vec;
use std::vec::Vec;

use super::{HEIGHT, LcdBackend, WIDTH};

/// Two 32-bit ARGB (0xAARRGGBB) framebuffers with hardware-style front/back
/// indices, so the double-buffered ("buffered") render mode runs on the host
/// exactly as it does on the tdo_y13 — which flips the DEBE scanout address
/// between two independent DRAM buffers. The framework draws into the BACK
/// buffer and publishes it with `end_frame`; the sim binary presents the FRONT
/// buffer to minifb each frame.
///
/// In "dirty" render mode `begin_frame`/`end_frame` are never called, so
/// `front == back == 0` throughout: every draw lands in the displayed buffer
/// immediately — identical to the original single-buffer behaviour.
pub struct DoubleBuffer {
    bufs: [RefCell<Vec<u32>>; 2],
    /// Index the window presents (the "displayed" buffer).
    front: Cell<u8>,
    /// Index draw primitives write to.
    back: Cell<u8>,
}

/// Shared handle to the double framebuffer. Cloned into both the LCD backend
/// (writer) and the host window pump (reader).
pub type Framebuffer = Rc<DoubleBuffer>;

pub fn new_framebuffer() -> Framebuffer {
    let pixels = WIDTH as usize * HEIGHT as usize;
    Rc::new(DoubleBuffer {
        bufs: [
            RefCell::new(vec![0u32; pixels]),
            RefCell::new(vec![0u32; pixels]),
        ],
        front: Cell::new(0),
        back: Cell::new(0),
    })
}

impl DoubleBuffer {
    /// Index of the buffer currently being drawn to.
    #[inline]
    pub fn back(&self) -> u8 {
        self.back.get()
    }

    /// Borrow the buffer the window should present (read-only).
    #[inline]
    pub fn front_buffer(&self) -> Ref<'_, Vec<u32>> {
        self.bufs[self.front.get() as usize].borrow()
    }

    /// Borrow the back buffer for drawing.
    #[inline]
    fn draw(&self) -> RefMut<'_, Vec<u32>> {
        self.bufs[self.back.get() as usize].borrow_mut()
    }

    /// Hardware-style `begin_frame`: if the back buffer is the one currently
    /// displayed (`front == back`, i.e. the previous frame was already
    /// swapped), move drawing to the other buffer so the visible frame is
    /// never disturbed mid-draw. Mirrors the tdo_y13 CMD5 / lcd5 toggle.
    #[inline]
    fn begin(&self) {
        if self.front.get() == self.back.get() {
            self.back.set(self.back.get() ^ 1);
        }
    }

    /// Hardware-style `end_frame`: publish the back buffer to the window
    /// (front ← back). Mirrors the tdo_y13 CMD4 / DEBE address flip.
    #[inline]
    fn end(&self) {
        self.front.set(self.back.get());
    }
}

pub struct SimLcd {
    fb: Framebuffer,
    cursor_x: Cell<u16>,
    cursor_y: Cell<u16>,
    rect_x_start: Cell<u16>,
    rect_x_end: Cell<u16>,
    rect_y_end: Cell<u16>,
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
        self.fb.begin();
    }

    fn end_frame(&mut self) {
        self.fb.end();
    }

    fn back_buf(&self) -> u8 {
        self.fb.back()
    }

    fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        if w == 0 || h == 0 {
            return;
        }
        let argb = rgb565_to_argb(color);
        let mut fb = self.fb.draw();
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
            self.fb.draw()[y as usize * WIDTH as usize + x as usize] = argb;
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
            self.fb.draw()[y as usize * WIDTH as usize + x as usize] = rgb565_to_argb(color);
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
        let mut fb = self.fb.draw();
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
        let mut fb = self.fb.draw();
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

        let buf = fb.front_buffer();
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
        assert_eq!(fb.front_buffer()[0], WHITE);
    }

    #[test]
    fn dirty_mode_writes_are_immediately_visible() {
        // No begin_frame/end_frame (dirty mode): front == back == 0, so a draw
        // lands directly in the displayed buffer.
        let fb = new_framebuffer();
        let lcd = SimLcd::new(fb.clone());
        assert_eq!(lcd.back_buf(), 0);
        lcd.fill_rect(0, 0, 2, 2, 0xFFFF);
        assert_eq!(fb.front_buffer()[0], WHITE);
    }

    #[test]
    fn buffered_swap_alternates_buffers() {
        // Buffered mode: each begin/end frame draws into the buffer that is NOT
        // currently displayed, then publishes it. Two frames touch both buffers.
        let fb = new_framebuffer();
        let mut lcd = SimLcd::new(fb.clone());

        // Frame 1: front=0,back=0 -> begin toggles back to 1.
        lcd.begin_frame();
        assert_eq!(lcd.back_buf(), 1);
        lcd.fill_rect(0, 0, 1, 1, 0xFFFF); // white into buffer 1
        lcd.end_frame();
        assert_eq!(fb.front_buffer()[0], WHITE); // window now shows buffer 1

        // Frame 2: front=1,back=1 -> begin toggles back to 0 (the stale buffer).
        lcd.begin_frame();
        assert_eq!(lcd.back_buf(), 0);
        lcd.fill_rect(0, 0, 1, 1, BLACK_565); // into buffer 0
        lcd.end_frame();
        assert_eq!(fb.front_buffer()[0], BLACK); // window shows buffer 0
    }

    const BLACK_565: u16 = 0x0000;

    #[test]
    fn blend_pixel_clips_to_screen() {
        let fb = new_framebuffer();
        let lcd = SimLcd::new(fb.clone());
        // Must not panic / write out of bounds.
        lcd.blend_pixel(WIDTH, HEIGHT, 0xFFFF, 128);
        lcd.blend_rect(WIDTH - 1, HEIGHT - 1, 10, 10, 0xFFFF, 128);
    }
}
