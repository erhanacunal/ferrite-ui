//! How this board draws widgets (`Platform::Renderer`).
//!
//! The render pipeline (DFS order, clip regions, restores) stays in
//! ferrite-core; [`ExynosRenderer::draw`] is asked for each widget before
//! the built-in drawing and returns `false` for what it leaves to it. See
//! `ferrite_core::renderer` for the contract an override keeps.
//!
//! **Sprites (`g2d_render`).** A large widget that lets what is behind it
//! show through (translucent, rounded, a freeform shape, an image without a
//! background) costs the CPU a read-modify-write per pixel on every redraw,
//! and with the uncached shadow those reads go to DRAM. Such a widget is
//! drawn once, the built-in way, into a premultiplied ARGB8888 sprite
//! (`lcd::begin_sprite`: the same primitives, composited onto transparent),
//! and from then on the G2D blends the sprite over the screen, once per
//! clip rect (`lcd::blit_sprite`). The tablet's wallpaper circles, restored
//! behind every sliding page, are the case this is for.
//!
//! A sprite is made the second time an appearance is drawn, in a later frame ([`Cache::seen`]):
//! a widget whose size or colour animates gets a new appearance every frame,
//! and drawing each into a fresh sprite (draw, cache clean, allocation) cost
//! far more than drawing it directly.
//!
//! Sprites are kept by what they show ([`appearance`]), not by widget: a
//! widget that moves keeps its sprite, two equal widgets share one, and a
//! program switch needs no invalidation. The least recently used go first
//! when the cache is over [`CACHE_BYTES`] or [`CACHE_ENTRIES`].
//!
//! **Pictures (`g2d_render`).** A widget that is only an opaque raw picture
//! of exactly its size (a full-screen wallpaper as the root's background
//! image) is copied by the G2D from the flash shadow, RGB565 to XRGB8888,
//! per clip rect ([`draw_picture`]): restoring behind a sliding page is then
//! one engine copy instead of a CPU loop reading the picture 128 bytes at a
//! time. The background colour under it is skipped (the picture covers it).
//!
//! The G2D's blend rounds a little differently from the CPU's (at most 2 per
//! channel, `g2d_test`), so sprite-drawn widgets are not bit-identical to the
//! built-in drawing.

use alloc::vec::Vec;

use ferrite_core::clip::ClipRegion;
use ferrite_core::ctx::Ctx;
use ferrite_core::pool::ObjPool;
use ferrite_core::render;
use ferrite_core::renderer::WidgetRenderer;
use ferrite_core::types::{Edges, Rect};
use ferrite_core::widget::{
    FLAG_CHECKED, FLAG_PRESSED, KIND_BASE, KIND_BUTTON, KIND_BUTTON_GROUP, KIND_CIRCLE, KIND_ELLIPSE, KIND_IMAGE,
    KIND_LINE, KIND_POLYGON, KIND_RECTANGLE, WidgetId,
};

use crate::lcd;
use crate::platform::Exynos4412Platform as P;

/// Smaller widgets are drawn by the CPU: a G2D operation costs a setup and
/// a wait, and a sprite its memory.
const SPRITE_MIN_PIXELS: usize = 64 * 64;
/// Memory the sprites may hold (4 bytes per pixel).
const CACHE_BYTES: usize = 48 << 20;
/// Sprites kept at most.
const CACHE_ENTRIES: usize = 96;
/// Appearances remembered as drawn once (no sprite yet).
const SEEN: usize = 32;
/// Sprite rows are padded to this many pixels and start on a 64-byte
/// boundary: what the G2D tests used, whole cache lines for the clean.
const ROW_ALIGN: usize = 16;

pub struct ExynosRenderer;

impl WidgetRenderer<P> for ExynosRenderer {
    fn draw(ctx: &Ctx<P>, id: WidgetId, abs: &Rect, clip: &ClipRegion) -> bool {
        if !lcd::g2d_draw_ready() || ctx.lcd.is_portrait() {
            return false;
        }
        if crate::model3d::draw(ctx, id, abs, clip) {
            return true;
        }
        if draw_picture(ctx, id, abs, clip) {
            return true;
        }
        if !wants_sprite(&ctx.objs, id, abs) {
            return false;
        }
        let key = appearance(ctx, id, abs);
        let cache = cache();
        let at = match cache.find(key) {
            Some(at) => at,
            // Drawn once before: worth a sprite now.
            None if cache.seen(key) => match cache.insert(ctx, id, abs, key) {
                Some(at) => at,
                None => return false,
            },
            None => {
                // First sighting: drawn by the CPU this time.
                #[cfg(feature = "perf")]
                ferrite_core::perf::add_count(ferrite_core::perf::count::SPRITE_FIRST, 1);
                return false;
            }
        };
        let sprite = &cache.entries[at];
        let screen = Rect::new(0, 0, ctx.lcd.width(), ctx.lcd.height());
        for cr in clip.iter() {
            // Clip rects lie inside the widget's box; the screen cuts a
            // widget partly off it (a sliding page).
            let Some(r) = cr.intersection(abs).and_then(|r| r.intersection(&screen)) else { continue };
            let (sx, sy) = ((r.x - abs.x) as usize, (r.y - abs.y) as usize);
            let (dx, dy) = (r.x as usize, r.y as usize);
            let px = sprite.pixels();
            #[cfg(feature = "perf")]
            ferrite_core::perf::add_count(ferrite_core::perf::count::SPRITE_BLITS, 1);
            if !lcd::blit_sprite(px, sprite.w, sprite.h, sprite.stride, sx, sy, dx, dy, r.w as usize, r.h as usize) {
                // The G2D gave up (it stays off): this rect by the CPU.
                render::draw_default(ctx, id, abs, &ClipRegion::from_rect(r));
            }
        }
        true
    }
}

/// Draws widget `id` if it is only an opaque picture of exactly its size: a
/// base or image widget, no border, square, opaque, whose (background)
/// image is raw RGB565 without alpha and `abs`'s size. Everything else such
/// a widget draws (its background) is under the picture. False = not such a
/// widget, nothing drawn.
fn draw_picture(ctx: &Ctx<P>, id: WidgetId, abs: &Rect, clip: &ClipRegion) -> bool {
    let w = ctx.objs.get(id);
    if !matches!(w.kind, KIND_BASE | KIND_IMAGE) {
        return false;
    }
    let e = ctx.objs.ext(id);
    if e.image_id == 0 || e.border != Edges::ZERO || e.border_radius != 0 || e.alpha != 255 {
        return false;
    }
    let Some(img) = ctx.images.find(e.image_id) else { return false };
    if !img.is_raw_opaque() || img.width != abs.w || img.height != abs.h {
        return false;
    }
    let (pw, ph) = (img.width as usize, img.height as usize);
    let Some(addr) = picture_addr(img.data_addr(), pw * ph * 2) else { return false };
    let screen = Rect::new(0, 0, ctx.lcd.width(), ctx.lcd.height());
    for cr in clip.iter() {
        let Some(r) = cr.intersection(abs).and_then(|r| r.intersection(&screen)) else { continue };
        let (sx, sy) = ((r.x - abs.x) as usize, (r.y - abs.y) as usize);
        let (dx, dy) = (r.x as usize, r.y as usize);
        if !lcd::copy_rgb565(addr, pw, ph, sx, sy, dx, dy, r.w as usize, r.h as usize) {
            // The G2D gave up or expands 565 differently: this rect by the CPU.
            render::draw_default(ctx, id, abs, &ClipRegion::from_rect(r));
        }
    }
    true
}

/// Pictures whose pixels were cleaned from the data cache, with the flash
/// write generation at the time (`flash::generation`).
const CLEANED: usize = 8;
static mut CLEANED_AT: [Option<(u32, u32)>; CLEANED] = [None; CLEANED];
static mut CLEANED_NEXT: usize = 0;

/// The physical address of the `len` picture bytes at flash `addr`, cleaned
/// from the data cache once per flash write generation (the G2D reads DRAM).
/// None if the G2D cannot take it (outside the region, or not 4-byte
/// aligned: an FS image from before `ferrite_build` aligned image pixels).
fn picture_addr(addr: u32, len: usize) -> Option<usize> {
    let phys = crate::flash::phys(addr, len)?;
    if phys % 4 != 0 {
        return None;
    }
    let generation = crate::flash::generation();
    // SAFETY: CPU0 only, never reentrant (see `cache`).
    let (seen, next) = unsafe { (&mut *(&raw mut CLEANED_AT), &mut *(&raw mut CLEANED_NEXT)) };
    if !seen.contains(&Some((addr, generation))) {
        exynos_4412_rs::mmu::clean_dcache_range(phys, len);
        seen[*next] = Some((addr, generation));
        *next = (*next + 1) % CLEANED;
    }
    Some(phys)
}

/// A widget worth a sprite: one of the kinds whose whole drawing depends
/// only on its own appearance, large, and showing what is behind it (an
/// opaque square one is a plain fill).
fn wants_sprite(objs: &ObjPool, id: WidgetId, abs: &Rect) -> bool {
    let kind = objs.get(id).kind;
    let kind_ok = matches!(
        kind,
        KIND_BASE
            | KIND_BUTTON
            | KIND_BUTTON_GROUP
            | KIND_IMAGE
            | KIND_RECTANGLE
            | KIND_CIRCLE
            | KIND_ELLIPSE
            | KIND_POLYGON
            | KIND_LINE
    );
    kind_ok
        && (abs.w as usize) * (abs.h as usize) >= SPRITE_MIN_PIXELS
        && abs.w <= lcd::PANEL.width as u16
        && abs.h <= lcd::PANEL.height as u16
        && render::shows_behind(objs, id)
}

/// 64-bit FNV-1a over everything the built-in drawing of these kinds reads,
/// position excluded (the sprite is drawn at 0,0).
fn appearance(ctx: &Ctx<P>, id: WidgetId, abs: &Rect) -> u64 {
    let objs = &ctx.objs;
    let w = objs.get(id);
    let e = objs.ext(id);
    let mut h = Fnv::new();
    h.u32(w.kind as u32);
    h.u32(abs.w as u32 | (abs.h as u32) << 16);
    h.u32(render::effective_bg(objs, w, &e) as u32 | (w.background_color as u32) << 16);
    h.u32(w.border_color as u32 | (e.press_color as u32) << 16);
    h.u32((w.flags & (FLAG_PRESSED | FLAG_CHECKED)) as u32);
    let b = e.border;
    h.u32(u32::from_le_bytes([b.top, b.right, b.bottom, b.left]));
    let p = e.padding;
    h.u32(u32::from_le_bytes([p.top, p.right, p.bottom, p.left]));
    h.u32(e.border_radius as u32 | (e.alpha as u32) << 16);
    h.u32(objs.gradient_color(id) as u32 | (objs.gradient_dir(id) as u32) << 16);
    h.u32(e.image_id as u32);
    if e.image_id != 0 {
        if let Some(img) = ctx.images.find(e.image_id) {
            h.u32(img.data_addr());
            h.u32(img.width as u32 | (img.height as u32) << 16);
        }
    }
    if w.kind == KIND_POLYGON {
        // The points.
        for &byte in objs.str_get(e.text_id) {
            h.byte(byte);
        }
    }
    h.0
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xCBF2_9CE4_8422_2325)
    }
    fn byte(&mut self, b: u8) {
        self.0 = (self.0 ^ b as u64).wrapping_mul(0x0000_0100_0000_01B3);
    }
    fn u32(&mut self, v: u32) {
        for b in v.to_le_bytes() {
            self.byte(b);
        }
    }
}

struct Sprite {
    key: u64,
    w: usize,
    h: usize,
    /// Pixels per row in `px` (`w` rounded up to `ROW_ALIGN`).
    stride: usize,
    /// The pixels start `start` words into `px` (64-byte aligned).
    px: Vec<u32>,
    start: usize,
    /// `Cache::tick` at the last use.
    used: u32,
}

impl Sprite {
    fn pixels(&self) -> *const u32 {
        self.px[self.start..].as_ptr()
    }
}

struct Cache {
    entries: Vec<Sprite>,
    bytes: usize,
    tick: u32,
    /// Appearances drawn once (ring), with the frame (`lcd::frames`) they
    /// were drawn in.
    seen: [(u64, u32); SEEN],
    seen_at: usize,
}

impl Cache {
    /// Whether `key` was drawn already in an earlier frame; else remembers
    /// it. A second drawing in the same frame (a restore repainting a
    /// widget pass 2 just drew, like a clock hand at a new angle) does not
    /// count: such an appearance may never come back, and its sprite was
    /// made for nothing (~5 ms for a clock hand's box).
    fn seen(&mut self, key: u64) -> bool {
        let frame = lcd::frames();
        if let Some(i) = self.seen.iter().position(|&(k, _)| k == key) {
            if self.seen[i].1 == frame {
                return false;
            }
            self.seen[i] = (0, 0);
            return true;
        }
        self.seen[self.seen_at] = (key, frame);
        self.seen_at = (self.seen_at + 1) % SEEN;
        false
    }

    fn find(&mut self, key: u64) -> Option<usize> {
        let at = self.entries.iter().position(|s| s.key == key)?;
        self.tick = self.tick.wrapping_add(1);
        self.entries[at].used = self.tick;
        Some(at)
    }

    /// Draws widget `id` into a new sprite, making room first. None if it
    /// cannot get the memory.
    fn insert(&mut self, ctx: &Ctx<P>, id: WidgetId, abs: &Rect, key: u64) -> Option<usize> {
        let (w, h) = (abs.w as usize, abs.h as usize);
        let stride = w.next_multiple_of(ROW_ALIGN);
        let words = stride * h + ROW_ALIGN;
        let bytes = words * 4;
        if bytes > CACHE_BYTES {
            return None;
        }
        while !self.entries.is_empty() && (self.bytes + bytes > CACHE_BYTES || self.entries.len() >= CACHE_ENTRIES) {
            // The least recently used: the furthest back from `tick`.
            let lru = (0..self.entries.len()).max_by_key(|&i| self.tick.wrapping_sub(self.entries[i].used))?;
            let old = self.entries.swap_remove(lru);
            self.bytes -= old.px.len() * 4;
        }
        let mut px = Vec::new();
        px.try_reserve_exact(words).ok()?;
        // Transparent: the drawing composites onto it.
        px.resize(words, 0);
        let start = (px.as_ptr() as usize).next_multiple_of(ROW_ALIGN * 4) - px.as_ptr() as usize;
        let start = start / 4;
        let base = px[start..].as_mut_ptr();
        #[cfg(feature = "perf")]
        let t0 = crate::perf::cycles();
        // SAFETY: `base` has stride * h words in `px`, and only the
        // primitives touch them until `end_sprite`; `wants_sprite` keeps w, h
        // within the panel.
        unsafe { lcd::begin_sprite(base, w, h, stride) };
        let local = Rect::new(0, 0, abs.w, abs.h);
        render::draw_default(ctx, id, &local, &ClipRegion::from_rect(local));
        lcd::end_sprite(base, stride * h);
        #[cfg(feature = "perf")]
        ferrite_core::perf::add_sub(ferrite_core::perf::phase::SPRITE, crate::perf::cycles().wrapping_sub(t0));
        self.tick = self.tick.wrapping_add(1);
        self.bytes += bytes;
        self.entries.push(Sprite { key, w, h, stride, px, start, used: self.tick });
        Some(self.entries.len() - 1)
    }
}

static mut CACHE: Cache = Cache { entries: Vec::new(), bytes: 0, tick: 0, seen: [(0, 0); SEEN], seen_at: 0 };

fn cache() -> &'static mut Cache {
    // SAFETY: the renderer runs on CPU0 only, never reentrantly (drawing a
    // sprite does not draw widgets through the renderer).
    unsafe { &mut *(&raw mut CACHE) }
}
