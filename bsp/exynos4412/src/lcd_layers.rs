//! Retained layers on the FIMD's overlay windows (`layers` feature; core `layer.rs`).
//!
//! Layer `n` is window `n + 1`, above window 0 (the screen: shadow + scan buffers A/B). Each
//! layer owns one heap block of three premultiplied ARGB8888 surfaces of its size: a
//! *shadow* the primitives draw into (cached, like a sprite: [`super::Target`] with `layer`)
//! and two *scan buffers* the FIMD shows in turn. Drawing marks the layer's own dirty box.
//!
//! [`prepare`] (in `flush`, no present in flight) copies each shown layer's dirty box
//! (+ the box of its previous flip, which the other buffer lacks) into the buffer the FIMD
//! does not scan, cleans it to DRAM, and works out which windows' registers change: a new
//! buffer, a move, a plane alpha, shown or hidden. Those go out together with window 0's
//! next flip (`super::show`, one `hold_updates` .. `release_updates`, so both appear in the
//! same frame), or at once if window 0 has nothing new ([`commit_now`]). A layer that only
//! moves or fades costs these register writes and no drawing.
//!
//! A buffer is written again only once the FIMD scans the other one (`overlay_scan_address`,
//! or after `FLIP_MS`); a freed block is kept until its window's hide has applied.

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use exynos_4412_rs::cpu;
use exynos_4412_rs::drivers::display::{Fimd, Overlay};
use exynos_4412_rs::mmu;
use ferrite_core::fb;

use super::{EMPTY, FLIP_MS, FRAMES, H, LAYER_TARGET, NO_LAYER, Rect, Target, W, copy_row, is_empty, union};
use crate::systick;

/// Layers = FIMD windows 1-4.
pub const COUNT: usize = 4;

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
    w: usize,
    h: usize,
    /// Drawn in the shadow since the last copy.
    dirty: Rect,
    /// Copied into the front buffer at the last flip: the other one lacks it.
    prev: Rect,
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

    fn scan(&self, i: usize) -> usize {
        self.base + (1 + i) * self.size() * 4
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

/// See `LcdBackend::layer_alloc`.
pub fn alloc(n: usize, w: usize, h: usize) -> bool {
    if n >= COUNT || w == 0 || h == 0 {
        return false;
    }
    free(n);
    const ALIGN: usize = 64;
    let words = 3 * w * h + ALIGN / 4;
    let mut mem = Vec::new();
    if mem.try_reserve_exact(words).is_err() {
        return false;
    }
    // All transparent.
    mem.resize(words, 0u32);
    let base = (mem.as_ptr() as usize).next_multiple_of(ALIGN);
    // The FIMD reads the scan buffers from DRAM.
    mmu::clean_dcache_range(base + w * h * 4, 2 * w * h * 4);
    let place = Place { x: 0, y: 0, alpha: 255, visible: false };
    state().layers[n] =
        Some(Layer { _mem: mem, base, w, h, dirty: EMPTY, prev: EMPTY, front: 0, flip_at: None, place, index: n });
    true
}

/// See `LcdBackend::layer_free`: hidden with the next commit, the block kept until then.
pub fn free(n: usize) {
    let st = state();
    let Some(layer) = st.layers.get_mut(n).and_then(Option::take) else { return };
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
        Some(l) => Target { px: l.base as *mut u32, stride: l.w, w: l.w, h: l.h, sprite: true, layer: n as u8 },
        // Not allocated: draws nothing.
        None => Target { px: core::ptr::null_mut(), stride: 0, w: 0, h: 0, sprite: true, layer: NO_LAYER },
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
        l.dirty = union(l.dirty, [x, y, x + w, y + h]);
    }
}

/// Something waits for [`prepare`]: a shown layer with new pixels, window settings that
/// changed, a freed block whose hide has not gone out.
pub fn pending() -> bool {
    let st = state();
    st.layers.iter().enumerate().any(|(n, l)| match l {
        Some(l) => (l.place.visible && !is_empty(&l.dirty)) || l.overlay() != st.shown[n],
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
        if l.place.visible && !is_empty(&l.dirty) && !l.settled() {
            return None;
        }
    }
    let mut commit = Commit::default();
    for (n, slot) in st.layers.iter_mut().enumerate() {
        if let Some(l) = slot {
            // A hidden layer keeps its pixels in the shadow until it shows again.
            if l.place.visible && !is_empty(&l.dirty) {
                copy_to_back(l);
                l.front = 1 - l.front;
                l.prev = l.dirty;
                l.dirty = EMPTY;
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
    let r = union(l.prev, l.dirty);
    let (x0, y0, x1, y1) = (r[0].min(l.w), r[1].min(l.h), r[2].min(l.w), r[3].min(l.h));
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let (src, dst) = (l.base as *const u32, l.scan(1 - l.front) as *mut u32);
    let w = x1 - x0;
    for y in y0..y1 {
        let at = y * l.w + x0;
        // SAFETY: inside both surfaces (clipped above); they don't overlap.
        unsafe { copy_row(src.add(at), dst.add(at), w) };
        mmu::clean_dcache_range(dst as usize + at * 4, w * 4);
    }
    #[cfg(feature = "perf")]
    super::COPY_BYTES.fetch_add((w * (y1 - y0) * 4) as u32, Ordering::Relaxed);
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
    for o in state().shown.iter().flatten() {
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
}
