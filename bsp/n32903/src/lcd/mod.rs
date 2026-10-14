//! LCD backend for N32903 — VPOST scanning an RGB565 frame buffer in SDRAM.
//!
//! Two 800×480 RGB565 frame buffers live in .bss. The VPOST shows the *front*
//! buffer (`LCM_FSADDR`) while the CPU draws into the *back* one; `end_frame`
//! writes the D-cache back and points FSADDR at the back buffer. The new
//! address is latched at the next VSYNC; rather than block for it there,
//! `begin_frame` waits only if it would otherwise start drawing into the
//! old front buffer before that VSYNC (which would tear). Frames are usually
//! further apart than one refresh, so in practice nothing waits.
//!
//! In `dirty` render mode the framework never calls `begin_frame`/`end_frame`,
//! so `BACK` stays equal to `FRONT` and drawing goes straight to the visible
//! buffer. The buffers are cacheable, so the platform's `frame_end` cleans the
//! D-cache every loop iteration to make those writes visible to the VPOST.

use ferrite_core::fb;
use ferrite_core::lcd::LcdBackend;
use n32903_rs::cache;
use n32903_rs::lcd::{self as hal_lcd, Lcd};
use n32903_rs::registers::{
    LCDCInt_VINT, REG_LCM_FSADDR, REG_LCM_LCDCInt, reg_read32, reg_write32,
};

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

/// `end_frame` pointed FSADDR at a new buffer and no VSYNC has been seen
/// since: the previous front buffer may still be scanning out.
static mut FLIP_PENDING: bool = false;

/// Block until the VSYNC that latches the last `end_frame`'s buffer.
fn wait_flip() {
    unsafe {
        if FLIP_PENDING {
            while reg_read32(REG_LCM_LCDCInt) & LCDCInt_VINT == 0 {}
            FLIP_PENDING = false;
        }
    }
}

/// The HAL LCD driver, kept for the lifetime of the program.
static mut HAL: Option<Lcd> = None;

#[inline]
fn fb_ptr(index: u8) -> *mut u16 {
    unsafe { (&raw mut FRAMEBUFFERS.0[index as usize]).cast::<u16>() }
}

#[inline]
fn back() -> *mut u16 {
    fb_ptr(unsafe { BACK })
}

/// `w` pixels of row `y` starting at column `x` in frame buffer `fb`.
///
/// # Safety
/// `x + w <= WIDTH`, `y < HEIGHT`, `fb` from `fb_ptr`; no other live
/// reference to those pixels.
#[inline]
unsafe fn row_slice<'a>(fb: *mut u16, x: usize, y: usize, w: usize) -> &'a mut [u16] {
    unsafe { core::slice::from_raw_parts_mut(fb.add(y * WIDTH + x), w) }
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
        // The new back buffer is the old front one: don't draw into it while
        // the VPOST may still be scanning it out.
        wait_flip();
        unsafe { BACK = FRONT ^ 1 };
    }

    fn end_frame(&mut self) {
        unsafe {
            cache::dcache_clean();
            reg_write32(REG_LCM_FSADDR, fb_ptr(BACK) as u32);
            // Start watching for the VSYNC that latches it (write-0 clears;
            // a VSYNC racing this clear only costs one extra frame of wait).
            reg_write32(REG_LCM_LCDCInt, reg_read32(REG_LCM_LCDCInt) & !LCDCInt_VINT);
            FLIP_PENDING = true;
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
            // SAFETY: (x, row, w) was clipped to the panel above.
            fb::fill_u16(unsafe { row_slice(fb, x, row, w) }, color);
        }
    }

    fn begin_pixels(&self, x: u16, y: u16, w: u16, _h: u16) {
        unsafe {
            (*(&raw mut PIXEL)).begin(x, y, w);
            PIXEL_FB = back();
        }
    }

    fn write_pixel(&self, color: u16) {
        unsafe {
            if let Some((x, y)) = (*(&raw mut PIXEL)).step(WIDTH, HEIGHT) {
                *PIXEL_FB.add(y * WIDTH + x) = color;
            }
        }
    }

    fn write_pixels(&self, colors: &[u16]) {
        unsafe {
            let fb = PIXEL_FB;
            (*(&raw mut PIXEL)).advance(colors.len(), WIDTH, HEIGHT, |x, y, len, skip| {
                // SAFETY: the cursor only reports on-panel spans.
                row_slice(fb, x, y, len).copy_from_slice(&colors[skip..skip + len]);
            });
        }
    }

    fn write_run(&self, color: u16, n: usize) {
        unsafe {
            let fb = PIXEL_FB;
            (*(&raw mut PIXEL)).advance(n, WIDTH, HEIGHT, |x, y, len, _| {
                // SAFETY: the cursor only reports on-panel spans.
                fb::fill_u16(row_slice(fb, x, y, len), color);
            });
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
        let a = fb::alpha5(alpha);
        let fb = back();
        for row in y..y + h {
            // SAFETY: (x, row, w) was clipped to the panel above.
            for p in unsafe { row_slice(fb, x, row, w) } {
                *p = fb::blend_rgb565(*p, color, a);
            }
        }
    }

    fn blend_pixel(&self, x: u16, y: u16, color: u16, alpha: u8) {
        if alpha == 0 || x as usize >= WIDTH || y as usize >= HEIGHT {
            return;
        }
        unsafe {
            let p = back().add(y as usize * WIDTH + x as usize);
            *p = if alpha == 255 { color } else { fb::blend_rgb565(*p, color, fb::alpha5(alpha)) };
        }
    }
}

// Pixel-stream state for begin_pixels/write_pixel (single-threaded).
static mut PIXEL: fb::StreamCursor = fb::StreamCursor::new();
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
