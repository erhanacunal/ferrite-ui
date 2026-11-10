//! Anti-aliased triangle meshes for a GPU: the shapes the CPU draws in `smooth.rs`, as
//! triangles whose vertices carry a coverage (HWUI's "vertex alpha" anti-aliasing,
//! `PathTessellator::getFillVerticesFromPerimeterAA`). An edge gets a fringe one pixel wide:
//! coverage 1 on its inner side, 0 on its outer side, interpolated between, so the fragment
//! shader only multiplies its colour by the interpolated coverage.
//!
//! The geometry is `smooth.rs`'s, in continuous coordinates (pixel `px` spans `px..px + 1`,
//! its centre `px + 0.5` is where a GPU samples it):
//! - a rounded rect's corner `r` is a disk around the centre of the pixel `r` in from both
//!   edges, solid to distance `r`, empty from `r + 1`, linear between (`disk_cov`): a fringe
//!   from radius `r` to `r + 1` around that centre;
//! - straight edges lie on pixel boundaries, so they need no fringe: solid rects up to them
//!   sample exactly the pixels the CPU covers fully.
//!
//! Output: triangle lists (three [`Vtx`] a triangle) in [`Chunk`]s, each with its bounding
//! box, so a caller drawing in strips can skip the chunks outside one. The pieces of a shape
//! share edges but never overlap, so a translucent colour blends every pixel once (with a
//! GPU's fill rules, which give a pixel centre on a shared edge to one side only).

use alloc::vec::Vec;

/// A vertex: position in pixels, coverage 0..1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vtx {
    pub x: f32,
    pub y: f32,
    pub a: f32,
}

/// Triangles `start..start + len` of the vertex list (`len` a multiple of 3), inside the box
/// `x0..x1` × `y0..y1` (whole pixels, rounded outwards).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub start: usize,
    pub len: usize,
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

/// Collects triangles into chunks.
pub struct Mesh<'a> {
    pub verts: &'a mut Vec<Vtx>,
    pub chunks: &'a mut Vec<Chunk>,
}

impl Mesh<'_> {
    /// Starts a chunk; [`end`](Self::end) closes it.
    fn begin(&self) -> usize {
        self.verts.len()
    }

    fn end(&mut self, start: usize) {
        let len = self.verts.len() - start;
        if len == 0 {
            return;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for v in &self.verts[start..] {
            x0 = x0.min(v.x);
            y0 = y0.min(v.y);
            x1 = x1.max(v.x);
            y1 = y1.max(v.y);
        }
        self.chunks.push(Chunk {
            start,
            len,
            x0: libm::floorf(x0) as i32,
            y0: libm::floorf(y0) as i32,
            x1: libm::ceilf(x1) as i32,
            y1: libm::ceilf(y1) as i32,
        });
    }

    fn tri(&mut self, a: Vtx, b: Vtx, c: Vtx) {
        self.verts.extend_from_slice(&[a, b, c]);
    }

    /// A quad `a b c d` (in order around it) as two triangles.
    fn quad(&mut self, a: Vtx, b: Vtx, c: Vtx, d: Vtx) {
        self.tri(a, b, c);
        self.tri(a, c, d);
    }

    /// A solid (coverage 1) rect; nothing if empty.
    fn rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let v = |x, y| Vtx { x, y, a: 1.0 };
        self.quad(v(x0, y0), v(x1, y0), v(x1, y1), v(x0, y1));
    }
}

/// Segments per quarter circle of radius `r`: the chord's sagitta `r·θ²/8` stays under
/// ~0.05 px (θ = π/2 / n).
fn segments(r: f32) -> usize {
    (libm::ceilf(2.5 * libm::sqrtf(r.max(0.0))) as usize).clamp(2, 32)
}

/// A quarter of a disk's edge around (`cx`, `cy`), towards (`sx`, `sy`) (±1 each), as rings
/// at radii `rings[k].0` with coverage `rings[k].1`, from the horizontal ray (θ = 0) to the
/// vertical one: quads between consecutive rings, and a fan from the centre to the first ring
/// when it has coverage (a filled corner).
fn quarter(m: &mut Mesh<'_>, cx: f32, cy: f32, sx: f32, sy: f32, rings: &[(f32, f32)]) {
    let outer = rings.last().map_or(0.0, |r| r.0);
    let n = segments(outer);
    let at = |i: usize, rad: f32, a: f32| {
        let t = core::f32::consts::FRAC_PI_2 * i as f32 / n as f32;
        // Exact ends: the rays the neighbouring pieces' edges lie on.
        let (c, s) = match i {
            0 => (1.0, 0.0),
            _ if i == n => (0.0, 1.0),
            _ => (libm::cosf(t), libm::sinf(t)),
        };
        Vtx { x: cx + sx * rad * c, y: cy + sy * rad * s, a }
    };
    let centre = Vtx { x: cx, y: cy, a: rings[0].1 };
    for i in 0..n {
        let (r0, a0) = rings[0];
        if a0 > 0.0 && r0 > 0.0 {
            m.tri(centre, at(i, r0, a0), at(i + 1, r0, a0));
        }
        for k in 1..rings.len() {
            let ((ra, aa), (rb, ab)) = (rings[k - 1], rings[k]);
            if aa == 0.0 && ab == 0.0 {
                continue;
            }
            m.quad(at(i, ra, aa), at(i, rb, ab), at(i + 1, rb, ab), at(i + 1, ra, aa));
        }
    }
}

/// `r` as `smooth_round_rect` takes it for a `w` × `h` box: at most `(w - 1) / 2` and
/// `(h - 1) / 2`, so the corner centres don't cross.
fn clamp_radius(w: i32, h: i32, r: i32) -> i32 {
    r.min((w - 1) / 2).min((h - 1) / 2).max(0)
}

/// The rounded rect `x, y, w, h` with corner radius `r`, filled: as
/// `LcdImpl::fill_rounded_rect` covers it.
pub fn rounded_fill(m: &mut Mesh<'_>, x: i32, y: i32, w: i32, h: i32, r: i32) {
    if w <= 0 || h <= 0 {
        return;
    }
    let r = clamp_radius(w, h, r);
    let (x0, y0, x1, y1) = (x as f32, y as f32, (x + w) as f32, (y + h) as f32);
    if r == 0 {
        let s = m.begin();
        m.rect(x0, y0, x1, y1);
        m.end(s);
        return;
    }
    // Corner centres (pixel centres `r` in from the edges).
    let k = r as f32 + 0.5;
    let (cl, cr, ct, cb) = (x0 + k, x1 - k, y0 + k, y1 - k);
    // The middle column, then the left and right bands between the corners.
    let s = m.begin();
    m.rect(cl, y0, cr, y1);
    m.rect(x0, ct, cl, cb);
    m.rect(cr, ct, x1, cb);
    m.end(s);
    let rings = [(r as f32, 1.0), (r as f32 + 1.0, 0.0)];
    for (cx, sx) in [(cl, -1.0), (cr, 1.0)] {
        for (cy, sy) in [(ct, -1.0), (cb, 1.0)] {
            let s = m.begin();
            quarter(m, cx, cy, sx, sy, &rings);
            m.end(s);
        }
    }
}

/// The border ring of the rounded rect `x, y, w, h` with corner radius `r`, `t` pixels
/// wide (inner corner radius `r - t`): as `LcdImpl::stroke_rounded_rect` covers it. A box no
/// wider or taller than `2·t` has no hole: filled.
#[allow(clippy::too_many_arguments)]
pub fn rounded_ring(m: &mut Mesh<'_>, x: i32, y: i32, w: i32, h: i32, r: i32, t: i32) {
    if w <= 0 || h <= 0 || t <= 0 {
        return;
    }
    if w <= 2 * t || h <= 2 * t {
        return rounded_fill(m, x, y, w, h, r);
    }
    let r = clamp_radius(w, h, r);
    let (x0, y0, x1, y1) = (x as f32, y as f32, (x + w) as f32, (y + h) as f32);
    let tf = t as f32;
    let (xi0, yi0, xi1, yi1) = (x0 + tf, y0 + tf, x1 - tf, y1 - tf);
    let k = r as f32 + 0.5;
    let (cl, cr, ct, cb) = (x0 + k, x1 - k, y0 + k, y1 - k);
    if r >= t {
        // Concentric corners (inner radius ri = r - t around the same centres): straight bands
        // between the corner centres, the corners as quarter rings with both edges soft.
        let s = m.begin();
        m.rect(cl, y0, cr, yi0);
        m.rect(cl, yi1, cr, y1);
        m.rect(x0, ct, xi0, cb);
        m.rect(xi1, ct, x1, cb);
        m.end(s);
        let ri = (r - t) as f32;
        let rings = [(ri, 0.0), (ri + 1.0, 1.0), (r as f32, 1.0), (r as f32 + 1.0, 0.0)];
        for (cx, sx) in [(cl, -1.0), (cr, 1.0)] {
            for (cy, sy) in [(ct, -1.0), (cb, 1.0)] {
                let s = m.begin();
                quarter(m, cx, cy, sx, sy, &rings);
                m.end(s);
            }
        }
        return;
    }
    // r < t: the hole's corners are square. Bands between the t × t corner squares, and each
    // square filled up to a rounded outer corner (centre inside it, `cl < xi0`).
    let s = m.begin();
    m.rect(xi0, y0, xi1, yi0);
    m.rect(xi0, yi1, xi1, y1);
    m.rect(x0, yi0, xi0, yi1);
    m.rect(xi1, yi0, x1, yi1);
    // The squares' parts beyond their corner disk: the strip on the far side of the centre
    // across, and the strip below / above it.
    m.rect(cl, y0, xi0, ct);
    m.rect(x0, ct, xi0, yi0);
    m.rect(xi1, y0, cr, ct);
    m.rect(xi1, ct, x1, yi0);
    m.rect(cl, cb, xi0, y1);
    m.rect(x0, yi1, xi0, cb);
    m.rect(xi1, cb, cr, y1);
    m.rect(xi1, yi1, x1, cb);
    m.end(s);
    if r == 0 {
        // Square outer corners too: the corner squares' last pixels.
        let s = m.begin();
        m.rect(x0, y0, cl, ct);
        m.rect(cr, y0, x1, ct);
        m.rect(x0, cb, cl, y1);
        m.rect(cr, cb, x1, y1);
        m.end(s);
        return;
    }
    let rings = [(r as f32, 1.0), (r as f32 + 1.0, 0.0)];
    for (cx, sx) in [(cl, -1.0), (cr, 1.0)] {
        for (cy, sy) in [(ct, -1.0), (cb, 1.0)] {
            let s = m.begin();
            quarter(m, cx, cy, sx, sy, &rings);
            m.end(s);
        }
    }
}

/// Segments of a polyline's chunk (a frame takes only the chunks that meet it).
const POLY_CHUNK: usize = 32;

/// The unit circle in 8 steps: a disk of radius 1 deviates from it by under 0.08 px.
const DISK: [(f32, f32); 8] = [
    (1.0, 0.0),
    (core::f32::consts::FRAC_1_SQRT_2, core::f32::consts::FRAC_1_SQRT_2),
    (0.0, 1.0),
    (-core::f32::consts::FRAC_1_SQRT_2, core::f32::consts::FRAC_1_SQRT_2),
    (-1.0, 0.0),
    (-core::f32::consts::FRAC_1_SQRT_2, -core::f32::consts::FRAC_1_SQRT_2),
    (0.0, -1.0),
    (core::f32::consts::FRAC_1_SQRT_2, -core::f32::consts::FRAC_1_SQRT_2),
];

/// A polyline 1 px wide through the pixel centres of `pts`, as `LcdImpl::pen(_, 1)` covers
/// it: coverage 1 − the distance from the line (`smooth.rs` `Stroke::cov` with a radius of
/// ½), round ends.
///
/// Each segment is a strip from coverage 0 on one side through 1 on the line to 0 on the
/// other, along its own normal; the two ends get a disk (1 at the centre, 0 at radius 1).
/// Where strips overlap inside a turn, the GPU blends one colour "over" itself
/// (`a + b·(1 − a)`, never past 1) while the CPU takes the strongest segment: the two agree
/// along straight runs and differ a little inside sharp turns. Points within
/// [`SIMPLIFY`] of a straight run are dropped first (a graph's flat stretches and gentle
/// slopes, sampled every pixel, become a few long segments).
///
/// Measured against the CPU (`smooth::tests::tessellated_polylines_match_the_cpu_pen`): a
/// disk at every join (to fill the wedge between strips) changed no pixel by more than the
/// turns already differ and doubled the vertices; mitered joins (strips sharing offsets)
/// fold over on a curve sampled every pixel, where a turn's offset reaches past the 1 px
/// segment next to it (27 % of the ink off).
pub fn polyline(m: &mut Mesh<'_>, pts: &[(i32, i32)]) {
    let v = |(x, y): (f32, f32), a: f32| Vtx { x, y, a };
    let mut q: Vec<(f32, f32)> = Vec::with_capacity(pts.len());
    for &(x, y) in pts {
        let c = (x as f32 + 0.5, y as f32 + 0.5);
        if q.last() != Some(&c) {
            q.push(c);
        }
    }
    // Greedy simplification: from each kept point, the furthest point whose chord passes
    // within SIMPLIFY of every point it skips, all of them ahead along it.
    let mut p: Vec<(f32, f32)> = Vec::with_capacity(q.len());
    let mut i = 0;
    while i < q.len() {
        p.push(q[i]);
        let mut j = i + 1;
        while j + 1 < q.len() {
            let (a, c) = (q[i], q[j + 1]);
            let (dx, dy) = (c.0 - a.0, c.1 - a.1);
            let l = libm::sqrtf(dx * dx + dy * dy);
            let fits = q[i + 1..=j].iter().all(|b| {
                ((b.0 - a.0) * dy - (b.1 - a.1) * dx).abs() <= SIMPLIFY * l && (b.0 - a.0) * dx + (b.1 - a.1) * dy >= 0.0
            });
            if !fits {
                break;
            }
            j += 1;
        }
        i = j;
    }
    let Some(&first) = p.first() else { return };
    let mut s = m.begin();
    disk(m, first);
    for i in 1..p.len() {
        let (a, b) = (p[i - 1], p[i]);
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let l = libm::sqrtf(dx * dx + dy * dy);
        let o = (-dy / l, dx / l);
        let (la, lb) = ((a.0 + o.0, a.1 + o.1), (b.0 + o.0, b.1 + o.1));
        let (ra, rb) = ((a.0 - o.0, a.1 - o.1), (b.0 - o.0, b.1 - o.1));
        m.quad(v(la, 0.0), v(lb, 0.0), v(b, 1.0), v(a, 1.0));
        m.quad(v(a, 1.0), v(b, 1.0), v(rb, 0.0), v(ra, 0.0));
        if i % POLY_CHUNK == 0 {
            m.end(s);
            s = m.begin();
        }
    }
    if p.len() > 1 {
        disk(m, p[p.len() - 1]);
    }
    m.end(s);
}

/// Points closer than this (px) to the chord that skips them are dropped: 0.1 changed no
/// pixel in the tests, 0.2 put the peaks' mean error from 2 % to 8 %.
const SIMPLIFY: f32 = 0.1;

/// Triangles `tris` (three vertices each) cut to the rect `x0..x1` × `y0..y1`, appended to
/// `out` as triangles (coverage interpolated along the cut edges): what a scissor would leave
/// of them, for a GPU that clips only to its whole frame.
pub fn clip(tris: &[Vtx], x0: f32, y0: f32, x1: f32, y1: f32, out: &mut Vec<Vtx>) {
    for t in tris.chunks_exact(3) {
        let inside = |v: &Vtx| v.x >= x0 && v.x <= x1 && v.y >= y0 && v.y <= y1;
        if t.iter().all(inside) {
            out.extend_from_slice(t);
            continue;
        }
        if t.iter().all(|v| v.x <= x0) || t.iter().all(|v| v.x >= x1) || t.iter().all(|v| v.y <= y0) || t.iter().all(|v| v.y >= y1) {
            continue;
        }
        // Sutherland–Hodgman against each edge: a triangle becomes at most 7 vertices.
        let mut poly = [Vtx { x: 0.0, y: 0.0, a: 0.0 }; 9];
        let mut n = 3;
        poly[..3].copy_from_slice(t);
        // (axis: 0 = x, 1 = y; bound; keep values >= bound when `low`)
        for (axis, bound, low) in [(0, x0, true), (0, x1, false), (1, y0, true), (1, y1, false)] {
            let at = |v: &Vtx| if axis == 0 { v.x } else { v.y };
            let keep = |v: &Vtx| if low { at(v) >= bound } else { at(v) <= bound };
            let mut next = [Vtx { x: 0.0, y: 0.0, a: 0.0 }; 9];
            let mut m = 0;
            for i in 0..n {
                let (a, b) = (poly[i], poly[(i + 1) % n]);
                if keep(&a) {
                    next[m] = a;
                    m += 1;
                }
                if keep(&a) != keep(&b) {
                    let s = (bound - at(&a)) / (at(&b) - at(&a));
                    next[m] = Vtx { x: a.x + (b.x - a.x) * s, y: a.y + (b.y - a.y) * s, a: a.a + (b.a - a.a) * s };
                    if axis == 0 {
                        next[m].x = bound;
                    } else {
                        next[m].y = bound;
                    }
                    m += 1;
                }
            }
            poly = next;
            n = m;
            if n < 3 {
                break;
            }
        }
        for i in 1..n.saturating_sub(1) {
            out.extend_from_slice(&[poly[0], poly[i], poly[i + 1]]);
        }
    }
}

/// A fan around `c`: coverage 1 at the centre, 0 at radius 1.
fn disk(m: &mut Mesh<'_>, c: (f32, f32)) {
    let centre = Vtx { x: c.0, y: c.1, a: 1.0 };
    let at = |i: usize| Vtx { x: c.0 + DISK[i % 8].0, y: c.1 + DISK[i % 8].1, a: 0.0 };
    for i in 0..8 {
        m.tri(centre, at(i), at(i + 1));
    }
}

#[cfg(test)]
pub(crate) mod raster {
    //! A software rasterizer for the tests: what a GPU makes of a [`Vtx`] mesh. As hardware
    //! does, vertices snap to a sub-pixel grid (1/16 px) and edge functions are exact
    //! integers, so triangles sharing an edge are watertight; pixel centres on an edge go to
    //! one side by the top-left rule. Coverages add up ([`coverage`]: each piece of a shape
    //! covers its pixels once, so the sum is the shape's coverage) or blend "over" one another
    //! ([`coverage_over`]: overlapping pieces of one colour, as the GPU draws them).

    use super::Vtx;
    use alloc::vec;
    use alloc::vec::Vec;

    const SUB: f32 = 16.0;

    /// Coverage sums of an `n` × `n` grid.
    pub fn coverage(verts: &[Vtx], n: usize) -> Vec<f32> {
        raster(verts, n, |acc, c| *acc += c)
    }

    /// Coverage of an `n` × `n` grid as one colour blended "over" itself, triangle by
    /// triangle: overlaps combine as `a + b·(1 − a)` (what a GPU draws).
    pub fn coverage_over(verts: &[Vtx], n: usize) -> Vec<f32> {
        raster(verts, n, |acc, c| *acc += c * (1.0 - *acc))
    }

    fn raster(verts: &[Vtx], n: usize, add: impl Fn(&mut f32, f32)) -> Vec<f32> {
        let mut out = vec![0.0f32; n * n];
        let snap = |v: Vtx| (libm::roundf(v.x * SUB) as i64, libm::roundf(v.y * SUB) as i64, v.a);
        for t in verts.chunks_exact(3) {
            let (mut a, mut b, c) = (snap(t[0]), snap(t[1]), snap(t[2]));
            let cross = |p: (i64, i64, f32), q: (i64, i64, f32), x: i64, y: i64| (q.0 - p.0) * (y - p.1) - (q.1 - p.1) * (x - p.0);
            let area = cross(a, b, c.0, c.1);
            if area == 0 {
                continue;
            }
            if area < 0 {
                core::mem::swap(&mut a, &mut b);
            }
            let area = area.abs();
            // Top-left rule, y down, counter-clockwise on screen (interior on the positive
            // side): a top edge goes left, a left edge goes down.
            let owns = |p: (i64, i64, f32), q: (i64, i64, f32)| {
                let (dx, dy) = (q.0 - p.0, q.1 - p.1);
                (dy == 0 && dx < 0) || dy > 0
            };
            let xs = [a.0, b.0, c.0];
            let ys = [a.1, b.1, c.1];
            let (minx, maxx) = (*xs.iter().min().unwrap(), *xs.iter().max().unwrap());
            let (miny, maxy) = (*ys.iter().min().unwrap(), *ys.iter().max().unwrap());
            let px0 = (minx.div_euclid(16) - 1).max(0) as usize;
            let py0 = (miny.div_euclid(16) - 1).max(0) as usize;
            let px1 = ((maxx.div_euclid(16) + 1).max(0) as usize).min(n);
            let py1 = ((maxy.div_euclid(16) + 1).max(0) as usize).min(n);
            for py in py0..py1 {
                for px in px0..px1 {
                    let (x, y) = (px as i64 * 16 + 8, py as i64 * 16 + 8);
                    let w0 = cross(b, c, x, y);
                    let w1 = cross(c, a, x, y);
                    let w2 = cross(a, b, x, y);
                    let inside = |w: i64, p, q| w > 0 || (w == 0 && owns(p, q));
                    if inside(w0, b, c) && inside(w1, c, a) && inside(w2, a, b) {
                        let cov = (w0 as f32 * a.2 + w1 as f32 * b.2 + w2 as f32 * c.2) / area as f32;
                        add(&mut out[py * n + px], cov);
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mesh_of(f: impl FnOnce(&mut Mesh<'_>)) -> (Vec<Vtx>, Vec<Chunk>) {
        let (mut verts, mut chunks) = (Vec::new(), Vec::new());
        f(&mut Mesh { verts: &mut verts, chunks: &mut chunks });
        (verts, chunks)
    }

    #[test]
    fn chunks_cover_their_triangles() {
        let (verts, chunks) = mesh_of(|m| rounded_ring(m, 5, 7, 60, 40, 12, 3));
        assert!(!chunks.is_empty());
        let mut next = 0;
        for c in &chunks {
            assert_eq!(c.start, next);
            assert_eq!(c.len % 3, 0);
            next = c.start + c.len;
            for v in &verts[c.start..c.start + c.len] {
                assert!(v.x >= c.x0 as f32 && v.x <= c.x1 as f32 && v.y >= c.y0 as f32 && v.y <= c.y1 as f32);
            }
        }
        assert_eq!(next, verts.len());
    }

    /// Clipped triangles cover inside the rect what the whole mesh covers there, nothing
    /// outside.
    #[test]
    fn clipping_keeps_the_coverage_inside_the_rect() {
        const N: usize = 64;
        let curve: Vec<(i32, i32)> = (0..50).map(|i| (6 + i, 30 + ((i * 13) % 17) - 8)).collect();
        let meshes = [
            mesh_of(|m| rounded_ring(m, 5, 7, 50, 40, 12, 3)).0,
            mesh_of(|m| rounded_fill(m, 3, 4, 40, 30, 9)).0,
            mesh_of(|m| polyline(m, &curve)).0,
        ];
        for verts in &meshes {
            let full = raster::coverage_over(verts, N);
            for (x0, y0, x1, y1) in [(10, 10, 30, 25), (0, 0, 64, 64), (20, 28, 21, 33), (40, 0, 64, 20)] {
                let mut cut = Vec::new();
                clip(verts, x0 as f32, y0 as f32, x1 as f32, y1 as f32, &mut cut);
                assert_eq!(cut.len() % 3, 0);
                let got = raster::coverage_over(&cut, N);
                for y in 0..N {
                    for x in 0..N {
                        let inside = (x0..x1).contains(&(x as i32)) && (y0..y1).contains(&(y as i32));
                        let want = if inside { full[y * N + x] } else { 0.0 };
                        // New vertices on the cut snap to the 1/16 px grid like any other,
                        // which moves the interpolated coverage a little: up to ~1/32 across
                        // a line's strip, whose coverage changes by 1 a pixel (8/255).
                        let g = got[y * N + x];
                        assert!((g - want).abs() < 0.04, "({x}, {y}) in {x0},{y0}..{x1},{y1}: {g} vs {want}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_square_box_is_two_triangles() {
        let (verts, chunks) = mesh_of(|m| rounded_fill(m, 3, 4, 10, 6, 0));
        assert_eq!(verts.len(), 6);
        assert_eq!(chunks, [Chunk { start: 0, len: 6, x0: 3, y0: 4, x1: 13, y1: 10 }]);
    }
}
