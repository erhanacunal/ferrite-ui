//! Anti-aliased ("smooth") primitives for blend-capable backends
//! (`LcdBackend::HAS_ALPHA`). The public `LcdImpl` primitives route here when
//! the backend can blend, so the Nextion/e-paper builds compile all of this
//! out. Edge pixels get a coverage value and go through `blend_pixel`; solid
//! interiors stay as `fill_rect` spans, so filled shapes cost about the same
//! as their aliased versions.
//!
//! Coverage conventions follow TFT_eSPI's smooth graphics, which this module
//! adapts (integer-only — the ARM926 targets have no FPU, except the ellipse
//! distance estimate):
//!   - `sqrt_fraction` — fixed-point sqrt fraction for circular edge alpha
//!   - `fillSmoothCircle` / `drawArc` / `fillSmoothRoundRect` — row scans that
//!     fill solid spans and blend only the 1 px AA band (here generalized to
//!     elliptical ring sectors and rounded-rect rings)
//!   - `drawWedgeLine` — distance-to-segment coverage with edge tracking
//!
//! Polygon fills use exact area coverage instead: a signed-area accumulation
//! scanline rasterizer (the technique font renderers use), one row at a time.
//!
//! A disk of radius `r` is solid out to distance `r` from the centre pixel
//! and fades to 0 at `r + 1` (50 % at `r + 0.5`, the same edge the aliased
//! primitives use), so smooth and aliased shapes have the same footprint.
//!
//! ---------------------------------------------------------------------------
//! Portions derived from TFT_eSPI (https://github.com/Bodmer/TFT_eSPI),
//! also listed in THIRD_PARTY_NOTICES.md at the repository root:
//!
//! Software License Agreement (FreeBSD License)
//!
//! Copyright (c) 2026 Bodmer (https://github.com/Bodmer)
//!
//! All rights reserved.
//!
//! Redistribution and use in source and binary forms, with or without
//! modification, are permitted provided that the following conditions are met:
//!
//! 1. Redistributions of source code must retain the above copyright notice, this
//!    list of conditions and the following disclaimer.
//! 2. Redistributions in binary form must reproduce the above copyright notice,
//!    this list of conditions and the following disclaimer in the documentation
//!    and/or other materials provided with the distribution.
//!
//! THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
//! ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
//! WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
//! DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE FOR
//! ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
//! (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
//! LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND
//! ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
//! (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
//! SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//!
//! The views and conclusions contained in the software and documentation are those
//! of the authors and should not be interpreted as representing official policies,
//! either expressed or implied, of the FreeBSD Project.
//! ---------------------------------------------------------------------------

use super::{LcdBackend, LcdImpl, isqrt, lerp_rgb565, sin_cos_deg};
use crate::types::Offset;
use alloc::vec::Vec;

/// Fractional part of `sqrt(num)` as 8 bits (TFT_eSPI `sqrt_fraction`):
/// a bit-by-bit fixed-point square root that auto-scales from U8.8 up to
/// U15.16. Valid for `num <= 2^30`.
fn sqrt_fraction(mut num: u32) -> u8 {
    if num > 0x4000_0000 {
        return 0;
    }
    let mut bsh: u32 = 0x0000_4000;
    let mut fpr: u32 = 0;
    let mut osh: u32 = 0;
    while num > bsh {
        bsh <<= 2;
        osh += 1;
    }
    loop {
        let bod = bsh + fpr;
        if num >= bod {
            num -= bod;
            fpr = bsh + bod;
        }
        num <<= 1;
        bsh >>= 1;
        if bsh == 0 {
            break;
        }
    }
    (fpr >> osh) as u8
}

/// Coverage of a pixel at squared distance `d2` from the centre of a disk of
/// radius `r`: solid to `r`, 0 from `r + 1`, `1 - frac(d)` in between.
#[inline]
fn disk_cov(d2: u32, r: u32) -> u8 {
    if d2 <= r * r {
        255
    } else if d2 >= (r + 1) * (r + 1) {
        0
    } else {
        !sqrt_fraction(d2)
    }
}

/// `a * b / 255`, rounded.
#[inline]
fn mul8(a: u8, b: u8) -> u8 {
    let p = a as u16 * b as u16 + 128;
    ((p + (p >> 8)) >> 8) as u8
}

/// Largest `dx >= 0` with `dx² + dy² < limit²` (`-1` when none).
#[inline]
fn ext_lt(limit: i32, dy: i32) -> i32 {
    let rem = limit * limit - dy * dy;
    if rem <= 0 { -1 } else { isqrt(rem - 1) as i32 }
}

/// Largest `dx >= 0` with `dx² + dy² <= limit²` (`-1` when none).
#[inline]
fn ext_le(limit: i32, dy: i32) -> i32 {
    let rem = limit * limit - dy * dy;
    if rem < 0 { -1 } else { isqrt(rem) as i32 }
}

/// Signed distance (px, positive outside) from `(dx, dy)` to the ellipse
/// with semi-axes `a`, `b`: the implicit function normalized by its
/// gradient. Exact for circles, within a fraction of a pixel near the edge
/// of an ellipse — plenty for a 1 px coverage ramp.
fn ellipse_sd(dx: f32, dy: f32, a: f32, b: f32) -> f32 {
    let (ux, uy) = (dx / a, dy / b);
    let q = libm::sqrtf(ux * ux + uy * uy);
    let (gx, gy) = (ux / a, uy / b);
    let g = libm::sqrtf(gx * gx + gy * gy);
    if g < 1e-6 {
        return -a.min(b);
    }
    (q - 1.0) * q / g
}

/// Ramp a signed distance to 0..255 coverage over one pixel centred on 0.
#[inline]
fn ramp(v: f32) -> u8 {
    if v <= 0.0 {
        0
    } else if v >= 1.0 {
        255
    } else {
        (v * 255.0 + 0.5) as u8
    }
}

/// Elliptical ring in half-pixel units ("2" suffix = doubled): centre at
/// pixel coordinate `c2 / 2` (odd = between two pixels), solid out to radius
/// `r2 / 2` and 50 % at `r2 / 2 + ½`, the hole's edge 50 % at `ir2 / 2 − ½`
/// (`ir2 <= 2` = no hole). Circles take the integer `sqrt_fraction` path,
/// ellipses a float distance estimate.
struct Ring {
    rx2: i32,
    ry2: i32,
    irx2: i32,
    iry2: i32,
    circle: bool,
    hole: bool,
}

/// Per-row pixel classes of a ring, as doubled `|dx|` thresholds.
struct RingRow {
    /// `|dx2| > outer` → nothing to draw.
    outer: i32,
    /// `|dx2| <= solid` → full outer coverage (`-1` = none).
    solid: i32,
    /// `|dx2| <= hole` → inside the hole (`-1` = none).
    hole: i32,
    /// `|dx2| >= inner_full` → full inner coverage.
    inner_full: i32,
}

/// `2 · sqrt(n / 4)` = `sqrt(n)` in Q8, from `isqrt` and `sqrt_fraction`.
#[inline]
fn sqrt_q8(n: u32) -> i32 {
    ((isqrt(n as i32) as i32) << 8) | sqrt_fraction(n) as i32
}

impl Ring {
    fn new(rx2: i32, ry2: i32, irx2: i32, iry2: i32) -> Self {
        Ring {
            rx2,
            ry2,
            irx2,
            iry2,
            circle: rx2 == ry2 && irx2 == iry2,
            hole: irx2 > 2 && iry2 > 2,
        }
    }

    /// Rows with `|dy2| <= reach` can hold pixels.
    fn reach(&self) -> i32 {
        if self.circle { self.ry2 + 2 } else { self.ry2 + 4 }
    }

    fn row(&self, dy2: i32) -> Option<RingRow> {
        let dy2 = dy2.abs();
        if self.circle {
            let r2 = self.rx2;
            let outer = ext_lt(r2 + 2, dy2);
            if outer < 0 {
                return None;
            }
            let (hole, inner_full) = if self.hole {
                let ir2 = self.irx2;
                // First |dx2| with dx2² + dy2² >= ir2².
                let rem = ir2 * ir2 - dy2 * dy2;
                let full = if rem <= 0 {
                    0
                } else {
                    let q = isqrt(rem) as i32;
                    if q * q == rem { q } else { q + 1 }
                };
                (ext_le(ir2 - 2, dy2), full)
            } else {
                (-1, 0)
            };
            Some(RingRow { outer, solid: ext_le(r2, dy2), hole, inner_full })
        } else {
            // Conservative float bounds (±1 px); the per-pixel estimate decides.
            let ext = |a2: i32, b2: i32| -> i32 {
                if b2 <= 0 {
                    return -1;
                }
                let t = dy2 as f32 / b2 as f32;
                if t >= 1.0 { -1 } else { (a2 as f32 * libm::sqrtf(1.0 - t * t)) as i32 }
            };
            let outer = ext(self.rx2 + 4, self.ry2 + 4);
            if outer < 0 {
                return None;
            }
            let solid = ext(self.rx2 - 2, self.ry2 - 2);
            let (hole, inner_full) = if self.hole {
                (ext(self.irx2 - 4, self.iry2 - 4), ext(self.irx2 + 2, self.iry2 + 2) + 1)
            } else {
                (-1, 0)
            };
            Some(RingRow { outer, solid, hole, inner_full })
        }
    }

    /// Ring coverage of the pixel at doubled offset `(dx2, dy2)`.
    fn cov(&self, dx2: i32, dy2: i32) -> u8 {
        if self.circle {
            // n = (2d)²; outer ramp = r + 1 − d, inner = d − ir + 1 (TFT_eSPI's
            // `~sqrt_fraction(d²)` / `sqrt_fraction(d²)` at whole-pixel centres).
            let n = (dx2 * dx2 + dy2 * dy2) as u32;
            let r2 = self.rx2 as u32;
            let co = if n <= r2 * r2 {
                255
            } else if n >= (r2 + 2) * (r2 + 2) {
                0
            } else {
                ((((r2 + 2) << 8) as i32 - sqrt_q8(n)) / 2).clamp(0, 255) as u8
            };
            if !self.hole || co == 0 {
                return co;
            }
            let ir2 = self.irx2 as u32;
            let ci = if n >= ir2 * ir2 {
                255
            } else if n <= (ir2 - 2) * (ir2 - 2) {
                0
            } else {
                ((sqrt_q8(n) - ((ir2 - 2) << 8) as i32) / 2).clamp(0, 255) as u8
            };
            mul8(co, ci)
        } else {
            // The estimate drifts near the tips of thin ellipses; nothing at
            // or past the ramp's bounding box can be covered.
            if dx2.abs() >= self.rx2 + 2 || dy2.abs() >= self.ry2 + 2 {
                return 0;
            }
            let (fx, fy) = (dx2 as f32 * 0.5, dy2 as f32 * 0.5);
            let (a, b) = ((self.rx2 + 1) as f32 * 0.5, (self.ry2 + 1) as f32 * 0.5);
            let co = ramp(0.5 - ellipse_sd(fx, fy, a, b));
            if !self.hole || co == 0 {
                return co;
            }
            let (ia, ib) = ((self.irx2 - 1) as f32 * 0.5, (self.iry2 - 1) as f32 * 0.5);
            mul8(co, ramp(0.5 + ellipse_sd(fx, fy, ia, ib)))
        }
    }
}

/// Angular window of an arc: clockwise from `start` to `end` degrees (0° =
/// +x, y down — the `draw_arc` convention). Coverage ramps across each
/// bounding ray, which anti-aliases the arc's straight ends.
struct Sector {
    s_sin: i32,
    s_cos: i32,
    e_sin: i32,
    e_cos: i32,
    /// Sweep > 180°: the union of the two half-planes, else their intersection.
    wide: bool,
}

impl Sector {
    /// `None` for a full turn (`start == end` mod 360).
    fn new(start: i16, end: i16) -> Option<Self> {
        let sweep = (end as i32 - start as i32).rem_euclid(360);
        if sweep == 0 {
            return None;
        }
        let (s_sin, s_cos) = sin_cos_deg(start);
        let (e_sin, e_cos) = sin_cos_deg(end);
        Some(Sector {
            s_sin: s_sin as i32,
            s_cos: s_cos as i32,
            e_sin: e_sin as i32,
            e_cos: e_cos as i32,
            wide: sweep > 180,
        })
    }

    /// Coverage at doubled offset `(dx2, dy2)`.
    fn cov(&self, dx2: i32, dy2: i32) -> u8 {
        // Signed Q8 distances to the start/end rays' lines, positive inside.
        let ds = (self.s_cos * dy2 - self.s_sin * dx2) / 2;
        let de = (self.e_sin * dx2 - self.e_cos * dy2) / 2;
        let cs = (128 + ds).clamp(0, 255);
        let ce = (128 + de).clamp(0, 255);
        (if self.wide { cs.max(ce) } else { cs.min(ce) }) as u8
    }
}

/// What a `sym_row` row is painted with.
#[derive(Clone, Copy)]
enum Paint {
    Solid(u16),
    /// Horizontal gradient: `c1` at `x0` to `c2` at `x0 + n`
    /// (`fill_gradient_rounded_rect` / `hspan_gradient` sampling).
    HGrad { x0: i32, n: u16, c1: u16, c2: u16 },
}

impl Paint {
    #[inline]
    fn at(&self, x: i32) -> u16 {
        match *self {
            Paint::Solid(c) => c,
            Paint::HGrad { x0, n, c1, c2 } => lerp_rgb565(c1, c2, (x - x0).clamp(0, n as i32) as u16, n),
        }
    }
}

/// How `sym_row` treats a left-half pixel.
enum Seg {
    /// Compute its coverage.
    Eval,
    /// Fully covered up to (excluding) this x.
    Solid(i32),
    /// Inside the hole — nothing more on this half-row.
    Hole,
}

impl<B: LcdBackend> LcdImpl<B> {
    #[inline]
    fn aa_px(&self, x: i32, y: i32, color: u16, a: u8) {
        if a != 0 {
            self.blend_pixel(x as u16, y as u16, color, a);
        }
    }

    fn aa_span(&self, x0: i32, x1: i32, y: i32, color: u16, alpha: u8) {
        if alpha == 255 {
            self.hline_clipped(x0 as i16, x1 as i16, y as i16, color);
        } else {
            let l = x0.max(0);
            if x1 >= l {
                self.blend_rect(l as u16, y as u16, (x1 - l + 1) as u16, 1, color, alpha);
            }
        }
    }

    /// A covered run of a `sym_row` row. Gradients carry no alpha (as in
    /// the aliased gradient fills).
    fn paint_span(&self, x0: i32, x1: i32, y: i32, paint: Paint, alpha: u8) {
        match paint {
            Paint::Solid(c) => self.aa_span(x0, x1, y, c, alpha),
            Paint::HGrad { x0: gx, n, c1, c2 } => {
                self.hspan_gradient(x0 as i16, x1 as i16, y as i16, gx as i16, n, c1, c2)
            }
        }
    }

    /// One row of a shape that is mirror-symmetric about `x = s / 2`. Walks
    /// the left half from `x0`, classifying pixels with `seg` and evaluating
    /// edge pixels with `cov`; every pixel and span is mirrored onto the
    /// right half (a centre column is drawn once).
    #[allow(clippy::too_many_arguments)]
    fn sym_row(
        &self,
        y: i32,
        x0: i32,
        s: i32,
        paint: Paint,
        alpha: u8,
        seg: impl Fn(i32) -> Seg,
        cov: impl Fn(i32) -> u8,
    ) {
        let xm = s.div_euclid(2);
        let flush = |a: i32, b: i32| {
            if s - b <= b + 1 {
                // The mirrored span touches this one: draw them as one.
                self.paint_span(a, s - a, y, paint, alpha);
            } else {
                self.paint_span(a, b, y, paint, alpha);
                self.paint_span(s - b, s - a, y, paint, alpha);
            }
        };
        let mut run: Option<i32> = None;
        let mut px = x0;
        while px <= xm {
            match seg(px) {
                Seg::Hole => break,
                Seg::Solid(end) => {
                    run.get_or_insert(px);
                    px = end.min(xm + 1).max(px + 1);
                }
                Seg::Eval => {
                    let c = cov(px);
                    if c == 255 {
                        run.get_or_insert(px);
                    } else {
                        if let Some(a) = run.take() {
                            flush(a, px - 1);
                        }
                        let c = mul8(c, alpha);
                        self.aa_px(px, y, paint.at(px), c);
                        if s - px != px {
                            self.aa_px(s - px, y, paint.at(s - px), c);
                        }
                    }
                    px += 1;
                }
            }
        }
        if let Some(a) = run {
            flush(a, (px - 1).min(xm));
        }
    }

    /// Anti-aliased elliptical ring (sector), centred on pixel `(cx, cy)`,
    /// in the integer-radius convention of the aliased primitives:
    /// `irx`/`iry <= 1` fills the whole ellipse; `arc` limits it to a sweep.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn smooth_ring(
        &self,
        cx: i16,
        cy: i16,
        rx: i16,
        ry: i16,
        irx: i16,
        iry: i16,
        arc: Option<(i16, i16)>,
        color: u16,
        alpha: u8,
    ) {
        if rx <= 0 || ry <= 0 {
            return;
        }
        let d = |v: i16| 2 * v as i32;
        let ring = Ring::new(d(rx), d(ry), d(irx), d(iry));
        self.smooth_ring2(d(cx), d(cy), &ring, arc, color, alpha);
    }

    /// Anti-aliased ellipse inscribed in the `w`×`h` box at `(x, y)`: its
    /// edge (50 % coverage) runs along the box border, so the soft fringe
    /// stays inside the box. `t > 0` draws a ring `t` px wide instead.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn smooth_oval(&self, x: i16, y: i16, w: u16, h: u16, t: u16, color: u16) {
        if w == 0 || h == 0 {
            return;
        }
        let (w, h, t) = (w as i32, h as i32, t as i32);
        let (irx2, iry2) = if t > 0 { (w - 2 * t + 1, h - 2 * t + 1) } else { (0, 0) };
        let ring = Ring::new(w - 1, h - 1, irx2, iry2);
        self.smooth_ring2(2 * x as i32 + w - 1, 2 * y as i32 + h - 1, &ring, None, color, 255);
    }

    fn smooth_ring2(&self, cx2: i32, cy2: i32, ring: &Ring, arc: Option<(i16, i16)>, color: u16, alpha: u8) {
        let sector = arc.and_then(|(s, e)| Sector::new(s, e));
        let reach = ring.reach();
        for y in (cy2 - reach).div_euclid(2)..=(cy2 + reach + 1).div_euclid(2) {
            let dy2 = 2 * y - cy2;
            let Some(row) = ring.row(dy2) else { continue };
            // Left-most pixel with |dx2| <= outer.
            let x0 = (cx2 - row.outer + 1).div_euclid(2);
            match &sector {
                None => self.sym_row(
                    y,
                    x0,
                    cx2,
                    Paint::Solid(color),
                    alpha,
                    |px| {
                        let adx2 = cx2 - 2 * px;
                        if adx2 <= row.hole {
                            Seg::Hole
                        } else if adx2 <= row.solid && adx2 >= row.inner_full {
                            Seg::Solid((cx2 - row.inner_full).div_euclid(2) + 1)
                        } else {
                            Seg::Eval
                        }
                    },
                    |px| ring.cov(2 * px - cx2, dy2),
                ),
                Some(sec) => {
                    // Not mirror-symmetric: evaluate every ring pixel.
                    for px in x0..=cx2 - x0 {
                        let dx2 = 2 * px - cx2;
                        if dx2.abs() <= row.hole {
                            continue;
                        }
                        let c = ring.cov(dx2, dy2);
                        if c != 0 {
                            let c = mul8(mul8(c, sec.cov(dx2, dy2)), alpha);
                            self.aa_px(px, y, color, c);
                        }
                    }
                }
            }
        }
    }

    /// Anti-aliased rounded rect: filled when `t == 0`, otherwise a border
    /// ring `t` px wide (inner corner radius `r - t`). Corners are disks in
    /// the `fill_rounded_rect` geometry; straight edges stay pixel-aligned.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn smooth_round_rect(
        &self,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        r: u16,
        t: u16,
        color: u16,
        alpha: u8,
    ) {
        self.smooth_round_rect_paint(x, y, w, h, r, t, |_| Paint::Solid(color), alpha);
    }

    /// Anti-aliased rounded-rect fill with a horizontal (`dir == 1`) or
    /// vertical gradient from `c1` to `c2` across the rect, sampled like
    /// `fill_gradient_rounded_rect`. Edge pixels take their own gradient
    /// colour.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn smooth_gradient_round_rect(
        &self,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        r: u16,
        c1: u16,
        c2: u16,
        dir: u8,
    ) {
        let (gw, gh) = (w.saturating_sub(1), h.saturating_sub(1));
        self.smooth_round_rect_paint(
            x,
            y,
            w,
            h,
            r,
            0,
            |row| {
                if dir == 1 {
                    Paint::HGrad { x0: x as i32, n: gw, c1, c2 }
                } else {
                    Paint::Solid(lerp_rgb565(c1, c2, (row - y as i32).clamp(0, gh as i32) as u16, gh))
                }
            },
            255,
        );
    }

    /// `smooth_round_rect` with a per-row paint.
    #[allow(clippy::too_many_arguments)]
    fn smooth_round_rect_paint(
        &self,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        r: u16,
        t: u16,
        paint: impl Fn(i32) -> Paint,
        alpha: u8,
    ) {
        if w == 0 || h == 0 {
            return;
        }
        let (x, y, w, h) = (x as i32, y as i32, w as i32, h as i32);
        // Keep the left/right (top/bottom) corner centres from crossing so
        // every pixel has one nearest corner.
        let r = (r as i32).min((w - 1) / 2).min((h - 1) / 2);
        let t = t as i32;
        let inner = t > 0 && w > 2 * t && h > 2 * t;
        if let (0, 0, Paint::Solid(color)) = (t, r, paint(y)) {
            if alpha == 255 {
                self.fill_rect(x as u16, y as u16, w as u16, h as u16, color);
            } else {
                self.blend_rect(x as u16, y as u16, w as u16, h as u16, color, alpha);
            }
            return;
        }
        let (cxl, cyt, cyb) = (x + r, y + r, y + h - 1 - r);
        let (xi, yi, wi, hi) = (x + t, y + t, w - 2 * t, h - 2 * t);
        let ri = if inner { (r - t).max(0).min((wi - 1) / 2).min((hi - 1) / 2) } else { 0 };
        let (cxli, cyti, cybi) = (xi + ri, yi + ri, yi + hi - 1 - ri);
        let s = 2 * x + w - 1;

        for row in y..y + h {
            let dy = (cyt - row).max(row - cyb).max(0);
            let ofull = cxl - ext_le(r, dy);
            // Inner box on this row: (dyi, first px with coverage, first full px).
            let irow = (inner && row >= yi && row < yi + hi).then(|| {
                let dyi = (cyti - row).max(row - cybi).max(0);
                (dyi, cxli - ext_lt(ri + 1, dyi), cxli - ext_le(ri, dyi))
            });
            let cov = |px: i32| -> u8 {
                let dx = (cxl - px).max(0);
                let co = disk_cov((dx * dx + dy * dy) as u32, r as u32);
                match irow {
                    Some((dyi, _, _)) if co != 0 => {
                        let dxi = (cxli - px).max(0);
                        mul8(co, !disk_cov((dxi * dxi + dyi * dyi) as u32, ri as u32))
                    }
                    _ => co,
                }
            };
            self.sym_row(
                row,
                x,
                s,
                paint(row),
                alpha,
                |px| match irow {
                    Some((_, izero, ifull)) => {
                        if px >= ifull {
                            Seg::Hole
                        } else if px < ofull || px >= izero {
                            Seg::Eval
                        } else {
                            Seg::Solid(izero)
                        }
                    }
                    None if px < ofull => Seg::Eval,
                    None => Seg::Solid(i32::MAX),
                },
                cov,
            );
        }
    }

    /// Anti-aliased line with round caps, `hw_q8` = half-width in 1/256 px
    /// (128 = a 1 px line). TFT_eSPI `drawWedgeLine` with equal end radii,
    /// in integer fixed point: coverage = `hw + ½ − distance to segment`.
    /// Rows are scanned outward from the leftmost cap; the shape is convex,
    /// so each row's first covered pixel is never left of the previous one's.
    pub(super) fn smooth_line(&self, ax: i16, ay: i16, bx: i16, by: i16, hw_q8: i32, color: u16) {
        // Q16 distance² must fit isqrt's i32: cap the half-width at 150 px.
        let reach_q8 = hw_q8.clamp(0, 150 << 8) + 128;
        let reach = (reach_q8 + 255) >> 8;
        let (ax, ay, bx, by) = (ax as i32, ay as i32, bx as i32, by as i32);
        let (x0, x1) = (ax.min(bx) - reach, ax.max(bx) + reach);
        let (y0, y1) = (ay.min(by) - reach, ay.max(by) + reach);
        let (ux, uy) = ((bx - ax) as i64, (by - ay) as i64);
        let l2 = ux * ux + uy * uy;
        let lim2 = (reach_q8 as i64) * (reach_q8 as i64);

        // 0..=256 coverage of pixel (px, py).
        let cov = |px: i32, py: i32| -> i32 {
            let (qx, qy) = ((px - ax) as i64, (py - ay) as i64);
            let h = if l2 == 0 { 0 } else { (((qx * ux + qy * uy) << 16) / l2).clamp(0, 1 << 16) };
            let ex = ((qx << 16) - ux * h) >> 8; // Q8
            let ey = ((qy << 16) - uy * h) >> 8;
            let d2 = ex * ex + ey * ey; // Q16
            if d2 >= lim2 {
                return 0;
            }
            (reach_q8 - isqrt(d2 as i32) as i32).min(256)
        };

        let ys = if ax <= bx { ay } else { by };
        let scan = |py: i32, xs: &mut i32| {
            let mut run: Option<i32> = None;
            let mut first = None;
            let mut px = (*xs - 1).max(x0);
            while px <= x1 {
                let c = cov(px, py);
                if c <= 0 {
                    if first.is_some() {
                        break;
                    }
                    px += 1;
                    continue;
                }
                first.get_or_insert(px);
                if c >= 256 {
                    run.get_or_insert(px);
                } else {
                    if let Some(a) = run.take() {
                        self.aa_span(a, px - 1, py, color, 255);
                    }
                    self.aa_px(px, py, color, c as u8);
                }
                px += 1;
            }
            if let Some(a) = run {
                self.aa_span(a, px - 1, py, color, 255);
            }
            if let Some(f) = first {
                *xs = f;
            }
        };
        let mut xs = x0;
        for py in ys..=y1 {
            scan(py, &mut xs);
        }
        let mut xs = x0;
        for py in (y0..ys).rev() {
            scan(py, &mut xs);
        }
    }
}

/// Add the part of edge `p0 → p1` inside pixel row `[y, y + 1)` to the row
/// accumulator `acc` (index 0 = pixel `left`). Coordinates are continuous,
/// pixel `i` spanning `[i, i + 1)`. Each touched cell gets the signed area
/// the edge sweeps in it; a running sum along the row then yields the
/// covered fraction of every pixel. Contributions left of `left` fold into
/// index 0 (exact for a prefix sum); those right of the row are dropped.
fn accumulate_edge(acc: &mut [f32], left: i32, y: f32, p0: (f32, f32), p1: (f32, f32)) {
    let (p0, p1, dir) = if p0.1 < p1.1 { (p0, p1, 1.0) } else { (p1, p0, -1.0) };
    let top = p0.1.max(y);
    let bot = p1.1.min(y + 1.0);
    if bot <= top {
        return;
    }
    let dxdy = (p1.0 - p0.0) / (p1.1 - p0.1);
    let xa = p0.0 + (top - p0.1) * dxdy;
    let xb = p0.0 + (bot - p0.1) * dxdy;
    let d = (bot - top) * dir;
    let (x0, x1) = if xa < xb { (xa, xb) } else { (xb, xa) };
    let mut add = |xi: i32, v: f32| {
        let i = (xi - left).max(0) as usize;
        if i < acc.len() {
            acc[i] += v;
        }
    };
    let x0f = libm::floorf(x0);
    let x0i = x0f as i32;
    let x1c = libm::ceilf(x1);
    let x1i = x1c as i32;
    if x1i <= x0i + 1 {
        // Within one cell: split by the edge's mean x.
        let xm = 0.5 * (xa + xb) - x0f;
        add(x0i, d - d * xm);
        add(x0i + 1, d * xm);
    } else {
        // Across cells: triangle in the first and last, trapezoids between.
        let s = 1.0 / (x1 - x0);
        let fx0 = x0 - x0f;
        let a0 = 0.5 * s * (1.0 - fx0) * (1.0 - fx0);
        let fx1 = x1 - x1c + 1.0;
        let am = 0.5 * s * fx1 * fx1;
        add(x0i, d * a0);
        if x1i == x0i + 2 {
            add(x0i + 1, d * (1.0 - a0 - am));
        } else {
            let a1 = s * (1.5 - fx0);
            add(x0i + 1, d * (a1 - a0));
            for xi in x0i + 2..x1i - 1 {
                add(xi, d * s);
            }
            let a2 = a1 + (x1i - x0i - 3) as f32 * s;
            add(x1i - 1, d * (1.0 - a2 - am));
        }
        add(x1i, d * am);
    }
}

impl<B: LcdBackend> LcdImpl<B> {
    /// Anti-aliased even-odd polygon fill. Vertices sit on pixel centres
    /// (as in the aliased `fill_polygon`); every pixel's coverage is the
    /// exact area of it inside the polygon. Returns false (nothing drawn)
    /// if the row buffer can't be allocated.
    pub(super) fn smooth_polygon(&self, points: &[Offset], color: u16) -> bool {
        let (mut x_min, mut x_max) = (points[0].x as i32, points[0].x as i32);
        let (mut y_min, mut y_max) = (points[0].y as i32, points[0].y as i32);
        for p in points {
            x_min = x_min.min(p.x as i32);
            x_max = x_max.max(p.x as i32);
            y_min = y_min.min(p.y as i32);
            y_max = y_max.max(p.y as i32);
        }
        // Rows/columns limited to the screen; edges left of it still count.
        let left = x_min.max(0);
        let right = x_max.min(B::WIDTH as i32 - 1);
        let (top, bottom) = (y_min.max(0), y_max.min(B::HEIGHT as i32 - 1));
        if right < left || bottom < top {
            return true;
        }
        let len = (right - left + 1) as usize;
        let mut acc: Vec<f32> = Vec::new();
        if acc.try_reserve_exact(len).is_err() {
            return false;
        }
        acc.resize(len, 0.0);
        let at = |i: usize| (points[i].x as f32 + 0.5, points[i].y as f32 + 0.5);

        for y in top..=bottom {
            acc.fill(0.0);
            for i in 0..points.len() {
                let j = if i + 1 == points.len() { 0 } else { i + 1 };
                accumulate_edge(&mut acc, left, y as f32, at(i), at(j));
            }
            let mut sum = 0.0f32;
            let mut run: Option<i32> = None;
            for (i, v) in acc.iter().enumerate() {
                sum += v;
                // Even-odd: winding w → coverage |w| folded into 0..1..0.
                let w = libm::fabsf(sum) % 2.0;
                let c = if w > 1.0 { 2.0 - w } else { w };
                let a = (c * 255.0 + 0.5) as i32;
                let x = left + i as i32;
                if a >= 255 {
                    run.get_or_insert(x);
                    continue;
                }
                if let Some(r) = run.take() {
                    self.aa_span(r, x - 1, y, color, 255);
                }
                if a > 0 {
                    self.aa_px(x, y, color, a as u8);
                }
            }
            if let Some(r) = run {
                self.aa_span(r, right, y, color, 255);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqrt_fraction_matches_float() {
        for n in [2u32, 3, 5, 10, 99, 1000, 12345, 65535, 1 << 20, 123_456_789] {
            let f = libm::sqrt(n as f64);
            let want = ((f - libm::floor(f)) * 256.0) as i32;
            let got = sqrt_fraction(n) as i32;
            assert!((got - want).abs() <= 1, "n={n} got={got} want={want}");
        }
    }

    use core::cell::RefCell;
    use std::vec::Vec;

    const N: usize = 96;

    /// Blend-capable grid recording each pixel's last coverage and how many
    /// times it was written (a pixel written twice in one primitive would be
    /// blended over itself — a visible dark seam).
    struct AlphaGrid {
        cov: RefCell<Vec<u8>>,
        hits: RefCell<Vec<u8>>,
        color: RefCell<Vec<u16>>,
        /// Pixel-stream window (x, y, w) and the next index in it.
        stream: RefCell<(u16, u16, u16, u16)>,
    }
    impl AlphaGrid {
        fn put(&self, x: u16, y: u16, c: u16, a: u8) {
            if (x as usize) < N && (y as usize) < N {
                let i = y as usize * N + x as usize;
                self.cov.borrow_mut()[i] = a;
                self.hits.borrow_mut()[i] += 1;
                self.color.borrow_mut()[i] = c;
            }
        }
    }
    impl LcdBackend for AlphaGrid {
        const WIDTH: u16 = N as u16;
        const HEIGHT: u16 = N as u16;
        const HAS_ALPHA: bool = true;
        fn begin_frame(&mut self) {}
        fn end_frame(&mut self) {}
        fn back_buf(&self) -> u8 {
            0
        }
        fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, c: u16) {
            self.blend_rect(x, y, w, h, c, 255);
        }
        fn blend_rect(&self, x: u16, y: u16, w: u16, h: u16, c: u16, alpha: u8) {
            for yy in y..y.saturating_add(h) {
                for xx in x..x.saturating_add(w) {
                    self.put(xx, yy, c, alpha);
                }
            }
        }
        fn blend_pixel(&self, x: u16, y: u16, c: u16, alpha: u8) {
            self.put(x, y, c, alpha);
        }
        fn begin_pixels(&self, x: u16, y: u16, w: u16, _: u16) {
            *self.stream.borrow_mut() = (x, y, w.max(1), 0);
        }
        fn write_pixel(&self, c: u16) {
            let (x, y, w, i) = *self.stream.borrow();
            self.put(x + i % w, y + i / w, c, 255);
            self.stream.borrow_mut().3 = i + 1;
        }
        fn send_command(&self, _: u16) {}
        fn send_data(&self, _: u16) {}
    }

    fn draw(f: impl Fn(&LcdImpl<AlphaGrid>)) -> (Vec<u8>, Vec<u8>) {
        let (cov, hits, _) = draw_colored(f);
        (cov, hits)
    }

    /// Coverage, write counts and last colour of every pixel.
    fn draw_colored(f: impl Fn(&LcdImpl<AlphaGrid>)) -> (Vec<u8>, Vec<u8>, Vec<u16>) {
        let lcd = LcdImpl::with_backend(AlphaGrid {
            cov: RefCell::new(alloc::vec![0; N * N]),
            hits: RefCell::new(alloc::vec![0; N * N]),
            color: RefCell::new(alloc::vec![0; N * N]),
            stream: RefCell::new((0, 0, 1, 0)),
        });
        f(&lcd);
        let be = lcd.backend();
        (be.cov.borrow().clone(), be.hits.borrow().clone(), be.color.borrow().clone())
    }

    fn assert_single_hits(hits: &[u8], what: &str) {
        if let Some(p) = hits.iter().position(|&h| h > 1) {
            panic!("{what}: pixel ({}, {}) written {} times", p % N, p / N, hits[p]);
        }
    }

    #[test]
    fn circle_and_ellipse_fills_write_each_pixel_once() {
        for r in [1i16, 2, 3, 7, 20, 40] {
            let (_, hits) = draw(|l| l.fill_circle(48, 48, r, 1));
            assert_single_hits(&hits, &format!("circle r={r}"));
        }
        for (rx, ry) in [(40i16, 20i16), (10, 35), (3, 9), (44, 44)] {
            let (_, hits) = draw(|l| l.fill_ellipse(48, 48, rx, ry, 1));
            assert_single_hits(&hits, &format!("ellipse {rx}x{ry}"));
        }
    }

    #[test]
    fn smooth_circle_keeps_the_aliased_footprint() {
        // Solid core ⊆ aliased disk ⊆ anti-aliased footprint.
        for r in [3i16, 10, 25, 40] {
            let (cov, _) = draw(|l| l.fill_circle(48, 48, r, 1));
            for y in 0..N as i32 {
                for x in 0..N as i32 {
                    let d2 = (x - 48).pow(2) + (y - 48).pow(2);
                    let c = cov[y as usize * N + x as usize];
                    let r = r as i32;
                    if d2 <= r * r {
                        assert_eq!(c, 255, "r={r} ({x},{y}) inside not solid");
                    }
                    if d2 >= (r + 1) * (r + 1) {
                        assert_eq!(c, 0, "r={r} ({x},{y}) outside drawn");
                    }
                }
            }
        }
    }

    #[test]
    fn rings_and_arcs_write_each_pixel_once() {
        for (r, t) in [(30i16, 1i16), (30, 4), (12, 6), (40, 40), (5, 2)] {
            let (_, hits) = draw(|l| l.stroke_ellipse(48, 48, r, r, t, 1));
            assert_single_hits(&hits, &format!("ring r={r} t={t}"));
            let (_, hits) = draw(|l| l.stroke_ellipse(48, 48, r, r * 2 / 3 + 1, t, 1));
            assert_single_hits(&hits, &format!("ellipse ring r={r} t={t}"));
        }
        for (s, e) in [(135i16, 45i16), (0, 90), (200, 210), (350, 10)] {
            let (cov, hits) = draw(|l| l.fill_arc(48, 48, 40, 32, s, e, 1));
            assert_single_hits(&hits, &format!("arc {s}..{e}"));
            assert!(cov.iter().any(|&c| c == 255), "arc {s}..{e} has no solid pixel");
        }
    }

    #[test]
    fn arc_stays_inside_its_sweep() {
        // 0°..90° clockwise from +x = the lower-right quadrant (y down).
        let (cov, _) = draw(|l| l.fill_arc(48, 48, 40, 30, 0, 90, 1));
        for y in 0..N {
            for x in 0..N {
                if cov[y * N + x] > 0 {
                    assert!(x >= 47 && y >= 47, "({x},{y}) outside 0..90");
                }
            }
        }
    }

    #[test]
    fn round_rects_write_each_pixel_once_with_hard_straight_edges() {
        for (w, h, r, t) in [(60u16, 30u16, 10u16, 0u16), (60, 30, 15, 0), (50, 50, 25, 3), (80, 40, 12, 2), (20, 9, 4, 1), (30, 30, 0, 2)] {
            let (cov, hits) = draw(|l| {
                if t == 0 {
                    l.fill_rounded_rect(8, 8, w, h, r, 1)
                } else {
                    l.stroke_rounded_rect(8, 8, w, h, r, t, 1)
                }
            });
            assert_single_hits(&hits, &format!("rrect {w}x{h} r={r} t={t}"));
            // Nothing outside the box; mid-left edge column solid.
            for y in 0..N {
                for x in 0..N {
                    let inside = (8..8 + w as usize).contains(&x) && (8..8 + h as usize).contains(&y);
                    assert!(inside || cov[y * N + x] == 0, "rrect {w}x{h} r={r}: ({x},{y}) outside");
                }
            }
            assert_eq!(cov[(8 + h as usize / 2) * N + 8], 255, "rrect {w}x{h} r={r} t={t} edge");
        }
    }

    #[test]
    fn lines_write_each_pixel_once() {
        for (x0, y0, x1, y1) in [(5i16, 5i16, 90i16, 40i16), (90, 5, 5, 90), (10, 50, 80, 50), (40, 5, 40, 90), (30, 30, 30, 30)] {
            for t in [1u16, 2, 5] {
                let (cov, hits) = draw(|l| l.draw_line_thick(x0, y0, x1, y1, 1, t));
                assert_single_hits(&hits, &format!("line {x0},{y0}->{x1},{y1} t={t}"));
                assert!(cov[y0 as usize * N + x0 as usize] == 255, "endpoint not solid");
            }
        }
    }

    #[test]
    fn ovals_stay_in_their_box_and_are_symmetric() {
        for (w, h) in [(84u16, 84u16), (85, 85), (60, 30), (31, 70), (4, 4), (2, 9)] {
            for t in [0u16, 1, 3] {
                let (cov, hits) = draw(|l| l.draw_oval(5, 7, w, h, t, 1));
                assert_single_hits(&hits, &format!("oval {w}x{h} t={t}"));
                let (w, h) = (w as usize, h as usize);
                for y in 0..N {
                    for x in 0..N {
                        let c = cov[y * N + x];
                        let inside = (5..5 + w).contains(&x) && (7..7 + h).contains(&y);
                        assert!(inside || c == 0, "oval {w}x{h} t={t}: ({x},{y}) outside box");
                        if inside {
                            let (mx, my) = (2 * 5 + w - 1 - x, 2 * 7 + h - 1 - y);
                            assert_eq!(c, cov[y * N + mx], "oval {w}x{h} t={t}: x-mirror of ({x},{y})");
                            assert_eq!(c, cov[my * N + x], "oval {w}x{h} t={t}: y-mirror of ({x},{y})");
                        }
                    }
                }
            }
        }
    }

    /// Aliased reference fill (the non-AA `fill_polygon`) as a pixel mask.
    fn aliased_mask(pts: &[Offset]) -> Vec<bool> {
        struct Mask(RefCell<Vec<bool>>);
        impl LcdBackend for Mask {
            const WIDTH: u16 = N as u16;
            const HEIGHT: u16 = N as u16;
            fn begin_frame(&mut self) {}
            fn end_frame(&mut self) {}
            fn back_buf(&self) -> u8 {
                0
            }
            fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, _: u16) {
                for yy in y as usize..(y as usize + h as usize).min(N) {
                    for xx in x as usize..(x as usize + w as usize).min(N) {
                        self.0.borrow_mut()[yy * N + xx] = true;
                    }
                }
            }
            fn begin_pixels(&self, _: u16, _: u16, _: u16, _: u16) {}
            fn write_pixel(&self, _: u16) {}
            fn send_command(&self, _: u16) {}
            fn send_data(&self, _: u16) {}
        }
        let lcd = LcdImpl::with_backend(Mask(RefCell::new(alloc::vec![false; N * N])));
        lcd.fill_polygon(pts, 1);
        lcd.backend().0.borrow().clone()
    }

    fn pts(list: &[(i16, i16)]) -> Vec<Offset> {
        list.iter().map(|&(x, y)| Offset { x, y }).collect()
    }

    const POLYS: &[&[(i16, i16)]] = &[
        &[(10, 5), (90, 20), (40, 85)],                          // triangle
        &[(5, 5), (90, 5), (90, 90), (5, 90)],                   // axis-aligned square
        &[(10, 10), (80, 10), (45, 40), (80, 80), (10, 80)],     // concave
        &[(48, 2), (60, 90), (2, 30), (94, 30), (36, 90)],       // self-intersecting star
        &[(3, 50), (93, 47), (93, 53)],                          // thin sliver
    ];

    #[test]
    fn polygon_fill_writes_each_pixel_once() {
        for poly in POLYS {
            let p = pts(poly);
            let (_, hits) = draw(|l| l.fill_polygon(&p, 1));
            assert_single_hits(&hits, &format!("polygon {poly:?}"));
        }
    }

    #[test]
    fn polygon_coverage_sums_to_its_area() {
        for poly in POLYS[..3].iter().chain(&POLYS[4..]) {
            let p = pts(poly);
            let (cov, _) = draw(|l| l.fill_polygon(&p, 1));
            let n = p.len();
            let twice: i32 = (0..n)
                .map(|i| {
                    let (a, b) = (p[i], p[(i + 1) % n]);
                    a.x as i32 * b.y as i32 - b.x as i32 * a.y as i32
                })
                .sum();
            let area = twice.abs() as f32 / 2.0;
            let got: f32 = cov.iter().map(|&c| c as f32 / 255.0).sum();
            assert!((got - area).abs() <= area * 0.01 + 2.0, "{poly:?}: area {area} got {got}");
        }
    }

    #[test]
    fn polygon_interior_solid_and_exterior_empty() {
        // Against the aliased fill (whose truncated crossings make it up to
        // 1 px fatter): pixels 2 px deep inside it (5×5 all filled) are
        // solid; pixels with no filled pixel within 2 px are untouched.
        for poly in POLYS {
            let p = pts(poly);
            let (cov, _) = draw(|l| l.fill_polygon(&p, 1));
            let mask = aliased_mask(&p);
            for y in 2..N - 2 {
                for x in 2..N - 2 {
                    let around = (y - 2..=y + 2).flat_map(|yy| (x - 2..=x + 2).map(move |xx| (xx, yy)));
                    let filled = around.filter(|&(xx, yy)| mask[yy * N + xx]).count();
                    let c = cov[y * N + x];
                    if filled == 25 {
                        assert_eq!(c, 255, "{poly:?}: interior ({x},{y}) = {c}");
                    }
                    if filled == 0 {
                        assert_eq!(c, 0, "{poly:?}: exterior ({x},{y}) = {c}");
                    }
                }
            }
        }
    }

    #[test]
    fn polygon_left_of_screen_matches_shifted_copy() {
        // Folding off-screen contributions into column 0 must be exact.
        let tri = [(10i16, 5i16), (90, 20), (40, 85)];
        let inside = pts(&tri);
        let shifted: Vec<Offset> = inside.iter().map(|p| Offset { x: p.x - 30, y: p.y }).collect();
        let (a, _) = draw(|l| l.fill_polygon(&inside, 1));
        let (b, _) = draw(|l| l.fill_polygon(&shifted, 1));
        for y in 0..N {
            for x in 0..N - 30 {
                // ±1: float rounding at exactly half coverage.
                let (pb, pa) = (b[y * N + x] as i32, a[y * N + x + 30] as i32);
                assert!((pb - pa).abs() <= 1, "({x},{y}): {pb} vs {pa}");
            }
        }
    }

    #[test]
    fn gradient_round_rect_matches_solid_footprint_and_samples_the_gradient() {
        let (c1, c2) = (0xF800u16, 0x001F);
        for (w, h, r) in [(80u16, 40u16, 12u16), (60, 60, 30), (30, 70, 9), (17, 9, 4)] {
            let (solid, _) = draw(|l| l.fill_rounded_rect(5, 7, w, h, r, 1));
            for dir in [1u8, 2] {
                let what = format!("{w}x{h} r={r} dir={dir}");
                let (cov, hits, col) = draw_colored(|l| l.fill_gradient_rounded_rect(5, 7, w, h, r, c1, c2, dir));
                assert_single_hits(&hits, &what);
                assert!(cov == solid, "{what}: coverage differs from the solid fill");
                for y in 0..N {
                    for x in 0..N {
                        if cov[y * N + x] == 0 {
                            continue;
                        }
                        let want = if dir == 1 {
                            lerp_rgb565(c1, c2, (x - 5) as u16, w - 1)
                        } else {
                            lerp_rgb565(c1, c2, (y - 7) as u16, h - 1)
                        };
                        assert_eq!(col[y * N + x], want, "{what}: colour at ({x},{y})");
                    }
                }
            }
        }
    }

    #[test]
    fn mul8_endpoints() {
        assert_eq!(mul8(255, 255), 255);
        assert_eq!(mul8(255, 0), 0);
        assert_eq!(mul8(0, 255), 0);
        assert_eq!(mul8(255, 128), 128);
    }
}
