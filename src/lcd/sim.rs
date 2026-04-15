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

impl LcdBackend for SimLcd {
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
            self.fb.borrow_mut()[y as usize * WIDTH as usize + x as usize] =
                rgb565_to_argb(color);
        }
    }
}
