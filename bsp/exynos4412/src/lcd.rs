//! LCD backend — FIMD window 0 scanning an XRGB8888 framebuffer in DRAM.
//!
//! Two 1280×800 buffers live in `.bss`, both cached: blending reads what it
//! draws on, and uncached reads wait for DRAM each time.
//!
//! - [`SHADOW`] is where every primitive draws, in both render modes. The FIMD
//!   never reads it, so it is never cleaned.
//! - [`SCREEN`] is what the FIMD scans. Only [`flush`] writes it: it copies the
//!   dirty bounding box from the shadow, row by row, top down, right after a
//!   vsync starts, and cleans each row out of the D-cache as soon as it is
//!   copied. The copy runs ahead of the scan, so a frame shows up whole.
//!
//! Drawing on the scanned buffer directly showed every redraw's intermediate
//! states (the erase before the paint), and in random 8-pixel pieces: dirty
//! cache lines reach DRAM in eviction order, not drawing order. That looked
//! like the static on an old TV screen.
//!
//! Buffered mode needs nothing more: the shadow already holds every frame, so
//! `back_buf` is 1 for the renderer's per-buffer tracking and `end_frame` only
//! flushes.

use core::cell::UnsafeCell;

use exynos_4412_rs::drivers::display::{self, Fimd, Framebuffer, LTN101AL03, LVDS_SETTLE_MS, Timing};
use exynos_4412_rs::mmu;
use ferrite_core::fb;
use ferrite_core::lcd::LcdBackend;

use crate::systick;

pub const PANEL: Timing = LTN101AL03;
const W: usize = PANEL.width;
const H: usize = PANEL.height;
const PIXELS: usize = W * H;

#[repr(C, align(4096))]
struct Buffers(UnsafeCell<[[u32; PIXELS]; 2]>);

// SAFETY: only the main loop (CPU0, no ISR) draws; see the module docs.
unsafe impl Sync for Buffers {}

static BUFFERS: Buffers = Buffers(UnsafeCell::new([[0; PIXELS]; 2]));

/// Buffer the FIMD scans.
const SCREEN: usize = 0;
/// Buffer the primitives draw into.
const SHADOW: usize = 1;
/// Inside `begin_frame` .. `end_frame` (buffered mode): `back_buf` is 1 then.
static mut IN_FRAME: bool = false;
/// Dirty box since the last flush: x0, y0, x1, y1 (exclusive); x0 > x1 = empty.
static mut DIRTY: [usize; 4] = [usize::MAX, usize::MAX, 0, 0];

static mut CURSOR: fb::StreamCursor = fb::StreamCursor::new();

fn buf_ptr(index: usize) -> *mut u32 {
    // SAFETY: in bounds of the static array.
    unsafe { (BUFFERS.0.get() as *mut u32).add(index * PIXELS) }
}

fn back() -> *mut u32 {
    buf_ptr(SHADOW)
}

/// `w` pixels of row `y` from column `x`.
///
/// # Safety
/// `x + w <= W`, `y < H`, and no other live reference to those pixels.
unsafe fn row<'a>(fb: *mut u32, x: usize, y: usize, w: usize) -> &'a mut [u32] {
    unsafe { core::slice::from_raw_parts_mut(fb.add(y * W + x), w) }
}

fn mark(x: usize, y: usize, w: usize, h: usize) {
    unsafe {
        let d = &mut *(&raw mut DIRTY);
        d[0] = d[0].min(x);
        d[1] = d[1].min(y);
        d[2] = d[2].max(x + w);
        d[3] = d[3].max(y + h);
    }
}

#[inline]
pub fn rgb565_to_xrgb(c: u16) -> u32 {
    let r = ((c >> 11) & 0x1F) as u8;
    let g = ((c >> 5) & 0x3F) as u8;
    let b = (c & 0x1F) as u8;
    0xFF00_0000 | display::rgb(r << 3 | r >> 2, g << 2 | g >> 4, b << 3 | b >> 2)
}

fn clip(x: u16, y: u16, w: u16, h: u16) -> Option<(usize, usize, usize, usize)> {
    let (x, y) = (x as usize, y as usize);
    if w == 0 || h == 0 || x >= W || y >= H {
        return None;
    }
    Some((x, y, (w as usize).min(W - x), (h as usize).min(H - y)))
}

/// Shows what was drawn since the last flush: copies the dirty box from the
/// shadow to the scanned buffer, starting at a vsync.
pub fn flush() {
    let d = unsafe { &mut *(&raw mut DIRTY) };
    if d[0] >= d[2] {
        return;
    }
    let (x, w) = (d[0], d[2] - d[0]);
    // No vsync (FIMD off): copy anyway.
    let _ = Fimd::new().wait_vsync();
    let (src, dst) = (buf_ptr(SHADOW), buf_ptr(SCREEN));
    for y in d[1]..d[3] {
        let at = y * W + x;
        // SAFETY: the box is clipped to the screen (`mark` callers), and the
        // two buffers are distinct.
        unsafe { core::ptr::copy_nonoverlapping(src.add(at), dst.add(at), w) };
        mmu::clean_dcache_range(dst as usize + at * 4, w * 4);
    }
    *d = [usize::MAX, usize::MAX, 0, 0];
    // SAFETY: a barrier only.
    unsafe { core::arch::asm!("dsb", options(nostack, preserves_flags)) };
}

pub struct FimdLcd;

impl LcdBackend for FimdLcd {
    const WIDTH: u16 = W as u16;
    const HEIGHT: u16 = H as u16;
    // DRAM framebuffer: read-modify-write blending (full anti-aliasing).
    // Mirror in tools/devices/exynos4412.json caps.alpha.
    const HAS_ALPHA: bool = true;

    fn begin_frame(&mut self) {
        unsafe { IN_FRAME = true };
    }

    fn end_frame(&mut self) {
        unsafe { IN_FRAME = false };
        flush();
    }

    fn back_buf(&self) -> u8 {
        // The renderer's buffer index; the shadow holds every frame.
        unsafe { IN_FRAME as u8 }
    }

    fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        let Some((x, y, w, h)) = clip(x, y, w, h) else { return };
        let c = rgb565_to_xrgb(color);
        let fb = back();
        for r in y..y + h {
            fb::fill_u32(unsafe { row(fb, x, r, w) }, c);
        }
        mark(x, y, w, h);
    }

    fn draw_pixel(&self, x: u16, y: u16, color: u16) {
        let (x, y) = (x as usize, y as usize);
        if x < W && y < H {
            unsafe { *back().add(y * W + x) = rgb565_to_xrgb(color) };
            mark(x, y, 1, 1);
        }
    }

    fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16) {
        unsafe { (*(&raw mut CURSOR)).begin(x, y, w) };
        if let Some((x, y, w, h)) = clip(x, y, w, h) {
            mark(x, y, w, h);
        }
    }

    fn write_pixel(&self, color: u16) {
        if let Some((x, y)) = unsafe { (*(&raw mut CURSOR)).step(W, H) } {
            unsafe { *back().add(y * W + x) = rgb565_to_xrgb(color) };
        }
    }

    fn write_pixels(&self, colors: &[u16]) {
        let fb = back();
        unsafe {
            (*(&raw mut CURSOR)).advance(colors.len(), W, H, |x, y, len, skip| {
                for (d, &c) in row(fb, x, y, len).iter_mut().zip(&colors[skip..skip + len]) {
                    *d = rgb565_to_xrgb(c);
                }
            });
        }
    }

    fn write_run(&self, color: u16, n: usize) {
        let (fb, c) = (back(), rgb565_to_xrgb(color));
        unsafe {
            (*(&raw mut CURSOR)).advance(n, W, H, |x, y, len, _| fb::fill_u32(row(fb, x, y, len), c));
        }
    }

    fn send_command(&self, _cmd: u16) {}
    fn send_data(&self, _data: u16) {}

    fn blend_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16, alpha: u8) {
        if alpha == 255 {
            return self.fill_rect(x, y, w, h, color);
        }
        let Some((x, y, w, h)) = clip(x, y, w, h) else { return };
        if alpha == 0 {
            return;
        }
        let (src, a, fb) = (rgb565_to_xrgb(color), fb::alpha8(alpha), back());
        for r in y..y + h {
            for p in unsafe { row(fb, x, r, w) } {
                *p = fb::blend_argb8888(*p, src, a);
            }
        }
        mark(x, y, w, h);
    }

    fn blend_pixel(&self, x: u16, y: u16, color: u16, alpha: u8) {
        let (x, y) = (x as usize, y as usize);
        if alpha == 0 || x >= W || y >= H {
            return;
        }
        let p = unsafe { &mut *back().add(y * W + x) };
        let src = rgb565_to_xrgb(color);
        *p = if alpha == 255 { src } else { fb::blend_argb8888(*p, src, fb::alpha8(alpha)) };
        mark(x, y, 1, 1);
    }
}

/// Powers the FIMD, starts it on buffer 0 and turns on LVDS + backlight.
pub fn init() -> Result<FimdLcd, display::Error> {
    let fimd = Fimd::new();
    fimd.power_on()?;
    display::configure_pins();
    {
        // A short-lived view, only so `start` can read the address and stride;
        // dropped before anything draws through the raw pointers above.
        // SAFETY: nothing else references buffer 0 yet.
        let pixels = unsafe { core::slice::from_raw_parts_mut(buf_ptr(SCREEN), PIXELS) };
        let screen = Framebuffer::new_cached(pixels, W, H).ok_or(display::Error::BadSize)?;
        mmu::clean_dcache_range(screen.address(), PIXELS * 4);
        fimd.start(&PANEL, &screen)?;
    }
    display::set_lvds(true);
    systick::delay_ms(LVDS_SETTLE_MS);
    Ok(FimdLcd)
}
