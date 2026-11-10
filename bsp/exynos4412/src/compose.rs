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
//! - its box (background, gradient, translucency, border, rounded corners): solid pieces, and
//!   when rounded anti-aliased triangles (`ferrite_core::lcd::tess`, coverage per vertex;
//!   [`TESS`] off: the `ui_round` shader's corner squares), as `draw_widget` lays them out;
//! - a label's text (without a picture): one quad per glyph from the glyph atlas ([`Atlas`],
//!   white × coverage, times the text colour), laid out by `render::label_lines`;
//! - a raw opaque RGB565 picture (a background image, an image widget's picture) sampled where
//!   it is in the flash shadow (`ui_flat`);
//! - a 3D model widget: its buffer (`model3d::texture`);
//! - a circle / ellipse shape or a gauge: the parts its drawing issues (`render::round_parts`)
//!   as anti-aliased triangles (`tess::oval`, `tess::wedge`); a convex polygon shape as a
//!   mitered fill (`tess::convex_fill`) and its outline (`tess::outline`). A shape whose
//!   appearance is cached (or was seen in an earlier frame) comes from its sprite instead;
//! - anything else (sliders, lines, concave polygons, compressed or alpha images) drawn by the CPU
//!   the built-in way into a premultiplied sprite (`renderer::Sprite`), cached by appearance like
//!   the renderer's (a label's text included), and sampled.
//!
//! **Frames.** The core's areas are first merged pairwise while one frame over both is
//! estimated cheaper (`merge_areas`: ~0.38 ms fixed per frame, ~11 µs per thousand pixels),
//! and until none overlap. A render that would still take more than `FREE_FRAMES` frames
//! averaging under `MIN_FRAME_KPX` thousand pixels is declined (`small_areas`).
//! An area is cut into frames of at most `gpu::MAX_TILES` tiles (full-width strips:
//! 96 lines of a 1280-pixel line). A frame starts at the last widget that paints the whole
//! frame opaquely: nothing before it shows. Its pieces, clipped to the frame, are batched while
//! the program and texture stay the same; each batch is one mesh of the frame and one draw.
//!
//! It declines (the built-in passes draw this render) when it cannot do all of it: the GPU or
//! its shaders missing (`gpu::Ui`), portrait mode, an area under the on-screen keyboard
//! (`ObjPool::overlay`), an `on_paint` widget, a widget larger than the panel that needs a sprite, no memory, or a GPU
//! error (printed). A GPU error after some frames were shown leaves them: they are right.
//!
//! Scroll containers are drawn like the passes draw them: a child not wholly inside the
//! viewport is left out with its subtree, nothing is cut to the viewport, and the scrollbar
//! (`render::scrollbar_parts`) goes over the container's subtree.
//!
//! The GPU's anti-aliasing and rounding differ a little from the CPU's, so the pixels are not
//! identical to the built-in drawing's.

use alloc::vec::Vec;

use exynos_4412_rs::drivers::mali::Filter;
use exynos_4412_rs::gpu::{self, Area, Blend, Frame, Gpu, Target};
use ferrite_core::ctx::Ctx;
use ferrite_core::lcd::tess;
use ferrite_core::pool::ObjPool;
use ferrite_core::render::{self, RoundPart};
use ferrite_core::types::{Color, Edges, Rect};
use ferrite_core::widget::{
    FLAG_CLIP_CHILDREN, FLAG_HAS_ON_PAINT, KIND_BASE, KIND_BUTTON, KIND_BUTTON_GROUP, KIND_IMAGE, KIND_LABEL,
    KIND_CHECKBOX, KIND_CIRCLE, KIND_DROPDOWN, KIND_ELLIPSE, KIND_GAUGE, KIND_GRAPH, KIND_INPUT, KIND_LINE, KIND_MODEL3D, KIND_POLYGON,
    KIND_PROGRESS, KIND_RADIO, KIND_RECTANGLE, KIND_SLIDER, WidgetId,
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

    fn rect(&self) -> Rect {
        Rect::new(self.x0 as i16, self.y0 as i16, self.w() as u16, self.h() as u16)
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
    /// times `color` (solid colours: the white texel times it).
    Flat { rect: R, u: (f32, f32), v: (f32, f32), color: Lin, tex: Tex },
    /// `ui_flat` with the white texel: triangles `start..start + len` of `State::tverts`
    /// (`ferrite_core::lcd::tess`, coverage per vertex) inside `bbox`, times `color`. Not cut
    /// to a frame: the GPU clips them to it.
    Tris { bbox: R, start: u32, len: u32, color: Lin, white: usize },
}

impl Prim {
    fn rect(&self) -> R {
        match *self {
            Prim::Round { rect, .. } | Prim::Flat { rect, .. } => rect,
            Prim::Tris { bbox, .. } => bbox,
        }
    }

    fn batch(&self) -> Batch {
        match *self {
            Prim::Round { .. } => Batch::Round,
            Prim::Flat { tex, .. } => Batch::Flat(tex),
            Prim::Tris { white, .. } => Batch::Flat(Tex { addr: white, w: 1, h: 1, stride: 64, fmt: Fmt::Argb }),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Batch {
    Round,
    Flat(Tex),
}

/// A widget's image: where it is drawn (screen), its texture (`shift` texels from the
/// texture's left edge), and whether it is opaque.
#[derive(Clone, Copy)]
struct Picture {
    at: R,
    tex: Tex,
    shift: usize,
    opaque: bool,
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
    arc: Vec<[f32; 4]>,
    fill: Vec<[f32; 4]>,
    border: Vec<[f32; 4]>,
    uv: Vec<[f32; 2]>,
    color: Vec<[f32; 4]>,
    /// Anti-aliased triangles of this render's `Prim::Tris` (`tess`), and the tessellator's
    /// chunks of the shape being added.
    tverts: Vec<tess::Vtx>,
    tchunks: Vec<tess::Chunk>,
    /// Round parts' triangles from earlier renders.
    meshes: MeshCache,
    /// Glyphs of labels, for `ui_flat` text.
    atlas: Atlas,
    /// One glyph's coverage while it is put in the atlas.
    cov: Vec<u8>,
    /// This render draws label text from the atlas ([`GLYPHS`] on, the atlas allocated).
    glyphs: bool,
    /// A graph's curve points, and triangles cut to a box (`tess_in`), kept for their room.
    gpts: Vec<(i32, i32)>,
    cov_tris: Vec<tess::Vtx>,
    /// Images that aren't raw opaque RGB565, decoded into textures.
    images: Images,
    /// The GPU reads RGB565 rows of any stride (`Ui::any_565_stride`), for this render.
    any_565: bool,
    /// A GPU error was printed: only the first is.
    told: bool,
    /// One opaque white texel (heap, 64-byte aligned, cleaned): solid pieces are `ui_flat`
    /// with it, times their colour. 0 until the first render.
    white: usize,
}

static mut STATE: State = State {
    prims: Vec::new(),
    items: Vec::new(),
    scratch: Vec::new(),
    pos: Vec::new(),
    shape: Vec::new(),
    arc: Vec::new(),
    fill: Vec::new(),
    border: Vec::new(),
    uv: Vec::new(),
    color: Vec::new(),
    tverts: Vec::new(),
    tchunks: Vec::new(),
    meshes: MeshCache { entries: Vec::new(), clock: 0 },
    atlas: Atlas::new(),
    cov: Vec::new(),
    glyphs: false,
    gpts: Vec::new(),
    cov_tris: Vec::new(),
    images: Images { entries: Vec::new(), bytes: 0, render: 0 },
    any_565: false,
    told: false,
    white: 0,
};

/// Triangles of round parts (`tess::oval`/`wedge`) by the part, colour aside: the clock
/// face, its dots and the gauges' tracks are tessellated once. Positions are absolute, so a
/// moved shape is a new entry; entries unused for [MESH_KEEP] lookups go once the cache
/// holds [MESH_MAX].
struct MeshCache {
    entries: Vec<MeshEntry>,
    /// Lookups so far: an entry's `used` is the clock at its last one.
    clock: u32,
}

struct MeshEntry {
    part: RoundPart,
    used: u32,
    verts: Vec<tess::Vtx>,
    chunks: Vec<tess::Chunk>,
}

const MESH_MAX: usize = 256;
const MESH_KEEP: u32 = 1024;

impl MeshCache {
    /// The part with its colour left out (the triangles don't depend on it).
    fn key(p: &RoundPart) -> RoundPart {
        let mut k = *p;
        match &mut k {
            RoundPart::Oval { color, .. }
            | RoundPart::Arc { color, .. }
            | RoundPart::Wedge { color, .. }
            | RoundPart::Rect { color, .. }
            | RoundPart::RoundRect { color, .. }
            | RoundPart::Path { color, .. } => *color = 0,
        }
        k
    }

    fn find(&mut self, p: &RoundPart) -> Option<usize> {
        self.clock = self.clock.wrapping_add(1);
        let k = Self::key(p);
        let at = self.entries.iter().position(|e| e.part == k)?;
        self.entries[at].used = self.clock;
        Some(at)
    }

    fn insert(&mut self, p: RoundPart, verts: Vec<tess::Vtx>, chunks: Vec<tess::Chunk>) -> usize {
        if self.entries.len() >= MESH_MAX {
            let now = self.clock;
            self.entries.retain(|e| now.wrapping_sub(e.used) < MESH_KEEP);
            if self.entries.len() >= MESH_MAX {
                self.entries.clear();
            }
        }
        self.entries.push(MeshEntry { part: Self::key(&p), used: self.clock, verts, chunks });
        self.entries.len() - 1
    }
}

/// Images that are not raw opaque RGB565 (compressed, indexed, with alpha, JPEG) decoded
/// once into premultiplied ARGB8888 textures (the image drawn into a sprite target), kept by
/// the image's pixel data address: every widget showing it samples the same texture, whatever
/// its box. Least recently used ones go past [`IMAGE_BYTES`], never one this render uses.
struct Images {
    entries: Vec<ImgTex>,
    bytes: usize,
    /// Renders so far: an entry's `used` is the render that last sampled it.
    render: u32,
}

struct ImgTex {
    data: u32,
    w: usize,
    h: usize,
    /// Pixels per row (`w` rounded up to 16), the pixels from `start` words into `px`.
    stride: usize,
    px: Vec<u32>,
    start: usize,
    used: u32,
}

const IMAGE_BYTES: usize = 32 << 20;
const IMAGE_ENTRIES: usize = 64;

impl Images {
    /// `img` as a texture (decoded now if not held); None without the memory, or larger
    /// than the panel.
    fn tex(&mut self, ctx: &Ctx<P>, img: &ferrite_core::image::Image) -> Option<Tex> {
        let (w, h) = (img.width as usize, img.height as usize);
        let found = self.entries.iter().position(|e| e.data == img.data_addr() && (e.w, e.h) == (w, h));
        let at = match found {
            Some(at) => at,
            None => {
                if w == 0 || h == 0 || w > lcd::PANEL.width || h > lcd::PANEL.height {
                    return None;
                }
                let stride = w.next_multiple_of(16);
                let words = stride * h + 16;
                let render = self.render;
                while self.bytes + words * 4 > IMAGE_BYTES || self.entries.len() >= IMAGE_ENTRIES {
                    let lru = (0..self.entries.len())
                        .filter(|&i| self.entries[i].used != render)
                        .max_by_key(|&i| render.wrapping_sub(self.entries[i].used))?;
                    let old = self.entries.swap_remove(lru);
                    self.bytes -= old.px.len() * 4;
                }
                let mut px = Vec::new();
                px.try_reserve_exact(words).ok()?;
                // Transparent: the image composites onto it.
                px.resize(words, 0u32);
                let start = ((px.as_ptr() as usize).next_multiple_of(64) - px.as_ptr() as usize) / 4;
                let base = px[start..].as_mut_ptr();
                let t = now_us();
                // SAFETY: `base` has stride * h words in `px`, only the primitives touch them
                // until `end_sprite`; w, h are within the panel.
                unsafe { lcd::begin_sprite(base, w, h, stride) };
                img.draw(&ctx.lcd, &ctx.flash, 0, 0);
                lcd::end_sprite(base, stride * h);
                sprite_time(KIND_IMAGE, now_us() - t);
                self.bytes += px.len() * 4;
                self.entries.push(ImgTex { data: img.data_addr(), w, h, stride, px, start, used: render });
                self.entries.len() - 1
            }
        };
        let e = &mut self.entries[at];
        e.used = self.render;
        Some(Tex { addr: e.px[e.start..].as_ptr() as usize, w: e.w, h: e.h, stride: e.stride * 4, fmt: Fmt::Argb })
    }
}

/// Side of the glyph atlas in texels (ARGB8888: 4 MB). The GPU's mediump texture
/// coordinates were checked exact across 1280 texels (`gpu.rs` self-test).
const ATLAS: usize = 1024;

/// Label glyphs as premultiplied white × coverage (ARGB8888), packed in rows: a glyph is put
/// in the first time a render shows it and stays until the atlas is full; then it is emptied
/// at the start of the next render (the frames are synchronous: nothing samples it then). A
/// label whose glyph doesn't fit is drawn from a sprite meanwhile.
struct Atlas {
    /// Heap, 64-byte aligned, `ATLAS`² texels; 0 = not allocated yet, 1 = no memory.
    addr: usize,
    /// (font, codepoint) key → the glyph's top left in the atlas.
    slots: alloc::collections::BTreeMap<u64, (u16, u16)>,
    /// Where the next glyph goes: the current row's top and next free column, its height.
    x: u16,
    y: u16,
    row_h: u16,
    full: bool,
}

impl Atlas {
    const fn new() -> Self {
        Self { addr: 0, slots: alloc::collections::BTreeMap::new(), x: 0, y: 0, row_h: 0, full: false }
    }

    fn tex(&self) -> Tex {
        Tex { addr: self.addr, w: ATLAS, h: ATLAS, stride: ATLAS * 4, fmt: Fmt::Argb }
    }

    /// Ready for a render: allocated (false if there is no memory), emptied if it filled up.
    fn begin(&mut self) -> bool {
        if self.addr == 0 {
            let layout = core::alloc::Layout::from_size_align(ATLAS * ATLAS * 4, 64).unwrap();
            // SAFETY: a non-zero size; the block is kept for good.
            let p = unsafe { alloc::alloc::alloc_zeroed(layout) };
            self.addr = if p.is_null() { 1 } else { p as usize };
            if !p.is_null() {
                exynos_4412_rs::mmu::clean_dcache_range(self.addr, ATLAS * ATLAS * 4);
            }
        }
        if self.addr == 1 {
            return false;
        }
        if self.full {
            self.slots.clear();
            (self.x, self.y, self.row_h, self.full) = (0, 0, 0, false);
        }
        true
    }

    /// Glyph `g` of `font` (codepoint `cp`) in the atlas: its top left. None if it doesn't fit.
    fn slot(
        &mut self,
        flash: &ferrite_core::flash::FlashImpl<<P as ferrite_core::platform::Platform>::FlashB>,
        font: &ferrite_core::font::Font,
        cp: u16,
        g: &ferrite_core::font::GfxGlyph,
        cov: &mut Vec<u8>,
    ) -> Option<(u16, u16)> {
        let key = (font.bitmap_addr() as u64) << 32 | (font.bpp as u64) << 24 | (font.font_id as u64) << 16 | cp as u64;
        if let Some(&at) = self.slots.get(&key) {
            return Some(at);
        }
        if self.full {
            return None;
        }
        // One texel apart (none would be sampled across with exact coordinates; a margin
        // costs nothing).
        let (w, h) = (g.width as u16 + 1, g.height as u16 + 1);
        if self.x + w > ATLAS as u16 {
            (self.x, self.y, self.row_h) = (0, self.y + self.row_h, 0);
        }
        if self.y + h > ATLAS as u16 || w > ATLAS as u16 {
            self.full = true;
            return None;
        }
        let at = (self.x, self.y);
        self.x += w;
        self.row_h = self.row_h.max(h);
        let n = g.width as usize * g.height as usize;
        cov.clear();
        cov.resize(n, 0);
        font.glyph_coverage(flash, g, cov);
        let gw = g.width as usize;
        for row in 0..g.height as usize {
            let line = self.addr + ((at.1 as usize + row) * ATLAS + at.0 as usize) * 4;
            // SAFETY: inside the atlas block (the glyph fits where it is put).
            let px = unsafe { core::slice::from_raw_parts_mut(line as *mut u32, gw) };
            for (p, &c) in px.iter_mut().zip(&cov[row * gw..(row + 1) * gw]) {
                *p = c as u32 * 0x0101_0101;
            }
            exynos_4412_rs::mmu::clean_dcache_range(line, gw * 4);
        }
        self.slots.insert(key, at);
        Some(at)
    }
}

fn state() -> &'static mut State {
    // SAFETY: CPU0 only, from the render pass, never reentrantly.
    unsafe { &mut *(&raw mut STATE) }
}

/// Off = the passes draw everything. On at boot since 2026-11-10 (board, render per present,
/// compositor vs passes): Clock app 2.0 vs 8.4–10 ms, Monitor stress test 2.2 vs 2.7 ms
/// (after merging areas into fewer frames and `ui_round` for small corners), page slide
/// 9.8–10.2 vs 9.4–9.5 ms. HTTP `POST /api/compose?on=1|0` switches it, for A/B runs and
/// screenshots of the same scene both ways.
pub static ENABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
/// Rounded boxes as anti-aliased triangles (`ferrite_core::lcd::tess`: coverage per vertex,
/// `ui_flat` two instructions a pixel) rather than `ui_round` corner squares (the shader
/// works the coverage out per pixel, ~a dozen instructions). On by default; HTTP
/// `POST /api/compose?tess=0|1` switches it for A/B runs.
pub static TESS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
/// With [`TESS`], only corners of at least this radius are triangles; smaller ones stay
/// `ui_round` squares (HTTP `?tess_min=N`). The shader's cost grows with r², the triangles'
/// (CPU building, cache cleans, the GP) with √r: the Monitor stress test's ~120 r = 8 boxes a
/// render took 3.85 ms per present as triangles, 2.0 ms as `ui_round` (passes 2.7 ms).
pub static TESS_MIN_RADIUS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(12);
/// Label text as glyph quads from the atlas ([`Atlas`]) rather than a sprite of the whole
/// label (drawn by the CPU, and again whenever the text changes). On by default; HTTP
/// `POST /api/compose?glyphs=0|1` switches it for A/B runs.
pub static GLYPHS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
/// Graphs as solid fill columns + a tessellated polyline rather than a sprite (drawn by the
/// CPU whenever a sample changes: ~5 ms a second for the tablet's perf card). On by default;
/// HTTP `POST /api/compose?graphs=0|1` switches it for A/B runs.
pub static GRAPHS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
/// Circle / ellipse / convex polygon shapes and gauges as tessellated ovals, arcs, wedges and fills (`render::round_parts`,
/// `tess::oval`/`wedge`) rather than sprites. On by default; HTTP `POST /api/compose?shapes=0|1`
/// switches it for A/B runs.
pub static SHAPES: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
/// [`ENABLED`] changed: the whole screen is repainted the new way ([`service`]).
pub static REPAINT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Why renders were composed or declined, counted since boot (HTTP `/api/status`): the
/// names of [`COUNTS`]' slots.
pub const COUNT_NAMES: [&str; 10] = [
    "composed",
    "switched_off",
    "portrait",
    "keyboard",
    "no_gpu",
    "no_shaders",
    "on_paint",
    "widget",
    "gpu_error",
    "small_areas",
];
const TAKEN: usize = 0;
const OFF: usize = 1;
const PORTRAIT: usize = 2;
const KEYBOARD: usize = 3;
const NO_GPU: usize = 4;
const NO_UI: usize = 5;
const ON_PAINT: usize = 6;
const WIDGET: usize = 7;
const GPU_ERROR: usize = 8;
const SMALL: usize = 9;
pub static COUNTS: [core::sync::atomic::AtomicU32; 10] = [const { core::sync::atomic::AtomicU32::new(0) }; 10];

/// Frames a render may take before [`MIN_FRAME_KPX`] applies (HTTP `?free_frames=`).
pub static FREE_FRAMES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(2);
/// Past [`FREE_FRAMES`] frames, the average frame must cover this many thousand pixels, or
/// the render is declined (HTTP `?min_kpx=`, 0 = never). A frame's fixed cost (~0.38 ms on the
/// board: resets, starting the units, cache cleans, the CPU setting it up) against the PPs'
/// ~11 µs per thousand pixels: the Monitor stress test's ~7.5 frames of ~9 Kpx a render took
/// 5.1 ms per present against the passes' 2.7 ms.
pub static MIN_FRAME_KPX: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(32);

/// A frame's fixed cost and the PPs' cost per thousand pixels, in µs (board, 2026-11-10).
const FRAME_FIXED_US: i32 = 380;
const FRAME_KPX_US: i32 = 11;

/// GPU frames `a` (whole tiles) is cut into: full-width strips of at most `MAX_TILES` tiles.
fn frames_of(a: &R) -> i32 {
    let across = ((a.w() + TILE - 1) / TILE).max(1);
    let lines = ((gpu::MAX_TILES as i32 / across).max(1) * TILE).min(a.h().max(TILE));
    (a.h() + lines - 1) / lines
}

/// The estimated cost of `a`'s frames, µs.
fn cost_of(a: &R) -> i32 {
    frames_of(a) * FRAME_FIXED_US + a.w() * a.h() / 1000 * FRAME_KPX_US
}

/// `areas` merged pairwise while one frame over both costs less than theirs apart
/// ([`cost_of`]), and until none overlap.
fn merge_areas(areas: &[Rect]) -> Vec<R> {
    let mut out: Vec<R> = areas.iter().map(R::of).filter(|a| !a.is_empty()).collect();
    loop {
        let mut best: Option<(usize, usize, i32)> = None;
        for i in 0..out.len() {
            for j in i + 1..out.len() {
                let (a, b) = (out[i], out[j]);
                let u = R { x0: a.x0.min(b.x0), y0: a.y0.min(b.y0), x1: a.x1.max(b.x1), y1: a.y1.max(b.y1) };
                let gain = if a.meets(&b) { i32::MAX } else { cost_of(&a) + cost_of(&b) - cost_of(&u) };
                if gain > 0 && best.is_none_or(|(_, _, g)| gain > g) {
                    best = Some((i, j, gain));
                }
            }
        }
        let Some((i, j, _)) = best else { return out };
        let b = out.swap_remove(j);
        let a = &mut out[i];
        *a = R { x0: a.x0.min(b.x0), y0: a.y0.min(b.y0), x1: a.x1.max(b.x1), y1: a.y1.max(b.y1) };
    }
}

/// Where the compositor's time goes, summed since boot (HTTP `/api/status`): the names of
/// [`TIMES`]' slots. GPU frames, their draws, pieces and pixels; the HAL's GP / PP / whole-frame
/// times (resets and L2 clear included in the last); CPU time building and running the frames,
/// and in whole `compose` calls (gathering the pieces and drawing sprites included).
/// Then: the pieces' gathering (`collect`, sprites included); sprites drawn by the CPU (cache
/// misses and this-render-only ones) and their time; the slowest such sprite's time and its
/// widget kind (`*_max`: the largest seen since boot). Then the CPU time of starting frames
/// (`Gpu::frame`) and of running them (`Frame::show`: cache cleans + the hardware time).
pub const TIME_NAMES: [&str; 16] = [
    "frames",
    "draws",
    "pieces",
    "kpx",
    "gp_us",
    "pp_us",
    "frame_hw_us",
    "frame_us",
    "compose_us",
    "collect_us",
    "sprites",
    "sprite_us",
    "sprite_max_us",
    "sprite_max_kind",
    "begin_us",
    "show_us",
];
const T_FRAMES: usize = 0;
const T_DRAWS: usize = 1;
const T_PIECES: usize = 2;
const T_KPX: usize = 3;
const T_GP: usize = 4;
const T_PP: usize = 5;
const T_HW: usize = 6;
const T_FRAME: usize = 7;
const T_COMPOSE: usize = 8;
const T_COLLECT: usize = 9;
const T_SPRITES: usize = 10;
const T_SPRITE_US: usize = 11;
const T_SPRITE_MAX: usize = 12;
const T_SPRITE_MAX_KIND: usize = 13;
const T_BEGIN: usize = 14;
const T_SHOW: usize = 15;
pub static TIMES: [core::sync::atomic::AtomicU64; 16] = [const { core::sync::atomic::AtomicU64::new(0) }; 16];

/// A sprite of a widget of `kind` took `us` to draw.
fn sprite_time(kind: u8, us: u64) {
    use core::sync::atomic::Ordering::Relaxed;
    add_time(T_SPRITES, 1);
    add_time(T_SPRITE_US, us);
    if us > TIMES[T_SPRITE_MAX].load(Relaxed) {
        TIMES[T_SPRITE_MAX].store(us, Relaxed);
        TIMES[T_SPRITE_MAX_KIND].store(kind as u64, Relaxed);
    }
}

fn add_time(slot: usize, n: u64) {
    TIMES[slot].fetch_add(n, core::sync::atomic::Ordering::Relaxed);
}

/// Microseconds on the MCT (24 MHz).
fn now_us() -> u64 {
    exynos_4412_rs::drivers::timer::Mct::new().count() / 24
}

fn count(slot: usize) -> bool {
    COUNTS[slot].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    slot == TAKEN
}

/// CPU0, once per pass (`frame_end`): repaints everything after [`ENABLED`] changed.
pub fn service(ctx: &mut Ctx<P>) {
    use core::sync::atomic::Ordering;
    if REPAINT.swap(false, Ordering::AcqRel) {
        let root = ctx.objs.root;
        ctx.objs.mark_dirty(root);
    }
}

/// Repaints `areas` (see the module's documentation); false = declined, nothing drawn.
pub fn compose(ctx: &Ctx<P>, areas: &[Rect]) -> bool {
    if !ENABLED.load(core::sync::atomic::Ordering::Relaxed) {
        return count(OFF);
    }
    if ctx.lcd.is_portrait() {
        return count(PORTRAIT);
    }
    use core::sync::atomic::Ordering::Relaxed;
    let merged = merge_areas(areas);
    let frames: i32 = merged.iter().map(frames_of).sum();
    let kpx: i32 = merged.iter().map(|a| a.w() * a.h() / 1000).sum();
    let min_kpx = MIN_FRAME_KPX.load(Relaxed) as i32;
    if frames > FREE_FRAMES.load(Relaxed) as i32 && kpx < min_kpx * frames {
        return count(SMALL);
    }
    let merged: Vec<Rect> = merged.iter().map(R::rect).collect();
    let areas = &merged[..];
    if let Some(o) = ctx.objs.overlay {
        if areas.iter().any(|a| a.intersects(&o)) {
            return count(KEYBOARD);
        }
    }
    let Some(shared) = crate::gpu::get() else { return count(NO_GPU) };
    let Some(ui) = shared.ui else { return count(NO_UI) };
    let gpu = &mut shared.gpu;
    let t0 = now_us();
    let s = state();
    if s.white == 0 {
        match white_texel() {
            Some(addr) => s.white = addr,
            None => return count(WIDGET),
        }
    }
    s.prims.clear();
    s.items.clear();
    s.scratch.clear();
    s.tverts.clear();
    s.glyphs = GLYPHS.load(core::sync::atomic::Ordering::Relaxed) && s.atlas.begin();
    s.images.render = s.images.render.wrapping_add(1);
    s.any_565 = ui.any_565_stride;
    let collected = s.collect(ctx, &ui, gpu, areas);
    add_time(T_COLLECT, now_us() - t0);
    let result = match collected {
        Err(slot) => slot,
        Ok(()) if s.draw(&ui, gpu, areas) => TAKEN,
        Ok(()) => GPU_ERROR,
    };
    // The sprites drawn for this render only go (the frames are over).
    s.scratch.clear();
    add_time(T_COMPOSE, now_us() - t0);
    count(result)
}

impl State {
    /// Every visible widget that touches `areas`, in DFS order, as pieces. Err(the reason's
    /// [`COUNTS`] slot) to decline.
    fn collect(&mut self, ctx: &Ctx<P>, ui: &Ui, gpu: &mut Gpu, areas: &[Rect]) -> Result<(), usize> {
        let objs = &ctx.objs;
        let areas: Vec<R> = areas.iter().map(R::of).collect();
        let keep_after = renderer::cache().tick();
        let n = objs.dfs_len();
        // Scroll containers whose subtree is being walked: (its end, the container). Their
        // scrollbars go over their children, once the walk leaves the subtree.
        let mut open: Vec<(usize, WidgetId)> = Vec::new();
        let mut i = 0;
        loop {
            while let Some(&(end, sc)) = open.last()
                && end <= i
            {
                open.pop();
                self.scrollbar(objs, sc, &areas);
            }
            if i >= n {
                break;
            }
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
            let w = objs.get(id);
            if w.flags & FLAG_CLIP_CHILDREN != 0 {
                open.push((objs.sub_end_at(i), id));
            }
            let r = R::of(&abs);
            if !areas.iter().any(|a| a.meets(&r)) {
                i += 1;
                continue;
            }
            if w.flags & FLAG_HAS_ON_PAINT != 0 {
                return Err(ON_PAINT);
            }
            let start = self.prims.len();
            let Some(opaque) = self.widget(ctx, ui, gpu, id, &abs, keep_after) else { return Err(WIDGET) };
            self.items.push(Item { start, opaque });
            i += 1;
        }
        Ok(())
    }

    /// Scroll container `id`'s scrollbar (`render::scrollbar_parts`: track, thumb), if it has
    /// one where `areas` are. The passes draw it after every widget; here it goes over the
    /// container's own subtree only, in painter's order, so a later widget over the list
    /// stays on top.
    fn scrollbar(&mut self, objs: &ObjPool, id: WidgetId, areas: &[R]) {
        let Some(parts) = render::scrollbar_parts(objs, id) else { return };
        let track = R::of(&parts[0].0);
        if !areas.iter().any(|a| a.meets(&track)) {
            return;
        }
        let start = self.prims.len();
        for (rect, color) in parts {
            self.solid(R::of(&rect), Lin::flat(rgb(color, 1.0)));
        }
        self.items.push(Item { start, opaque: Some(track) });
    }

    /// Widget `id`'s pieces; the rect it paints opaquely, if any. None to decline.
    fn widget(&mut self, ctx: &Ctx<P>, ui: &Ui, gpu: &mut Gpu, id: WidgetId, abs: &Rect, keep_after: u32) -> Option<Option<R>> {
        let objs = &ctx.objs;
        let w = objs.get(id);
        let r = R::of(abs);
        #[cfg(feature = "layers")]
        if let Some(s) = objs.composed_root(id) {
            // A composed layer's root: its surface (premultiplied ARGB), times the plane alpha.
            let screen = R { x0: 0, y0: 0, x1: ctx.lcd.width() as i32, y1: ctx.lcd.height() as i32 };
            return self.layer(objs, ui, s, &r, &screen).then_some(None);
        }
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
            if w.kind == KIND_GRAPH && GRAPHS.load(core::sync::atomic::Ordering::Relaxed) {
                return Some(self.graph(objs, id, abs));
            }
            if matches!(w.kind, KIND_CIRCLE | KIND_ELLIPSE | KIND_GAUGE | KIND_POLYGON)
                && SHAPES.load(core::sync::atomic::Ordering::Relaxed)
                && objs.ext(id).image_id == 0
            {
                // A shape that stays the same is one textured quad from a cached sprite, far
                // fewer vertices than its triangles (board, Clock app: 10.6 vs 6.0 ms a
                // second with the face and dots as triangles). Triangles for what the cache
                // doesn't hold: a first sighting, and gauges (no appearance key: their value
                // changes).
                if renderer::appearance_complete(w.kind) {
                    let cache = renderer::cache();
                    let key = renderer::appearance(ctx, id, abs);
                    let cached = cache.find(key).is_some()
                        || (cache.seen(key) && {
                            let t = now_us();
                            let got = cache.insert(ctx, id, abs, key, Some(keep_after));
                            sprite_time(w.kind, now_us() - t);
                            got.is_some()
                        });
                    if cached {
                        return self.sprite(ctx, id, abs, keep_after).map(|_| None);
                    }
                }
                if w.kind != KIND_POLYGON {
                    return Some(self.round(objs, id, abs));
                }
                if self.polygon(objs, id, abs) {
                    return Some(None);
                }
                // Not convex (or too thin to shrink): the CPU's sprite.
            }
            if matches!(w.kind, KIND_PROGRESS | KIND_SLIDER | KIND_CHECKBOX | KIND_RADIO | KIND_LINE)
                && SHAPES.load(core::sync::atomic::Ordering::Relaxed)
                && objs.ext(id).image_id == 0
            {
                // Their box and the few parts over it (`render::round_parts`): rects, rounded
                // rects and rings, ovals, a tick, a line's wedge.
                return Some(self.round(objs, id, abs));
            }
            if matches!(w.kind, KIND_INPUT | KIND_DROPDOWN) && self.glyphs {
                if let Some(opaque) = self.field(ctx, id, abs) {
                    return Some(opaque);
                }
            }
            if w.kind == KIND_LABEL && self.glyphs {
                if let Some(opaque) = self.label(ctx, id, abs) {
                    return Some(opaque);
                }
            }
            return self.sprite(ctx, id, abs, keep_after).map(|_| None);
        }
        let e = objs.ext(id);
        let Some(picture) = self.picture(ctx, w.kind, abs, &e) else {
            // An image the GPU can't have: the whole widget from a sprite.
            return self.sprite(ctx, id, abs, keep_after).map(|_| None);
        };
        self.boxes(objs, id, &r);
        let mut opaque = render::covers_opaquely(objs, id, abs, abs).then_some(r);
        if self.put_picture(picture, &r) && e.border_radius == 0 {
            opaque = Some(r);
        }
        Some(opaque)
    }

    /// Widget's image as `draw_widget` places it: a background image at the inside's top
    /// left, an image widget's centred in the inside. Some(None) = no image to draw; None =
    /// one the GPU can't have (no memory). Raw opaque RGB565 is sampled where it is in the
    /// flash shadow; anything else from its decoded texture ([`Images`]).
    fn picture(&mut self, ctx: &Ctx<P>, kind: u8, abs: &Rect, e: &ferrite_core::widget::WidgetExt) -> Option<Option<Picture>> {
        if e.image_id == 0 || kind == KIND_GRAPH {
            return Some(None);
        }
        let inner = R::of(&render::inner_rect(abs, &e.border));
        let Some(img) = ctx.images.find(e.image_id) else { return Some(None) };
        if inner.is_empty() {
            return Some(None);
        }
        let (pw, ph) = (img.width as i32, img.height as i32);
        let (x, y) = if kind == KIND_IMAGE {
            let x = if inner.w() > pw { inner.x0 + (inner.w() - pw) / 2 } else { inner.x0 };
            let y = if inner.h() > ph { inner.y0 + (inner.h() - ph) / 2 } else { inner.y0 };
            (x, y)
        } else {
            (inner.x0, inner.y0)
        };
        let at = R { x0: x, y0: y, x1: x + pw, y1: y + ph };
        if img.is_raw_opaque()
            && let Some(addr) = renderer::picture_addr(img.data_addr(), (pw * ph * 2) as usize)
        {
            // From the 64-byte boundary before it: `shift` texels to the left.
            let shift = (addr % 64) / 2;
            let stride = pw as usize * 2;
            if addr % 2 == 0 && (self.any_565 || stride % 64 == 0) && pw as usize + shift <= 4096 {
                let tex = Tex { addr: addr - addr % 64, w: pw as usize + shift, h: ph as usize, stride, fmt: Fmt::Rgb565 };
                return Some(Some(Picture { at, tex, shift, opaque: true }));
            }
        }
        let tex = self.images.tex(ctx, img)?;
        Some(Some(Picture { at, tex, shift: 0, opaque: false }))
    }

    /// `p`, cut to the widget's box `r` (as the scissor). True if it covers the box opaquely.
    fn put_picture(&mut self, p: Option<Picture>, r: &R) -> bool {
        let Some(p) = p else { return false };
        let shown = p.at.meet(r);
        if shown.is_empty() {
            return false;
        }
        self.flat(shown, p.at.x0, p.at.y0, p.tex, p.shift);
        p.opaque && shown == *r
    }

    /// Composed layer `s` (root box `r`): one quad sampling the part of its surface on the
    /// screen. The texture starts at that part (from the 64-byte boundary before it), so it
    /// is at most a screen wide: the fragment shader's texel coordinates are exact across
    /// 1280 texels (`gpu.rs` self-test), not across a 2560-wide pager. False to decline.
    #[cfg(feature = "layers")]
    fn layer(&mut self, objs: &ObjPool, ui: &Ui, s: usize, r: &R, screen: &R) -> bool {
        let slot = &objs.layers.slots[s];
        let shown = r.meet(screen);
        if shown.is_empty() || slot.alpha == 0 {
            return true;
        }
        let (sx, sy) = ((shown.x0 - r.x0) as usize, (shown.y0 - r.y0) as usize);
        let (w, h) = (shown.w() as usize, shown.h() as usize);
        let Some((base, lw, lh)) = lcd::layer_texture(s, sx, sy, w, h) else { return false };
        let stride = lw * 4;
        if sx + w > lw || sy + h > lh || (!ui.any_565_stride && stride % 64 != 0) {
            return false;
        }
        let addr = base + (sy * lw + sx) * 4;
        let shift = (addr % 64) / 4;
        let tex = Tex { addr: addr - addr % 64, w: w + shift, h, stride, fmt: Fmt::Argb };
        let (tw, th) = (tex.w as f32, tex.h as f32);
        let u = ((shift as f32 - shown.x0 as f32) / tw, 1.0 / tw);
        let v = (-(shown.y0 as f32) / th, 1.0 / th);
        let a = slot.alpha as f32 / 255.0;
        self.prims.push(Prim::Flat { rect: shown, u, v, color: Lin::flat([a; 4]), tex });
        true
    }

    /// A label without a picture: its box, then a quad per glyph from the atlas, laid out as
    /// `render::label_lines` lays the text out, cut to the box (as the drawing's scissor), in
    /// the text colour times the glyph's coverage. Its opaque rect; None (nothing added) if a
    /// glyph doesn't fit in the atlas.
    fn label(&mut self, ctx: &Ctx<P>, id: WidgetId, abs: &Rect) -> Option<Option<R>> {
        let objs = &ctx.objs;
        let w = objs.get(id);
        let e = objs.ext(id);
        let r = R::of(abs);
        // A background image goes between the box and the text (`draw_widget`).
        let picture = self.picture(ctx, w.kind, abs, &e)?;
        let (prims, tverts) = (self.prims.len(), self.tverts.len());
        self.boxes(objs, id, &r);
        let mut opaque = render::covers_opaquely(objs, id, abs, abs).then_some(r);
        if self.put_picture(picture, &r) && e.border_radius == 0 {
            opaque = Some(r);
        }
        let color = Lin::flat(rgb(e.text_color, 1.0));
        let tex = self.atlas.tex();
        let a = ATLAS as f32;
        let mut ok = true;
        render::label_lines(ctx, w, abs, &e, |font, line, x, y| {
            font.for_each_glyph(line, x, y, |cp, g, gx, gy| {
                let (gx, gy) = (gx as i32, gy as i32);
                let at = R { x0: gx, y0: gy, x1: gx + g.width as i32, y1: gy + g.height as i32 };
                let shown = at.meet(&r);
                if !ok || shown.is_empty() {
                    return;
                }
                let Some((sx, sy)) = self.atlas.slot(&ctx.flash, font, cp, g, &mut self.cov) else {
                    ok = false;
                    return;
                };
                // Screen x shows texel sx + (x - gx), y likewise.
                let u = ((sx as i32 - gx) as f32 / a, 1.0 / a);
                let v = ((sy as i32 - gy) as f32 / a, 1.0 / a);
                self.prims.push(Prim::Flat { rect: shown, u, v, color, tex });
            });
        });
        if !ok {
            self.prims.truncate(prims);
            self.tverts.truncate(tverts);
            return None;
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
            if TESS.load(core::sync::atomic::Ordering::Relaxed)
                && e.border_radius as u32 >= TESS_MIN_RADIUS.load(core::sync::atomic::Ordering::Relaxed)
            {
                // As `draw_widget`: the fill (inset half the border, its radius as much
                // smaller), then the border ring over it.
                // From the mesh cache: the same box at the origin, moved into place.
                if draw_bg {
                    let rad = (e.border_radius as i32 - inset).max(0);
                    let (w, h) = (g.w(), g.h());
                    let key = RoundPart::RoundRect { x: 0, y: 0, w: w as u16, h: h as u16, r: rad as u16, t: 0, color: 0 };
                    self.tess_cached_at(key, (g.x0, g.y0), fill, None, |m| tess::rounded_fill(m, 0, 0, w, h, rad));
                }
                if bw > 0 {
                    let rad = e.border_radius as i32;
                    let (w, h) = (r.w(), r.h());
                    let key = RoundPart::RoundRect { x: 0, y: 0, w: w as u16, h: h as u16, r: rad as u16, t: bw as u16, color: 0 };
                    self.tess_cached_at(key, (r.x0, r.y0), Lin::flat(border), None, |m| tess::rounded_ring(m, 0, 0, w, h, rad, bw));
                }
                return;
            }
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

    /// `fill` over `rect`: the white texel times it (`ui_flat`, two instructions a pixel,
    /// against `ui_round`'s dozen).
    fn solid(&mut self, rect: R, fill: Lin) {
        if !rect.is_empty() {
            self.prims.push(solid_prim(rect, fill, self.white));
        }
    }

    /// A shape from the tessellator (`f` adds it), in `color`: one `Prim::Tris` a chunk, so
    /// a frame takes only the chunks that meet it.
    fn tess(&mut self, color: Lin, f: impl FnOnce(&mut tess::Mesh<'_>)) {
        self.tess_in(color, None, f);
    }

    /// [`tess`](Self::tess), cut to `clip` (as the drawing's scissor would): a chunk reaching
    /// past it is replaced by its clipped triangles (`tess::clip`).
    fn tess_in(&mut self, color: Lin, clip: Option<R>, f: impl FnOnce(&mut tess::Mesh<'_>)) {
        self.tchunks.clear();
        f(&mut tess::Mesh { verts: &mut self.tverts, chunks: &mut self.tchunks });
        self.emit(color, clip);
    }

    /// A round part's triangles (`tess::oval`/`wedge`, `f` adds them), cut to `clip`, from
    /// [`MeshCache`] when the same part was tessellated before (a clock face and its dots stay
    /// put while the hands move: their trigonometry was most of the render's CPU time).
    fn tess_cached(&mut self, part: RoundPart, color: Lin, clip: R, f: impl FnOnce(&mut tess::Mesh<'_>)) {
        self.tess_cached_at(part, (0, 0), color, Some(clip), f);
    }

    /// [`tess_cached`](Self::tess_cached) of a shape `f` adds at the origin, keyed by `part`
    /// (the shape at the origin), moved by `at` when copied: the same box anywhere (a card
    /// sliding with its page) hits the cache.
    fn tess_cached_at(&mut self, part: RoundPart, at: (i32, i32), color: Lin, clip: Option<R>, f: impl FnOnce(&mut tess::Mesh<'_>)) {
        let found = match self.meshes.find(&part) {
            Some(i) => i,
            None => {
                let (mut verts, mut chunks) = (Vec::new(), Vec::new());
                f(&mut tess::Mesh { verts: &mut verts, chunks: &mut chunks });
                self.meshes.insert(part, verts, chunks)
            }
        };
        let e = &self.meshes.entries[found];
        let base = self.tverts.len();
        let (dx, dy) = at;
        if (dx, dy) == (0, 0) {
            self.tverts.extend_from_slice(&e.verts);
        } else {
            let (fx, fy) = (dx as f32, dy as f32);
            self.tverts.extend(e.verts.iter().map(|v| tess::Vtx { x: v.x + fx, y: v.y + fy, a: v.a }));
        }
        self.tchunks.clear();
        self.tchunks.extend(e.chunks.iter().map(|c| tess::Chunk {
            start: c.start + base,
            len: c.len,
            x0: c.x0 + dx,
            y0: c.y0 + dy,
            x1: c.x1 + dx,
            y1: c.y1 + dy,
        }));
        self.emit(color, clip);
    }

    /// `self.tchunks` (triangles in `self.tverts`) as `Prim::Tris`, cut to `clip`.
    fn emit(&mut self, color: Lin, clip: Option<R>) {
        for i in 0..self.tchunks.len() {
            let c = self.tchunks[i];
            let mut bbox = R { x0: c.x0, y0: c.y0, x1: c.x1, y1: c.y1 };
            let (mut start, mut len) = (c.start, c.len);
            if let Some(k) = clip {
                if !k.contains(&bbox) {
                    if !k.meets(&bbox) {
                        continue;
                    }
                    // The cut triangles go after everything; they refer only to themselves.
                    let at = self.tverts.len();
                    let mut cut = core::mem::take(&mut self.cov_tris);
                    cut.clear();
                    tess::clip(&self.tverts[start..start + len], k.x0 as f32, k.y0 as f32, k.x1 as f32, k.y1 as f32, &mut cut);
                    self.tverts.extend_from_slice(&cut);
                    self.cov_tris = cut;
                    (start, len) = (at, self.tverts.len() - at);
                    bbox = bbox.meet(&k);
                    if len == 0 {
                        continue;
                    }
                }
            }
            self.prims.push(Prim::Tris { bbox, start: start as u32, len: len as u32, color, white: self.white });
        }
    }

    /// A graph (`render::draw_graph`): its box, the fill under the curve as solid columns
    /// (`fill_under_segment`'s, runs of equal ones merged), then the curve as a 1 px
    /// anti-aliased polyline (`tess::polyline`), both cut to the box. The CPU interleaves fill
    /// and line per segment; here the line goes over all of the fill. Its opaque rect.
    fn graph(&mut self, objs: &ObjPool, id: WidgetId, abs: &Rect) -> Option<R> {
        let e = objs.ext(id);
        let r = R::of(abs);
        self.boxes(objs, id, &r);
        let opaque = render::covers_opaquely(objs, id, abs, abs).then_some(r);
        let inner = render::inner_rect(abs, &e.border);
        let style = render::graph_style(&inner, &e);
        let mut pts = core::mem::take(&mut self.gpts);
        pts.clear();
        render::graph_points(objs, &inner, &e, |x, y| pts.push((x as i32, y as i32)));
        if let Some(fill) = style.fill {
            let fill = Lin::flat(rgb(fill, 1.0));
            let base = style.baseline as i32;
            // The column at x from `fill_under_segment`: rows top..bottom.
            let mut run: Option<(i32, i32, i32, i32)> = None; // x0, x1, top, bottom
            let mut column = |this: &mut Self, x: i32, y: i32| {
                let (top, bot) = (y.min(base), y.max(base));
                if bot <= top {
                    return;
                }
                match run {
                    Some((x0, x1, t, b)) if x1 == x && t == top && b == bot => run = Some((x0, x + 1, t, b)),
                    _ => {
                        if let Some((x0, x1, t, b)) = run {
                            this.solid(R { x0, y0: t, x1, y1: b }.meet(&r), fill);
                        }
                        run = Some((x, x + 1, top, bot));
                    }
                }
            };
            for w in pts.windows(2) {
                let ((x0, y0), (x1, y1)) = (w[0], w[1]);
                if x1 == x0 {
                    column(self, x0, y0);
                    continue;
                }
                let (dx, dy) = (x1 - x0, y1 - y0);
                let step = if dx > 0 { 1 } else { -1 };
                let mut x = x0;
                while x != x1 {
                    column(self, x, y0 + dy * (x - x0) / dx);
                    x += step;
                }
            }
            if let Some((x0, x1, t, b)) = run {
                self.solid(R { x0, y0: t, x1, y1: b }.meet(&r), fill);
            }
        }
        self.tess_in(Lin::flat(rgb(style.line, 1.0)), Some(r), |m| tess::polyline(m, &pts));
        self.gpts = pts;
        opaque
    }

    /// A circle / ellipse shape or a gauge: a gauge's box (`draw_widget` paints it first), then
    /// the anti-aliased parts its drawing issues (`render::round_parts`) as tessellated ovals,
    /// arcs and wedges, cut to the box. Each part blends over the ones before, as on the CPU.
    /// Its opaque rect.
    fn round(&mut self, objs: &ObjPool, id: WidgetId, abs: &Rect) -> Option<R> {
        let w = objs.get(id);
        let e = objs.ext(id);
        let r = R::of(abs);
        let mut opaque = None;
        if !matches!(w.kind, KIND_CIRCLE | KIND_ELLIPSE | KIND_LINE) {
            self.boxes(objs, id, &r);
            opaque = render::covers_opaquely(objs, id, abs, abs).then_some(r);
        }
        // A line is cut by the scissor only (its round caps reach past its box), the rest by
        // their box.
        let clip = if w.kind == KIND_LINE { R { x0: -0x4000, y0: -0x4000, x1: 0x4000, y1: 0x4000 } } else { r };
        let r = clip;
        let mut parts = [None; 10];
        let mut n = 0;
        render::round_parts(w, abs, &e, |p| {
            if n < parts.len() {
                parts[n] = Some(p);
                n += 1;
            }
        });
        for &p in parts.iter().flatten() {
            self.part(p, r);
        }
        opaque
    }

    /// An input or a dropdown: its box, then what `render::field_contents` lists, cut to the
    /// box: text as atlas glyph quads (each over a solid cell of the background when the CPU
    /// paints glyph cells opaquely), the cursor bar and the chevron as parts. Its opaque rect;
    /// None (nothing added) if a glyph doesn't fit in the atlas.
    fn field(&mut self, ctx: &Ctx<P>, id: WidgetId, abs: &Rect) -> Option<Option<R>> {
        let objs = &ctx.objs;
        let w = objs.get(id);
        let e = objs.ext(id);
        let r = R::of(abs);
        // A background image goes between the box and the text (`draw_widget`).
        let picture = self.picture(ctx, w.kind, abs, &e)?;
        let (prims, tverts) = (self.prims.len(), self.tverts.len());
        self.boxes(objs, id, &r);
        let mut opaque = render::covers_opaquely(objs, id, abs, abs).then_some(r);
        if self.put_picture(picture, &r) && e.border_radius == 0 {
            opaque = Some(r);
        }
        let color = Lin::flat(rgb(e.text_color, 1.0));
        let tex = self.atlas.tex();
        let a = ATLAS as f32;
        let mut ok = true;
        render::field_contents(ctx, w, abs, &e, |item| match item {
            render::FieldItem::Text { font, line, x, y, bg } => {
                font.for_each_glyph(line, x, y, |cp, g, gx, gy| {
                    let (gx, gy) = (gx as i32, gy as i32);
                    let at = R { x0: gx, y0: gy, x1: gx + g.width as i32, y1: gy + g.height as i32 };
                    let shown = at.meet(&r);
                    if !ok || shown.is_empty() {
                        return;
                    }
                    let Some((sx, sy)) = self.atlas.slot(&ctx.flash, font, cp, g, &mut self.cov) else {
                        ok = false;
                        return;
                    };
                    if let Some(bg) = bg {
                        self.solid(shown, Lin::flat(rgb(bg, 1.0)));
                    }
                    let u = ((sx as i32 - gx) as f32 / a, 1.0 / a);
                    let v = ((sy as i32 - gy) as f32 / a, 1.0 / a);
                    self.prims.push(Prim::Flat { rect: shown, u, v, color, tex });
                });
            }
            render::FieldItem::Part(p) => {
                if ok {
                    self.part(p, r);
                }
            }
        });
        if !ok {
            self.prims.truncate(prims);
            self.tverts.truncate(tverts);
            return None;
        }
        Some(opaque)
    }

    /// One part (`render::RoundPart`) as triangles or a solid, cut to `r`.
    fn part(&mut self, p: RoundPart, r: R) {
        match p {
            RoundPart::Oval { x, y, w, h, t, color } => {
                let (a, b) = (w as f32 / 2.0, h as f32 / 2.0);
                let (cx, cy) = (x as f32 + a, y as f32 + b);
                // `smooth_oval`'s ring has a hole while its inner radii are over ½.
                let t = t as f32;
                let hole = (t > 0.0 && a - t > 0.5 && b - t > 0.5).then_some((a - t, b - t));
                self.tess_cached(p, Lin::flat(rgb(color, 1.0)), r, |m| tess::oval(m, cx, cy, a, b, hole, None));
            }
            RoundPart::Arc { cx, cy, r: rad, ir, start, end, color } => {
                let (fx, fy) = (cx as f32 + 0.5, cy as f32 + 0.5);
                let a = rad as f32 + 0.5;
                let hole = (ir > 1).then(|| (ir as f32 - 0.5, ir as f32 - 0.5));
                let arc = Some((start as f32, end as f32));
                self.tess_cached(p, Lin::flat(rgb(color, 1.0)), r, |m| tess::oval(m, fx, fy, a, a, hole, arc));
            }
            RoundPart::Wedge { ax, ay, bx, by, ra_q8, rb_q8, color } => {
                let (ra, rb) = (ra_q8.clamp(0, 150 << 8) as f32 / 256.0, rb_q8.clamp(0, 150 << 8) as f32 / 256.0);
                let (ax, ay, bx, by) = (ax as f32 + 0.5, ay as f32 + 0.5, bx as f32 + 0.5, by as f32 + 0.5);
                self.tess_cached(p, Lin::flat(rgb(color, 1.0)), r, |m| tess::wedge(m, ax, ay, bx, by, ra, rb));
            }
            RoundPart::Rect { x, y, w, h, color } => {
                let rect = R { x0: x as i32, y0: y as i32, x1: x as i32 + w as i32, y1: y as i32 + h as i32 };
                self.solid(rect.meet(&r), Lin::flat(rgb(color, 1.0)));
            }
            RoundPart::RoundRect { x, y, w, h, r: rad, t, color } => {
                let (x, y, w, h, rad, t) = (x as i32, y as i32, w as i32, h as i32, rad as i32, t as i32);
                let color = Lin::flat(rgb(color, 1.0));
                if t == 0 {
                    self.tess_cached(p, color, r, |m| tess::rounded_fill(m, x, y, w, h, rad));
                } else {
                    self.tess_cached(p, color, r, |m| tess::rounded_ring(m, x, y, w, h, rad, t));
                }
            }
            RoundPart::Path { pts, n, t, color } => {
                let mut f = [(0.0f32, 0.0f32); 3];
                for (q, &(px, py)) in f.iter_mut().zip(&pts[..n as usize]) {
                    *q = (px as f32 + 0.5, py as f32 + 0.5);
                }
                let rad = t as f32 / 2.0;
                self.tess_cached(p, Lin::flat(rgb(color, 1.0)), r, |m| tess::outline(m, &f[..n as usize], rad, false));
            }
        }
    }

    /// A polygon shape (`render::polygon_shape`): its fill as a convex polygon with mitered
    /// edge ramps (`tess::convex_fill`), then its outline (`tess::outline`), cut to the box.
    /// False (nothing added) when the fill isn't convex or can't be shrunk.
    fn polygon(&mut self, objs: &ObjPool, id: WidgetId, abs: &Rect) -> bool {
        let w = objs.get(id);
        let e = objs.ext(id);
        let r = R::of(abs);
        let mut pts = [ferrite_core::types::Offset { x: 0, y: 0 }; ferrite_core::lcd::MAX_POLY_POINTS];
        let Some((n, fill, stroke, t)) = render::polygon_shape(objs, w, abs, &e, &mut pts) else { return true };
        let mut f = [(0.0f32, 0.0f32); ferrite_core::lcd::MAX_POLY_POINTS];
        for (q, p) in f.iter_mut().zip(&pts[..n]) {
            *q = (p.x as f32 + 0.5, p.y as f32 + 0.5);
        }
        let f = &f[..n];
        let (prims, tverts) = (self.prims.len(), self.tverts.len());
        if fill != 0 {
            self.tchunks.clear();
            if !tess::convex_fill(&mut tess::Mesh { verts: &mut self.tverts, chunks: &mut self.tchunks }, f) {
                self.tverts.truncate(tverts);
                self.prims.truncate(prims);
                return false;
            }
            self.emit(Lin::flat(rgb(fill, 1.0)), Some(r));
        }
        if stroke != 0 {
            let rad = t as f32 / 2.0;
            self.tess_in(Lin::flat(rgb(stroke, 1.0)), Some(r), |m| tess::outline(m, f, rad, true));
        }
        true
    }

    /// A rounded box: nine pieces, the corners `k` square (the radius, or the border if wider,
    /// at most half the box), each knowing where it is from its corner circle's centre. Only
    /// the corners need `ui_round`; the straight pieces' edges lie on pixel boundaries, so they
    /// are solids (border band + fill), which `ui_round` would draw the same, at a fraction of
    /// the shader cost. Solids first, then the corners: no piece overlaps another.
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
        let band = Lin::flat(border);
        let bw = bw.max(0);
        // Middle column (top, middle, bottom pieces) and middle row (left, right pieces).
        let (mx0, mx1) = (cols[1], cols[2]);
        let (my0, my1) = (rows[1], rows[2]);
        let top_band = (r.y0 + bw).min(my0);
        let bottom_band = (r.y1 - bw).max(my1);
        let left_band = (r.x0 + bw).min(mx0);
        let right_band = (r.x1 - bw).max(mx1);
        // Top piece: border band, then fill; bottom likewise upwards.
        self.solid(R { x0: mx0, y0: r.y0, x1: mx1, y1: top_band }, band);
        self.solid(R { x0: mx0, y0: top_band, x1: mx1, y1: my0 }, fill);
        self.solid(R { x0: mx0, y0: bottom_band, x1: mx1, y1: r.y1 }, band);
        self.solid(R { x0: mx0, y0: my1, x1: mx1, y1: bottom_band }, fill);
        // Left and right pieces, and the middle.
        self.solid(R { x0: r.x0, y0: my0, x1: left_band, y1: my1 }, band);
        self.solid(R { x0: left_band, y0: my0, x1: mx0, y1: my1 }, fill);
        self.solid(R { x0: right_band, y0: my0, x1: r.x1, y1: my1 }, band);
        self.solid(R { x0: mx1, y0: my0, x1: right_band, y1: my1 }, fill);
        self.solid(R { x0: mx0, y0: my0, x1: mx1, y1: my1 }, fill);
        for row in [0, 2] {
            for col in [0, 2] {
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
        self.prims.push(Prim::Flat { rect, u, v, color: Lin::flat([1.0; 4]), tex });
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
                None if cache.seen(key) => {
                    let t = now_us();
                    let at = cache.insert(ctx, id, abs, key, Some(keep_after));
                    sprite_time(kind, now_us() - t);
                    at
                }
                None => None,
            };
        }
        let (px, w, h, stride) = match found {
            Some(at) => {
                let s = cache.entry(at);
                (s.pixels() as usize, s.w, s.h, s.stride)
            }
            None => {
                let t = now_us();
                let s = Sprite::draw(ctx, id, abs, 0)?;
                sprite_time(kind, now_us() - t);
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
        let started = now_us();
        let first = self.items.iter().rposition(|it| it.opaque.is_some_and(|o| o.contains(c))).unwrap_or(0);
        let from = self.items.get(first).map_or(self.prims.len(), |it| it.start);
        let mut frame = gpu.frame(Area::new(c.x0 as usize, c.y0 as usize, c.w() as usize, c.h() as usize), 0xFF00_0000);
        add_time(T_BEGIN, now_us() - started);
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
            add_time(T_PIECES, 1);
        }
        if let Some(done) = batch {
            self.flush(ui, &mut frame, done, scale)?;
            drew = true;
        }
        if !drew {
            // Nothing there: black, as an empty frame would leave the area as it was.
            let black = solid_prim(*c, Lin::flat([0.0, 0.0, 0.0, 1.0]), self.white);
            self.push(&black, c, c);
            self.flush(ui, &mut frame, black.batch(), scale)?;
        }
        let showing = now_us();
        let shown = frame.show();
        add_time(T_SHOW, now_us() - showing);
        add_time(T_FRAMES, 1);
        add_time(T_KPX, (c.w() * c.h()) as u64 / 1000);
        if let Ok(t) = &shown {
            add_time(T_GP, t.gp_us);
            add_time(T_PP, t.pps_us);
            add_time(T_HW, t.total_us);
        }
        add_time(T_FRAME, now_us() - started);
        #[cfg(feature = "perf")]
        ferrite_core::perf::add_sub(ferrite_core::perf::phase::G2D, crate::perf::cycles().wrapping_sub(t0));
        shown.map(|_| ())
    }

    /// Two triangles of `p` over `rect` (inside the frame `c`) into the batch's vertices; a
    /// `Prim::Tris` whole (the GPU clips it to the frame).
    fn push(&mut self, p: &Prim, rect: &R, c: &R) {
        if let Prim::Tris { start, len, color, .. } = *p {
            let (start, len) = (start as usize, len as usize);
            let (ox, oy) = (c.x0 as f32, c.y0 as f32);
            for v in &self.tverts[start..start + len] {
                self.pos.push([v.x - ox, v.y - oy]);
                self.uv.push([0.5, 0.5]);
                let k = color.at(v.x, v.y);
                self.color.push([k[0] * v.a, k[1] * v.a, k[2] * v.a, k[3] * v.a]);
            }
            return;
        }
        let corners = [(rect.x0, rect.y0), (rect.x1, rect.y0), (rect.x0, rect.y1), (rect.x1, rect.y0), (rect.x1, rect.y1), (rect.x0, rect.y1)];
        for (x, y) in corners {
            let (fx, fy) = (x as f32, y as f32);
            self.pos.push([(x - c.x0) as f32, (y - c.y0) as f32]);
            match *p {
                Prim::Round { qx, qy, radius, bw, fill, border, .. } => {
                    self.shape.push([qx.0 + qx.1 * fx, qy.0 + qy.1 * fy, radius, bw]);
                    self.arc.push(arc(radius, bw));
                    self.fill.push(fill.at(fx, fy));
                    self.border.push(border);
                }
                Prim::Flat { u, v, color, .. } => {
                    self.uv.push([u.0 + u.1 * fx, v.0 + v.1 * fy]);
                    self.color.push(color.at(fx, fy));
                }
                Prim::Tris { .. } => unreachable!("pushed above"),
            }
        }
    }

    /// The batch's vertices as one mesh of the frame, and its draw.
    fn flush(&mut self, ui: &Ui, frame: &mut Frame<'_>, batch: Batch, scale: [f32; 2]) -> Result<(), gpu::Error> {
        add_time(T_DRAWS, 1);
        let result = match batch {
            Batch::Round => frame
                .mesh(&ui.round)
                .attribute("pos", &self.pos)
                .attribute("shape", &self.shape)
                .attribute("arc", &self.arc)
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
        self.arc.clear();
        self.fill.clear();
        self.border.clear();
        self.uv.clear();
        self.color.clear();
        result
    }
}

/// The `arc` attribute of `ui_round` for a corner of `radius` with a border `bw` wide:
/// 1/r, r/2, (r/ri)^2, ri/2 with ri = r - bw; zeros where that circle is under a pixel (the
/// corner, or the border's inner corner, is then square).
pub fn arc(radius: f32, bw: f32) -> [f32; 4] {
    if radius < 1.0 {
        return [0.0; 4];
    }
    let ri = radius - bw;
    if ri < 1.0 {
        return [1.0 / radius, radius / 2.0, 0.0, 0.0];
    }
    let k = radius / ri;
    [1.0 / radius, radius / 2.0, k * k, ri / 2.0]
}

/// `fill` over `rect` as the white texel at `white` times it.
fn solid_prim(rect: R, fill: Lin, white: usize) -> Prim {
    let tex = Tex { addr: white, w: 1, h: 1, stride: 64, fmt: Fmt::Argb };
    Prim::Flat { rect, u: (0.5, 0.0), v: (0.5, 0.0), color: fill, tex }
}

/// One opaque white texel for [`solid_prim`]: a 64-byte heap block, kept for good, cleaned out
/// of the caches for the GPU. None if the heap has no room.
fn white_texel() -> Option<usize> {
    let layout = core::alloc::Layout::from_size_align(64, 64).ok()?;
    // SAFETY: a non-zero size; the block is never freed.
    let p = unsafe { alloc::alloc::alloc(layout) } as *mut u32;
    if p.is_null() {
        return None;
    }
    // SAFETY: 16 words in the block.
    unsafe { core::slice::from_raw_parts_mut(p, 16).fill(0xFFFF_FFFF) };
    exynos_4412_rs::mmu::clean_dcache_range(p as usize, 64);
    Some(p as usize)
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
