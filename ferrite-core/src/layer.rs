//! Retained layers (`layers` feature): widget subtrees drawn into their own
//! surfaces, which the display composites over the screen (exynos4412: FIMD
//! hardware windows 1-4 above window 0; sim: in software, [`SoftLayers`]).
//!
//! A program marks a subtree root with `set(layer, n)` (`PROP_LAYER`, 1..=
//! [`MAX_LAYERS`]). While the layer is *active* its widgets are drawn into
//! layer `n - 1`'s surface, in coordinates relative to the root's box, and
//! left out of the screen's own passes (they neither draw there nor occlude
//! or restore anything there). Moving the root (`loc_x`/`loc_y`/`location`,
//! their animations), hiding/showing it and `layer_alpha` (`PROP_LAYER_ALPHA`,
//! the plane alpha) only change where and how the surface is shown: nothing
//! is redrawn. Changes inside the subtree redraw only its surface.
//!
//! A layer is active only when the result looks the same as drawing it in the
//! tree (`ObjPool::layer_eligible`): the backend has that many layers, the
//! screen is not in portrait, the root is not inside a scroll container, no
//! widget of the subtree has `on_paint` (VM drawing goes to the screen), the
//! on-screen keyboard does not overlap it, and no visible widget after the
//! subtree in DFS order (one that would be drawn over it) overlaps its box. A
//! layer that stops being eligible is drawn in the tree again, and one that
//! becomes eligible moves into its surface: what the screen drew of it is
//! restored first. Only the dirty render mode uses layers; the buffered mode
//! turns them off.
//!
//! Content is clipped to the root's box (the surface is that size). Surfaces
//! hold premultiplied ARGB8888, transparent where nothing is drawn; a
//! translucent widget composites "over" what the surface holds below it.

use crate::pool::ObjPool;
use crate::types::{Offset, Rect};
use crate::widget::{FLAG_DIRTY_AB, FLAG_HAS_ON_PAINT, FLAG_RENDERED, WidgetId};

/// Layers a program can name (`PROP_LAYER` 1..=MAX_LAYERS).
pub const MAX_LAYERS: usize = 4;

/// One `PROP_LAYER` number.
#[derive(Clone, Copy)]
pub struct Slot {
    /// Subtree root; NONE = unused.
    pub root: WidgetId,
    /// Plane alpha (`PROP_LAYER_ALPHA`), 255 = as drawn.
    pub alpha: u8,
    /// Drawn into its surface (else in the tree as usual).
    pub active: bool,
    /// Surface size in pixels (0 = none allocated).
    pub w: u16,
    pub h: u16,
    /// Root's screen position at the last layer pass: prev rects recorded
    /// since are relative to it.
    pub at: Offset,
    /// The whole surface must be redrawn at the next layer pass.
    pub full: bool,
    /// Just became active: what the screen drew of the subtree, to restore.
    pub restore: Rect,
}

impl Slot {
    const EMPTY: Slot = Slot {
        root: WidgetId::NONE,
        alpha: 255,
        active: false,
        w: 0,
        h: 0,
        at: Offset { x: 0, y: 0 },
        full: false,
        restore: Rect { x: 0, y: 0, w: 0, h: 0 },
    };
}

/// Which DFS entries a render pass draws.
#[derive(Clone, Copy)]
pub enum Scope {
    /// All of them (no layer is active).
    All,
    /// The screen: all but the active layers' subtrees (`[lo, hi)` ranges).
    Base { excl: [(u32, u32); MAX_LAYERS], n: u8 },
    /// One layer's subtree, `[lo, hi)`.
    Layer(u32, u32),
}

/// Layer state in the object pool (`ObjPool::layers`).
#[derive(Clone, Copy)]
pub struct Layers {
    pub slots: [Slot; MAX_LAYERS],
    /// A placement (position, plane alpha, visibility) may have changed.
    pub placed: bool,
    /// Scope of the running render pass.
    pub scope: Scope,
    /// Subtracted from every absolute rect during a layer pass (the root's
    /// screen position): geometry is then surface coordinates.
    pub origin: Offset,
}

impl Layers {
    pub const fn new() -> Self {
        Layers {
            slots: [Slot::EMPTY; MAX_LAYERS],
            placed: false,
            scope: Scope::All,
            origin: Offset { x: 0, y: 0 },
        }
    }
}

impl Default for Layers {
    fn default() -> Self {
        Self::new()
    }
}

impl ObjPool {
    /// `set(layer, n)`: make `id` the root of layer `n` (1..=MAX_LAYERS), or
    /// no layer's (0). The tree draws it as usual until the next render
    /// decides it is eligible.
    pub fn set_layer(&mut self, id: WidgetId, n: i32) {
        if !self.is_live(id) {
            return;
        }
        for s in 0..MAX_LAYERS {
            let slot = self.layers.slots[s];
            let mine = slot.root == id;
            let taken = n as usize == s + 1;
            if (mine && !taken) || (taken && !mine && slot.root.is_some()) {
                // Leaves this slot (or is pushed out of it): drawn in the tree.
                let root = slot.root;
                self.layers.slots[s] = Slot { root: WidgetId::NONE, ..Slot::EMPTY };
                if slot.active {
                    self.layer_drop_flags(root);
                }
            }
            if taken && !mine {
                self.layers.slots[s] = Slot { root: id, ..Slot::EMPTY };
            }
        }
        self.layers.placed = true;
        self.maybe_dirty = true;
    }

    /// The layer `id` is the root of (1..=MAX_LAYERS), 0 = none.
    pub fn layer_of(&self, id: WidgetId) -> i32 {
        self.layers.slots.iter().position(|s| s.root == id && id.is_some()).map_or(0, |s| s as i32 + 1)
    }

    /// `set(layer_alpha, a)` on a layer root: its plane alpha.
    pub fn set_layer_alpha(&mut self, id: WidgetId, a: i32) {
        if let Some(s) = self.layers.slots.iter_mut().find(|s| s.root == id && id.is_some()) {
            s.alpha = a.clamp(0, 255) as u8;
            self.layers.placed = true;
            self.maybe_dirty = true;
        }
    }

    /// The plane alpha of the layer `id` is the root of (255 if none).
    pub fn layer_alpha(&self, id: WidgetId) -> i32 {
        self.layers.slots.iter().find(|s| s.root == id && id.is_some()).map_or(255, |s| s.alpha as i32)
    }

    /// `id` is the root of an active layer: its moves and visibility changes
    /// are placements, not redraws.
    pub fn is_active_layer_root(&self, id: WidgetId) -> bool {
        id.is_some() && self.layers.slots.iter().any(|s| s.active && s.root == id)
    }

    /// A placement of an active layer root changed (`loc_x`, `visible`…):
    /// true = handled, no dirty marking needed.
    pub fn layer_placement_changed(&mut self, id: WidgetId) -> bool {
        if self.is_active_layer_root(id) {
            self.layers.placed = true;
            self.maybe_dirty = true;
            return true;
        }
        false
    }

    /// The layer's widgets are drawn in the tree again: everything of them redrawn there.
    fn layer_drop_flags(&mut self, root: WidgetId) {
        if self.is_live(root) {
            self.mark_dirty(root);
        }
    }

    /// Whether DFS entry `i` belongs to the running pass.
    #[inline]
    pub fn in_scope(&self, i: usize) -> bool {
        let i = i as u32;
        match self.layers.scope {
            Scope::All => true,
            Scope::Base { excl, n } => !excl[..n as usize].iter().any(|&(lo, hi)| i >= lo && i < hi),
            Scope::Layer(lo, hi) => i >= lo && i < hi,
        }
    }

    /// The running pass is a layer's.
    #[inline]
    pub fn in_layer_pass(&self) -> bool {
        matches!(self.layers.scope, Scope::Layer(..))
    }

    /// Where the subtree of DFS entry `ri` (a layer root) may be drawn as a
    /// layer: see the module docs. `overlay` = the keyboard's rect.
    pub(crate) fn layer_eligible(&self, me: usize, ri: usize, end: usize) -> bool {
        if self.scroll_parent_at(ri).is_some() {
            return false;
        }
        let root = self.abs_at(ri);
        if root.is_empty() {
            return false;
        }
        for i in ri..end {
            if self.get(self.dfs_at(i)).flags & FLAG_HAS_ON_PAINT != 0 {
                return false;
            }
        }
        if !self.vis_at(ri) {
            // Hidden: nothing shows, nothing to check against.
            return true;
        }
        if self.overlay.is_some_and(|k| k.intersects(&root)) {
            return false;
        }
        let mut j = end;
        while j < self.dfs_len() {
            // Another layer's subtree: composited above this one only if its
            // number is higher (the backend stacks surfaces by number).
            let id = self.dfs_at(j);
            if let Some(other) = self.layers.slots.iter().position(|s| s.root.is_some() && s.root == id) {
                if other < me && self.vis_at(j) && self.abs_at(j).intersects(&root) {
                    return false;
                }
                j = self.sub_end_at(j);
                continue;
            }
            if self.vis_at(j) && self.abs_at(j).intersects(&root) {
                return false;
            }
            j += 1;
        }
        true
    }

    /// The box the screen last drew the subtree `[ri, end)` in (for when it
    /// moves into its surface): the bounds of every rendered widget's
    /// current or pending previous rect.
    pub(crate) fn layer_drawn_bounds(&self, ri: usize, end: usize) -> Rect {
        let mut out = Rect::new(0, 0, 0, 0);
        for i in ri..end {
            let id = self.dfs_at(i);
            if self.get(id).flags & FLAG_RENDERED == 0 {
                continue;
            }
            let prev = self.prev_rect(id, 0);
            let r = if prev.is_empty() { self.abs_at(i) } else { prev };
            out = if out.is_empty() { r } else { bounds(&out, &r) };
        }
        out
    }

    /// Make every widget of `[lo, hi)` dirty and forget their pending
    /// previous rects and rendered marks (a surface cleared whole).
    pub(crate) fn layer_redraw_all(&mut self, lo: usize, hi: usize) {
        for i in lo..hi {
            let id = self.dfs_at(i);
            self.set_prev_rect(id, 0, Rect::new(0, 0, 0, 0));
            let w = self.get_mut(id);
            w.flags = (w.flags | FLAG_DIRTY_AB) & !FLAG_RENDERED;
            self.forget_drawn(id);
        }
    }

    /// Some widget of `[lo, hi)` is dirty / has a pending previous rect.
    pub(crate) fn layer_has_dirty(&self, lo: usize, hi: usize, with_prev: bool) -> bool {
        (lo..hi).any(|i| {
            let id = self.dfs_at(i);
            self.get(id).is_dirty() && (!with_prev || !self.prev_rect(id, 0).is_empty())
        })
    }

    /// DFS index of `id`, if it is in the tree.
    pub(crate) fn dfs_index(&self, id: WidgetId) -> Option<usize> {
        (0..self.dfs_len()).find(|&i| self.dfs_at(i) == id)
    }
}

fn bounds(a: &Rect, b: &Rect) -> Rect {
    let x0 = (a.x as i32).min(b.x as i32);
    let y0 = (a.y as i32).min(b.y as i32);
    let x1 = (a.x as i32 + a.w as i32).max(b.x as i32 + b.w as i32);
    let y1 = (a.y as i32 + a.h as i32).max(b.y as i32 + b.h as i32);
    Rect::new(x0 as i16, y0 as i16, (x1 - x0) as u16, (y1 - y0) as u16)
}

// --- Software surfaces --------------------------------------------------------------------------

/// Software layer surfaces for a backend without display windows (the sim,
/// tests): the backend forwards its primitives here while a layer is
/// targeted ([`SoftLayers::target`] is `Some`) and calls [`composite`] when
/// it shows a frame.
///
/// [`composite`]: SoftLayers::composite
pub struct SoftLayers {
    surfaces: core::cell::RefCell<[Option<Surface>; MAX_LAYERS]>,
    target: core::cell::Cell<Option<usize>>,
    /// `begin_pixels` window (x0, x1, y1) and cursor (x, y).
    stream: core::cell::Cell<(u16, u16, u16, u16, u16)>,
}

/// One software surface.
pub struct Surface {
    pub w: u16,
    pub h: u16,
    /// Premultiplied 0xAARRGGBB.
    pub px: alloc::vec::Vec<u32>,
    pub x: i32,
    pub y: i32,
    pub alpha: u8,
    pub visible: bool,
}

#[inline]
fn rgb888(c: u16) -> (u32, u32, u32) {
    let r = ((c >> 11) & 0x1F) as u32;
    let g = ((c >> 5) & 0x3F) as u32;
    let b = (c & 0x1F) as u32;
    ((r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2))
}

/// `c` with coverage `a` over premultiplied `dst`.
#[inline]
pub fn over_premul(dst: u32, c: u16, a: u8) -> u32 {
    if a == 255 {
        let (r, g, b) = rgb888(c);
        return 0xFF00_0000 | r << 16 | g << 8 | b;
    }
    let a = a as u32;
    let na = 255 - a;
    let (r, g, b) = rgb888(c);
    let ch = |s: u32, d: u32| (s * a + d * na + 127) / 255;
    let da = dst >> 24;
    let oa = a + (da * na + 127) / 255;
    oa << 24 | ch(r, (dst >> 16) & 0xFF) << 16 | ch(g, (dst >> 8) & 0xFF) << 8 | ch(b, dst & 0xFF)
}

impl Default for SoftLayers {
    fn default() -> Self {
        Self::new()
    }
}

impl SoftLayers {
    pub fn new() -> Self {
        Self {
            surfaces: core::cell::RefCell::new([const { None }; MAX_LAYERS]),
            target: core::cell::Cell::new(None),
            stream: core::cell::Cell::new((0, 0, 0, 0, 0)),
        }
    }

    pub fn alloc(&self, n: usize, w: u16, h: u16) -> bool {
        if n >= MAX_LAYERS {
            return false;
        }
        let mut px = alloc::vec::Vec::new();
        if px.try_reserve_exact(w as usize * h as usize).is_err() {
            return false;
        }
        px.resize(w as usize * h as usize, 0);
        self.surfaces.borrow_mut()[n] = Some(Surface { w, h, px, x: 0, y: 0, alpha: 255, visible: false });
        true
    }

    /// Layer `n` has a surface.
    pub fn is_allocated(&self, n: usize) -> bool {
        self.surfaces.borrow().get(n).is_some_and(Option::is_some)
    }

    pub fn free(&self, n: usize) {
        if n < MAX_LAYERS {
            self.surfaces.borrow_mut()[n] = None;
        }
    }

    pub fn set_target(&self, n: Option<usize>) {
        self.target.set(n.filter(|&n| n < MAX_LAYERS));
    }

    /// The targeted layer, if any: primitives go to it.
    #[inline]
    pub fn target(&self) -> Option<usize> {
        self.target.get()
    }

    pub fn place(&self, n: usize, x: i32, y: i32, alpha: u8, visible: bool) {
        if let Some(Some(s)) = self.surfaces.borrow_mut().get_mut(n) {
            s.x = x;
            s.y = y;
            s.alpha = alpha;
            s.visible = visible;
        }
    }

    /// Runs `f` on each pixel of the targeted surface inside the rect.
    fn each(&self, x: u16, y: u16, w: u16, h: u16, mut f: impl FnMut(&mut u32, usize)) {
        let Some(n) = self.target.get() else { return };
        let mut ss = self.surfaces.borrow_mut();
        let Some(s) = ss[n].as_mut() else { return };
        let x1 = (x as usize + w as usize).min(s.w as usize);
        let y1 = (y as usize + h as usize).min(s.h as usize);
        let sw = s.w as usize;
        for yy in y as usize..y1 {
            for xx in x as usize..x1 {
                f(&mut s.px[yy * sw + xx], xx - x as usize);
            }
        }
    }

    pub fn clear(&self, x: u16, y: u16, w: u16, h: u16) {
        self.each(x, y, w, h, |p, _| *p = 0);
    }

    pub fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, c: u16) {
        let v = over_premul(0, c, 255);
        self.each(x, y, w, h, |p, _| *p = v);
    }

    pub fn blend_rect(&self, x: u16, y: u16, w: u16, h: u16, c: u16, a: u8) {
        if a == 0 {
            return;
        }
        self.each(x, y, w, h, |p, _| *p = over_premul(*p, c, a));
    }

    pub fn blend_span(&self, x: u16, y: u16, c: u16, alphas: &[u8]) {
        self.each(x, y, alphas.len() as u16, 1, |p, i| {
            if alphas[i] != 0 {
                *p = over_premul(*p, c, alphas[i]);
            }
        });
    }

    pub fn blend_row(&self, x: u16, y: u16, colors: &[u16], alphas: &[u8]) {
        self.each(x, y, colors.len().min(alphas.len()) as u16, 1, |p, i| {
            if alphas[i] != 0 {
                *p = over_premul(*p, colors[i], alphas[i]);
            }
        });
    }

    pub fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16) {
        self.stream.set((x, x.saturating_add(w), y.saturating_add(h), x, y));
    }

    pub fn write_pixel(&self, c: u16) {
        let (x0, x1, y1, x, y) = self.stream.get();
        if y >= y1 || x0 >= x1 {
            return;
        }
        self.fill_rect(x, y, 1, 1, c);
        let (nx, ny) = if x + 1 >= x1 { (x0, y + 1) } else { (x + 1, y) };
        self.stream.set((x0, x1, y1, nx, ny));
    }

    /// Some surface is shown.
    pub fn any_visible(&self) -> bool {
        self.surfaces.borrow().iter().flatten().any(|s| s.visible && s.alpha != 0)
    }

    /// Composites the visible surfaces, in layer order, over `dst`
    /// (0x__RRGGBB pixels, `w` x `h`; the top byte is written 0xFF).
    pub fn composite(&self, dst: &mut [u32], w: usize, h: usize) {
        for s in self.surfaces.borrow().iter().flatten() {
            if !s.visible || s.alpha == 0 {
                continue;
            }
            let plane = s.alpha as u32;
            for sy in 0..s.h as usize {
                let y = s.y + sy as i32;
                if y < 0 || y >= h as i32 {
                    continue;
                }
                for sx in 0..s.w as usize {
                    let x = s.x + sx as i32;
                    if x < 0 || x >= w as i32 {
                        continue;
                    }
                    let p = s.px[sy * s.w as usize + sx];
                    if p == 0 {
                        continue;
                    }
                    let d = &mut dst[y as usize * w + x as usize];
                    let pa = ((p >> 24) * plane + 127) / 255;
                    let na = 255 - pa;
                    let ch = |sh: u32| {
                        let sc = (((p >> sh) & 0xFF) * plane + 127) / 255;
                        (sc + (((*d >> sh) & 0xFF) * na + 127) / 255).min(255)
                    };
                    *d = 0xFF00_0000 | ch(16) << 16 | ch(8) << 8 | ch(0);
                }
            }
        }
    }
}

#[cfg(all(test, feature = "mock"))]
mod tests {
    use super::*;
    use crate::backlight::BacklightImpl;
    use crate::ctx::Ctx;
    use crate::flash::FlashImpl;
    use crate::font::FontList;
    use crate::gpio::GpioImpl;
    use crate::image::ImageList;
    use crate::lcd::{LcdBackend, LcdImpl};
    use crate::mock::*;
    use crate::platform::Platform;
    use crate::render::{render_all, render_dirty};
    use crate::rtc::RtcImpl;
    use crate::systick::SystickImpl;
    use crate::types::{Edges, Size};
    use crate::usart::UsartImpl;
    use crate::widget::{FLAG_VISIBLE, KIND_BASE};
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};

    const S: usize = 128;

    /// 128x128 XRGB8888 readback screen with `L` software layers; counts the
    /// primitives drawn on the screen and into layers.
    struct Fb<const L: usize> {
        px: RefCell<Vec<u32>>,
        layers: SoftLayers,
        screen_calls: Cell<usize>,
        layer_calls: Cell<usize>,
    }

    impl<const L: usize> Fb<L> {
        fn count(&self) -> bool {
            if self.layers.target().is_some() {
                self.layer_calls.set(self.layer_calls.get() + 1);
                true
            } else {
                self.screen_calls.set(self.screen_calls.get() + 1);
                false
            }
        }

        /// What the display shows: the screen with the layers over it.
        fn shown(&self) -> Vec<u32> {
            let mut v = self.px.borrow().clone();
            self.layers.composite(&mut v, S, S);
            v
        }
    }

    impl<const L: usize> LcdBackend for Fb<L> {
        const WIDTH: u16 = S as u16;
        const HEIGHT: u16 = S as u16;
        const HAS_ALPHA: bool = true;
        const LAYERS: usize = L;
        // Like the sim and exynos: the screen's pass 0 shifts moved panels.
        const CAN_MOVE: bool = true;
        fn move_rects(&self, rects: &[Rect], dx: i16, dy: i16) -> bool {
            if self.layers.target().is_some() {
                return false;
            }
            let before = self.px.borrow().clone();
            let mut px = self.px.borrow_mut();
            for r in rects {
                for y in r.y..r.bottom() {
                    for x in r.x..r.right() {
                        px[(y + dy) as usize * S + (x + dx) as usize] = before[y as usize * S + x as usize];
                    }
                }
            }
            self.screen_calls.set(self.screen_calls.get() + 1);
            true
        }
        fn begin_frame(&mut self) {}
        fn end_frame(&mut self) {}
        fn back_buf(&self) -> u8 {
            0
        }
        fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, c: u16) {
            self.blend_rect(x, y, w, h, c, 255);
        }
        fn blend_rect(&self, x: u16, y: u16, w: u16, h: u16, c: u16, a: u8) {
            if self.count() {
                return self.layers.blend_rect(x, y, w, h, c, a);
            }
            let mut px = self.px.borrow_mut();
            for yy in y as usize..(y as usize + h as usize).min(S) {
                for xx in x as usize..(x as usize + w as usize).min(S) {
                    let p = &mut px[yy * S + xx];
                    *p = 0xFF00_0000 | over_premul(*p | 0xFF00_0000, c, a);
                }
            }
        }
        fn blend_pixel(&self, x: u16, y: u16, c: u16, a: u8) {
            self.blend_rect(x, y, 1, 1, c, a);
        }
        fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16) {
            self.layers.begin_pixels(x, y, w, h);
        }
        fn write_pixel(&self, c: u16) {
            if self.layers.target().is_some() {
                self.layers.write_pixel(c);
            }
        }
        fn send_command(&self, _: u16) {}
        fn send_data(&self, _: u16) {}
        fn layer_alloc(&self, n: usize, w: u16, h: u16) -> bool {
            self.layers.alloc(n, w, h)
        }
        fn layer_free(&self, n: usize) {
            self.layers.free(n);
        }
        fn layer_target(&self, n: Option<usize>) {
            self.layers.set_target(n);
        }
        fn layer_clear(&self, x: u16, y: u16, w: u16, h: u16) {
            self.layers.clear(x, y, w, h);
        }
        fn layer_place(&self, n: usize, x: i32, y: i32, alpha: u8, visible: bool) {
            self.layers.place(n, x, y, alpha, visible);
        }
    }

    struct Plat<const L: usize>;
    impl<const L: usize> Platform for Plat<L> {
        type LcdB = Fb<L>;
        type FlashB = MockFlash;
        type TouchB = MockTouch;
        type BacklightB = MockBacklight;
        type RtcB = MockRtc;
        type UsartB = MockUsart;
        type SystickB = MockSystick;
        #[cfg(feature = "audio")]
        type AudioB = crate::audio::NoAudio;
        #[cfg(feature = "jpeg")]
        type JpegB = crate::jpeg::NoJpeg;
        type GpioB = MockGpio;
        type EepromB = MockEeprom;
        type Renderer = crate::renderer::DefaultRenderer;
    }

    fn ctx<const L: usize>() -> Ctx<Plat<L>> {
        Ctx {
            lcd: LcdImpl::with_backend(Fb {
                px: RefCell::new(vec![0xFF00_0000; S * S]),
                layers: SoftLayers::new(),
                screen_calls: Cell::new(0),
                layer_calls: Cell::new(0),
            }),
            flash: FlashImpl::with_backend(MockFlash),
            fonts: FontList::new(),
            images: ImageList::new(),
            objs: ObjPool::new(),
            fs: None,
            backlight: BacklightImpl::with_backend(MockBacklight),
            rtc: RtcImpl::with_backend(MockRtc),
            usart: UsartImpl::with_backend(MockUsart),
            systick: SystickImpl::init(),
            #[cfg(feature = "audio")]
            audio: crate::audio::AudioImpl::none(),
            #[cfg(feature = "jpeg")]
            jpeg: crate::jpeg::JpegImpl::none(),
            gpio: GpioImpl::with_backend(MockGpio::default()),
            eeprom: crate::eeprom::EepromImpl::with_backend(MockEeprom::default()),
            cursor_visible: false,
            #[cfg(feature = "multi_program")]
            tasks: crate::task::TaskTable::new(),
        }
    }

    fn rect(objs: &mut ObjPool, parent: WidgetId, r: Rect, bg: u16, radius: u16, alpha: u8) -> WidgetId {
        let id = objs.alloc_widget().unwrap();
        objs.add_child(parent, id);
        let w = objs.get_mut(id);
        w.kind = KIND_BASE;
        w.location = Offset { x: r.x, y: r.y };
        w.size = Size { w: r.w, h: r.h };
        w.background_color = bg;
        objs.update_ext(id, |e| {
            e.border_radius = radius;
            e.alpha = alpha;
        });
        id
    }

    struct Scene {
        panel: WidgetId,
        child: WidgetId,
        /// Base widget after the panel, at (100, 100) 20x20.
        late: WidgetId,
    }

    /// A backdrop with translucent blobs, a rounded bordered panel with
    /// opaque, translucent and rounded children (layer 1 when `layer`), and
    /// a small base widget after it.
    fn scene<const L: usize>(c: &mut Ctx<Plat<L>>, layer: bool) -> Scene {
        let o = &mut c.objs;
        let root = o.alloc_widget().unwrap();
        o.root = root;
        o.get_mut(root).size = Size { w: S as u16, h: S as u16 };
        o.get_mut(root).background_color = 0x1082;
        rect(o, root, Rect::new(-10, 40, 90, 90), 0xF81F, 45, 90);
        rect(o, root, Rect::new(70, -20, 70, 60), 0x07E0, 20, 60);
        let panel = rect(o, root, Rect::new(20, 30, 60, 40), 0x39E7, 8, 255);
        o.update_ext(panel, |e| e.border = Edges { top: 2, right: 2, bottom: 2, left: 2 });
        o.get_mut(panel).border_color = 0xFFE0;
        let child = rect(o, panel, Rect::new(5, 5, 20, 10), 0x001F, 0, 255);
        rect(o, panel, Rect::new(30, 4, 22, 22), 0xF800, 6, 128);
        rect(o, panel, Rect::new(8, 20, 40, 12), 0x07FF, 5, 200);
        let late = rect(o, root, Rect::new(100, 100, 20, 20), 0xFFFF, 0, 255);
        if layer {
            o.set_layer(panel, 1);
        }
        Scene { panel, child, late }
    }

    fn move_to<const L: usize>(c: &mut Ctx<Plat<L>>, id: WidgetId, x: i16, y: i16) {
        c.objs.will_move(id);
        c.objs.get_mut(id).location = Offset { x, y };
        c.objs.mark_moved(id);
    }

    fn set_visible<const L: usize>(c: &mut Ctx<Plat<L>>, id: WidgetId, on: bool) {
        let w = c.objs.get_mut(id);
        w.flags = if on { w.flags | FLAG_VISIBLE } else { w.flags & !FLAG_VISIBLE };
        if !c.objs.layer_placement_changed(id) {
            c.objs.mark_dirty(id);
        }
    }

    /// Same picture within rounding (premultiplied vs straight blending).
    fn assert_same(what: &str, a: &[u32], b: &[u32]) {
        for i in 0..S * S {
            let d = |s: u32| (((a[i] >> s) & 0xFF) as i32 - ((b[i] >> s) & 0xFF) as i32).abs();
            assert!(
                d(16) <= 2 && d(8) <= 2 && d(0) <= 2,
                "{what}: pixel ({}, {}) {:06x} vs {:06x}",
                i % S,
                i / S,
                a[i] & 0xFF_FFFF,
                b[i] & 0xFF_FFFF
            );
        }
    }

    /// (screen, layer) primitive calls since the last call.
    fn calls<const L: usize>(c: &Ctx<Plat<L>>) -> (usize, usize) {
        let b = c.lcd.backend();
        let n = (b.screen_calls.get(), b.layer_calls.get());
        b.screen_calls.set(0);
        b.layer_calls.set(0);
        n
    }

    fn both<const L: usize>(a: &mut Ctx<Plat<L>>, b: &mut Ctx<Plat<0>>, what: &str) {
        render_dirty(a);
        render_dirty(b);
        assert_same(what, &a.lcd.backend().shown(), &b.lcd.backend().shown());
    }

    #[test]
    fn moving_a_layer_matches_the_tree_and_draws_nothing() {
        let mut a = ctx::<2>();
        let mut b = ctx::<0>();
        let sa = scene(&mut a, true);
        let sb = scene(&mut b, true);
        render_all(&mut a);
        render_all(&mut b);
        assert!(a.objs.is_active_layer_root(sa.panel));
        assert!(!b.objs.is_active_layer_root(sb.panel), "no layers on this backend");
        assert_same("first frame", &a.lcd.backend().shown(), &b.lcd.backend().shown());
        calls(&a);
        // Around, partly off every edge, then back.
        for (step, &(x, y)) in [(24, 34), (-20, 30), (-20, -15), (90, 10), (-15, 95), (20, 30)].iter().enumerate() {
            move_to(&mut a, sa.panel, x, y);
            move_to(&mut b, sb.panel, x, y);
            both(&mut a, &mut b, &alloc::format!("step {step} ({x}, {y})"));
            assert_eq!(calls(&a), (0, 0), "step {step}: a moved layer redraws nothing");
        }
    }

    #[test]
    fn a_change_inside_a_layer_redraws_only_its_surface() {
        let mut a = ctx::<2>();
        let mut b = ctx::<0>();
        let sa = scene(&mut a, true);
        let sb = scene(&mut b, true);
        render_all(&mut a);
        render_all(&mut b);
        calls(&a);
        a.objs.get_mut(sa.child).background_color = 0xFD20;
        a.objs.mark_dirty(sa.child);
        b.objs.get_mut(sb.child).background_color = 0xFD20;
        b.objs.mark_dirty(sb.child);
        both(&mut a, &mut b, "child recoloured");
        let (screen, layer) = calls(&a);
        assert_eq!(screen, 0);
        assert!(layer > 0);
        // A child that moves while the root moves too (its prev rect is from the old place).
        move_to(&mut a, sa.child, 30, 22);
        move_to(&mut b, sb.child, 30, 22);
        move_to(&mut a, sa.panel, 10, 50);
        move_to(&mut b, sb.panel, 10, 50);
        both(&mut a, &mut b, "child and root moved");
        assert_eq!(calls(&a).0, 0);
        // The child alone moves inside the layer.
        move_to(&mut a, sa.child, 2, 2);
        move_to(&mut b, sb.child, 2, 2);
        both(&mut a, &mut b, "child moved");
        assert_eq!(calls(&a).0, 0);
    }

    #[test]
    fn hiding_and_showing_a_layer_keeps_or_redraws_its_content() {
        let mut a = ctx::<2>();
        let mut b = ctx::<0>();
        let sa = scene(&mut a, true);
        let sb = scene(&mut b, true);
        render_all(&mut a);
        render_all(&mut b);
        calls(&a);
        set_visible(&mut a, sa.panel, false);
        set_visible(&mut b, sb.panel, false);
        both(&mut a, &mut b, "hidden");
        assert_eq!(calls(&a), (0, 0), "hiding a layer redraws nothing");
        set_visible(&mut a, sa.panel, true);
        set_visible(&mut b, sb.panel, true);
        both(&mut a, &mut b, "shown again");
        assert_eq!(calls(&a), (0, 0), "its surface kept the content");
        // Changed while hidden: redrawn when shown.
        set_visible(&mut a, sa.panel, false);
        set_visible(&mut b, sb.panel, false);
        both(&mut a, &mut b, "hidden again");
        a.objs.get_mut(sa.child).background_color = 0x07E0;
        a.objs.mark_dirty(sa.child);
        b.objs.get_mut(sb.child).background_color = 0x07E0;
        b.objs.mark_dirty(sb.child);
        both(&mut a, &mut b, "changed while hidden");
        set_visible(&mut a, sa.panel, true);
        set_visible(&mut b, sb.panel, true);
        both(&mut a, &mut b, "shown after a change");
    }

    #[test]
    fn a_layer_under_a_later_widget_is_drawn_in_the_tree() {
        let mut a = ctx::<2>();
        let mut b = ctx::<0>();
        let sa = scene(&mut a, true);
        let sb = scene(&mut b, true);
        render_all(&mut a);
        render_all(&mut b);
        // Under `late` (100, 100): not a layer meanwhile.
        move_to(&mut a, sa.panel, 70, 70);
        move_to(&mut b, sb.panel, 70, 70);
        both(&mut a, &mut b, "under a later widget");
        assert!(!a.objs.is_active_layer_root(sa.panel));
        assert_eq!(a.lcd.layers_allocated(), 0);
        // Away again: back into its surface, what the screen drew of it restored.
        move_to(&mut a, sa.panel, 10, 10);
        move_to(&mut b, sb.panel, 10, 10);
        both(&mut a, &mut b, "a layer again");
        assert!(a.objs.is_active_layer_root(sa.panel));
        // The later widget moves over it (the layer itself doesn't move).
        move_to(&mut a, sa.late, 30, 20);
        move_to(&mut b, sb.late, 30, 20);
        both(&mut a, &mut b, "covered by a moved widget");
        assert!(!a.objs.is_active_layer_root(sa.panel));
    }

    #[test]
    fn plane_alpha_fades_without_redrawing() {
        let mut a = ctx::<2>();
        let sa = scene(&mut a, true);
        render_all(&mut a);
        let opaque = a.lcd.backend().shown();
        calls(&a);
        a.objs.set_layer_alpha(sa.panel, 0);
        render_dirty(&mut a);
        assert_eq!(calls(&a), (0, 0));
        let gone = a.lcd.backend().shown();
        a.objs.set_layer_alpha(sa.panel, 128);
        render_dirty(&mut a);
        assert_eq!(calls(&a), (0, 0));
        let half = a.lcd.backend().shown();
        // Inside the opaque child at (27, 37): halfway between with and without the layer.
        let i = 40 * S + 30;
        for s in [16, 8, 0] {
            let (o, g, h) = ((opaque[i] >> s) & 0xFF, (gone[i] >> s) & 0xFF, (half[i] >> s) & 0xFF);
            assert!((h as i32 - ((o + g) / 2) as i32).abs() <= 2, "channel {s}: {h} vs {o}/{g}");
        }
        assert_eq!(gone[i], a.lcd.backend().px.borrow()[i], "alpha 0 shows the screen");
    }

    #[test]
    fn unmarking_a_layer_draws_it_in_the_tree() {
        let mut a = ctx::<2>();
        let mut b = ctx::<0>();
        let sa = scene(&mut a, true);
        let _ = scene(&mut b, false);
        render_all(&mut a);
        render_all(&mut b);
        a.objs.set_layer(sa.panel, 0);
        render_dirty(&mut a);
        assert_eq!(a.lcd.layers_allocated(), 0);
        assert_eq!(a.lcd.backend().layers.target(), None);
        assert_same("unmarked", &a.lcd.backend().px.borrow(), &b.lcd.backend().shown());
    }
}
