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
//! only waits ([`fence`], at the start of every primitive) while CPU2 has not
//! yet copied the rows the primitive writes, never for the vsync: CPU2 copies
//! top to bottom and publishes its progress per row (`COPY_LO`), so CPU0,
//! which mostly redraws top to bottom too, draws behind it. If CPU2 is still busy at the next
//! flush, the box keeps growing and goes with a later one. Without CPU2
//! (before it starts, or if it failed to) [`flush`] presents on CPU0, waiting
//! for the switch.
//!
//! **G2D present on CPU0** (`g2d_present`, instead of the CPU2 presenter):
//! [`flush`] cleans the caches (L1 by set/way, the PL310 by way: the G2D
//! reads DRAM) and starts the G2D on the first 32-row band; the G2D interrupt
//! (on CPU0) starts the next band and, after the last, switches the FIMD
//! (`STATE` = FLIP). A later [`flush`] sees the switch at the scanned address
//! and goes back to IDLE. [`fence`] keeps the same row rule, and while it
//! waits it takes band ends itself (the interrupt may be masked). CPU2 is
//! not started.
//!
//! Drawing on the scanned buffer directly showed every redraw's intermediate
//! states (the erase before the paint), and in random 8-pixel pieces: dirty
//! cache lines reach DRAM in eviction order, not drawing order. That looked
//! like the static on an old TV screen.
//!
//! **Uncached shadow + G2D drawing** (`g2d_render`): the shadow is drawn
//! through the non-cacheable alias too, so the G2D sees what the CPU drew
//! without any cache maintenance: the present no longer cleans the caches
//! (~1 ms per present), and large fills and translucent fills go to the G2D
//! ([`g2d_fill`], [`g2d_blend_color`]), as do the board renderer's sprites
//! ([`blit_sprite`], `renderer.rs`). The cost: CPU blends read DRAM
//! uncached. A G2D operation runs synchronously, IRQs masked, after any
//! present copy (which owns the G2D and reads the shadow) has ended.
//!
//! **Sprites** ([`begin_sprite`] .. [`end_sprite`]): the primitives draw into
//! a caller's ARGB8888 buffer instead of the shadow, premultiplied: a fill
//! is opaque, a blend composites "over" what the buffer holds (transparent
//! at first), so the buffer can later be blended onto the screen.
//!
//! Buffered mode needs nothing more: the shadow already holds every frame, so
//! `back_buf` is 1 for the renderer's per-buffer tracking and `end_frame` only
//! flushes.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};

use exynos_4412_rs::drivers::display::{self, Fimd, Framebuffer, LTN101AL03, LVDS_SETTLE_MS, Timing};
#[cfg(feature = "g2d_render")]
use exynos_4412_rs::drivers::g2d::Alpha;
use exynos_4412_rs::drivers::g2d::{self, Filter, Format, G2d, Surface};
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::{cpu, mmu, smp, soc};
use ferrite_core::fb;
use ferrite_core::lcd::LcdBackend;
use ferrite_core::types::Rect as UiRect;

use crate::systick;

#[cfg(feature = "layers")]
#[path = "lcd_layers.rs"]
mod layers;

/// Some layer surface is allocated or shown (FIMD windows 1-4 are the
/// renderer's then: the overlay board test stays off).
pub fn layers_in_use() -> bool {
    #[cfg(feature = "layers")]
    {
        layers::in_use()
    }
    #[cfg(not(feature = "layers"))]
    {
        false
    }
}

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

/// Where the CPU reaches the shadow: its non-cacheable alias with
/// `g2d_render` (set by [`init`]), else (null) its cached address.
static mut SHADOW_PX: *mut u32 = core::ptr::null_mut();

fn back() -> *mut u32 {
    let p = unsafe { *(&raw const SHADOW_PX) };
    if p.is_null() { buf_ptr(SHADOW) } else { p }
}

/// The shadow is drawn uncached: the G2D may read it as it is.
fn shadow_uncached() -> bool {
    unsafe { !(*(&raw const SHADOW_PX)).is_null() }
}

/// Where the primitives draw: the shadow, a sprite ([`begin_sprite`]) or a
/// layer's surface (`layers`: drawn like a sprite, premultiplied, by the CPU).
#[derive(Clone, Copy)]
struct Target {
    px: *mut u32,
    stride: usize,
    w: usize,
    h: usize,
    /// Premultiplied off-screen surface: no G2D, no present fence.
    sprite: bool,
    /// The layer it is ([`NO_LAYER`] = none): `mark` goes to its dirty box.
    layer: u8,
}

const NO_LAYER: u8 = 0xFF;

static mut SPRITE: Option<Target> = None;

/// The layer surface the primitives draw into (`LcdBackend::layer_target`).
static mut LAYER_TARGET: Option<Target> = None;

#[inline(always)]
fn target() -> Target {
    if let Some(t) = unsafe { *(&raw const SPRITE) } {
        return t;
    }
    match unsafe { *(&raw const LAYER_TARGET) } {
        Some(t) => t,
        None => Target { px: back(), stride: W, w: W, h: H, sprite: false, layer: NO_LAYER },
    }
}

/// The primitives draw into a layer's surface: the board renderer's G2D
/// paths (which draw on the shadow) must not be used.
pub fn layer_targeted() -> bool {
    unsafe { (*(&raw const LAYER_TARGET)).is_some() }
}

impl Target {
    fn clip(&self, x: u16, y: u16, w: u16, h: u16) -> Option<(usize, usize, usize, usize)> {
        let (x, y) = (x as usize, y as usize);
        if w == 0 || h == 0 || x >= self.w || y >= self.h {
            return None;
        }
        Some((x, y, (w as usize).min(self.w - x), (h as usize).min(self.h - y)))
    }

    /// `w` pixels of row `y` from column `x`.
    ///
    /// # Safety
    /// Inside the target (`clip`), and no other live reference to them.
    #[inline(always)]
    unsafe fn row<'a>(&self, x: usize, y: usize, w: usize) -> &'a mut [u32] {
        unsafe { core::slice::from_raw_parts_mut(self.px.add(y * self.stride + x), w) }
    }

    #[inline(always)]
    fn fence(&self, y0: usize, y1: usize) {
        if !self.sprite {
            fence(y0, y1);
        }
    }

    #[inline(always)]
    fn mark(&self, x: usize, y: usize, w: usize, h: usize) {
        if !self.sprite {
            mark(x, y, w, h);
        } else if self.layer != NO_LAYER {
            #[cfg(feature = "layers")]
            layers::mark(self.layer as usize, x, y, w, h);
        }
    }

    /// Opaque XRGB `src` blended over `d` with weight `a` (`fb::alpha8`).
    #[inline(always)]
    fn mix(&self, d: u32, src: u32, a: u32) -> u32 {
        if self.sprite { over(d, src, a) } else { fb::blend_argb8888(d, src, a) }
    }
}

/// Premultiplied "over" of opaque `src` (alpha byte 0xFF) with coverage `a`
/// (0..=256) onto premultiplied `d`: every byte, alpha included, becomes
/// `(s * a + d * (256 - a)) >> 8`.
#[inline(always)]
fn over(d: u32, src: u32, a: u32) -> u32 {
    let na = 256 - a;
    let rb = (((src & 0x00FF_00FF) * a + (d & 0x00FF_00FF) * na) >> 8) & 0x00FF_00FF;
    let ag = ((((src >> 8) & 0x00FF_00FF) * a + ((d >> 8) & 0x00FF_00FF) * na) >> 8) & 0x00FF_00FF;
    rb | ag << 8
}

/// From here to [`end_sprite`] the primitives draw into `px` (`w` × `h`
/// ARGB8888, premultiplied, rows of `stride` pixels) at coordinates 0,0 ..
/// w,h.
///
/// # Safety
/// `px` holds `stride * h` words and nothing else references them until
/// `end_sprite`; `w <= stride`, `w <= W`, `h <= H`.
pub unsafe fn begin_sprite(px: *mut u32, w: usize, h: usize, stride: usize) {
    unsafe { *(&raw mut SPRITE) = Some(Target { px, stride, w, h, sprite: true, layer: NO_LAYER }) };
}

/// Back to the shadow. Cleans the sprite (`px`, `words` long) out of the
/// caches, so the G2D reads what was drawn.
pub fn end_sprite(px: *const u32, words: usize) {
    unsafe { *(&raw mut SPRITE) = None };
    mmu::clean_dcache_range(px as usize, words * 4);
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


// ============================================================================
//  Present
// ============================================================================

/// Presenter state, shared by CPU0 and CPU2.
static STATE: AtomicU8 = AtomicU8::new(IDLE);
/// No present in progress: CPU0 may hand a box over.
const IDLE: u8 = 0;
/// CPU2 is copying from the shadow: CPU0 must not draw on the rows
/// `COPY_LO..COPY_HI` ([`fence`]).
const COPY: u8 = 1;
/// The copy is done, CPU2 waits for the FIMD to switch: CPU0 may draw.
const FLIP: u8 = 2;

/// While `STATE` = COPY, the shadow rows CPU2 may still read: from `COPY_LO`
/// (the first row not yet copied; CPU2 raises it after each row, Release, so
/// its reads of the rows below are done) to `COPY_HI` (exclusive). CPU0 sets
/// them to 0..H before handing a box over; CPU2 narrows them.
static COPY_LO: AtomicUsize = AtomicUsize::new(0);
static COPY_HI: AtomicUsize = AtomicUsize::new(0);

/// Present copy statistics for the profiler (`perf`): cycles, presents,
/// bytes, added by whichever core presents; CPU0 takes them.
static COPY_CYCLES: AtomicU32 = AtomicU32::new(0);
static COPY_COUNT: AtomicU32 = AtomicU32::new(0);
static COPY_BYTES: AtomicU32 = AtomicU32::new(0);

/// Presents since boot, every present path (`sysinfo::SYS_FRAMES`).
static FRAMES: AtomicU32 = AtomicU32::new(0);

/// Frames presented since boot (wraps).
pub fn frames() -> u32 {
    FRAMES.load(Ordering::Relaxed)
}

/// (cycles, presents, bytes) of the present copies since the last call.
pub fn take_present_stats() -> (u32, u32, u32) {
    (
        COPY_CYCLES.swap(0, Ordering::Relaxed),
        COPY_COUNT.swap(0, Ordering::Relaxed),
        COPY_BYTES.swap(0, Ordering::Relaxed),
    )
}

/// CPU0 presents with the G2D (`g2d_present`, [`start_presenter`]). Cleared
/// for good if the G2D ever fails; CPU0 then copies itself.
static G2D_ON: AtomicBool = AtomicBool::new(false);
/// Rows per G2D copy: `COPY_LO` moves after each, so a primitive waits for
/// at most one band (~70 µs at full width).
const BAND: usize = 32;
/// Polls of a band's end by [`wait_rows`] before giving up on the G2D (far
/// more than a band takes).
const G2D_POLLS: u32 = 5_000_000;
/// After the switch (FLIP), how long [`flush`] waits for the FIMD to scan
/// the new buffer before going on anyway (FIMD off).
const FLIP_MS: u32 = 100;
/// `systick::millis` at the switch.
static FLIP_AT: AtomicU32 = AtomicU32::new(0);

/// The G2D present in flight: CPU0 only, used by the task and by the G2D
/// interrupt, so every user but the interrupt masks IRQs around it.
struct G2dJob {
    /// The box being copied (`prev ∪ req`).
    r: Rect,
    /// End of the band the G2D copies now (rows `COPY_LO..band_end`).
    band_end: usize,
    /// The back scan buffer: physical address (G2D) and non-cacheable alias
    /// (CPU fallback).
    back: usize,
    nc: usize,
    #[cfg(feature = "perf")]
    t0: u32,
}

struct JobCell(UnsafeCell<G2dJob>);
// SAFETY: see `G2dJob`.
unsafe impl Sync for JobCell {}

static JOB: JobCell = JobCell(UnsafeCell::new(G2dJob {
    r: EMPTY,
    band_end: 0,
    back: 0,
    nc: 0,
    #[cfg(feature = "perf")]
    t0: 0,
}));

fn job() -> &'static mut G2dJob {
    // SAFETY: see `G2dJob`: IRQs are masked (or this is the interrupt).
    unsafe { &mut *JOB.0.get() }
}

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
    /// Layer window changes that go out with this present's switch (written
    /// before `STATE` = COPY, taken by [`show`]).
    #[cfg(feature = "layers")]
    layers: layers::Commit,
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
    #[cfg(feature = "layers")]
    layers: layers::Commit { windows: [None; layers::COUNT] },
}));

fn present_state() -> &'static mut Present {
    // SAFETY: see `Present`: only the core that currently presents uses it.
    unsafe { &mut *PRESENT.0.get() }
}

/// Polls of the scanned address before giving up (FIMD off): far more than
/// one frame.
const POLL_LIMIT: u32 = 10_000_000;

/// Times a pixel primitive for the profiler (`perf`: the `PIXELS` row),
/// after its `fence`, so display waits stay in their own row.
/// Counts `px` pixels the CPU blended on the screen (not into a sprite) for
/// the profiler (`CPU_BLEND_KPX`).
macro_rules! cpu_blend {
    ($t:expr, $px:expr) => {{
        #[cfg(feature = "perf")]
        if !$t.sprite {
            crate::perf::add_px(ferrite_core::perf::count::CPU_BLEND_KPX, $px);
        }
    }};
}

macro_rules! timed {
    ($body:block) => {{
        #[cfg(feature = "perf")]
        let t0 = crate::perf::cycles();
        let r = $body;
        #[cfg(feature = "perf")]
        ferrite_core::perf::add_sub(ferrite_core::perf::phase::PIXELS, crate::perf::cycles().wrapping_sub(t0));
        r
    }};
}

/// CPU0, before writing shadow rows `y0..y1`: waits while the presenter may
/// still read one of them.
#[inline]
fn fence(y0: usize, y1: usize) {
    if STATE.load(Ordering::Relaxed) == COPY {
        wait_rows(y0, y1);
    }
}

#[cold]
#[inline(never)]
fn wait_rows(y0: usize, y1: usize) {
    #[cfg(feature = "perf")]
    let t0 = crate::perf::cycles();
    // `lo` is read before `hi`: CPU2 only raises `lo` and lowers `hi` within
    // a copy, so the pair always covers what it may still read.
    let mut polls = 0u32;
    while STATE.load(Ordering::Acquire) == COPY {
        let lo = COPY_LO.load(Ordering::Acquire);
        let hi = COPY_HI.load(Ordering::Acquire);
        if y1 <= lo || y0 >= hi {
            break;
        }
        // The G2D present (this core's): take band ends here, as the
        // interrupt may be masked; a G2D that never finishes is given up.
        if !PRESENTER.load(Ordering::Relaxed) {
            polls += 1;
            if polls < G2D_POLLS {
                g2d_poll();
            } else {
                g2d_abort();
            }
        }
        core::hint::spin_loop();
    }
    #[cfg(feature = "perf")]
    crate::perf::add_wait(crate::perf::cycles().wrapping_sub(t0));
}

/// Whether drawing now would reach the screen any sooner
/// (`PlatformRuntime::render_due`): false while something drawn since the
/// last present is waiting for a present still in flight (copying, or
/// copied and waiting for the scan-out switch). `flush` hands it over once
/// that present is done; drawing more meanwhile is wasted.
pub fn render_due() -> bool {
    let nothing_waits = unsafe { is_empty(&*(&raw const DIRTY)) };
    #[cfg(feature = "layers")]
    let nothing_waits = nothing_waits && !layers::pending();
    nothing_waits || STATE.load(Ordering::Acquire) == IDLE
}

/// Shows what was drawn since the last flush. With the presenter on CPU2:
/// hands the dirty box over and returns at once (or, if CPU2 is still busy,
/// keeps it for a later flush). With the G2D: starts its copy and returns
/// (or keeps the box while the last present is not on screen yet). Without
/// either: presents on this core.
pub fn flush() {
    if !PRESENTER.load(Ordering::Acquire) {
        match STATE.load(Ordering::Acquire) {
            FLIP => g2d_flipped(),
            COPY => return,
            _ => {}
        }
    }
    let d = unsafe { &mut *(&raw mut DIRTY) };
    // Layers: their new pixels into their back buffers; the window changes
    // go out with window 0's switch below, or now if it has nothing.
    #[cfg(feature = "layers")]
    if STATE.load(Ordering::Acquire) == IDLE {
        // A layer flip not on screen yet: window 0 waits too (same frame).
        let Some(commit) = layers::prepare() else { return };
        if is_empty(d) {
            layers::commit_now(&commit);
            return;
        }
        present_state().layers = commit;
    }
    if is_empty(d) {
        return;
    }
    if !PRESENTER.load(Ordering::Acquire) {
        match STATE.load(Ordering::Acquire) {
            IDLE if G2D_ON.load(Ordering::Relaxed) => g2d_start(d),
            IDLE => {
                let p = present_state();
                p.req = *d;
                *d = EMPTY;
                present(p, |_, _| {}, || {});
            }
            _ => {}
        }
        return;
    }
    if STATE.load(Ordering::Acquire) != IDLE {
        return;
    }
    present_state().req = *d;
    *d = EMPTY;
    // Every row is busy until CPU2 says which ones it copies.
    COPY_LO.store(0, Ordering::Relaxed);
    COPY_HI.store(H, Ordering::Relaxed);
    STATE.store(COPY, Ordering::Release);
    // SAFETY: wakes CPU2 from its `wfe`.
    unsafe { core::arch::asm!("dsb", "sev", options(nostack, preserves_flags)) };
}

/// Waits until a present handed over by [`flush`] is on screen (boot and
/// error screens that must be visible before what follows).
pub fn flush_wait() {
    flush();
    while unsafe { !is_empty(&*(&raw const DIRTY)) } || STATE.load(Ordering::Acquire) != IDLE || layers_pending() {
        match STATE.load(Ordering::Acquire) {
            // The G2D present: drive it to its end.
            COPY if !PRESENTER.load(Ordering::Acquire) => wait_rows(0, H),
            COPY => {}
            // IDLE: present; FLIP: on CPU0, `flush` looks for the switch.
            _ => flush(),
        }
        core::hint::spin_loop();
    }
}

fn layers_pending() -> bool {
    #[cfg(feature = "layers")]
    {
        layers::pending()
    }
    #[cfg(not(feature = "layers"))]
    {
        false
    }
}

/// Switches the FIMD to scan buffer `i` from the next vsync, with the layer
/// window changes handed over for this present in the same frame.
fn show(p: &mut Present, i: usize) -> Result<(), display::Error> {
    let fimd = Fimd::new();
    #[cfg(feature = "layers")]
    let commit = core::mem::take(&mut p.layers);
    #[cfg(feature = "layers")]
    let hold = if commit.any() { 1 | commit.mask() } else { 0 };
    #[cfg(feature = "layers")]
    if hold != 0 {
        fimd.hold_updates(hold);
    }
    let r = match p.fb[i].as_ref() {
        Some(fb) => fimd.show(fb),
        None => Err(display::Error::BadSize),
    };
    #[cfg(feature = "layers")]
    if hold != 0 {
        commit.apply(&fimd);
        fimd.release_updates(hold);
    }
    r
}

/// CPU0, IDLE, G2D on: starts the G2D present of `d` (+ the previous box).
fn g2d_start(d: &mut Rect) {
    let p = present_state();
    let back_i = 1 - p.front;
    let Some(back) = p.fb[back_i].as_ref().map(|fb| fb.address()) else {
        G2D_ON.store(false, Ordering::Relaxed);
        return;
    };
    p.req = *d;
    *d = EMPTY;
    let r = union(p.prev, p.req);
    let nc = p.nc[back_i];
    #[cfg(feature = "perf")]
    let t0 = crate::perf::cycles();
    // The G2D reads DRAM: what this core drew is still in its L1 and in the
    // PL310. L1 by set/way is ~1000 operations, the PL310 by way one
    // background operation for the whole cache.
    if shadow_uncached() {
        // Drawn through the alias: only this core's buffered stores must
        // reach DRAM before the G2D reads them.
        // SAFETY: a barrier only.
        unsafe { core::arch::asm!("dsb", options(nostack, preserves_flags)) };
    } else {
        mmu::clean_l1_all();
        L2c::new().clean_all();
    }
    COPY_LO.store(r[1], Ordering::Relaxed);
    COPY_HI.store(r[3], Ordering::Relaxed);
    STATE.store(COPY, Ordering::Release);
    let masked = cpu::mask_irq();
    let j = job();
    j.r = r;
    j.band_end = r[1];
    j.back = back;
    j.nc = nc;
    #[cfg(feature = "perf")]
    {
        j.t0 = t0;
    }
    g2d_next(j);
    cpu::restore_irq(masked);
}

/// The G2D interrupt (CPU0): a band has ended.
pub fn g2d_isr() {
    g2d_step();
}

/// Takes a band end if there is one, IRQs masked around it.
fn g2d_poll() {
    let masked = cpu::mask_irq();
    g2d_step();
    cpu::restore_irq(masked);
}

/// IRQs masked: if the band in flight has ended, starts the next one (or
/// switches the FIMD after the last).
fn g2d_step() {
    let g = G2d::new();
    if STATE.load(Ordering::Acquire) == COPY && !PRESENTER.load(Ordering::Relaxed) {
        if g.ack() {
            g2d_next(job());
        }
    } else if g.is_done() && !g.ack() {
        // Not a band's end, nor an operation waited for (drawing waits with
        // this interrupt off at the GIC): one given up on that ended late.
        // Its line stays asserted until cleared.
        g2d_shutdown();
    }
}

/// IRQs masked: rows below `band_end` are copied; starts the next band, or
/// switches the FIMD. A G2D that refuses → CPU copy of the rest.
fn g2d_next(j: &mut G2dJob) {
    let y = j.band_end;
    COPY_LO.store(y, Ordering::Release);
    if y < j.r[3] {
        let y1 = (y + BAND).min(j.r[3]);
        if g2d_rows(j.back, j.r[0], y, y1, j.r[2] - j.r[0]) {
            j.band_end = y1;
            return;
        }
        G2D_ON.store(false, Ordering::Relaxed);
        exynos_4412_rs::println!("ferrite: G2D refused a present band ({:?}), CPU copy from now on", G2d::new().status());
        #[cfg(feature = "g2d_render")]
        g2d_give_up();
        g2d_shutdown();
        cpu_copy(j, y);
    }
    g2d_finish(j);
}

/// The G2D is given up for good (it did not finish, or refused): its
/// interrupt off at the GIC, then re-initialised as at boot (`power_on`:
/// interrupt disabled and cleared, nothing marked running), so a late end
/// raises nothing and no later check waits for it.
fn g2d_shutdown() {
    Gic::new().disable(soc::G2D_IRQ);
    G2d::new().power_on(g2d::DEFAULT_HZ);
}

/// The G2D did not finish a band: give it up for good, copy the rest of the
/// box on this core and switch.
#[cold]
fn g2d_abort() {
    let masked = cpu::mask_irq();
    if STATE.load(Ordering::Acquire) == COPY {
        G2D_ON.store(false, Ordering::Relaxed);
        exynos_4412_rs::println!("ferrite: G2D present timed out ({:?}), CPU copy from now on", G2d::new().status());
        #[cfg(feature = "g2d_render")]
        g2d_give_up();
        g2d_shutdown();
        let j = job();
        // From the band in flight on (the G2D may still write it: the same
        // pixels).
        cpu_copy(j, COPY_LO.load(Ordering::Relaxed));
        g2d_finish(j);
    }
    cpu::restore_irq(masked);
}

/// Copies rows `y..` of the job's box with the CPU.
fn cpu_copy(j: &G2dJob, y: usize) {
    let (src, dst) = (back(), j.nc as *mut u32);
    let (x, w) = (j.r[0], j.r[2] - j.r[0]);
    for row in y..j.r[3] {
        let at = row * W + x;
        // SAFETY: the box is clipped to the screen; `nc` is the back
        // buffer's alias (G2D mode needs one).
        unsafe { copy_row(src.add(at), dst.add(at), w) };
        COPY_LO.store(row + 1, Ordering::Release);
    }
}

/// IRQs masked: the copy is done; switch the FIMD to the back buffer
/// (at the next vsync) and let CPU0 draw everywhere (FLIP).
fn g2d_finish(j: &G2dJob) {
    // The copy has reached DRAM before the FIMD may scan it.
    // SAFETY: a barrier only.
    unsafe { core::arch::asm!("dsb", options(nostack, preserves_flags)) };
    #[cfg(feature = "perf")]
    {
        COPY_CYCLES.fetch_add(crate::perf::cycles().wrapping_sub(j.t0), Ordering::Relaxed);
        COPY_COUNT.fetch_add(1, Ordering::Relaxed);
        COPY_BYTES.fetch_add(((j.r[3] - j.r[1]) * (j.r[2] - j.r[0]) * 4) as u32, Ordering::Relaxed);
    }
    #[cfg(not(feature = "perf"))]
    let _ = j;
    FRAMES.fetch_add(1, Ordering::Relaxed);
    let p = present_state();
    let _ = show(p, 1 - p.front);
    FLIP_AT.store(systick::millis(), Ordering::Relaxed);
    STATE.store(FLIP, Ordering::Release);
}

/// FLIP, G2D present: once the FIMD scans the new buffer (or after
/// `FLIP_MS`: FIMD off), it is the front and the next present may start.
fn g2d_flipped() {
    let p = present_state();
    let back_i = 1 - p.front;
    let want = p.fb[back_i].as_ref().map(|fb| fb.address() as u32);
    let switched = want.is_none_or(|a| Fimd::new().scan_address() == a);
    if !switched && systick::millis().wrapping_sub(FLIP_AT.load(Ordering::Relaxed)) < FLIP_MS {
        return;
    }
    p.front = back_i;
    p.prev = p.req;
    STATE.store(IDLE, Ordering::Release);
}

/// One present: copy `prev ∪ req` from the shadow into the back buffer
/// (`rows(lo, hi)` first, then `rows(y + 1, hi)` after each row: the shadow
/// rows it may still read), run `copied` (the shadow is free again), switch
/// the FIMD to the back buffer and wait until it scans it.
fn present(p: &mut Present, rows: impl Fn(usize, usize), copied: impl FnOnce()) {
    let back_i = 1 - p.front;
    let r = union(p.prev, p.req);
    // The alias with an uncached shadow: a cached read could hit lines from
    // before CPU0's uncached writes.
    let src = back();
    let (x, w) = (r[0], r[2] - r[0]);
    let nc = p.nc[back_i];
    let dst = if nc != 0 { nc as *mut u32 } else { buf_ptr(back_i) };
    rows(r[1], r[3]);
    #[cfg(feature = "perf")]
    let t0 = crate::perf::cycles();
    for y in r[1]..r[3] {
        let at = y * W + x;
        // SAFETY: both boxes are clipped to the screen (`mark` callers);
        // the shadow and the scan buffers are distinct.
        unsafe { copy_row(src.add(at), dst.add(at), w) };
        if nc == 0 {
            mmu::clean_dcache_range(dst as usize + at * 4, w * 4);
        }
        rows(y + 1, r[3]);
    }
    // The copy has reached DRAM before the FIMD may scan it.
    // SAFETY: a barrier only.
    unsafe { core::arch::asm!("dsb", options(nostack, preserves_flags)) };
    #[cfg(feature = "perf")]
    {
        COPY_CYCLES.fetch_add(crate::perf::cycles().wrapping_sub(t0), Ordering::Relaxed);
        COPY_COUNT.fetch_add(1, Ordering::Relaxed);
        COPY_BYTES.fetch_add(((r[3] - r[1]) * w * 4) as u32, Ordering::Relaxed);
    }
    FRAMES.fetch_add(1, Ordering::Relaxed);
    copied();

    let fimd = Fimd::new();
    if show(p, back_i).is_ok()
        && let Some(want) = p.fb[back_i].as_ref().map(|fb| fb.address() as u32)
    {
        let mut polls = 0;
        // No switch (FIMD off): go on as if it happened, as the registers
        // already name the new buffer.
        while fimd.scan_address() != want && polls < POLL_LIMIT {
            polls += 1;
            core::hint::spin_loop();
        }
    }
    p.front = back_i;
    p.prev = p.req;
}

/// Sets up how presents run (CPU0, after `Gic::init`, IRQs on): with the G2D
/// driven by CPU0 (`g2d_present`), else the presenter on `cpu`. Returns
/// which; on failure presents stay on CPU0 (CPU copy).
pub fn start_presenter(cpu: usize) -> Result<&'static str, smp::StartError> {
    // The G2D copy needs the L2 clean by way; erratum 727915 (a corrupting
    // clean + invalidate by way) is r2p0's, this board's PL310 is r3p2. Its
    // CPU fallback writes through the non-cacheable alias.
    #[cfg(feature = "g2d_present")]
    {
        let l2 = L2c::new();
        let p = present_state();
        let buffers = p.nc.iter().all(|&a| a != 0) && p.fb.iter().all(Option::is_some);
        if buffers && (!l2.is_enabled() || l2.revision() >= 0x05) {
            let hz = G2d::new().power_on(g2d::DEFAULT_HZ);
            let gic = Gic::new();
            gic.set_target(soc::G2D_IRQ, 0);
            gic.enable(soc::G2D_IRQ);
            G2D_ON.store(true, Ordering::Release);
            #[cfg(feature = "g2d_render")]
            enable_g2d_draw();
            exynos_4412_rs::println!("ferrite: present copy by G2D at {} MHz", hz / 1_000_000);
            return Ok("LCD presents by the G2D, driven by CPU0 (CPU2 not started)");
        }
    }
    smp::start_cpu(cpu, presenter)?;
    PRESENTER.store(true, Ordering::Release);
    // The presenter copies with NEON; the G2D is free for drawing.
    #[cfg(feature = "g2d_render")]
    {
        let hz = G2d::new().power_on(g2d::DEFAULT_HZ);
        enable_g2d_draw();
        exynos_4412_rs::println!("ferrite: G2D at {} MHz for drawing", hz / 1_000_000);
    }
    Ok("LCD presenter on CPU2")
}

/// Starts the G2D copy of rows `y0..y1`, columns `x..x + w`, from the shadow
/// to the scan buffer at physical address `back` (its end: the interrupt or
/// [`G2d::ack`]). False if it refused. The shadow must be in DRAM
/// ([`g2d_start`]'s cleans).
fn g2d_rows(back: usize, x: usize, y0: usize, y1: usize, w: usize) -> bool {
    let shadow = Surface { address: buf_ptr(SHADOW) as usize, stride: W * 4, width: W, height: H, format: Format::Xrgb8888 };
    let target = Surface { address: back, ..shadow };
    let rect = g2d::Rect::new(x, y0, w, y1 - y0);
    let g = G2d::new();
    g.copy(&shadow, rect, &target, rect, Filter::Nearest).is_ok()
}

/// Copies `n` pixels from `src` (the cached shadow) to `dst`: NEON, 64 bytes
/// per step, with the source prefetched (`pld`) 256 bytes ahead. The shadow
/// box is mostly not in the caches (a full screen is 4 MB, the L2 1 MB), so
/// the loads would wait for DRAM line by line without it.
///
/// # Safety
/// `src..src+n` readable, `dst..dst+n` writable, not overlapping; both 4-byte
/// aligned (NEON `vld1.32`/`vst1.32` allow that: SCTLR.A is clear).
#[inline(always)]
unsafe fn copy_row(mut src: *const u32, mut dst: *mut u32, n: usize) {
    let mut blocks = n / 16;
    if blocks != 0 {
        // SAFETY: the caller's ranges; d0-d7 are clobbered.
        unsafe {
            core::arch::asm!(
                "2:",
                "pld [{s}, #256]",
                "pld [{s}, #288]",
                "vld1.32 {{d0-d3}}, [{s}]!",
                "vld1.32 {{d4-d7}}, [{s}]!",
                "subs {b}, {b}, #1",
                "vst1.32 {{d0-d3}}, [{d}]!",
                "vst1.32 {{d4-d7}}, [{d}]!",
                "bne 2b",
                s = inout(reg) src,
                d = inout(reg) dst,
                b = inout(reg) blocks,
                out("d0") _, out("d1") _, out("d2") _, out("d3") _,
                out("d4") _, out("d5") _, out("d6") _, out("d7") _,
                options(nostack),
            );
        }
    }
    let _ = blocks;
    for i in 0..n % 16 {
        // SAFETY: the tail of the caller's ranges.
        unsafe { *dst.add(i) = *src.add(i) };
    }
}

/// Blends the opaque XRGB `src` over every pixel of `px` with weight `a`
/// (`fb::alpha8`, 1..=255 here: alpha 255 is a fill): NEON, 8 pixels per
/// step, bit-identical to `fb::blend_argb8888`. Each byte channel is
/// `(s * a + d * (256 - a)) >> 8` in a u16 lane (at most 255 * 256, so
/// nothing carries into the next channel), the X byte forced to 0xFF.
///
/// # Safety
/// `px` is the cached shadow (read-modify-write; `pld` ahead).
#[inline(always)]
unsafe fn blend_fill(px: &mut [u32], src: u32, a: u32) {
    debug_assert!((1..=255).contains(&a));
    let n = px.len();
    let mut p = px.as_mut_ptr();
    let mut blocks = n / 8;
    if blocks != 0 {
        // s * a per byte channel, for the 2 pixels of a d register.
        let sa: [u16; 8] = core::array::from_fn(|i| ((src >> (8 * (i % 4))) & 0xFF) as u16 * a as u16);
        let na = 256 - a;
        // SAFETY: reads and writes `blocks * 8` words from `p`, all inside
        // `px`; q8-q11 and q13-q15 are clobbered, `sa` is only read.
        unsafe {
            core::arch::asm!(
                "vld1.16 {{d28-d29}}, [{sa}]",
                "vdup.8 d30, {na}",
                "vmov.i32 q13, #0xFF000000",
                "2:",
                "pld [{p}, #192]",
                "vld1.32 {{d0-d3}}, [{p}]",
                "vmull.u8 q8, d0, d30",
                "vmull.u8 q9, d1, d30",
                "vmull.u8 q10, d2, d30",
                "vmull.u8 q11, d3, d30",
                "vadd.i16 q8, q8, q14",
                "vadd.i16 q9, q9, q14",
                "vadd.i16 q10, q10, q14",
                "vadd.i16 q11, q11, q14",
                "vshrn.i16 d0, q8, #8",
                "vshrn.i16 d1, q9, #8",
                "vshrn.i16 d2, q10, #8",
                "vshrn.i16 d3, q11, #8",
                "vorr q0, q0, q13",
                "vorr q1, q1, q13",
                "subs {b}, {b}, #1",
                "vst1.32 {{d0-d3}}, [{p}]!",
                "bne 2b",
                p = inout(reg) p,
                b = inout(reg) blocks,
                sa = in(reg) sa.as_ptr(),
                na = in(reg) na,
                out("d0") _, out("d1") _, out("d2") _, out("d3") _,
                out("d16") _, out("d17") _, out("d18") _, out("d19") _,
                out("d20") _, out("d21") _, out("d22") _, out("d23") _,
                out("d26") _, out("d27") _, out("d28") _, out("d29") _, out("d30") _,
                options(nostack),
            );
        }
    }
    let _ = blocks;
    for i in 0..n % 8 {
        // SAFETY: the tail of `px`.
        unsafe { *p.add(i) = fb::blend_argb8888(*p.add(i), src, a) };
    }
}

/// The presenter core: waits for a hand-over, presents, repeats. No
/// interrupts: `sev` from CPU0 wakes it.
fn presenter(_cpu: usize) -> ! {
    #[cfg(feature = "perf")]
    crate::perf::enable_counter();
    loop {
        while STATE.load(Ordering::Acquire) != COPY {
            // SAFETY: sleeps until an event (CPU0's `sev`); a spurious wake
            // just checks again.
            unsafe { core::arch::asm!("wfe", options(nomem, nostack, preserves_flags)) };
        }
        let rows = |lo, hi| {
            // Each store only shrinks the busy range, so any pair CPU0 reads
            // (`lo`, then `hi`) still covers what CPU2 may read.
            COPY_HI.store(hi, Ordering::Release);
            COPY_LO.store(lo, Ordering::Release);
        };
        present(present_state(), rows, || STATE.store(FLIP, Ordering::Release));
        STATE.store(IDLE, Ordering::Release);
    }
}

pub struct FimdLcd;

/// Fills of at least this many pixels go to the G2D (`g2d_render`): it
/// fills at ~3.2 GB/s, the CPU through the alias at ~1.8 GB/s, and each G2D
/// operation costs a setup and a wait.
#[cfg(feature = "g2d_render")]
const G2D_FILL_MIN: usize = 64 * 1024;
/// Translucent fills of at least this many pixels go to the G2D: the CPU
/// would read every pixel back from DRAM uncached.
#[cfg(feature = "g2d_render")]
const G2D_BLEND_MIN: usize = 2048;

impl LcdBackend for FimdLcd {
    const WIDTH: u16 = W as u16;
    const HEIGHT: u16 = H as u16;
    // DRAM framebuffer: read-modify-write blending (full anti-aliasing).
    // Mirror in tools/devices/exynos4412.json caps.alpha.
    const HAS_ALPHA: bool = true;
    // `move_rects`: the G2D through the scratch surface, else the CPU.
    const CAN_MOVE: bool = true;
    // Each moved rect costs two G2D copies (~5 µs setup each): small boxes
    // are redrawn instead.
    const MOVE_MIN_PX: u32 = 128 * 128;

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
        let t = target();
        let Some((x, y, w, h)) = t.clip(x, y, w, h) else { return };
        let c = rgb565_to_xrgb(color);
        #[cfg(feature = "g2d_render")]
        if !t.sprite && w * h >= G2D_FILL_MIN && g2d_fill(x, y, w, h, c) {
            return;
        }
        t.fence(y, y + h);
        timed!({
            for r in y..y + h {
                fb::fill_u32(unsafe { t.row(x, r, w) }, c);
            }
            t.mark(x, y, w, h);
        })
    }

    fn draw_pixel(&self, x: u16, y: u16, color: u16) {
        let t = target();
        let (x, y) = (x as usize, y as usize);
        if x < t.w && y < t.h {
            t.fence(y, y + 1);
            timed!({
                unsafe { t.row(x, y, 1)[0] = rgb565_to_xrgb(color) };
                t.mark(x, y, 1, 1);
            })
        }
    }

    // The stream writes (write_pixel/s, write_run) belong to the stream
    // `begin_pixels` opened, within one primitive: its fence covers them.
    fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16) {
        let t = target();
        // The stream writes only inside the clipped window.
        let window = t.clip(x, y, w, h);
        if let Some((_, wy, _, wh)) = window {
            t.fence(wy, wy + wh);
        }
        unsafe { (*(&raw mut CURSOR)).begin(x, y, w) };
        if let Some((x, y, w, h)) = window {
            t.mark(x, y, w, h);
        }
    }

    fn write_pixel(&self, color: u16) {
        let t = target();
        timed!({
            if let Some((x, y)) = unsafe { (*(&raw mut CURSOR)).step(t.w, t.h) } {
                unsafe { t.row(x, y, 1)[0] = rgb565_to_xrgb(color) };
            }
        })
    }

    fn write_pixels(&self, colors: &[u16]) {
        let t = target();
        timed!({
            unsafe {
                (*(&raw mut CURSOR)).advance(colors.len(), t.w, t.h, |x, y, len, skip| {
                    for (d, &c) in t.row(x, y, len).iter_mut().zip(&colors[skip..skip + len]) {
                        *d = rgb565_to_xrgb(c);
                    }
                });
            }
        })
    }

    fn write_run(&self, color: u16, n: usize) {
        let (t, c) = (target(), rgb565_to_xrgb(color));
        timed!({
            unsafe {
                (*(&raw mut CURSOR)).advance(n, t.w, t.h, |x, y, len, _| fb::fill_u32(t.row(x, y, len), c));
            }
        })
    }

    fn send_command(&self, _cmd: u16) {}
    fn send_data(&self, _data: u16) {}

    fn move_rects(&self, rects: &[UiRect], dx: i16, dy: i16) -> bool {
        // The caller (`LcdImpl::move_rects`) checked that every source and
        // destination is inside the screen.
        if unsafe { (*(&raw const SPRITE)).is_some() } || layer_targeted() || rects.is_empty() {
            return false;
        }
        let rs = rects.iter().map(|r| (r.x as usize, r.y as usize, r.w as usize, r.h as usize));
        let (dx, dy) = (dx as isize, dy as isize);
        let to = |x: usize, y: usize| (x.wrapping_add_signed(dx), y.wrapping_add_signed(dy));
        #[cfg(feature = "g2d_render")]
        for way in [Moved::Bands, Moved::Buffered] {
            match g2d_move(way, rects, dx as i16, dy as i16) {
                Moved::Done => return true,
                // Sources may be overwritten already: no other way can
                // finish it (the caller redraws instead).
                Moved::Partly => return false,
                _ => {}
            }
        }
        cpu_move(rs, to)
    }

    fn blend_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16, alpha: u8) {
        if alpha == 255 {
            return self.fill_rect(x, y, w, h, color);
        }
        let t = target();
        let Some((x, y, w, h)) = t.clip(x, y, w, h) else { return };
        if alpha == 0 {
            return;
        }
        let (src, a) = (rgb565_to_xrgb(color), fb::alpha8(alpha));
        #[cfg(feature = "g2d_render")]
        if !t.sprite && w * h >= G2D_BLEND_MIN && g2d_blend_color(x, y, w, h, src, alpha) {
            return;
        }
        t.fence(y, y + h);
        cpu_blend!(t, w * h);
        timed!({
            for r in y..y + h {
                let px = unsafe { t.row(x, r, w) };
                if t.sprite {
                    for p in px {
                        *p = over(*p, src, a);
                    }
                } else {
                    // SAFETY: `row` is inside the shadow (clipped above).
                    unsafe { blend_fill(px, src, a) };
                }
            }
            t.mark(x, y, w, h);
        })
    }

    fn blend_pixel(&self, x: u16, y: u16, color: u16, alpha: u8) {
        let t = target();
        let (x, y) = (x as usize, y as usize);
        if alpha == 0 || x >= t.w || y >= t.h {
            return;
        }
        t.fence(y, y + 1);
        cpu_blend!(t, 1);
        timed!({
            let p = unsafe { &mut t.row(x, y, 1)[0] };
            let src = rgb565_to_xrgb(color);
            *p = if alpha == 255 { src } else { t.mix(*p, src, fb::alpha8(alpha)) };
            t.mark(x, y, 1, 1);
        })
    }

    // Text and alpha images: one fence, timer and mark per row instead of
    // per pixel (each `blend_pixel` paid all three).
    fn blend_span(&self, x: u16, y: u16, color: u16, alphas: &[u8]) {
        let t = target();
        let Some((x, y, w, _)) = t.clip(x, y, alphas.len() as u16, 1) else { return };
        t.fence(y, y + 1);
        cpu_blend!(t, w);
        timed!({
            let src = rgb565_to_xrgb(color);
            for (p, &a) in unsafe { t.row(x, y, w) }.iter_mut().zip(alphas) {
                match a {
                    0 => {}
                    255 => *p = src,
                    _ => *p = t.mix(*p, src, fb::alpha8(a)),
                }
            }
            t.mark(x, y, w, 1);
        })
    }

    fn blend_row(&self, x: u16, y: u16, colors: &[u16], alphas: &[u8]) {
        let t = target();
        let n = colors.len().min(alphas.len());
        let Some((x, y, w, _)) = t.clip(x, y, n as u16, 1) else { return };
        t.fence(y, y + 1);
        cpu_blend!(t, w);
        timed!({
            let px = unsafe { t.row(x, y, w) };
            for ((p, &c), &a) in px.iter_mut().zip(colors).zip(alphas) {
                match a {
                    0 => {}
                    255 => *p = rgb565_to_xrgb(c),
                    _ => *p = t.mix(*p, rgb565_to_xrgb(c), fb::alpha8(a)),
                }
            }
            t.mark(x, y, w, 1);
        })
    }

    // Retained layers on FIMD windows 1-4 (`lcd_layers.rs`).
    #[cfg(feature = "layers")]
    const LAYERS: usize = layers::COUNT;

    #[cfg(feature = "layers")]
    fn layer_alloc(&self, n: usize, w: u16, h: u16) -> bool {
        layers::alloc(n, w as usize, h as usize)
    }

    #[cfg(feature = "layers")]
    fn layer_free(&self, n: usize) {
        layers::free(n);
    }

    #[cfg(feature = "layers")]
    fn layer_target(&self, n: Option<usize>) {
        layers::target(n);
    }

    #[cfg(feature = "layers")]
    fn layer_clear(&self, x: u16, y: u16, w: u16, h: u16) {
        layers::clear(x, y, w, h);
    }

    #[cfg(feature = "layers")]
    fn layer_place(&self, n: usize, x: i32, y: i32, alpha: u8, visible: bool) {
        layers::place(n, x, y, alpha, visible);
    }
}

// ============================================================================
//  G2D drawing (`g2d_render`)
// ============================================================================

/// The G2D may draw on the shadow: it is powered, idle between presents,
/// and the shadow is uncached. Cleared for good if an operation times out.
#[cfg(feature = "g2d_render")]
static G2D_DRAW: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "g2d_render")]
fn enable_g2d_draw() {
    if shadow_uncached() {
        G2D_DRAW.store(true, Ordering::Release);
    } else {
        exynos_4412_rs::println!("ferrite: shadow outside the uncached alias, no G2D drawing");
    }
}

/// G2D drawing is on ([`blit_sprite`] may be used).
pub fn g2d_draw_ready() -> bool {
    #[cfg(feature = "g2d_render")]
    {
        G2D_DRAW.load(Ordering::Relaxed)
    }
    #[cfg(not(feature = "g2d_render"))]
    {
        false
    }
}

#[cfg(feature = "g2d_render")]
fn shadow_surface() -> Surface {
    // The G2D takes physical addresses: the shadow's identity-mapped one.
    Surface { address: buf_ptr(SHADOW) as usize, stride: W * 4, width: W, height: H, format: Format::Xrgb8888 }
}

/// How long one drawing operation may take: a full-screen blend takes
/// ~3 ms.
#[cfg(feature = "g2d_render")]
const G2D_OP_MS: u32 = 50;

/// G2D drawing is off for good: the CPU draws, into the cached shadow again
/// (its reads are fast there, and a CPU present copy reads it). Safe at any
/// time on CPU0: the shadow has no cached line (invalidated at `init`, only
/// reached through the alias since), so cached reads fetch what DRAM holds.
#[cfg(feature = "g2d_render")]
fn g2d_give_up() {
    G2D_DRAW.store(false, Ordering::Relaxed);
    // SAFETY: a barrier only: the alias's stores are in DRAM first.
    unsafe { core::arch::asm!("dsb", options(nostack, preserves_flags)) };
    unsafe { *(&raw mut SHADOW_PX) = core::ptr::null_mut() };
}

/// Runs one G2D operation on the shadow and waits for its end. First waits
/// for a present copy in flight (it reads the shadow, and the G2D present
/// owns the G2D). Interrupts stay enabled (audio refills, the console); only
/// the G2D's own is off at the GIC meanwhile, so `g2d_isr` does not take
/// this operation's end. False if G2D drawing is off or the operation failed
/// (then the caller draws with the CPU).
#[cfg(feature = "g2d_render")]
fn g2d_op(what: &str, px: usize, op: impl FnOnce(&G2d) -> Result<(), g2d::Error>) -> bool {
    if !G2D_DRAW.load(Ordering::Relaxed) {
        return false;
    }
    #[cfg(feature = "perf")]
    let t0 = crate::perf::cycles();
    let ok = g2d_op_inner(what, op);
    #[cfg(feature = "perf")]
    {
        ferrite_core::perf::add_sub(ferrite_core::perf::phase::G2D, crate::perf::cycles().wrapping_sub(t0));
        crate::perf::add_px(ferrite_core::perf::count::G2D_KPX, px);
    }
    #[cfg(not(feature = "perf"))]
    let _ = px;
    ok
}

#[cfg(feature = "g2d_render")]
fn g2d_op_inner(what: &str, op: impl FnOnce(&G2d) -> Result<(), g2d::Error>) -> bool {
    if STATE.load(Ordering::Acquire) == COPY {
        // Rows 0..H: until the copy has ended.
        wait_rows(0, H);
    }
    timed!({
        let gic = Gic::new();
        let irq_on = gic.is_enabled(soc::G2D_IRQ);
        if irq_on {
            gic.disable(soc::G2D_IRQ);
        }
        // What this core wrote through the alias reaches DRAM before the
        // G2D reads it (a blend reads the destination).
        // SAFETY: a barrier only.
        unsafe { core::arch::asm!("dsb", options(nostack, preserves_flags)) };
        let g = G2d::new();
        let result = op(&g);
        let mut ok = result.is_ok();
        if let Err(e) = result {
            // Refused before starting (a bad rect): this one by the CPU.
            exynos_4412_rs::println!("ferrite: G2D {} refused: {:?}", what, e);
        } else {
            let t0 = systick::millis();
            while !g.ack() {
                if systick::millis().wrapping_sub(t0) > G2D_OP_MS {
                    ok = false;
                    exynos_4412_rs::println!("ferrite: G2D {} timed out ({:?}), CPU drawing from now on", what, g.status());
                    g2d_give_up();
                    // The present cannot use it either.
                    G2D_ON.store(false, Ordering::Relaxed);
                    g2d_shutdown();
                    break;
                }
                core::hint::spin_loop();
            }
        }
        // SAFETY: a barrier only: the G2D's writes before this core's next
        // reads of the shadow.
        unsafe { core::arch::asm!("dsb", options(nostack, preserves_flags)) };
        if irq_on && ok {
            gic.enable(soc::G2D_IRQ);
        }
        ok
    })
}

/// Fills a shadow rect (clipped to the screen) with XRGB `c` by the G2D.
#[cfg(feature = "g2d_render")]
fn g2d_fill(x: usize, y: usize, w: usize, h: usize, c: u32) -> bool {
    let ok = g2d_op("fill", w * h, |g| g.fill(&shadow_surface(), g2d::Rect::new(x, y, w, h), c));
    if ok {
        mark(x, y, w, h);
    }
    ok
}

/// The source of [`g2d_blend_color`]: a screen-sized ARGB8888 surface only
/// the G2D writes, holding `color` (premultiplied) in its top-left `w` × `h`.
#[cfg(feature = "g2d_render")]
struct Scratch {
    /// Holds the surface at `base` (64-byte aligned, whole cache lines).
    _px: alloc::vec::Vec<u32>,
    base: usize,
    color: u32,
    w: usize,
    h: usize,
}

#[cfg(feature = "g2d_render")]
static mut SCRATCH: Option<Scratch> = None;

/// The scratch surface, allocated at the first use. Its memory is cleaned
/// and invalidated once: no cached line of it (the heap's earlier use) may
/// be written back over the G2D's fills, and the CPU never touches it again.
/// The heap gives no line alignment, so the surface starts on a 64-byte
/// boundary inside a larger block: an invalidate must cover whole lines
/// (the HAL refuses others, as they could hold a neighbour's data).
#[cfg(feature = "g2d_render")]
fn scratch() -> Option<&'static mut Scratch> {
    // SAFETY: CPU0 only, outside interrupts.
    let slot = unsafe { &mut *(&raw mut SCRATCH) };
    if slot.is_none() {
        const ALIGN: usize = 64;
        let mut px = alloc::vec::Vec::new();
        px.try_reserve_exact(PIXELS + ALIGN / 4).ok()?;
        // SAFETY: u32 has no invalid values; the contents are never read
        // by the CPU, only filled by the G2D.
        unsafe { px.set_len(PIXELS + ALIGN / 4) };
        let base = (px.as_ptr() as usize).next_multiple_of(ALIGN);
        // PIXELS * 4 is a multiple of the line size: whole lines only.
        mmu::clean_dcache_range(base, PIXELS * 4);
        mmu::invalidate_dcache_range(base, PIXELS * 4);
        *slot = Some(Scratch { _px: px, base, color: 0, w: 0, h: 0 });
    }
    slot.as_mut()
}

/// Blends XRGB `src` at `alpha` over a shadow rect (clipped) by the G2D.
#[cfg(feature = "g2d_render")]
fn g2d_blend_color(x: usize, y: usize, w: usize, h: usize, src: u32, alpha: u8) -> bool {
    if !G2D_DRAW.load(Ordering::Relaxed) {
        return false;
    }
    let Some(sc) = scratch() else { return false };
    let a = alpha as u32;
    let pm = |shift: u32| (((src >> shift) & 0xFF) * a + 127) / 255;
    let pixel = a << 24 | pm(16) << 16 | pm(8) << 8 | pm(0);
    let surface = Surface { address: sc.base, stride: W * 4, width: W, height: H, format: Format::Argb8888 };
    if sc.color != pixel || sc.w < w || sc.h < h {
        // Fill what this blend and the last ones need.
        let (fw, fh) = if sc.color == pixel { (sc.w.max(w), sc.h.max(h)) } else { (w, h) };
        sc.w = 0;
        if !g2d_op("scratch fill", fw * fh, |g| g.fill(&surface, g2d::Rect::new(0, 0, fw, fh), pixel)) {
            return false;
        }
        (sc.color, sc.w, sc.h) = (pixel, fw, fh);
    }
    let ok = g2d_op("colour blend", w * h, |g| {
        let (from, to) = (g2d::Rect::new(0, 0, w, h), g2d::Rect::new(x, y, w, h));
        g.blend(&surface, from, Alpha::Premultiplied, &shadow_surface(), to, Filter::Nearest, 255)
    });
    if ok {
        mark(x, y, w, h);
    }
    ok
}

/// Blends `w` × `h` pixels of a premultiplied ARGB8888 sprite (`px`, `sw` ×
/// `sh`, rows of `stride` pixels, cleaned by [`end_sprite`]) from (`sx`,
/// `sy`) over the shadow at (`dx`, `dy`). The rect must be inside both.
/// False if the G2D did not draw it.
#[allow(clippy::too_many_arguments)]
pub fn blit_sprite(
    px: *const u32,
    sw: usize,
    sh: usize,
    stride: usize,
    sx: usize,
    sy: usize,
    dx: usize,
    dy: usize,
    w: usize,
    h: usize,
) -> bool {
    #[cfg(feature = "g2d_render")]
    {
        let sprite = Surface { address: px as usize, stride: stride * 4, width: sw, height: sh, format: Format::Argb8888 };
        let ok = g2d_op("sprite blit", w * h, |g| {
            let (from, to) = (g2d::Rect::new(sx, sy, w, h), g2d::Rect::new(dx, dy, w, h));
            g.blend(&sprite, from, Alpha::Premultiplied, &shadow_surface(), to, Filter::Nearest, 255)
        });
        if ok {
            mark(dx, dy, w, h);
        }
        ok
    }
    #[cfg(not(feature = "g2d_render"))]
    {
        let _ = (px, sw, sh, stride, sx, sy, dx, dy, w, h);
        false
    }
}

/// The shadow as a GPU target: physical address, stride in bytes, width,
/// height. None unless G2D drawing is on: only then is the shadow drawn
/// through the uncached alias, so the CPU sees what the GPU writes there.
pub fn gpu_target() -> Option<(usize, usize, usize, usize)> {
    g2d_draw_ready().then(|| (buf_ptr(SHADOW) as usize, W * 4, W, H))
}

/// A copy of the shadow (XRGB8888, `PANEL` size, rows top to bottom), for
/// screenshots (HTTP `GET /api/screen`). CPU0, between passes. None if the
/// heap has no room.
pub fn grab() -> Option<alloc::vec::Vec<u32>> {
    let mut copy = alloc::vec::Vec::new();
    copy.try_reserve_exact(PIXELS).ok()?;
    // SAFETY: the shadow holds PIXELS words; nothing draws while CPU0 is here.
    copy.extend_from_slice(unsafe { core::slice::from_raw_parts(back() as *const u32, PIXELS) });
    // What the FIMD shows over it.
    #[cfg(feature = "layers")]
    layers::composite(&mut copy, W, H);
    Some(copy)
}

/// Before the GPU writes the shadow: waits for a present copy in flight
/// (it reads the shadow), and puts what this core wrote through the alias
/// into DRAM first. False if the shadow is not the GPU's to write (G2D
/// drawing is off: [`gpu_target`]).
pub fn gpu_begin() -> bool {
    if !g2d_draw_ready() {
        return false;
    }
    if STATE.load(Ordering::Acquire) == COPY {
        wait_rows(0, H);
    }
    // SAFETY: a barrier only.
    unsafe { core::arch::asm!("dsb", options(nostack, preserves_flags)) };
    true
}

/// The GPU wrote `w` × `h` shadow pixels at (`x`, `y`): the next present
/// takes them.
pub fn gpu_drew(x: usize, y: usize, w: usize, h: usize) {
    // SAFETY: a barrier only: the GPU's writes before this core's next reads.
    unsafe { core::arch::asm!("dsb", options(nostack, preserves_flags)) };
    mark(x, y, w, h);
}

/// Copies `w` × `h` pixels of an opaque RGB565 picture (`addr`, a physical
/// address, `sw` × `sh`, rows of `sw` pixels; whole cache lines cleaned by
/// the caller) from (`sx`, `sy`) to the shadow at (`dx`, `dy`), by the G2D,
/// expanding to XRGB8888 like [`rgb565_to_xrgb`]. The rect must be inside
/// both. False if the G2D did not draw it (off, or it expands 565
/// differently: [`g2d_565_matches`]).
#[allow(clippy::too_many_arguments)]
pub fn copy_rgb565(addr: usize, sw: usize, sh: usize, sx: usize, sy: usize, dx: usize, dy: usize, w: usize, h: usize) -> bool {
    #[cfg(feature = "g2d_render")]
    {
        if !g2d_565_matches() {
            return false;
        }
        let picture = Surface { address: addr, stride: sw * 2, width: sw, height: sh, format: Format::Rgb565 };
        let ok = g2d_op("picture copy", w * h, |g| {
            let (from, to) = (g2d::Rect::new(sx, sy, w, h), g2d::Rect::new(dx, dy, w, h));
            g.copy(&picture, from, &shadow_surface(), to, Filter::Nearest)
        });
        if ok {
            mark(dx, dy, w, h);
            #[cfg(feature = "perf")]
            ferrite_core::perf::add_count(ferrite_core::perf::count::PICTURE_COPIES, 1);
        }
        ok
    }
    #[cfg(not(feature = "g2d_render"))]
    {
        let _ = (addr, sw, sh, sx, sy, dx, dy, w, h);
        false
    }
}

/// Copies a `w` × `h` rect at (`x`, `y`) from one premultiplied ARGB8888
/// surface to another of the same size (`sw` × `sh`, rows of `sw` pixels;
/// physical addresses, the source in DRAM) by the G2D: a layer's shadow
/// into its scan buffer. False if it did not (G2D drawing off, it changes
/// the alpha byte ([`g2d_argb_matches`]), or it failed: part of the rect
/// may be written then).
#[allow(clippy::too_many_arguments)]
pub(crate) fn g2d_copy_argb(src: usize, dst: usize, sw: usize, sh: usize, x: usize, y: usize, w: usize, h: usize) -> bool {
    #[cfg(feature = "g2d_render")]
    {
        if !g2d_argb_matches() {
            return false;
        }
        let from = Surface { address: src, stride: sw * 4, width: sw, height: sh, format: Format::Argb8888 };
        let to = Surface { address: dst, ..from };
        let r = g2d::Rect::new(x, y, w, h);
        g2d_op("layer copy", w * h, |g| g.copy(&from, r, &to, r, Filter::Nearest))
    }
    #[cfg(not(feature = "g2d_render"))]
    {
        let _ = (src, dst, sw, sh, x, y, w, h);
        false
    }
}

/// 0 = not checked yet, 1 = a G2D ARGB8888 copy keeps every pixel as it is,
/// 2 = it does not (layer copies by the CPU).
#[cfg(feature = "g2d_render")]
static G2D_ARGB: AtomicU8 = AtomicU8::new(0);

/// Whether a G2D copy between ARGB8888 surfaces keeps the pixels exactly,
/// alpha byte included (premultiplied, translucent and transparent ones),
/// checked once.
#[cfg(feature = "g2d_render")]
fn g2d_argb_matches() -> bool {
    match G2D_ARGB.load(Ordering::Relaxed) {
        1 => return true,
        2 => return false,
        _ => {}
    }
    #[repr(C, align(64))]
    struct Lines<T>(T);
    const N: usize = 32;
    static mut SRC: Lines<[u32; N]> = Lines([0; N]);
    static mut DST: Lines<[u32; N]> = Lines([0; N]);
    // SAFETY: CPU0 only, never reentrant; the G2D is the only other user,
    // inside `g2d_op` below.
    let (src, dst) = unsafe { (&mut (*(&raw mut SRC)).0, &mut (*(&raw mut DST)).0) };
    for (i, p) in src.iter_mut().enumerate() {
        // Alpha 0, 255 and levels between, colour channels at most alpha.
        let a = [0u32, 255, 1, 128, 200, 37][i % 6];
        let c = |k: u32| (a * (k + i as u32 * 7) / 255) % (a + 1);
        *p = a << 24 | c(11) << 16 | c(97) << 8 | c(203);
    }
    dst.fill(0x5A5A_5A5A);
    mmu::clean_dcache_range(src.as_ptr() as usize, N * 4);
    mmu::clean_dcache_range(dst.as_ptr() as usize, N * 4);
    mmu::invalidate_dcache_range(dst.as_ptr() as usize, N * 4);
    let from = Surface { address: src.as_ptr() as usize, stride: N * 4, width: N, height: 1, format: Format::Argb8888 };
    let to = Surface { address: dst.as_ptr() as usize, ..from };
    let ok = g2d_op("ARGB check", N, |g| g.copy(&from, g2d::Rect::all(&from), &to, g2d::Rect::all(&to), Filter::Nearest));
    if !ok {
        return false;
    }
    mmu::invalidate_dcache_range(dst.as_ptr() as usize, N * 4);
    // SAFETY: read after the G2D's writes (the op waited for its end).
    let got = |i: usize| unsafe { core::ptr::read_volatile(dst.as_ptr().add(i)) };
    let bad = (0..N).find(|&i| got(i) != src[i]);
    if let Some(i) = bad {
        exynos_4412_rs::println!(
            "ferrite: G2D ARGB copy changes pixels ({:08x} -> {:08x}): layer copies by the CPU",
            src[i],
            got(i)
        );
    }
    G2D_ARGB.store(if bad.is_none() { 1 } else { 2 }, Ordering::Relaxed);
    bad.is_none()
}

/// Most pieces a banded [`g2d_move`] copies one by one (each costs a G2D
/// setup of ~5 µs; a move by a few pixels goes through the scratch surface
/// instead).
#[cfg(feature = "g2d_render")]
const MOVE_BANDS_MAX: usize = 64;

/// How [`g2d_move`] moves (`Bands`, `Buffered`) and how it went.
#[cfg(feature = "g2d_render")]
#[derive(Clone, Copy, PartialEq)]
enum Moved {
    /// Straight from source to destination, in bands that never overwrite a
    /// source not copied yet ([`ferrite_core::lcd::move_bands`]): half the
    /// pixel traffic of `Buffered`.
    Bands,
    /// Every source into the scratch surface (at its own position; the
    /// rects don't overlap), then every one from there to its destination.
    Buffered,
    /// Done.
    Done,
    /// Not tried (G2D drawing off, too many bands, no scratch surface):
    /// nothing written.
    NotTried,
    /// An operation failed after a destination was written: some sources
    /// may be overwritten.
    Partly,
}

/// Moves shadow rects by (`dx`, `dy`) by the G2D, the `way` way.
#[cfg(feature = "g2d_render")]
fn g2d_move(way: Moved, rects: &[UiRect], dx: i16, dy: i16) -> Moved {
    if !G2D_DRAW.load(Ordering::Relaxed) {
        return Moved::NotTried;
    }
    let shadow = shadow_surface();
    // One G2D copy of `r` from `src` to `dst` at (`tx`, `ty`).
    let copy = |what: &str, src: &Surface, r: &UiRect, dst: &Surface, tx: usize, ty: usize| {
        let (x, y, w, h) = (r.x as usize, r.y as usize, r.w as usize, r.h as usize);
        #[cfg(feature = "perf")]
        ferrite_core::perf::add_count(ferrite_core::perf::count::MOVE_OPS, 1);
        let (from, to) = (g2d::Rect::new(x, y, w, h), g2d::Rect::new(tx, ty, w, h));
        g2d_op(what, w * h, |g| g.copy(src, from, dst, to, Filter::Nearest))
    };
    let dest = |r: &UiRect| ((r.x + dx) as usize, (r.y + dy) as usize);
    match way {
        Moved::Bands => {
            let Some(bands) = ferrite_core::lcd::move_bands(rects, dx, dy, MOVE_BANDS_MAX) else {
                return Moved::NotTried;
            };
            for (i, b) in bands.iter().enumerate() {
                let (tx, ty) = dest(b);
                // The first band's destination covers no source (it is the
                // furthest along the move): failing there loses nothing.
                if !copy("move", &shadow, b, &shadow, tx, ty) {
                    return if i == 0 { Moved::NotTried } else { Moved::Partly };
                }
                mark(tx, ty, b.w as usize, b.h as usize);
            }
            Moved::Done
        }
        _ => {
            let Some(sc) = scratch() else { return Moved::NotTried };
            // Its colour is overwritten: the next colour blend fills it again.
            sc.w = 0;
            let tmp = Surface { address: sc.base, stride: W * 4, width: W, height: H, format: Format::Xrgb8888 };
            for r in rects {
                if !copy("move read", &shadow, r, &tmp, r.x as usize, r.y as usize) {
                    return Moved::NotTried;
                }
            }
            for r in rects {
                let (tx, ty) = dest(r);
                // Even the first write may cover another rect's source (a
                // failed or timed-out copy may have written part of it):
                // the CPU must not read the shadow after it.
                if !copy("move write", &tmp, r, &shadow, tx, ty) {
                    return Moved::Partly;
                }
                mark(tx, ty, r.w as usize, r.h as usize);
            }
            Moved::Done
        }
    }
}

/// Source pixels of [`cpu_move`], kept between frames (a drawer slide moves
/// up to a screen per frame).
static mut MOVE_BUF: alloc::vec::Vec<u32> = alloc::vec::Vec::new();

/// [`FimdLcd::move_rects`] by the CPU: every source row into `MOVE_BUF`,
/// then out to the destinations. False if the buffer can't be allocated.
fn cpu_move(
    rects: impl Iterator<Item = (usize, usize, usize, usize)> + Clone,
    to: impl Fn(usize, usize) -> (usize, usize),
) -> bool {
    // SAFETY: CPU0 only, outside interrupts.
    let buf = unsafe { &mut *(&raw mut MOVE_BUF) };
    let total: usize = rects.clone().map(|(_, _, w, h)| w * h).sum();
    buf.clear();
    if buf.try_reserve(total).is_err() {
        return false;
    }
    let px = back();
    timed!({
        for (x, y, w, h) in rects.clone() {
            for r in y..y + h {
                // SAFETY: inside the shadow (checked by the caller).
                buf.extend_from_slice(unsafe { core::slice::from_raw_parts(px.add(r * W + x), w) });
            }
        }
        let mut at = 0;
        for (x, y, w, h) in rects {
            let (tx, ty) = to(x, y);
            fence(ty, ty + h);
            for r in 0..h {
                // SAFETY: the destination is inside the shadow; `buf` is
                // not part of it.
                unsafe { copy_row(buf.as_ptr().add(at), px.add((ty + r) * W + tx), w) };
                at += w;
            }
            mark(tx, ty, w, h);
        }
    });
    true
}

/// 0 = not checked yet, 1 = the G2D's RGB565 expansion matches the CPU's,
/// 2 = it does not (pictures are drawn by the CPU).
#[cfg(feature = "g2d_render")]
static G2D_565: AtomicU8 = AtomicU8::new(0);

/// Whether a G2D copy from RGB565 gives exactly [`rgb565_to_xrgb`]'s
/// pixels, checked once on every level of each channel: a picture is drawn
/// partly by the G2D and partly by the CPU (when it is off), and the two
/// must not show a seam.
#[cfg(feature = "g2d_render")]
fn g2d_565_matches() -> bool {
    match G2D_565.load(Ordering::Relaxed) {
        1 => return true,
        2 => return false,
        _ => {}
    }
    #[repr(C, align(64))]
    struct Lines<T>(T);
    const N: usize = 64;
    // Every red and blue level (5 bits) with every green level (6 bits),
    // and the extremes.
    static mut SRC: Lines<[u16; N]> = Lines([0; N]);
    static mut DST: Lines<[u32; N]> = Lines([0; N]);
    // SAFETY: CPU0 only, never reentrant; the G2D is the only other user,
    // inside `g2d_op` below.
    let (src, dst) = unsafe { (&mut (*(&raw mut SRC)).0, &mut (*(&raw mut DST)).0) };
    for (i, p) in src.iter_mut().enumerate() {
        let (r, g, b) = ((i % 32) as u16, i as u16, (31 - i % 32) as u16);
        *p = r << 11 | g << 5 | b;
    }
    dst.fill(0);
    mmu::clean_dcache_range(src.as_ptr() as usize, N * 2);
    mmu::clean_dcache_range(dst.as_ptr() as usize, N * 4);
    mmu::invalidate_dcache_range(dst.as_ptr() as usize, N * 4);
    let from = Surface { address: src.as_ptr() as usize, stride: N * 2, width: N, height: 1, format: Format::Rgb565 };
    let to = Surface { address: dst.as_ptr() as usize, stride: N * 4, width: N, height: 1, format: Format::Xrgb8888 };
    let ok = g2d_op("565 check", N, |g| g.copy(&from, g2d::Rect::all(&from), &to, g2d::Rect::all(&to), Filter::Nearest));
    if !ok {
        // G2D drawing went off: nothing decided about the expansion.
        return false;
    }
    mmu::invalidate_dcache_range(dst.as_ptr() as usize, N * 4);
    // SAFETY: read after the G2D's writes (the op waited for its end).
    let got = |i: usize| unsafe { core::ptr::read_volatile(dst.as_ptr().add(i)) };
    let bad = (0..N).find(|&i| got(i) != rgb565_to_xrgb(src[i]));
    if let Some(i) = bad {
        exynos_4412_rs::println!(
            "ferrite: G2D RGB565 expansion differs ({:04x} -> {:08x}, CPU {:08x}): pictures drawn by the CPU",
            src[i],
            got(i),
            rgb565_to_xrgb(src[i])
        );
    }
    G2D_565.store(if bad.is_none() { 1 } else { 2 }, Ordering::Relaxed);
    bad.is_none()
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
    #[cfg(feature = "g2d_render")]
    {
        let shadow = buf_ptr(SHADOW) as usize;
        if let (Some(nc), Some(_)) = (mmu::uncached(shadow), mmu::uncached(shadow + PIXELS * 4 - 1)) {
            // As for the scan buffers: no cached line of it may be written
            // back over the alias's stores later.
            mmu::clean_dcache_range(shadow, PIXELS * 4);
            mmu::invalidate_dcache_range(shadow, PIXELS * 4);
            unsafe { *(&raw mut SHADOW_PX) = nc as *mut u32 };
            exynos_4412_rs::println!("ferrite: LCD shadow via non-cacheable alias {:#x}", nc);
        }
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
