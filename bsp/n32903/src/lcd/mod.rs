//! LCD backend for N32903 — VPOST scanning an RGB565 frame buffer in SDRAM.
//!
//! Two 800×480 RGB565 frame buffers live in .bss. The VPOST shows the *front*
//! buffer (`LCM_FSADDR`) while the CPU draws into the *back* one; `end_frame`
//! writes the D-cache back, points FSADDR at the back buffer and waits for
//! VSYNC (the new address is latched at the next frame).
//!
//! In `dirty` render mode the framework never calls `begin_frame`/`end_frame`,
//! so `BACK` stays equal to `FRONT` and drawing goes straight to the visible
//! buffer. The buffers are cacheable, so the platform's `frame_end` cleans the
//! D-cache every loop iteration to make those writes visible to the VPOST.

use ferrite_core::lcd::LcdBackend;
use n32903_rs::cache;
use n32903_rs::lcd::{self as hal_lcd, Lcd};
use n32903_rs::registers::{REG_LCM_FSADDR, reg_write32};

const WIDTH: usize = 800;
const HEIGHT: usize = 480;
const FB_PIXELS: usize = WIDTH * HEIGHT;

/// Pixel clock. The Nextion firmware divides UPLL by 6 (CLKDIV1 N1 = 5).
const PIXEL_CLOCK_KHZ: u32 = 16_000;

/// Two RGB565 frame buffers, cache-line aligned.
#[repr(C, align(64))]
struct FrameBuffers([[u16; FB_PIXELS]; 2]);

static mut FRAMEBUFFERS: FrameBuffers = FrameBuffers([[0; FB_PIXELS]; 2]);

/// Buffer the VPOST currently scans out.
static mut FRONT: u8 = 0;
/// Buffer every draw primitive writes to.
static mut BACK: u8 = 0;

/// The HAL LCD, kept for `wait_vsync`.
static mut HAL: Option<Lcd> = None;

#[inline]
fn fb_ptr(index: u8) -> *mut u16 {
    unsafe { (&raw mut FRAMEBUFFERS.0[index as usize]).cast::<u16>() }
}

#[inline]
fn back() -> *mut u16 {
    fb_ptr(unsafe { BACK })
}

/// Blend RGB565 `src` over `dst` per channel: `out = (src*a + dst*(255-a)) / 255`.
#[inline]
fn blend_rgb565(dst: u16, src: u16, alpha: u8) -> u16 {
    let a = alpha as u32;
    let na = 255 - a;
    let ch = |shift: u32, mask: u32| {
        let s = (src as u32 >> shift) & mask;
        let d = (dst as u32 >> shift) & mask;
        ((s * a + d * na + 127) / 255) << shift
    };
    (ch(11, 0x1F) | ch(5, 0x3F) | ch(0, 0x1F)) as u16
}

/// Fill `n` pixels from `p` with `color`, two pixels per word store where
/// the alignment allows.
#[inline]
unsafe fn fill_span(p: *mut u16, n: usize, color: u16) {
    unsafe {
        let mut p = p;
        let mut n = n;
        if n > 0 && (p as usize) & 2 != 0 {
            *p = color;
            p = p.add(1);
            n -= 1;
        }
        let pair = (color as u32) | ((color as u32) << 16);
        let mut w = p as *mut u32;
        for _ in 0..n / 2 {
            *w = pair;
            w = w.add(1);
        }
        if n & 1 != 0 {
            *(w as *mut u16) = color;
        }
    }
}

/// Hardware backend: draws into the back frame buffer.
pub struct VpostLcd;

impl LcdBackend for VpostLcd {
    const WIDTH: u16 = WIDTH as u16;
    const HEIGHT: u16 = HEIGHT as u16;

    // Frame buffer in SDRAM → read-modify-write blending is cheap.
    // Mirrored in tools/devices/n32903.json (caps.alpha / caps.image_alpha).
    const HAS_ALPHA: bool = true;

    fn begin_frame(&mut self) {
        unsafe { BACK = FRONT ^ 1 };
    }

    fn end_frame(&mut self) {
        unsafe {
            cache::dcache_clean();
            reg_write32(REG_LCM_FSADDR, fb_ptr(BACK) as u32);
            if let Some(hal) = (&*(&raw const HAL)).as_ref() {
                hal.wait_vsync();
            }
            FRONT = BACK;
        }
    }

    fn back_buf(&self) -> u8 {
        unsafe { BACK }
    }

    fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        let (x, y) = (x as usize, y as usize);
        if w == 0 || h == 0 || x >= WIDTH || y >= HEIGHT {
            return;
        }
        let w = (w as usize).min(WIDTH - x);
        let h = (h as usize).min(HEIGHT - y);
        let fb = back();
        for row in y..y + h {
            unsafe { fill_span(fb.add(row * WIDTH + x), w, color) };
        }
    }

    fn begin_pixels(&self, x: u16, y: u16, w: u16, _h: u16) {
        unsafe {
            PIXEL_X = x;
            PIXEL_Y = y;
            PIXEL_START_X = x;
            PIXEL_W = w;
            PIXEL_FB = back();
        }
    }

    fn write_pixel(&self, color: u16) {
        unsafe {
            if (PIXEL_X as usize) < WIDTH && (PIXEL_Y as usize) < HEIGHT {
                *PIXEL_FB.add(PIXEL_Y as usize * WIDTH + PIXEL_X as usize) = color;
            }
            PIXEL_X += 1;
            if PIXEL_X >= PIXEL_START_X + PIXEL_W {
                PIXEL_X = PIXEL_START_X;
                PIXEL_Y += 1;
            }
        }
    }

    fn draw_pixel(&self, x: u16, y: u16, color: u16) {
        if (x as usize) < WIDTH && (y as usize) < HEIGHT {
            unsafe { *back().add(y as usize * WIDTH + x as usize) = color };
        }
    }

    fn send_command(&self, _cmd: u16) {
        // Not used for frame-buffer panels.
    }

    fn send_data(&self, _data: u16) {
        // Not used for frame-buffer panels.
    }

    fn blend_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16, alpha: u8) {
        if alpha == 0 {
            return;
        }
        if alpha == 255 {
            self.fill_rect(x, y, w, h, color);
            return;
        }
        let (x, y) = (x as usize, y as usize);
        if w == 0 || h == 0 || x >= WIDTH || y >= HEIGHT {
            return;
        }
        let w = (w as usize).min(WIDTH - x);
        let h = (h as usize).min(HEIGHT - y);
        let fb = back();
        for row in y..y + h {
            for col in x..x + w {
                unsafe {
                    let p = fb.add(row * WIDTH + col);
                    *p = blend_rgb565(*p, color, alpha);
                }
            }
        }
    }

    fn blend_pixel(&self, x: u16, y: u16, color: u16, alpha: u8) {
        if alpha == 0 || x as usize >= WIDTH || y as usize >= HEIGHT {
            return;
        }
        unsafe {
            let p = back().add(y as usize * WIDTH + x as usize);
            *p = if alpha == 255 { color } else { blend_rgb565(*p, color, alpha) };
        }
    }
}

// Per-pixel write state for begin_pixels/write_pixel (single-threaded).
static mut PIXEL_X: u16 = 0;
static mut PIXEL_Y: u16 = 0;
static mut PIXEL_START_X: u16 = 0;
static mut PIXEL_W: u16 = 0;
static mut PIXEL_FB: *mut u16 = core::ptr::null_mut();

/// Start the VPOST on buffer 0 (800×480 timing from the Nextion firmware).
///
/// # Safety
/// Call once at boot, after the MMU/caches are set up.
pub unsafe fn init() {
    unsafe {
        // The buffers are zero (.bss) — black. Make sure SDRAM has it before
        // the VPOST starts reading.
        cache::dcache_clean();
        let timing = hal_lcd::PanelTiming {
            pixel_clock_khz: PIXEL_CLOCK_KHZ,
            ..hal_lcd::NEXTION_800X480
        };
        let lcd = Lcd::init(fb_ptr(0) as u32, &timing);
        let upll = n32903_rs::clock::get_clock(n32903_rs::clock::ClockNode::UPll);
        crate::usart::log_fmt(format_args!(
            "lcd: UPLL {} kHz, pixel clock {} kHz, CLKDIV1 {:#010x}
",
            upll,
            lcd.pixel_clock_khz(),
            n32903_rs::registers::reg_read32(n32903_rs::registers::REG_CLKDIV1)
        ));
        HAL = Some(lcd);
    }
}

/// Write back drawing done straight into the visible buffer (dirty mode).
pub fn flush_cache() {
    unsafe { cache::dcache_clean() };
}

/// Return a new `VpostLcd` instance. Must call `init()` first.
pub fn new() -> VpostLcd {
    VpostLcd
}

/// Frame buffer the VPOST is showing (for the panic handler).
pub fn front_buffer() -> *mut u16 {
    fb_ptr(unsafe { FRONT })
}
