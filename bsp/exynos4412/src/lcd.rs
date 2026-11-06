//! LCD backend — FIMD window 0 scanning an XRGB8888 framebuffer in DRAM.
//!
//! Three 1280×800 buffers live in `.bss`:
//!
//! - [`SHADOW`] is where every primitive draws, in both render modes. It is
//!   cached: blending reads what it draws on, and uncached reads wait for DRAM
//!   each time. The FIMD never reads it, so it is never cleaned.
//! - [`SCAN_A`] / [`SCAN_B`]: the FIMD scans one (the front) while the other
//!   (the back) is brought up to date. A present copies the dirty box from the
//!   shadow into the back buffer, then switches the FIMD to it with
//!   `Fimd::show`, which takes effect at the next vsync (shadow registers), so
//!   frames never tear and the copy needs no timing. The back buffer last
//!   received the frame before the front's, so each present copies the union
//!   of the last two dirty boxes.
//!
//! The scan buffers are written through the HAL's Normal non-cacheable alias
//! ([`mmu::DRAM_UNCACHED`], the first 16 MiB of DRAM): stores merge in the
//! write buffer and go straight to DRAM, so no cache maintenance is needed.
//! Cleaning each row instead (one L1 + one PL310 operation per 32-byte line)
//! held the copy to ~200 MB/s: 17 ms for a full-screen update (29 fps).
//!
//! **Presenter on CPU2** ([`start_presenter`]): CPU0 hands a dirty box over at
//! [`flush`] and goes on; CPU2 copies, switches and waits for the vsync. CPU0
//! only waits ([`fence`], at the start of every primitive) while CPU2 is still
//! reading the shadow, never for the vsync. If CPU2 is still busy at the next
//! flush, the box keeps growing and goes with a later one. Without CPU2
//! (before it starts, or if it failed to) [`flush`] presents on CPU0, waiting
//! for the switch.
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
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use exynos_4412_rs::drivers::display::{self, Fimd, Framebuffer, LTN101AL03, LVDS_SETTLE_MS, Timing};
use exynos_4412_rs::{mmu, smp};
use ferrite_core::fb;
use ferrite_core::lcd::LcdBackend;

use crate::systick;

pub const PANEL: Timing = LTN101AL03;
const W: usize = PANEL.width;
const H: usize = PANEL.height;
const PIXELS: usize = W * H;

#[repr(C, align(4096))]
struct Buffers(UnsafeCell<[[u32; PIXELS]; 3]>);

// SAFETY: CPU0 draws into the shadow, CPU2 reads it only while CPU0 waits
// (`fence`); each scan buffer is written by one presenter at a time.
unsafe impl Sync for Buffers {}

static BUFFERS: Buffers = Buffers(UnsafeCell::new([[0; PIXELS]; 3]));

/// Scan buffers (the FIMD shows one, the other is the back).
const SCAN_A: usize = 0;
const SCAN_B: usize = 1;
/// Buffer the primitives draw into.
const SHADOW: usize = 2;
/// Inside `begin_frame` .. `end_frame` (buffered mode): `back_buf` is 1 then.
static mut IN_FRAME: bool = false;
/// Dirty box since the last hand-over: x0, y0, x1, y1 (exclusive); x0 > x1 =
/// empty. CPU0 only.
static mut DIRTY: Rect = EMPTY;

static mut CURSOR: fb::StreamCursor = fb::StreamCursor::new();

type Rect = [usize; 4];
const EMPTY: Rect = [usize::MAX, usize::MAX, 0, 0];

fn union(a: Rect, b: Rect) -> Rect {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

fn is_empty(r: &Rect) -> bool {
    r[0] >= r[2]
}

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
        *d = union(*d, [x, y, x + w, y + h]);
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

// ============================================================================
//  Present
// ============================================================================

/// Presenter state, shared by CPU0 and CPU2.
static STATE: AtomicU8 = AtomicU8::new(IDLE);
/// No present in progress: CPU0 may hand a box over.
const IDLE: u8 = 0;
/// CPU2 is copying from the shadow: CPU0 must not draw ([`fence`]).
const COPY: u8 = 1;
/// The copy is done, CPU2 waits for the FIMD to switch: CPU0 may draw.
const FLIP: u8 = 2;

/// CPU2 runs the presenter ([`start_presenter`] succeeded).
static PRESENTER: AtomicBool = AtomicBool::new(false);

/// The presenter's own state: touched by one core at a time (CPU0 before
/// CPU2 starts, CPU2 after), handed over through `STATE`/`smp::start_cpu`.
struct Present {
    /// The box CPU0 handed over (written before `STATE` = COPY).
    req: Rect,
    /// The box of the previous present: the back buffer still lacks it.
    prev: Rect,
    /// Scan buffer the FIMD shows now.
    front: usize,
    /// Per scan buffer: its framebuffer (for `Fimd::show`), and where the
    /// copy writes it: the non-cacheable alias, or 0 = cached address + a
    /// clean per row (the buffer is outside the alias).
    fb: [Option<Framebuffer>; 2],
    nc: [usize; 2],
}

struct PresentCell(UnsafeCell<Present>);
// SAFETY: see `Present`.
unsafe impl Sync for PresentCell {}

static PRESENT: PresentCell = PresentCell(UnsafeCell::new(Present {
    req: EMPTY,
    prev: EMPTY,
    front: SCAN_A,
    fb: [None, None],
    nc: [0, 0],
}));

fn present_state() -> &'static mut Present {
    // SAFETY: see `Present`: only the core that currently presents uses it.
    unsafe { &mut *PRESENT.0.get() }
}

/// Polls of the scanned address before giving up (FIMD off): far more than
/// one frame.
const POLL_LIMIT: u32 = 10_000_000;

/// CPU0, before writing the shadow: waits while the presenter reads it.
#[inline]
fn fence() {
    if STATE.load(Ordering::Relaxed) == COPY {
        wait_copy();
    }
}

#[cold]
#[inline(never)]
fn wait_copy() {
    #[cfg(feature = "perf")]
    let t0 = crate::perf::cycles();
    while STATE.load(Ordering::Acquire) == COPY {
        core::hint::spin_loop();
    }
    #[cfg(feature = "perf")]
    crate::perf::add_wait(crate::perf::cycles().wrapping_sub(t0));
}

/// Shows what was drawn since the last flush. With the presenter on CPU2:
/// hands the dirty box over and returns at once (or, if CPU2 is still busy,
/// keeps it for a later flush). Without it: presents on this core.
pub fn flush() {
    let d = unsafe { &mut *(&raw mut DIRTY) };
    if is_empty(d) {
        return;
    }
    if !PRESENTER.load(Ordering::Acquire) {
        let p = present_state();
        p.req = *d;
        *d = EMPTY;
        present(p, || {});
        return;
    }
    if STATE.load(Ordering::Acquire) != IDLE {
        return;
    }
    present_state().req = *d;
    *d = EMPTY;
    STATE.store(COPY, Ordering::Release);
    // SAFETY: wakes CPU2 from its `wfe`.
    unsafe { core::arch::asm!("dsb", "sev", options(nostack, preserves_flags)) };
}

/// Waits until a present handed over by [`flush`] is on screen (boot and
/// error screens that must be visible before what follows).
pub fn flush_wait() {
    flush();
    while unsafe { !is_empty(&*(&raw const DIRTY)) } || STATE.load(Ordering::Acquire) != IDLE {
        if STATE.load(Ordering::Acquire) == IDLE {
            flush();
        }
        core::hint::spin_loop();
    }
}

/// One present: copy `prev ∪ req` from the shadow into the back buffer, run
/// `copied` (the shadow is free again), switch the FIMD to the back buffer
/// and wait until it scans it.
fn present(p: &mut Present, copied: impl FnOnce()) {
    let back_i = 1 - p.front;
    let r = union(p.prev, p.req);
    let src = buf_ptr(SHADOW);
    let (x, w) = (r[0], r[2] - r[0]);
    let nc = p.nc[back_i];
    let dst = if nc != 0 { nc as *mut u32 } else { buf_ptr(back_i) };
    for y in r[1]..r[3] {
        let at = y * W + x;
        // SAFETY: both boxes are clipped to the screen (`mark` callers); the
        // shadow and the scan buffers are distinct.
        unsafe { core::ptr::copy_nonoverlapping(src.add(at), dst.add(at), w) };
        if nc == 0 {
            mmu::clean_dcache_range(dst as usize + at * 4, w * 4);
        }
    }
    // The copy has reached DRAM before the FIMD may scan it.
    // SAFETY: a barrier only.
    unsafe { core::arch::asm!("dsb", options(nostack, preserves_flags)) };
    copied();

    let fimd = Fimd::new();
    if let Some(fb) = p.fb[back_i].as_ref() {
        if fimd.show(fb).is_ok() {
            let want = fb.address() as u32;
            let mut polls = 0;
            // No switch (FIMD off): go on as if it happened, as the
            // registers already name the new buffer.
            while fimd.scan_address() != want && polls < POLL_LIMIT {
                polls += 1;
                core::hint::spin_loop();
            }
        }
    }
    p.front = back_i;
    p.prev = p.req;
}

/// Starts the presenter on `cpu` (CPU0, after `Gic::init`). On failure
/// presents stay on CPU0.
pub fn start_presenter(cpu: usize) -> Result<(), smp::StartError> {
    smp::start_cpu(cpu, presenter)?;
    PRESENTER.store(true, Ordering::Release);
    Ok(())
}

/// The presenter core: waits for a hand-over, presents, repeats. No
/// interrupts: `sev` from CPU0 wakes it.
fn presenter(_cpu: usize) -> ! {
    loop {
        while STATE.load(Ordering::Acquire) != COPY {
            // SAFETY: sleeps until an event (CPU0's `sev`); a spurious wake
            // just checks again.
            unsafe { core::arch::asm!("wfe", options(nomem, nostack, preserves_flags)) };
        }
        present(present_state(), || STATE.store(FLIP, Ordering::Release));
        STATE.store(IDLE, Ordering::Release);
    }
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
        fence();
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
            fence();
            unsafe { *back().add(y * W + x) = rgb565_to_xrgb(color) };
            mark(x, y, 1, 1);
        }
    }

    // The stream writes (write_pixel/s, write_run) belong to the stream
    // `begin_pixels` opened, within one primitive: its fence covers them.
    fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16) {
        fence();
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
        fence();
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
        fence();
        let p = unsafe { &mut *back().add(y * W + x) };
        let src = rgb565_to_xrgb(color);
        *p = if alpha == 255 { src } else { fb::blend_argb8888(*p, src, fb::alpha8(alpha)) };
        mark(x, y, 1, 1);
    }
}

/// Powers the FIMD, starts it on scan buffer A and turns on LVDS + backlight.
pub fn init() -> Result<FimdLcd, display::Error> {
    let fimd = Fimd::new();
    fimd.power_on()?;
    display::configure_pins();
    let p = present_state();
    for i in [SCAN_A, SCAN_B] {
        // The framebuffer is kept only for `Fimd::show` (address + size);
        // nothing reads or writes pixels through it.
        // SAFETY: nothing else references the scan buffers yet.
        let pixels = unsafe { core::slice::from_raw_parts_mut(buf_ptr(i), PIXELS) };
        let fb = Framebuffer::new_cached(pixels, W, H).ok_or(display::Error::BadSize)?;
        mmu::clean_dcache_range(fb.address(), PIXELS * 4);
        // From here on the buffer is written only through the non-cacheable
        // alias. Drop its cached lines (clean, after the clean above), so
        // none is ever written back over the alias's stores.
        if let Some(nc) = mmu::uncached(fb.address()) {
            mmu::invalidate_dcache_range(fb.address(), PIXELS * 4);
            p.nc[i] = nc;
        }
        p.fb[i] = Some(fb);
    }
    if p.nc.iter().all(|&a| a != 0) {
        exynos_4412_rs::println!("ferrite: LCD scan buffers via non-cacheable alias {:#x}, {:#x}", p.nc[0], p.nc[1]);
    } else {
        exynos_4412_rs::println!("ferrite: LCD present via cached copy + clean");
    }
    fimd.start(&PANEL, p.fb[SCAN_A].as_ref().ok_or(display::Error::BadSize)?)?;
    display::set_lvds(true);
    systick::delay_ms(LVDS_SETTLE_MS);
    Ok(FimdLcd)
}
