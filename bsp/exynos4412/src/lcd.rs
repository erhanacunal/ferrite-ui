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
//! Buffered mode needs nothing more: the shadow already holds every frame, so
//! `back_buf` is 1 for the renderer's per-buffer tracking and `end_frame` only
//! flushes.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};

use exynos_4412_rs::drivers::display::{self, Fimd, Framebuffer, LTN101AL03, LVDS_SETTLE_MS, Timing};
use exynos_4412_rs::drivers::g2d::{self, Filter, Format, G2d, Surface};
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::{cpu, mmu, smp, soc};
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

/// Times a pixel primitive for the profiler (`perf`: the `PIXELS` row),
/// after its `fence`, so display waits stay in their own row.
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
    while unsafe { !is_empty(&*(&raw const DIRTY)) } || STATE.load(Ordering::Acquire) != IDLE {
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
    mmu::clean_l1_all();
    L2c::new().clean_all();
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
    if STATE.load(Ordering::Acquire) == COPY && !PRESENTER.load(Ordering::Relaxed) && G2d::new().ack() {
        g2d_next(job());
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
        cpu_copy(j, y);
    }
    g2d_finish(j);
}

/// The G2D did not finish a band: give it up for good, copy the rest of the
/// box on this core and switch.
#[cold]
fn g2d_abort() {
    let masked = cpu::mask_irq();
    if STATE.load(Ordering::Acquire) == COPY {
        G2D_ON.store(false, Ordering::Relaxed);
        exynos_4412_rs::println!("ferrite: G2D present timed out, CPU copy from now on");
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
    let (src, dst) = (buf_ptr(SHADOW), j.nc as *mut u32);
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
    let p = present_state();
    if let Some(fb) = p.fb[1 - p.front].as_ref() {
        let _ = Fimd::new().show(fb);
    }
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
    let src = buf_ptr(SHADOW);
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
            exynos_4412_rs::println!("ferrite: present copy by G2D at {} MHz", hz / 1_000_000);
            return Ok("LCD presents by the G2D, driven by CPU0 (CPU2 not started)");
        }
    }
    smp::start_cpu(cpu, presenter)?;
    PRESENTER.store(true, Ordering::Release);
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
        fence(y, y + h);
        timed!({
            let c = rgb565_to_xrgb(color);
            let fb = back();
            for r in y..y + h {
                fb::fill_u32(unsafe { row(fb, x, r, w) }, c);
            }
            mark(x, y, w, h);
        })
    }

    fn draw_pixel(&self, x: u16, y: u16, color: u16) {
        let (x, y) = (x as usize, y as usize);
        if x < W && y < H {
            fence(y, y + 1);
            timed!({
                unsafe { *back().add(y * W + x) = rgb565_to_xrgb(color) };
                mark(x, y, 1, 1);
            })
        }
    }

    // The stream writes (write_pixel/s, write_run) belong to the stream
    // `begin_pixels` opened, within one primitive: its fence covers them.
    fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16) {
        // The stream writes only inside the clipped window.
        let window = clip(x, y, w, h);
        if let Some((_, wy, _, wh)) = window {
            fence(wy, wy + wh);
        }
        unsafe { (*(&raw mut CURSOR)).begin(x, y, w) };
        if let Some((x, y, w, h)) = window {
            mark(x, y, w, h);
        }
    }

    fn write_pixel(&self, color: u16) {
        timed!({
            if let Some((x, y)) = unsafe { (*(&raw mut CURSOR)).step(W, H) } {
                unsafe { *back().add(y * W + x) = rgb565_to_xrgb(color) };
            }
        })
    }

    fn write_pixels(&self, colors: &[u16]) {
        let fb = back();
        timed!({
            unsafe {
                (*(&raw mut CURSOR)).advance(colors.len(), W, H, |x, y, len, skip| {
                    for (d, &c) in row(fb, x, y, len).iter_mut().zip(&colors[skip..skip + len]) {
                        *d = rgb565_to_xrgb(c);
                    }
                });
            }
        })
    }

    fn write_run(&self, color: u16, n: usize) {
        let (fb, c) = (back(), rgb565_to_xrgb(color));
        timed!({
            unsafe {
                (*(&raw mut CURSOR)).advance(n, W, H, |x, y, len, _| fb::fill_u32(row(fb, x, y, len), c));
            }
        })
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
        fence(y, y + h);
        timed!({
            let (src, a, fb) = (rgb565_to_xrgb(color), fb::alpha8(alpha), back());
            for r in y..y + h {
                for p in unsafe { row(fb, x, r, w) } {
                    *p = fb::blend_argb8888(*p, src, a);
                }
            }
            mark(x, y, w, h);
        })
    }

    fn blend_pixel(&self, x: u16, y: u16, color: u16, alpha: u8) {
        let (x, y) = (x as usize, y as usize);
        if alpha == 0 || x >= W || y >= H {
            return;
        }
        fence(y, y + 1);
        timed!({
            let p = unsafe { &mut *back().add(y * W + x) };
            let src = rgb565_to_xrgb(color);
            *p = if alpha == 255 { src } else { fb::blend_argb8888(*p, src, fb::alpha8(alpha)) };
            mark(x, y, 1, 1);
        })
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
