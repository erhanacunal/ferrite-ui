//! Retained layers on the FIMD's overlay windows (`layers` feature; core `layer.rs`).
//!
//! Layer `n` is window `n + 1`, above window 0 (the screen: shadow + scan buffers A/B). Each
//! layer owns one heap block of three premultiplied ARGB8888 surfaces of its size: a
//! *shadow* the primitives draw into (cached, like a sprite: [`super::Target`] with `layer`)
//! and two *scan buffers* the FIMD shows in turn. Drawing marks the layer's own dirty boxes
//! ([`Boxes`]: a few, so changes far apart, like two cards on a pager's two pages, are not
//! copied as one box spanning both).
//!
//! [`prepare`] (in `flush`, no present in flight) copies each shown layer's dirty boxes
//! (+ the boxes of its previous flip, which the other buffer lacks) into the buffer the FIMD
//! does not scan, cleans it to DRAM, and works out which windows' registers change: a new
//! buffer, a move, a plane alpha, shown or hidden. Those go out together with window 0's
//! next flip (`super::show`, one `hold_updates` .. `release_updates`, so both appear in the
//! same frame), or at once if window 0 has nothing new ([`commit_now`]). A layer that only
//! moves or fades costs these register writes and no drawing.
//!
//! A buffer is written again only once the FIMD scans the other one (`overlay_scan_address`,
//! or after `FLIP_MS`); a freed block is kept until its window's hide has applied.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use exynos_4412_rs::cpu;
use exynos_4412_rs::drivers::display::{Fimd, Overlay};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::mmu;
use ferrite_core::fb;

use super::{EMPTY, FLIP_MS, FRAMES, H, LAYER_TARGET, NO_LAYER, Rect, Target, W, copy_row, is_empty, union};
use crate::systick;

/// Layers = FIMD windows 1-4.
pub const COUNT: usize = 4;

/// The CPU draws the layers' shadows through the uncached alias
/// ([`mmu::DRAM_UNCACHED`]) instead of the cached view (HTTP `POST
/// /api/layercache?uncached=0|1`, applied at the next [`prepare`]). Uncached,
/// the G2D (layer copies, sprite blits) reads and writes a shadow with no
/// cache maintenance, but every CPU blend reads DRAM.
pub static UNCACHED: AtomicBool = AtomicBool::new(false);

/// Copies of at least this many bytes clean the whole cache instead of
/// their lines.
const CLEAN_ALL_BYTES: usize = 256 * 1024;
/// Dirty boxes kept per layer.
const BOXES: usize = 4;

/// A few disjoint boxes (x0, y0, x1, y1), together covering what changed.
#[derive(Clone, Copy)]
struct Boxes {
    r: [Rect; BOXES],
    n: usize,
}

fn overlaps(a: &Rect, b: &Rect) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

fn area(r: &Rect) -> usize {
    (r[2] - r[0]) * (r[3] - r[1])
}

impl Boxes {
    const NONE: Boxes = Boxes { r: [EMPTY; BOXES], n: 0 };

    fn is_empty(&self) -> bool {
        self.n == 0
    }

    fn iter(&self) -> impl Iterator<Item = &Rect> {
        self.r[..self.n].iter()
    }

    fn remove(&mut self, i: usize) -> Rect {
        let r = self.r[i];
        self.n -= 1;
        self.r[i] = self.r[self.n];
        r
    }

    /// Adds `b`: merged with every box it overlaps; when all slots are
    /// taken, with the box it grows least.
    fn add(&mut self, mut b: Rect) {
        if is_empty(&b) {
            return;
        }
        loop {
            while let Some(i) = (0..self.n).find(|&i| overlaps(&self.r[i], &b)) {
                b = union(b, self.remove(i));
            }
            if self.n < BOXES {
                self.r[self.n] = b;
                self.n += 1;
                return;
            }
            let grow = |r: &Rect| area(&union(*r, b)) - area(r);
            let i = (0..self.n).min_by_key(|&i| grow(&self.r[i])).unwrap_or(0);
            b = union(b, self.remove(i));
        }
    }
}

/// Freed blocks waiting for their window's hide to apply.
const DEAD_MAX: usize = 8;
/// How long after its hide went out a freed block is kept: a few frames.
const DEAD_MS: u32 = 50;

#[derive(Clone, Copy, PartialEq)]
struct Place {
    x: i32,
    y: i32,
    alpha: u8,
    visible: bool,
}

struct Layer {
    /// Holds the three surfaces from `base`.
    _mem: Vec<u32>,
    /// The shadow's address (64-byte aligned); scan buffer `i` follows at `base + (1 + i) *
    /// size * 4`.
    base: usize,
    /// Where the CPU reaches the shadow: `base`, or its uncached alias.
    px: usize,
    /// `px` is the uncached alias: no line of the shadow is cached.
    uncached: bool,
    w: usize,
    h: usize,
    /// Drawn in the shadow since the last copy.
    dirty: Boxes,
    /// The whole shadow is in DRAM (cleaned whole, nothing drawn since): the
    /// G2D may read it for [`compose_source`] without cleaning again.
    in_dram: bool,
    /// Copied into the front buffer at the last flip: the other one lacks it.
    prev: Boxes,
    /// The scan buffer the registers name (or the pending commit will).
    front: usize,
    /// A buffer switch went out at this `millis`: the other buffer may still be scanned.
    flip_at: Option<u32>,
    place: Place,
    /// Its slot: window `1 + index`.
    index: usize,
}

impl Layer {
    fn size(&self) -> usize {
        self.w * self.h
    }

    /// Words per surface: whole 64-byte lines, so each scan buffer starts
    /// on one and can be invalidated whole.
    fn surface_words(&self) -> usize {
        surface_words(self.w, self.h)
    }

    fn scan(&self, i: usize) -> usize {
        self.base + (1 + i) * self.surface_words() * 4
    }

    /// Switches the CPU's view of the shadow to what [`UNCACHED`] asks (when
    /// the alias covers it). Cached → uncached: the shadow's lines go out to
    /// DRAM and are dropped, so later uncached writes are never overwritten by
    /// a write-back. Uncached → cached: nothing wrote the cached view
    /// meanwhile, so it holds no dirty line; stale clean ones are dropped.
    /// Between passes only (nothing holds the old `px`).
    fn apply_mode(&mut self) {
        let bytes = self.surface_words() * 4;
        let alias = mmu::uncached(self.base).filter(|_| mmu::uncached(self.base + bytes - 1).is_some());
        let want = UNCACHED.load(Ordering::Relaxed) && alias.is_some();
        if want == self.uncached {
            return;
        }
        if want {
            mmu::clean_dcache_range(self.base, bytes);
            mmu::invalidate_dcache_range(self.base, bytes);
            self.px = alias.unwrap_or(self.base);
        } else {
            // SAFETY: a barrier only: the alias's stores are in DRAM first.
            unsafe { core::arch::asm!("dsb", options(nostack, preserves_flags)) };
            mmu::invalidate_dcache_range(self.base, bytes);
            self.px = self.base;
        }
        self.uncached = want;
    }

    /// The window's settings this layer wants now: its on-screen part only
    /// (a window is at most 2048 px wide; a pager strip is wider), None if
    /// hidden or off-screen.
    fn overlay(&self) -> Option<Overlay> {
        if !self.place.visible {
            return None;
        }
        let (x, y) = (self.place.x, self.place.y);
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + self.w as i32).min(W as i32);
        let y1 = (y + self.h as i32).min(H as i32);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        let (sx, sy) = ((x0 - x) as usize, (y0 - y) as usize);
        Some(Overlay {
            address: self.scan(self.front) + (sy * self.w + sx) * 4,
            stride: self.w * 4,
            width: (x1 - x0) as usize,
            height: (y1 - y0) as usize,
            x: x0,
            y: y0,
            alpha: self.place.alpha,
        })
    }

    /// The FIMD scans the front buffer (no flip outstanding).
    fn settled(&mut self) -> bool {
        let Some(t) = self.flip_at else { return true };
        let a = Fimd::new().overlay_scan_address(1 + self.index) as usize;
        let front = self.scan(self.front);
        if (front..front + self.size() * 4).contains(&a) || systick::millis().wrapping_sub(t) >= FLIP_MS {
            self.flip_at = None;
            return true;
        }
        false
    }
}

/// Window register changes handed to a present: per layer, `Some(settings)` to write.
#[derive(Clone, Copy, Default)]
pub struct Commit {
    pub windows: [Option<Option<Overlay>>; COUNT],
}

impl Commit {
    pub fn any(&self) -> bool {
        self.windows.iter().any(Option::is_some)
    }

    /// `hold_updates` mask of the windows it writes.
    pub fn mask(&self) -> u32 {
        self.windows.iter().enumerate().filter(|(_, w)| w.is_some()).fold(0, |m, (n, _)| m | 2 << n)
    }

    /// Writes the windows' registers (inside a `hold_updates` of [`Self::mask`]).
    pub fn apply(&self, fimd: &Fimd) {
        for (n, w) in self.windows.iter().enumerate() {
            if let Some(o) = w
                && let Err(e) = fimd.set_overlay(1 + n, o.as_ref())
            {
                exynos_4412_rs::println!("ferrite: layer {} window: {:?}", n, e);
            }
        }
    }
}

struct State {
    layers: [Option<Layer>; COUNT],
    /// What the window registers hold, or will once the pending commit goes out.
    shown: [Option<Overlay>; COUNT],
    /// Freed blocks: (block, its hide went out in a commit, `millis` then).
    dead: [Option<(Vec<u32>, bool, u32)>; DEAD_MAX],
}

static mut STATE: State = State { layers: [const { None }; COUNT], shown: [None; COUNT], dead: [const { None }; DEAD_MAX] };

fn state() -> &'static mut State {
    // SAFETY: CPU0 only, outside interrupts (the primitives, `flush`).
    unsafe { &mut *(&raw mut STATE) }
}

fn surface_words(w: usize, h: usize) -> usize {
    (w * h).next_multiple_of(16)
}

/// See `LcdBackend::layer_alloc`.
pub fn alloc(n: usize, w: usize, h: usize) -> bool {
    if n >= COUNT || w == 0 || h == 0 {
        return false;
    }
    free(n);
    const ALIGN: usize = 64;
    let sw = surface_words(w, h);
    let words = 3 * sw + ALIGN / 4;
    let mut mem = Vec::new();
    if mem.try_reserve_exact(words).is_err() {
        return false;
    }
    // All transparent.
    mem.resize(words, 0u32);
    let base = (mem.as_ptr() as usize).next_multiple_of(ALIGN);
    // The FIMD reads the scan buffers from DRAM, and the G2D writes them
    // there ([`copy_to_back`]): no cached line of them may stay, or be
    // written back over the G2D's writes later.
    let scans = base + sw * 4;
    mmu::clean_dcache_range(scans, 2 * sw * 4);
    mmu::invalidate_dcache_range(scans, 2 * sw * 4);
    exynos_4412_rs::println!("ferrite: layer {} {}x{} allocated", n, w, h);
    let place = Place { x: 0, y: 0, alpha: 255, visible: false };
    let mut layer = Layer {
        _mem: mem,
        base,
        px: base,
        uncached: false,
        w,
        h,
        dirty: Boxes::NONE,
        in_dram: false,
        prev: Boxes::NONE,
        front: 0,
        flip_at: None,
        place,
        index: n,
    };
    layer.apply_mode();
    state().layers[n] = Some(layer);
    true
}

/// See `LcdBackend::layer_free`: hidden with the next commit, the block kept until then.
pub fn free(n: usize) {
    let st = state();
    let Some(layer) = st.layers.get_mut(n).and_then(Option::take) else { return };
    exynos_4412_rs::println!("ferrite: layer {} freed", n);
    unsafe {
        if (*(&raw const LAYER_TARGET)).is_some_and(|t| t.layer as usize == n) {
            *(&raw mut LAYER_TARGET) = None;
        }
    }
    let mem = layer._mem;
    match st.dead.iter_mut().find(|d| d.is_none()) {
        Some(slot) => *slot = Some((mem, false, 0)),
        None => {
            // No room to wait: hide it now (it may show a frame of freed memory).
            hide_now(n);
            drop(mem);
        }
    }
}

fn hide_now(n: usize) {
    let masked = cpu::mask_irq();
    let _ = Fimd::new().set_overlay(1 + n, None);
    cpu::restore_irq(masked);
    state().shown[n] = None;
}

/// See `LcdBackend::layer_target`.
pub fn target(n: Option<usize>) {
    let t = n.map(|n| match state().layers.get(n).and_then(Option::as_ref) {
        Some(l) => Target { px: l.px as *mut u32, stride: l.w, w: l.w, h: l.h, sprite: true, layer: n as u8, ox: 0, oy: 0 },
        // Not allocated: draws nothing.
        None => Target { px: core::ptr::null_mut(), stride: 0, w: 0, h: 0, sprite: true, layer: NO_LAYER, ox: 0, oy: 0 },
    });
    unsafe { *(&raw mut LAYER_TARGET) = t };
}

/// See `LcdBackend::layer_clear`.
pub fn clear(x: u16, y: u16, w: u16, h: u16) {
    let Some(t) = (unsafe { *(&raw const LAYER_TARGET) }) else { return };
    let Some((x, y, w, h)) = t.clip(x, y, w, h) else { return };
    for r in y..y + h {
        // SAFETY: clipped to the surface.
        fb::fill_u32(unsafe { t.row(x, r, w) }, 0);
    }
    t.mark(x, y, w, h);
}

/// See `LcdBackend::layer_place`.
pub fn place(n: usize, x: i32, y: i32, alpha: u8, visible: bool) {
    if let Some(l) = state().layers.get_mut(n).and_then(Option::as_mut) {
        l.place = Place { x, y, alpha, visible };
    }
}

/// A primitive drew on layer `n`'s shadow.
#[inline]
pub fn mark(n: usize, x: usize, y: usize, w: usize, h: usize) {
    if let Some(l) = state().layers.get_mut(n).and_then(Option::as_mut) {
        l.dirty.add([x, y, x + w, y + h]);
        l.in_dram = false;
    }
}

/// A composed layer (core `layer.rs`: something is drawn over it, so the screen's passes
/// blend its shadow in instead of its window showing it): layer `n`'s shadow as (physical
/// address, the CPU's view of it, width, height). With `for_g2d`, rect (`x`, `y`, `w`, `h`)
/// of it is first cleaned to DRAM, where the G2D reads it (nothing when drawn uncached; a
/// large rect cleans the whole cache once, until something is drawn on the layer again).
pub fn compose_source(n: usize, for_g2d: Option<(usize, usize, usize, usize)>) -> Option<(usize, usize, usize, usize)> {
    let l = state().layers.get_mut(n)?.as_mut()?;
    if let Some((x, y, w, h)) = for_g2d
        && !l.uncached
        && !l.in_dram
    {
        let l2 = L2c::new();
        if w * h * 4 >= CLEAN_ALL_BYTES && l2.is_enabled() {
            mmu::clean_l1_all();
            l2.clean_all();
            l.in_dram = true;
        } else {
            for row in y..y + h {
                mmu::clean_dcache_range(l.base + (row * l.w + x) * 4, w * 4);
            }
        }
    }
    Some((l.base, l.px, l.w, l.h))
}

/// Something waits for [`prepare`]: a shown layer with new pixels, window settings that
/// changed, a freed block whose hide has not gone out.
pub fn pending() -> bool {
    let st = state();
    st.layers.iter().enumerate().any(|(n, l)| match l {
        Some(l) => (l.place.visible && !l.dirty.is_empty()) || l.overlay() != st.shown[n],
        None => st.shown[n].is_some(),
    })
}

/// Some layer is allocated (the overlay board test stays off then).
pub fn in_use() -> bool {
    state().layers.iter().any(Option::is_some) || state().shown.iter().any(Option::is_some)
}

/// No present in flight (`flush`, STATE = IDLE): copies the shown layers' new pixels into
/// their back buffers and returns the window changes to commit. None = a layer's last flip is
/// not on screen yet: nothing is done, `flush` waits.
pub fn prepare() -> Option<Commit> {
    let st = state();
    let now = systick::millis();
    // Freed blocks whose hide went out a few frames ago.
    for d in st.dead.iter_mut() {
        if d.as_ref().is_some_and(|&(_, out, t)| out && now.wrapping_sub(t) >= DEAD_MS) {
            *d = None;
        }
    }
    for l in st.layers.iter_mut().flatten() {
        if l.place.visible && !l.dirty.is_empty() && !l.settled() {
            return None;
        }
    }
    for l in st.layers.iter_mut().flatten() {
        l.apply_mode();
    }
    let mut commit = Commit::default();
    for (n, slot) in st.layers.iter_mut().enumerate() {
        if let Some(l) = slot {
            // A hidden layer keeps its pixels in the shadow until it shows again.
            if l.place.visible && !l.dirty.is_empty() {
                copy_to_back(l);
                l.front = 1 - l.front;
                l.prev = l.dirty;
                l.dirty = Boxes::NONE;
                l.flip_at = Some(now);
            }
        }
        let want = slot.as_ref().and_then(Layer::overlay);
        if want != st.shown[n] {
            commit.windows[n] = Some(want);
            st.shown[n] = want;
        }
    }
    // Every freed block's hide is in this commit or an earlier one.
    for (_, out, t) in st.dead.iter_mut().flatten() {
        if !*out {
            (*out, *t) = (true, now);
        }
    }
    Some(commit)
}

/// `prev ∪ dirty` from the shadow into the buffer the FIMD does not scan, cleaned to DRAM.
fn copy_to_back(l: &Layer) {
    let mut boxes = l.prev;
    for r in l.dirty.iter() {
        boxes.add(*r);
    }
    // Clipped to the surface: (x, y, w, h).
    let mut list = [(0, 0, 0, 0); BOXES];
    let mut n = 0;
    for r in boxes.iter() {
        let (x0, y0, x1, y1) = (r[0].min(l.w), r[1].min(l.h), r[2].min(l.w), r[3].min(l.h));
        if x0 < x1 && y0 < y1 {
            list[n] = (x0, y0, x1 - x0, y1 - y0);
            n += 1;
        }
    }
    let list = &list[..n];
    let bytes: usize = list.iter().map(|&(_, _, w, h)| w * h * 4).sum();
    if bytes == 0 {
        return;
    }
    let (src, dst) = (l.px as *const u32, l.scan(1 - l.front) as *mut u32);
    // The G2D reads the shadow's boxes from DRAM. A lot of them: the whole
    // L1 by set/way (~1000 operations) and the PL310 by way (one background
    // operation) cost less than one L1 + one L2 operation per line.
    let l2 = L2c::new();
    if l.uncached {
        // In DRAM already (`g2d_op` puts a barrier before the copy).
    } else if bytes >= CLEAN_ALL_BYTES && l2.is_enabled() {
        mmu::clean_l1_all();
        l2.clean_all();
    } else {
        for &(x, y, w, h) in list {
            for row in y..y + h {
                mmu::clean_dcache_range(l.base + (row * l.w + x) * 4, w * 4);
            }
        }
    }
    for &(x, y, w, h) in list {
        if super::g2d_copy_argb(l.base, dst as usize, l.w, l.h, x, y, w, h) {
            continue;
        }
        // By the CPU (G2D drawing off or failed: it may have written part
        // of the box, which is written again here). The buffer holds no
        // cached line ([`alloc`], [`composite`]): what a row's lines get
        // here is cleaned out at once.
        for row in y..y + h {
            let at = row * l.w + x;
            // SAFETY: inside both surfaces (clipped above); they don't overlap.
            unsafe { copy_row(src.add(at), dst.add(at), w) };
            // Out to DRAM and out of the cache: whole lines, all inside the
            // buffer (it starts and ends on a line).
            let a = (dst as usize + at * 4) & !63;
            let e = (dst as usize + (at + w) * 4).next_multiple_of(64);
            mmu::clean_dcache_range(a, e - a);
            mmu::invalidate_dcache_range(a, e - a);
        }
    }
    #[cfg(feature = "perf")]
    super::COPY_BYTES.fetch_add(bytes as u32, Ordering::Relaxed);
}

/// Window 0 has nothing new: the layers' changes apply at the next vsync on their own.
pub fn commit_now(c: &Commit) {
    if !c.any() {
        return;
    }
    let fimd = Fimd::new();
    // The G2D present's interrupt writes SHADOWCON too.
    let masked = cpu::mask_irq();
    fimd.hold_updates(c.mask());
    c.apply(&fimd);
    fimd.release_updates(c.mask());
    cpu::restore_irq(masked);
    FRAMES.fetch_add(1, Ordering::Relaxed);
}

/// Composites the shown windows over `screen` (a copy of the shadow, `sw` × `sh`) the way
/// the FIMD does, for screenshots.
pub fn composite(screen: &mut [u32], sw: usize, sh: usize) {
    let st = state();
    // The scan buffers are written by the G2D: no stale cached line may be
    // read, nor left behind (a later CPU copy of part of a line would
    // write the rest of it back).
    let drop_lines = |st: &State| {
        for l in st.layers.iter().flatten() {
            mmu::invalidate_dcache_range(l.scan(0), 2 * l.surface_words() * 4);
        }
    };
    drop_lines(st);
    for o in st.shown.iter().flatten() {
        let pa = o.alpha as u32;
        for y in 0..o.height.min(sh.saturating_sub(o.y as usize)) {
            let row = (o.address + y * o.stride) as *const u32;
            for x in 0..o.width.min(sw.saturating_sub(o.x as usize)) {
                // SAFETY: inside a layer's scan buffer (`Layer::overlay`); a
                // freed block is kept until its hide has applied.
                let s = unsafe { *row.add(x) };
                let d = &mut screen[(o.y as usize + y) * sw + o.x as usize + x];
                let a = (s >> 24) * pa / 255;
                let ch = |sh: u32| {
                    let sc = ((s >> sh) & 0xFF) * pa / 255;
                    let dc = (*d >> sh) & 0xFF;
                    (sc + dc * (255 - a) / 255).min(255)
                };
                *d = 0xFF00_0000 | ch(16) << 16 | ch(8) << 8 | ch(0);
            }
        }
    }
    drop_lines(st);
}

/// The whole-line ranges holding rect (`x`, `y`, `w`, `h`) of the targeted surface `t`, row
/// by row, to `f`. The surface starts on a line and its size is a whole number of lines
/// ([`Layer::surface_words`]), so the ranges stay inside it.
fn rect_lines(t: &Target, x: usize, y: usize, w: usize, h: usize, f: fn(usize, usize)) {
    for row in y..y + h {
        let a = (t.px as usize + (row * t.stride + x) * 4) & !63;
        let e = (t.px as usize + (row * t.stride + x + w) * 4).next_multiple_of(64);
        f(a, e - a);
    }
}

/// Before the G2D blends into rect (`x`, `y`, `w`, `h`) of the targeted layer's shadow (the
/// CPU draws it cached): what the CPU drew there goes out to DRAM, as the blend reads it.
pub fn g2d_begin(t: &Target, x: usize, y: usize, w: usize, h: usize) {
    if shadow(t).is_none_or(|(_, uncached)| uncached) {
        return;
    }
    let l2 = L2c::new();
    if w * h * 4 >= CLEAN_ALL_BYTES && l2.is_enabled() {
        mmu::clean_l1_all();
        l2.clean_all();
    } else {
        rect_lines(t, x, y, w, h, mmu::clean_dcache_range);
    }
}

/// After the G2D blended into the rect (or failed, having written part of it): drops its
/// lines from the caches, so the CPU reads what the G2D wrote; lines pulled in meanwhile (by
/// speculation too) go with them. [`g2d_begin`] cleaned them and nothing wrote them since, so
/// the neighbours sharing a line on the rect's edges lose nothing.
pub fn g2d_end(t: &Target, x: usize, y: usize, w: usize, h: usize) {
    if shadow(t).is_none_or(|(_, uncached)| uncached) {
        return;
    }
    rect_lines(t, x, y, w, h, mmu::invalidate_dcache_range);
}

/// The targeted layer's shadow for the G2D: its physical address, and whether the CPU draws it
/// uncached (no cache maintenance then). None if `t` is no allocated layer.
pub fn shadow(t: &Target) -> Option<(usize, bool)> {
    let l = state().layers.get(t.layer as usize)?.as_ref()?;
    Some((l.base, l.uncached))
}
