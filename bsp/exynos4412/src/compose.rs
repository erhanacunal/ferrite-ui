//! The GPU compositor (`gpu_compose`): the Mali-400 repaints each dirty render's areas whole,
//! straight into the LCD shadow.
//!
//! `render_dirty` offers every dirty render to [`compose`] first (`WidgetRenderer::compose`):
//! the areas whose pixels change, on the 16-pixel tile grid. Each is drawn from scratch, every
//! visible widget that touches it in DFS order (painter's algorithm, premultiplied "over"
//! blending), so none of the CPU passes run: no occluders, clip regions or restores, and a
//! widget moving over a translucent one costs nothing more than drawing both.
//!
//! **What draws what.** A widget becomes a list of pieces ([`Prim`]):
//! - its box (background, gradient, translucency, border, rounded corners) from the
//!   `ui_round` shader: nine pieces when rounded (corners anti-aliased from the rounded box's
//!   distance), else the border strips and the inside, as `draw_widget` lays them out;
//! - a raw opaque RGB565 picture (a background image, an image widget's picture) sampled where
//!   it is in the flash shadow (`ui_flat`);
//! - a 3D model widget: its buffer (`model3d::texture`);
//! - anything else (text, gauges, sliders, shapes, compressed or alpha images) drawn by the CPU
//!   the built-in way into a premultiplied sprite (`renderer::Sprite`), cached by appearance like
//!   the renderer's (a label's text included), and sampled.
//!
//! **Frames.** An area is cut into frames of at most `gpu::MAX_TILES` tiles (full-width strips:
//! 96 lines of a 1280-pixel line). A frame starts at the last widget that paints the whole
//! frame opaquely: nothing before it shows. Its pieces, clipped to the frame, are batched while
//! the program and texture stay the same; each batch is one mesh of the frame and one draw.
//!
//! It declines (the built-in passes draw this render) when it cannot do all of it: the GPU or
//! its shaders missing (`gpu::Ui`), portrait mode, an area under the on-screen keyboard
//! (`ObjPool::overlay`), a scroll container (its viewport clipping and scrollbar), an
//! `on_paint` widget, a widget larger than the panel that needs a sprite, no memory, or a GPU
//! error (printed). A GPU error after some frames were shown leaves them: they are right.
//!
//! The GPU's anti-aliasing and rounding differ a little from the CPU's, so the pixels are not
//! identical to the built-in drawing's.

use alloc::vec::Vec;

use exynos_4412_rs::drivers::mali::Filter;
use exynos_4412_rs::gpu::{self, Area, Blend, Frame, Gpu, Target};
use ferrite_core::ctx::Ctx;
use ferrite_core::pool::ObjPool;
use ferrite_core::render;
use ferrite_core::types::{Color, Edges, Rect};
use ferrite_core::widget::{
    FLAG_CLIP_CHILDREN, FLAG_HAS_ON_PAINT, KIND_BASE, KIND_BUTTON, KIND_BUTTON_GROUP, KIND_IMAGE, KIND_LABEL,
    KIND_MODEL3D, KIND_RECTANGLE, WidgetId,
};

use crate::gpu::Ui;
use crate::lcd;
use crate::platform::Exynos4412Platform as P;
use crate::renderer::{self, Sprite};

pub mod shaders;

const TILE: i32 = 16;
/// Where a piece of a box away from its corners is against them: far inside.
const FAR: f32 = -256.0;

/// A rect by its edges, in screen pixels.
#[derive(Clone, Copy, PartialEq, Eq)]
struct R {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl R {
    fn of(r: &Rect) -> Self {
        Self { x0: r.x as i32, y0: r.y as i32, x1: r.x as i32 + r.w as i32, y1: r.y as i32 + r.h as i32 }
    }

    fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }

    fn meet(&self, o: &R) -> R {
        R { x0: self.x0.max(o.x0), y0: self.y0.max(o.y0), x1: self.x1.min(o.x1), y1: self.y1.min(o.y1) }
    }

    fn meets(&self, o: &R) -> bool {
        !self.meet(o).is_empty()
    }

    fn contains(&self, o: &R) -> bool {
        o.x0 >= self.x0 && o.y0 >= self.y0 && o.x1 <= self.x1 && o.y1 <= self.y1
    }

    fn inset(&self, l: i32, t: i32, r: i32, b: i32) -> R {
        R { x0: self.x0 + l, y0: self.y0 + t, x1: self.x1 - r, y1: self.y1 - b }
    }

    fn w(&self) -> i32 {
        self.x1 - self.x0
    }

    fn h(&self) -> i32 {
        self.y1 - self.y0
    }
}

/// A colour linear across the screen: `v + dx * x + dy * y` at (x, y), premultiplied.
#[derive(Clone, Copy)]
struct Lin {
    v: [f32; 4],
    dx: [f32; 4],
    dy: [f32; 4],
}

impl Lin {
    const CLEAR: Lin = Lin::flat([0.0; 4]);

    const fn flat(c: [f32; 4]) -> Self {
        Self { v: c, dx: [0.0; 4], dy: [0.0; 4] }
    }

    fn at(&self, x: f32, y: f32) -> [f32; 4] {
        core::array::from_fn(|i| self.v[i] + self.dx[i] * x + self.dy[i] * y)
    }

    /// `draw_widget`'s gradient over `g` from `c1` to `c2`: across (dir 1) or down (else),
    /// pixel centre `i` of `n + 1` at `c1 + (c2 - c1) * i / n` (`lerp_rgb565`).
    fn gradient(g: &R, c1: Color, c2: Color, dir: u8) -> Self {
        let (a, b) = (rgb(c1, 1.0), rgb(c2, 1.0));
        let (start, n) = if dir == 1 { (g.x0, g.w() - 1) } else { (g.y0, g.h() - 1) };
        if n <= 0 {
            return Self::flat(a);
        }
        let step: [f32; 4] = core::array::from_fn(|i| (b[i] - a[i]) / n as f32);
        // At the edge position p: the centre p + 0.5 is pixel p - start.
        let v = core::array::from_fn(|i| a[i] - step[i] * (start as f32 + 0.5));
        if dir == 1 { Self { v, dx: step, dy: [0.0; 4] } } else { Self { v, dx: [0.0; 4], dy: step } }
    }
}

/// RGB565 as premultiplied floats at `alpha` (0..1).
fn rgb(c: Color, alpha: f32) -> [f32; 4] {
    let r = ((c >> 11) & 0x1F) as f32 / 31.0;
    let g = ((c >> 5) & 0x3F) as f32 / 63.0;
    let b = (c & 0x1F) as f32 / 31.0;
    [r * alpha, g * alpha, b * alpha, alpha]
}

/// Texels the GPU samples where they are.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fmt {
    Argb,
    Rgb565,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Tex {
    /// 64-byte aligned.
    addr: usize,
    w: usize,
    h: usize,
    /// Bytes from a line to the next.
    stride: usize,
    fmt: Fmt,
}

/// A piece of a widget's drawing, in screen pixels.
#[derive(Clone, Copy)]
enum Prim {
    /// `ui_round`: over `rect`, the pixel at x is `qx.0 + qx.1 * x` (and y likewise) from the
    /// nearest corner circle's centre, outwards positive; corner radius, border width; the
    /// fill (inside the border) and the border colour.
    Round { rect: R, qx: (f32, f32), qy: (f32, f32), radius: f32, bw: f32, fill: Lin, border: [f32; 4] },
    /// `ui_flat`: `tex` over `rect`, texel coordinates `u.0 + u.1 * x` (and v likewise),
    /// times `color`.
    Flat { rect: R, u: (f32, f32), v: (f32, f32), color: [f32; 4], tex: Tex },
}

impl Prim {
    fn rect(&self) -> R {
        match *self {
            Prim::Round { rect, .. } | Prim::Flat { rect, .. } => rect,
        }
    }

    fn batch(&self) -> Batch {
        match *self {
            Prim::Round { .. } => Batch::Round,
            Prim::Flat { tex, .. } => Batch::Flat(tex),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Batch {
    Round,
    Flat(Tex),
}

/// Where a widget's pieces start in `prims`, and the rect it paints opaquely whole, if any.
struct Item {
    start: usize,
    opaque: Option<R>,
}

/// The compositor's scratch, kept between renders so its vectors keep their room.
struct State {
    prims: Vec<Prim>,
    items: Vec<Item>,
    /// Sprites drawn for this render only (their appearance not seen before, or not complete).
    scratch: Vec<Sprite>,
    pos: Vec<[f32; 2]>,
    shape: Vec<[f32; 4]>,
    fill: Vec<[f32; 4]>,
    border: Vec<[f32; 4]>,
    uv: Vec<[f32; 2]>,
    color: Vec<[f32; 4]>,
    /// A GPU error was printed: only the first is.
    told: bool,
}

static mut STATE: State = State {
    prims: Vec::new(),
    items: Vec::new(),
    scratch: Vec::new(),
    pos: Vec::new(),
    shape: Vec::new(),
    fill: Vec::new(),
    border: Vec::new(),
    uv: Vec::new(),
    color: Vec::new(),
    told: false,
};

fn state() -> &'static mut State {
    // SAFETY: CPU0 only, from the render pass, never reentrantly.
    unsafe { &mut *(&raw mut STATE) }
}

/// Repaints `areas` (see the module's documentation); false = declined, nothing drawn.
pub fn compose(ctx: &Ctx<P>, areas: &[Rect]) -> bool {
    if ctx.lcd.is_portrait() {
        return false;
    }
    if let Some(o) = ctx.objs.overlay {
        if areas.iter().any(|a| a.intersects(&o)) {
            return false;
        }
    }
    let Some(shared) = crate::gpu::get() else { return false };
    let Some(ui) = shared.ui else { return false };
    let gpu = &mut shared.gpu;
    let s = state();
    s.prims.clear();
    s.items.clear();
    s.scratch.clear();
    let ok = s.collect(ctx, &ui, gpu, areas) && s.draw(&ui, gpu, areas);
    // The sprites drawn for this render only go (the frames are over).
    s.scratch.clear();
    ok
}

impl State {
    /// Every visible widget that touches `areas`, in DFS order, as pieces. False to decline.
    fn collect(&mut self, ctx: &Ctx<P>, ui: &Ui, gpu: &mut Gpu, areas: &[Rect]) -> bool {
        let objs = &ctx.objs;
        let areas: Vec<R> = areas.iter().map(R::of).collect();
        let keep_after = renderer::cache().tick();
        let n = objs.dfs_len();
        let mut i = 0;
        while i < n {
            if !objs.vis_at(i) {
                i = objs.sub_end_at(i);
                continue;
            }
            let id = objs.dfs_at(i);
            let abs = objs.abs_at(i);
            let sp = objs.scroll_parent_at(i);
            if sp.is_some() {
                // As the passes: only children wholly inside the viewport are drawn.
                let v = R::of(&objs.scroll_viewport(sp));
                if !v.contains(&R::of(&abs)) {
                    i = objs.sub_end_at(i);
                    continue;
                }
            }
            let r = R::of(&abs);
            if !areas.iter().any(|a| a.meets(&r)) {
                i += 1;
                continue;
            }
            let w = objs.get(id);
            if w.flags & (FLAG_CLIP_CHILDREN | FLAG_HAS_ON_PAINT) != 0 {
                return false;
            }
            let start = self.prims.len();
            let Some(opaque) = self.widget(ctx, ui, gpu, id, &abs, keep_after) else { return false };
            self.items.push(Item { start, opaque });
            i += 1;
        }
        true
    }

    /// Widget `id`'s pieces; the rect it paints opaquely, if any. None to decline.
    fn widget(&mut self, ctx: &Ctx<P>, ui: &Ui, gpu: &mut Gpu, id: WidgetId, abs: &Rect, keep_after: u32) -> Option<Option<R>> {
        let objs = &ctx.objs;
        let w = objs.get(id);
        let r = R::of(abs);
        if w.kind == KIND_MODEL3D {
            match crate::model3d::texture(ctx, id, abs, gpu) {
                Some((addr, w16, h16)) => {
                    let tex = Tex { addr, w: w16, h: h16, stride: w16 * 4, fmt: Fmt::Argb };
                    self.flat(r, r.x0, r.y0, tex, 0);
                    return Some(Some(r));
                }
                // Its background, as the built-in drawing shows it then.
                None => return self.sprite(ctx, id, abs, keep_after).map(|_| None),
            }
        }
        if !matches!(w.kind, KIND_BASE | KIND_BUTTON | KIND_BUTTON_GROUP | KIND_RECTANGLE | KIND_IMAGE) {
            if is_empty_label(objs, id) {
                return Some(None);
            }
            return self.sprite(ctx, id, abs, keep_after).map(|_| None);
        }
        let e = objs.ext(id);
        // The picture: a background image at the inside's top left, or an image widget's,
        // centred in the inside.
        let mut picture = None;
        if e.image_id != 0 {
            let inner = R::of(&render::inner_rect(abs, &e.border));
            let pic = ctx.images.find(e.image_id).and_then(|img| {
                if inner.is_empty() || !img.is_raw_opaque() {
                    return None;
                }
                let (pw, ph) = (img.width as i32, img.height as i32);
                let (x, y) = if w.kind == KIND_IMAGE {
                    let x = if inner.w() > pw { inner.x0 + (inner.w() - pw) / 2 } else { inner.x0 };
                    let y = if inner.h() > ph { inner.y0 + (inner.h() - ph) / 2 } else { inner.y0 };
                    (x, y)
                } else {
                    (inner.x0, inner.y0)
                };
                let addr = renderer::picture_addr(img.data_addr(), (pw * ph * 2) as usize)?;
                // From the 64-byte boundary before it: `shift` texels to the left.
                let shift = (addr % 64) / 2;
                let stride = pw as usize * 2;
                if addr % 2 != 0 || (!ui.any_565_stride && stride % 64 != 0) || pw as usize + shift > 4096 {
                    return None;
                }
                let tex = Tex { addr: addr - addr % 64, w: pw as usize + shift, h: ph as usize, stride, fmt: Fmt::Rgb565 };
                Some((R { x0: x, y0: y, x1: x + pw, y1: y + ph }, tex, shift))
            });
            match pic {
                Some(p) => picture = Some(p),
                // Not a picture the GPU can read: the whole widget from a sprite.
                None => return self.sprite(ctx, id, abs, keep_after).map(|_| None),
            }
        }
        self.boxes(objs, id, &r);
        let mut opaque = render::covers_opaquely(objs, id, abs, abs).then_some(r);
        if let Some((at, tex, shift)) = picture {
            // Clipped to the widget's box, as the scissor does.
            let shown = at.meet(&r);
            if !shown.is_empty() {
                self.flat(shown, at.x0, at.y0, tex, shift);
                if shown == r && e.border_radius == 0 {
                    opaque = Some(r);
                }
            }
        }
        Some(opaque)
    }

    /// The widget's box as `draw_widget` paints it: border and background.
    fn boxes(&mut self, objs: &ObjPool, id: WidgetId, r: &R) {
        let w = objs.get(id);
        let e = objs.ext(id);
        let bg = render::effective_bg(objs, w, &e);
        let draw_bg = bg != 0 || (w.kind != KIND_LABEL && w.kind != KIND_IMAGE);
        let alpha = e.alpha as f32 / 255.0;
        let gdir = if bg == w.background_color { objs.gradient_dir(id) } else { 0 };
        let b = e.border;
        if e.border_radius > 0 {
            let bw = b.top.max(b.bottom).max(b.left).max(b.right) as i32;
            // The background reaches halfway under the border (`draw_widget`): its gradient
            // spans that box.
            let inset = bw / 2;
            let g = r.inset(inset, inset, inset, inset);
            let fill = if !draw_bg {
                Lin::CLEAR
            } else if gdir != 0 {
                Lin::gradient(&g, bg, objs.gradient_color(id), gdir)
            } else {
                Lin::flat(rgb(bg, alpha))
            };
            let border = if bw > 0 { rgb(w.border_color, 1.0) } else { [0.0; 4] };
            self.rounded(r, e.border_radius as i32, bw, fill, border);
            return;
        }
        let strips = [
            R { x0: r.x0, y0: r.y0, x1: r.x1, y1: r.y0 + b.top as i32 },
            R { x0: r.x0, y0: r.y1 - b.bottom as i32, x1: r.x1, y1: r.y1 },
            R { x0: r.x0, y0: r.y0 + b.top as i32, x1: r.x0 + b.left as i32, y1: r.y1 - b.bottom as i32 },
            R { x0: r.x1 - b.right as i32, y0: r.y0 + b.top as i32, x1: r.x1, y1: r.y1 - b.bottom as i32 },
        ];
        if b != Edges::ZERO {
            let border = Lin::flat(rgb(w.border_color, 1.0));
            for s in strips {
                self.solid(s, border);
            }
        }
        if draw_bg {
            let inner = r.inset(b.left as i32, b.top as i32, b.right as i32, b.bottom as i32);
            let fill = if gdir != 0 {
                Lin::gradient(&inner, bg, objs.gradient_color(id), gdir)
            } else {
                Lin::flat(rgb(bg, alpha))
            };
            self.solid(inner, fill);
        }
    }

    fn solid(&mut self, rect: R, fill: Lin) {
        if !rect.is_empty() {
            self.prims.push(Prim::Round { rect, qx: (FAR, 0.0), qy: (FAR, 0.0), radius: 0.0, bw: 0.0, fill, border: [0.0; 4] });
        }
    }

    /// A rounded box: nine pieces, the corners `k` square (the radius, or the border if wider,
    /// at most half the box), each knowing where it is from its corner circle's centre.
    fn rounded(&mut self, r: &R, radius: i32, bw: i32, fill: Lin, border: [f32; 4]) {
        let radius = radius.min(r.w() / 2).min(r.h() / 2).max(0);
        let kx = radius.max(bw).min(r.w() / 2);
        let ky = radius.max(bw).min(r.h() / 2);
        let cols = [r.x0, r.x0 + kx, r.x1 - kx, r.x1];
        let rows = [r.y0, r.y0 + ky, r.y1 - ky, r.y1];
        let rad = radius as f32;
        // Outwards from the left corners' centre (x0 + radius), the right ones' (x1 - radius).
        let qx = [(r.x0 as f32 + rad, -1.0), (FAR, 0.0), (-(r.x1 as f32 - rad), 1.0)];
        let qy = [(r.y0 as f32 + rad, -1.0), (FAR, 0.0), (-(r.y1 as f32 - rad), 1.0)];
        for row in 0..3 {
            for col in 0..3 {
                let rect = R { x0: cols[col], y0: rows[row], x1: cols[col + 1], y1: rows[row + 1] };
                if !rect.is_empty() {
                    self.prims.push(Prim::Round { rect, qx: qx[col], qy: qy[row], radius: rad, bw: bw as f32, fill, border });
                }
            }
        }
    }

    /// `tex` over `rect`, its texel `shift` at screen (`x`, `y`) (the texture's first texel
    /// `shift` to the left of it).
    fn flat(&mut self, rect: R, x: i32, y: i32, tex: Tex, shift: usize) {
        let (tw, th) = (tex.w as f32, tex.h as f32);
        let u = ((shift as f32 - x as f32) / tw, 1.0 / tw);
        let v = (-(y as f32) / th, 1.0 / th);
        self.prims.push(Prim::Flat { rect, u, v, color: [1.0; 4], tex });
    }

    /// Widget `id` from a sprite: cached by appearance when it is complete and was seen
    /// before, else drawn for this render only. None if it cannot be had.
    fn sprite(&mut self, ctx: &Ctx<P>, id: WidgetId, abs: &Rect, keep_after: u32) -> Option<()> {
        let kind = ctx.objs.get(id).kind;
        let cache = renderer::cache();
        let mut found = None;
        if renderer::appearance_complete(kind) {
            let key = renderer::appearance(ctx, id, abs);
            found = match cache.find(key) {
                Some(at) => Some(at),
                None if cache.seen(key) => cache.insert(ctx, id, abs, key, Some(keep_after)),
                None => None,
            };
        }
        let (px, w, h, stride) = match found {
            Some(at) => {
                let s = cache.entry(at);
                (s.pixels() as usize, s.w, s.h, s.stride)
            }
            None => {
                let s = Sprite::draw(ctx, id, abs, 0)?;
                let got = (s.pixels() as usize, s.w, s.h, s.stride);
                self.scratch.push(s);
                got
            }
        };
        let tex = Tex { addr: px, w, h, stride: stride * 4, fmt: Fmt::Argb };
        #[cfg(feature = "perf")]
        ferrite_core::perf::add_count(ferrite_core::perf::count::SPRITE_BLITS, 1);
        self.flat(R::of(abs), abs.x as i32, abs.y as i32, tex, 0);
        Some(())
    }

    /// Runs the frames of every area into the shadow. False if the GPU failed.
    fn draw(&mut self, ui: &Ui, gpu: &mut Gpu, areas: &[Rect]) -> bool {
        let Some((address, stride, width, height)) = lcd::gpu_target() else { return false };
        if !lcd::gpu_begin() {
            return false;
        }
        // SAFETY: the shadow, mapped at the GPU's start; between `gpu_begin` and `gpu_drew` only
        // the GPU writes it.
        if unsafe { gpu.set_target(Target { address, stride, width, height }) }.is_err() {
            return false;
        }
        for a in areas {
            let a = R::of(a);
            // Full-width strips of at most MAX_TILES tiles.
            let across = (a.w() + TILE - 1) / TILE;
            let lines = ((gpu::MAX_TILES as i32 / across.max(1)).max(1) * TILE).min(a.h().max(TILE));
            let mut y = a.y0;
            while y < a.y1 {
                let c = R { x0: a.x0, y0: y, x1: a.x1, y1: (y + lines).min(a.y1) };
                if let Err(e) = self.frame(ui, gpu, &c) {
                    if !self.told {
                        self.told = true;
                        exynos_4412_rs::println!("ferrite: GPU compositor frame failed: {:?}", e);
                    }
                    return false;
                }
                lcd::gpu_drew(c.x0 as usize, c.y0 as usize, c.w() as usize, c.h() as usize);
                y = c.y1;
            }
        }
        true
    }

    /// One frame over `c` (whole tiles, inside the shadow).
    fn frame(&mut self, ui: &Ui, gpu: &mut Gpu, c: &R) -> Result<(), gpu::Error> {
        #[cfg(feature = "perf")]
        let t0 = crate::perf::cycles();
        let first = self.items.iter().rposition(|it| it.opaque.is_some_and(|o| o.contains(c))).unwrap_or(0);
        let from = self.items.get(first).map_or(self.prims.len(), |it| it.start);
        let mut frame = gpu.frame(Area::new(c.x0 as usize, c.y0 as usize, c.w() as usize, c.h() as usize), 0xFF00_0000);
        let scale = [2.0 / c.w() as f32, 2.0 / c.h() as f32];
        let mut batch = None;
        let mut drew = false;
        for at in from..self.prims.len() {
            let p = self.prims[at];
            let rect = p.rect().meet(c);
            if rect.is_empty() {
                continue;
            }
            let b = p.batch();
            if batch != Some(b) {
                if let Some(done) = batch {
                    self.flush(ui, &mut frame, done, scale)?;
                    drew = true;
                }
                batch = Some(b);
            }
            self.push(&p, &rect, c);
        }
        if let Some(done) = batch {
            self.flush(ui, &mut frame, done, scale)?;
            drew = true;
        }
        if !drew {
            // Nothing there: black, as an empty frame would leave the area as it was.
            self.push(&Prim::Round { rect: *c, qx: (FAR, 0.0), qy: (FAR, 0.0), radius: 0.0, bw: 0.0, fill: Lin::flat([0.0, 0.0, 0.0, 1.0]), border: [0.0; 4] }, c, c);
            self.flush(ui, &mut frame, Batch::Round, scale)?;
        }
        let shown = frame.show();
        #[cfg(feature = "perf")]
        ferrite_core::perf::add_sub(ferrite_core::perf::phase::G2D, crate::perf::cycles().wrapping_sub(t0));
        shown.map(|_| ())
    }

    /// Two triangles of `p` over `rect` (inside the frame `c`) into the batch's vertices.
    fn push(&mut self, p: &Prim, rect: &R, c: &R) {
        let corners = [(rect.x0, rect.y0), (rect.x1, rect.y0), (rect.x0, rect.y1), (rect.x1, rect.y0), (rect.x1, rect.y1), (rect.x0, rect.y1)];
        for (x, y) in corners {
            let (fx, fy) = (x as f32, y as f32);
            self.pos.push([(x - c.x0) as f32, (y - c.y0) as f32]);
            match *p {
                Prim::Round { qx, qy, radius, bw, fill, border, .. } => {
                    self.shape.push([qx.0 + qx.1 * fx, qy.0 + qy.1 * fy, radius, bw]);
                    self.fill.push(fill.at(fx, fy));
                    self.border.push(border);
                }
                Prim::Flat { u, v, color, .. } => {
                    self.uv.push([u.0 + u.1 * fx, v.0 + v.1 * fy]);
                    self.color.push(color);
                }
            }
        }
    }

    /// The batch's vertices as one mesh of the frame, and its draw.
    fn flush(&mut self, ui: &Ui, frame: &mut Frame<'_>, batch: Batch, scale: [f32; 2]) -> Result<(), gpu::Error> {
        let result = match batch {
            Batch::Round => frame
                .mesh(&ui.round)
                .attribute("pos", &self.pos)
                .attribute("shape", &self.shape)
                .attribute("fill", &self.fill)
                .attribute("border", &self.border)
                .build()
                .map(|mesh| {
                    frame.draw(&mesh).uniform("scale", &scale).blend(Blend::Premultiplied);
                }),
            Batch::Flat(t) => {
                let format = match t.fmt {
                    Fmt::Argb => ui.argb,
                    Fmt::Rgb565 => ui.rgb565,
                };
                // SAFETY: sprites and model buffers are heap memory the GPU maps, cleaned when
                // drawn and kept until the frames are over (`scratch`, the cache's `keep_after`);
                // pictures are in the flash shadow, on the heap, cleaned by `picture_addr`.
                unsafe { frame.texture_at(t.addr, t.w, t.h, t.stride, format, Filter::Nearest) }.and_then(|texture| {
                    frame
                        .mesh(&ui.flat)
                        .attribute("pos", &self.pos)
                        .attribute("uv", &self.uv)
                        .attribute("color", &self.color)
                        .build()
                        .map(|mesh| {
                            frame.draw(&mesh).uniform("scale", &scale).texture("t", &texture).blend(Blend::Premultiplied);
                        })
                })
            }
        };
        self.pos.clear();
        self.shape.clear();
        self.fill.clear();
        self.border.clear();
        self.uv.clear();
        self.color.clear();
        result
    }
}

/// A label that paints nothing: no background, border, image or text.
fn is_empty_label(objs: &ObjPool, id: WidgetId) -> bool {
    let w = objs.get(id);
    if w.kind != KIND_LABEL {
        return false;
    }
    let e = objs.ext(id);
    render::effective_bg(objs, w, &e) == 0
        && e.border == Edges::ZERO
        && e.image_id == 0
        && (e.font_id == 0xFF || objs.str_get(e.text_id).is_empty())
}
