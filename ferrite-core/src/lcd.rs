/// Backend abstraction for LCD primitives.
/// Hardware backends implement this trait; the framework uses LcdImpl<B> for all drawing.
use core::cell::Cell;

use crate::types::{Offset, Rect};

/// Anti-aliased variants of the shape primitives, used on blend-capable
/// backends (`LcdBackend::HAS_ALPHA`).
mod smooth;

/// Maximum vertices a polygon shape can hold (bounds the scanline-fill
/// intersection buffer and the point parser; 16 × 4 B = 64 B of stack).
pub const MAX_POLY_POINTS: usize = 16;

pub trait LcdBackend {
    /// Panel dimensions in pixels. The framework builds its screen-clip rect
    /// from these instead of a feature-gated module const.
    const WIDTH: u16;
    const HEIGHT: u16;

    /// True when the backend can alpha-blend against existing pixels
    /// (requires framebuffer readback — impossible on the write-only Nextion
    /// FPGA bus). Backends that set this must override `blend_rect` and
    /// `blend_pixel`. Mirror the value in `tools/devices/<id>.json`
    /// (`caps.alpha` / `caps.image_alpha`).
    const HAS_ALPHA: bool = false;

    fn begin_frame(&mut self);
    fn end_frame(&mut self);
    fn back_buf(&self) -> u8;

    fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16);
    fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16);
    fn write_pixel(&self, color: u16);

    /// Stream `colors` into the `begin_pixels` window — same as calling
    /// `write_pixel` for each. Frame-buffer backends override this to clip
    /// once per row and copy straight into memory.
    fn write_pixels(&self, colors: &[u16]) {
        for &c in colors {
            self.write_pixel(c);
        }
    }

    /// Stream `n` pixels of one `color` into the `begin_pixels` window (RLE
    /// runs) — same as `n` calls to `write_pixel`.
    fn write_run(&self, color: u16, n: usize) {
        for _ in 0..n {
            self.write_pixel(color);
        }
    }

    fn send_command(&self, cmd: u16);
    fn send_data(&self, data: u16);

    /// Optional: flush dirty region (e-paper panels).
    fn flush_dirty(&self) {}

    /// Optional hook fired before a full redraw. E-paper panels override this to
    /// ghost-erase (drive all pixels white) so prior dark content does not bleed
    /// through. No-op on framebuffer panels.
    fn pre_full_redraw(&self) {}

    /// Default: fill a 1x1 rect (no bounds check — backend handles clipping).
    fn draw_pixel(&self, x: u16, y: u16, color: u16) {
        self.fill_rect(x, y, 1, 1, color);
    }

    /// Blend a solid rect over existing content. Default (no readback):
    /// opaque fill — a translucent widget background degrades to solid,
    /// which is better than half-missing UI. The designer hides the alpha
    /// editor for devices without `caps.alpha`, so this only triggers when
    /// an image built for an alpha-capable board runs elsewhere.
    fn blend_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16, alpha: u8) {
        let _ = alpha;
        self.fill_rect(x, y, w, h, color);
    }

    /// Blend a single pixel over existing content. Default (no readback):
    /// 1-bit cutout at threshold 128 — alpha images degrade to hard-edged
    /// transparency, preserving sprite silhouettes.
    fn blend_pixel(&self, x: u16, y: u16, color: u16, alpha: u8) {
        if alpha >= 128 {
            self.draw_pixel(x, y, color);
        }
    }
}

/// Generic LCD wrapper — backend-agnostic drawing primitives.
pub struct LcdImpl<B: LcdBackend> {
    be: B,
    /// Optional scissor rect. When set, `fill_rect` / `draw_pixel` (and so
    /// every primitive built on them — circles, ellipses, polygons, lines)
    /// are clipped to it. Used by the clipped draw path to confine the
    /// geometry-drawing shape primitives, which take no clip of their own.
    /// `None` (the normal case) is a single branch with no other cost.
    scissor: Cell<Option<Rect>>,
}

impl<B: LcdBackend> LcdImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self {
            be,
            scissor: Cell::new(None),
        }
    }

    /// Set (or clear) the scissor rect honored by `fill_rect`/`draw_pixel`.
    /// Callers must restore it to `None` when done.
    #[inline]
    pub fn set_scissor(&self, rect: Option<Rect>) {
        self.scissor.set(rect);
    }

    /// The wrapped backend (tests read recorded pixels through it).
    #[cfg(test)]
    pub(crate) fn backend(&self) -> &B {
        &self.be
    }

    /// Current scissor rect (`None` = unclipped).
    #[inline]
    pub fn scissor(&self) -> Option<Rect> {
        self.scissor.get()
    }

    // --- Primitives (pass-through to backend) ---

    pub fn begin_frame(&mut self) {
        self.be.begin_frame()
    }
    pub fn end_frame(&mut self) {
        self.be.end_frame()
    }
    pub fn back_buf(&self) -> u8 {
        self.be.back_buf()
    }

    #[inline]
    pub fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        if let Some((x, y, w, h)) = self.scissored(x, y, w, h) {
            self.be.fill_rect(x, y, w, h, color)
        }
    }

    /// Intersect a rect with the scissor rect; `None` when nothing is left.
    /// Without a scissor the rect passes through unchanged.
    #[inline]
    fn scissored(&self, x: u16, y: u16, w: u16, h: u16) -> Option<(u16, u16, u16, u16)> {
        let Some(c) = self.scissor.get() else {
            return Some((x, y, w, h));
        };
        // i32 math avoids u16 wrap when the rect starts left/above the scissor.
        let x0 = (x as i32).max(c.x as i32);
        let y0 = (y as i32).max(c.y as i32);
        let x1 = (x as i32 + w as i32).min(c.x as i32 + c.w as i32);
        let y1 = (y as i32 + h as i32).min(c.y as i32 + c.h as i32);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some((x0 as u16, y0 as u16, (x1 - x0) as u16, (y1 - y0) as u16))
    }

    #[inline]
    pub fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16) {
        self.be.begin_pixels(x, y, w, h)
    }

    #[inline(always)]
    pub fn write_pixel(&self, color: u16) {
        self.be.write_pixel(color)
    }

    #[inline]
    pub fn write_pixels(&self, colors: &[u16]) {
        self.be.write_pixels(colors)
    }

    #[inline]
    pub fn write_run(&self, color: u16, n: usize) {
        self.be.write_run(color, n)
    }

    #[inline]
    pub fn draw_pixel(&self, x: u16, y: u16, color: u16) {
        if self.scissored_out(x, y) {
            return;
        }
        self.be.draw_pixel(x, y, color)
    }

    /// True when a scissor rect is set and (x, y) lies outside it.
    #[inline]
    fn scissored_out(&self, x: u16, y: u16) -> bool {
        match self.scissor.get() {
            Some(c) => {
                let xi = x as i32;
                let yi = y as i32;
                xi < c.x as i32
                    || yi < c.y as i32
                    || xi >= c.x as i32 + c.w as i32
                    || yi >= c.y as i32 + c.h as i32
            }
            None => false,
        }
    }

    /// Whether the backend supports true alpha blending.
    #[inline]
    pub fn has_alpha(&self) -> bool {
        B::HAS_ALPHA
    }

    #[inline]
    pub fn blend_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16, alpha: u8) {
        if let Some((x, y, w, h)) = self.scissored(x, y, w, h) {
            self.be.blend_rect(x, y, w, h, color, alpha)
        }
    }

    #[inline]
    pub fn blend_pixel(&self, x: u16, y: u16, color: u16, alpha: u8) {
        if self.scissored_out(x, y) {
            return;
        }
        self.be.blend_pixel(x, y, color, alpha)
    }

    #[inline(always)]
    pub fn send_command(&self, cmd: u16) {
        self.be.send_command(cmd)
    }

    #[inline(always)]
    pub fn send_data(&self, data: u16) {
        self.be.send_data(data)
    }

    #[inline]
    pub fn flush_dirty(&self) {
        self.be.flush_dirty()
    }

    #[inline]
    pub fn pre_full_redraw(&self) {
        self.be.pre_full_redraw()
    }

    // --- Geometry (backend-agnostic, uses only primitives above) ---

    pub fn draw_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        if w == 0 || h == 0 {
            return;
        }
        self.fill_rect(x, y, w, 1, color);
        if h > 1 {
            self.fill_rect(x, y + h - 1, w, 1, color);
        }
        if h > 2 {
            self.fill_rect(x, y + 1, 1, h - 2, color);
        }
        if w > 1 && h > 2 {
            self.fill_rect(x + w - 1, y + 1, 1, h - 2, color);
        }
    }

    pub fn draw_line(&self, x0: i16, y0: i16, x1: i16, y1: i16, color: u16) {
        if B::HAS_ALPHA {
            self.smooth_line(x0, y0, x1, y1, 128, color);
            return;
        }
        let mut x = x0;
        let mut y = y0;
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx: i16 = if x0 < x1 { 1 } else { -1 };
        let sy: i16 = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;

        loop {
            self.draw_pixel(x as u16, y as u16, color);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    /// Bresenham line with a pixel thickness. `thickness <= 1` is `draw_line`;
    /// thicker lines are approximated by parallel passes offset perpendicular
    /// to the dominant axis — adequate for the small stroke widths shapes use.
    /// Blend-capable backends draw one anti-aliased round-capped stroke.
    pub fn draw_line_thick(&self, x0: i16, y0: i16, x1: i16, y1: i16, color: u16, thickness: u16) {
        let t = thickness.max(1);
        if B::HAS_ALPHA {
            self.smooth_line(x0, y0, x1, y1, t as i32 * 128, color);
            return;
        }
        if t == 1 {
            self.draw_line(x0, y0, x1, y1, color);
            return;
        }
        let half = (t / 2) as i16;
        let dx = (x1 - x0).abs();
        let dy = (y1 - y0).abs();
        for i in 0..t as i16 {
            let off = i - half;
            if dx >= dy {
                // Mostly horizontal — offset in Y.
                self.draw_line(x0, y0 + off, x1, y1 + off, color);
            } else {
                // Mostly vertical — offset in X.
                self.draw_line(x0 + off, y0, x1 + off, y1, color);
            }
        }
    }

    /// Connect `points` with straight segments of the given thickness. When
    /// `closed` is true the last point is also joined back to the first.
    pub fn draw_polyline(&self, points: &[Offset], color: u16, closed: bool, thickness: u16) {
        if points.len() < 2 {
            return;
        }
        for i in 0..points.len() - 1 {
            let a = points[i];
            let b = points[i + 1];
            self.draw_line_thick(a.x, a.y, b.x, b.y, color, thickness);
        }
        if closed {
            let a = points[points.len() - 1];
            let b = points[0];
            self.draw_line_thick(a.x, a.y, b.x, b.y, color, thickness);
        }
    }

    /// Even-odd scanline fill of a closed polygon. No allocation: at most
    /// `points.len()` (≤ [`MAX_POLY_POINTS`]) edge intersections per scanline.
    pub fn fill_polygon(&self, points: &[Offset], color: u16) {
        let n = points.len();
        if n < 3 {
            return;
        }
        let mut y_min = points[0].y;
        let mut y_max = points[0].y;
        for p in &points[1..] {
            if p.y < y_min {
                y_min = p.y;
            }
            if p.y > y_max {
                y_max = p.y;
            }
        }

        let mut xs = [0i16; MAX_POLY_POINTS];
        let mut y = y_min;
        while y <= y_max {
            // Collect edge crossings of the scanline (PNPOLY half-open rule).
            let mut count = 0usize;
            let mut j = n - 1;
            for i in 0..n {
                let yi = points[i].y;
                let yj = points[j].y;
                if (yi > y) != (yj > y) {
                    let xi = points[i].x as i32;
                    let xj = points[j].x as i32;
                    let x = xi + (y as i32 - yi as i32) * (xj - xi) / (yj as i32 - yi as i32);
                    if count < xs.len() {
                        xs[count] = x as i16;
                        count += 1;
                    }
                }
                j = i;
            }
            // Insertion-sort the crossings, then fill between pairs.
            for a in 1..count {
                let key = xs[a];
                let mut b = a;
                while b > 0 && xs[b - 1] > key {
                    xs[b] = xs[b - 1];
                    b -= 1;
                }
                xs[b] = key;
            }
            let mut k = 0;
            while k + 1 < count {
                self.hline_clipped(xs[k], xs[k + 1], y, color);
                k += 2;
            }
            y += 1;
        }
    }

    pub fn draw_circle(&self, cx: i16, cy: i16, r: i16, color: u16) {
        if r <= 0 {
            return;
        }
        if B::HAS_ALPHA {
            self.smooth_ring(cx, cy, r, r, r, r, None, color, 255);
            return;
        }
        let mut x: i16 = 0;
        let mut y: i16 = r;
        let mut d: i16 = 1 - r;

        while x <= y {
            Self::circle_points_draw(self, cx, cy, x, y, color);
            if d < 0 {
                d += 2 * x + 3;
            } else {
                d += 2 * (x - y) + 5;
                y -= 1;
            }
            x += 1;
        }
    }

    pub fn fill_circle(&self, cx: i16, cy: i16, r: i16, color: u16) {
        if r <= 0 {
            return;
        }
        if B::HAS_ALPHA {
            self.smooth_ring(cx, cy, r, r, 0, 0, None, color, 255);
            return;
        }
        // Same per-row extent as `fill_ellipse`/`stroke_ellipse`, so a circle
        // shape's fill and stroke line up.
        for dy in -r..=r {
            let dx = ellipse_half_width(r, r, dy);
            self.hline_clipped(cx - dx, cx + dx, cy + dy, color);
        }
    }

    /// Solid ellipse with horizontal/vertical radii `rx`/`ry`, filled by one
    /// span per row via the ellipse equation (reuses `isqrt`).
    pub fn fill_ellipse(&self, cx: i16, cy: i16, rx: i16, ry: i16, color: u16) {
        if rx <= 0 || ry <= 0 {
            return;
        }
        if B::HAS_ALPHA {
            self.smooth_ring(cx, cy, rx, ry, 0, 0, None, color, 255);
            return;
        }
        for dy in -ry..=ry {
            let dx = ellipse_half_width(rx, ry, dy);
            self.hline_clipped(cx - dx, cx + dx, cy + dy, color);
        }
    }

    /// Ellipse outline `thickness` px wide, inward from radii `rx`/`ry`: the
    /// ring between the outer ellipse and one shrunk by `thickness`, filled
    /// one row at a time. Unlike stacking 1px outlines (whose rasterizations
    /// leave pinholes between neighbouring radii) the ring is solid, and it
    /// uses the same per-row extent as `fill_ellipse`/`fill_circle`, so it
    /// meets their fill exactly.
    pub fn stroke_ellipse(&self, cx: i16, cy: i16, rx: i16, ry: i16, thickness: i16, color: u16) {
        if rx <= 0 || ry <= 0 || thickness <= 0 {
            return;
        }
        if B::HAS_ALPHA {
            // Inner edge at (r - t) + ½, the same ring the spans below fill.
            let (irx, iry) = (rx - thickness + 1, ry - thickness + 1);
            self.smooth_ring(cx, cy, rx, ry, irx, iry, None, color, 255);
            return;
        }
        let (irx, iry) = (rx - thickness, ry - thickness);
        for dy in -ry..=ry {
            let xo = ellipse_half_width(rx, ry, dy);
            let y = cy + dy;
            if irx > 0 && iry > 0 && dy.abs() <= iry {
                let xi = ellipse_half_width(irx, iry, dy);
                self.hline_clipped(cx - xo, cx - xi - 1, y, color);
                self.hline_clipped(cx + xi + 1, cx + xo, y, color);
            } else {
                self.hline_clipped(cx - xo, cx + xo, y, color);
            }
        }
    }

    /// Ellipse inscribed in the `w`×`h` box at `(x, y)`: filled when `t == 0`,
    /// else a ring `t` px wide. Blend-capable backends draw it anti-aliased
    /// with the edge on the box border (half-pixel centre for even sizes), so
    /// the soft fringe stays inside the box; others use the centre/radius
    /// primitives at `(x + w/2, y + h/2)`, radii `w/2`, `h/2`.
    pub fn draw_oval(&self, x: i16, y: i16, w: u16, h: u16, t: u16, color: u16) {
        if B::HAS_ALPHA {
            self.smooth_oval(x, y, w, h, t, color);
            return;
        }
        let (rx, ry) = ((w / 2) as i16, (h / 2) as i16);
        if t == 0 {
            self.fill_ellipse(x + rx, y + ry, rx, ry, color);
        } else {
            self.stroke_ellipse(x + rx, y + ry, rx, ry, t as i16, color);
        }
    }

    /// 1px ellipse outline — integer midpoint ellipse algorithm, 4-way symmetry.
    pub fn draw_ellipse(&self, cx: i16, cy: i16, rx: i16, ry: i16, color: u16) {
        if rx <= 0 || ry <= 0 {
            return;
        }
        if B::HAS_ALPHA {
            self.smooth_ring(cx, cy, rx, ry, rx, ry, None, color, 255);
            return;
        }
        let (cxi, cyi) = (cx as i64, cy as i64);
        let (rx, ry) = (rx as i64, ry as i64);
        let (rx2, ry2) = (rx * rx, ry * ry);
        let (two_rx2, two_ry2) = (2 * rx2, 2 * ry2);

        let mut x: i64 = 0;
        let mut y: i64 = ry;
        let mut px: i64 = 0;
        let mut py: i64 = two_rx2 * y;

        // Region 1 — slope > -1.
        let mut p: i64 = ry2 - rx2 * ry + rx2 / 4;
        while px < py {
            self.plot4_ellipse(cxi, cyi, x, y, color);
            x += 1;
            px += two_ry2;
            if p < 0 {
                p += ry2 + px;
            } else {
                y -= 1;
                py -= two_rx2;
                p += ry2 + px - py;
            }
        }
        // Region 2 — slope < -1.
        let mut p2: i64 = ry2 * (2 * x + 1) * (2 * x + 1) / 4 + rx2 * (y - 1) * (y - 1) - rx2 * ry2;
        while y >= 0 {
            self.plot4_ellipse(cxi, cyi, x, y, color);
            y -= 1;
            py -= two_rx2;
            if p2 > 0 {
                p2 += rx2 - py;
            } else {
                x += 1;
                px += two_ry2;
                p2 += rx2 - py + px;
            }
        }
    }

    #[inline]
    fn plot4_ellipse(&self, cx: i64, cy: i64, x: i64, y: i64, color: u16) {
        self.plot((cx + x) as i16, (cy + y) as i16, color);
        self.plot((cx - x) as i16, (cy + y) as i16, color);
        self.plot((cx + x) as i16, (cy - y) as i16, color);
        self.plot((cx - x) as i16, (cy - y) as i16, color);
    }

    fn circle_points_draw(&self, cx: i16, cy: i16, x: i16, y: i16, color: u16) {
        self.plot(cx + x, cy + y, color);
        self.plot(cx - x, cy + y, color);
        self.plot(cx + x, cy - y, color);
        self.plot(cx - x, cy - y, color);
        self.plot(cx + y, cy + x, color);
        self.plot(cx - y, cy + x, color);
        self.plot(cx + y, cy - x, color);
        self.plot(cx - y, cy - x, color);
    }

    #[inline]
    fn plot(&self, x: i16, y: i16, color: u16) {
        self.draw_pixel(x as u16, y as u16, color);
    }

    fn hline_clipped(&self, x0: i16, x1: i16, y: i16, color: u16) {
        let left = x0.max(0) as u16;
        let right = x1;
        if right < 0 || left > right as u16 {
            return;
        }
        let w = right as u16 - left + 1;
        self.fill_rect(left, y as u16, w, 1, color);
    }

    fn vline_clipped(&self, x: i16, y0: i16, y1: i16, color: u16) {
        let top = y0.max(0) as u16;
        let bottom = y1;
        if bottom < 0 || top > bottom as u16 {
            return;
        }
        let h = bottom as u16 - top + 1;
        self.fill_rect(x as u16, top, 1, h, color);
    }

    pub fn draw_rounded_rect(&self, x: u16, y: u16, w: u16, h: u16, r: u16, color: u16) {
        if w == 0 || h == 0 {
            return;
        }
        if B::HAS_ALPHA && r > 0 {
            self.smooth_round_rect(x as i16, y as i16, w, h, r, 1, color, 255);
            return;
        }
        let r = r.min(w / 2).min(h / 2);
        if r == 0 {
            self.draw_rect(x, y, w, h, color);
            return;
        }
        let xi = x as i16;
        let yi = y as i16;
        let wi = w as i16;
        let hi = h as i16;
        let ri = r as i16;

        self.hline_clipped(xi + ri, xi + wi - ri - 1, yi, color);
        self.hline_clipped(xi + ri, xi + wi - ri - 1, yi + hi - 1, color);
        self.vline_clipped(xi, yi + ri, yi + hi - ri - 1, color);
        self.vline_clipped(xi + wi - 1, yi + ri, yi + hi - ri - 1, color);

        let cx_tl = xi + ri;
        let cy_tl = yi + ri;
        let cx_tr = xi + wi - ri - 1;
        let cy_tr = yi + ri;
        let cx_bl = xi + ri;
        let cy_bl = yi + hi - ri - 1;
        let cx_br = xi + wi - ri - 1;
        let cy_br = yi + hi - ri - 1;

        let mut px: i16 = 0;
        let mut py: i16 = ri;
        let mut d: i16 = 1 - ri;

        while px <= py {
            self.plot(cx_tl - py, cy_tl - px, color);
            self.plot(cx_tl - px, cy_tl - py, color);
            self.plot(cx_tr + py, cy_tr - px, color);
            self.plot(cx_tr + px, cy_tr - py, color);
            self.plot(cx_bl - py, cy_bl + px, color);
            self.plot(cx_bl - px, cy_bl + py, color);
            self.plot(cx_br + py, cy_br + px, color);
            self.plot(cx_br + px, cy_br + py, color);

            if d < 0 {
                d += 2 * px + 3;
            } else {
                d += 2 * (px - py) + 5;
                py -= 1;
            }
            px += 1;
        }
    }

    /// Rounded-rect border `t` px wide: nested 1 px outlines, or one
    /// anti-aliased ring on blend-capable backends (stacked anti-aliased
    /// outlines would blend over each other's edges).
    #[allow(clippy::too_many_arguments)]
    pub fn stroke_rounded_rect(&self, x: i16, y: i16, w: u16, h: u16, r: u16, t: u16, color: u16) {
        if t == 0 || w == 0 || h == 0 {
            return;
        }
        if B::HAS_ALPHA && r > 0 {
            self.smooth_round_rect(x, y, w, h, r, t, color, 255);
            return;
        }
        for i in 0..t {
            let w = w.saturating_sub(i * 2);
            let h = h.saturating_sub(i * 2);
            if w == 0 || h == 0 {
                break;
            }
            let (xi, yi) = (x + i as i16, y + i as i16);
            if xi >= 0 && yi >= 0 {
                self.draw_rounded_rect(xi as u16, yi as u16, w, h, r.saturating_sub(i), color);
            }
        }
    }

    pub fn fill_rounded_rect(&self, x: u16, y: u16, w: u16, h: u16, r: u16, color: u16) {
        if w == 0 || h == 0 {
            return;
        }
        if B::HAS_ALPHA && r > 0 {
            self.smooth_round_rect(x as i16, y as i16, w, h, r, 0, color, 255);
            return;
        }
        let r = r.min(w / 2).min(h / 2);
        if r == 0 {
            self.fill_rect(x, y, w, h, color);
            return;
        }

        // One horizontal span per row: straight full-width band through the
        // middle, corners inset by the circle equation. Same geometry as
        // `fill_rounded_rect_blend`, just opaque.
        let ri = r as i32;
        let hi = h as i32;
        for row in 0..hi {
            let dy = if row < ri {
                ri - row
            } else if row >= hi - ri {
                row - (hi - 1 - ri)
            } else {
                0
            };
            let inset = if dy == 0 {
                0
            } else {
                (ri - isqrt(ri * ri - dy * dy) as i32) as u16
            };
            self.fill_rect(x + inset, y + row as u16, w - 2 * inset, 1, color);
        }
    }

    /// `fill_rounded_rect` with alpha. Built from one span per row (circle
    /// inset via `isqrt`) instead of the Midpoint Circle layout: the opaque
    /// version overdraws where corner spans meet the middle band — harmless
    /// when solid, but overlapping spans would blend twice and show darker
    /// bands. On non-alpha backends each span degrades to an opaque fill
    /// (see `LcdBackend::blend_rect`).
    pub fn fill_rounded_rect_blend(
        &self,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        r: u16,
        color: u16,
        alpha: u8,
    ) {
        if w == 0 || h == 0 {
            return;
        }
        if B::HAS_ALPHA && r > 0 {
            self.smooth_round_rect(x as i16, y as i16, w, h, r, 0, color, alpha);
            return;
        }
        let r = r.min(w / 2).min(h / 2);
        if r == 0 {
            self.blend_rect(x, y, w, h, color, alpha);
            return;
        }
        let ri = r as i32;
        let hi = h as i32;
        for row in 0..hi {
            // Distance from the straight-edged middle band into the arcs.
            let dy = if row < ri {
                ri - row
            } else if row >= hi - ri {
                row - (hi - 1 - ri)
            } else {
                0
            };
            let inset = if dy == 0 {
                0
            } else {
                (ri - isqrt(ri * ri - dy * dy) as i32) as u16
            };
            self.blend_rect(
                x + inset,
                y + row as u16,
                w - 2 * inset,
                1,
                color,
                alpha,
            );
        }
    }

    pub fn draw_gradient(&self, full: &Rect, c1: u16, c2: u16, dir: u8) {
        if full.is_empty() {
            return;
        }
        let (grad_w, grad_h) = if dir == 1 {
            (full.w, 1u16)
        } else {
            (1u16, full.h)
        };

        if grad_w == 0 && grad_h == 0 {
            return;
        }

        if dir == 1 {
            for col in 0..full.w {
                let color = lerp_rgb565(c1, c2, col, grad_w);
                self.vline_clipped(
                    full.x + col as i16,
                    full.y,
                    full.y + full.h as i16 - 1,
                    color,
                );
            }
        } else {
            for row in 0..full.h {
                let color = lerp_rgb565(c1, c2, row, grad_h);
                self.hline_clipped(
                    full.x,
                    full.x + full.w as i16 - 1,
                    full.y + row as i16,
                    color,
                );
            }
        }
    }

    /// Draw gradient bar between x0..x1 with lerp from c1 to c2.
    /// Clips to screen and full rect.
    pub fn hline_gradient(
        &self,
        full: &Rect,
        x0: i16,
        x1: i16,
        y: i16,
        c1: u16,
        c2: u16,
        grad_w: u16,
    ) {
        let full_x = full.x;
        let full_w = full.w as i16;
        let left = x0.max(full_x).max(0) as u16;
        let right = x1.min(full_x + full_w - 1);
        if right < 0 || left > right as u16 {
            return;
        }
        let w = right as u16 - left + 1;
        let x_base = left.saturating_sub(full_x.max(0) as u16);
        self.begin_pixels(left as u16, y as u16, w, 1);
        let mut grad = GradientStep::new(c1, c2, x_base, grad_w);
        for _ in 0..w {
            self.write_pixel(grad.next());
        }
    }

    pub fn draw_arc(&self, cx: i16, cy: i16, r: i16, start: i16, end: i16, color: u16) {
        if r <= 0 {
            return;
        }
        if B::HAS_ALPHA {
            self.smooth_ring(cx, cy, r, r, r, r, Some((start, end)), color, 255);
            return;
        }
        let start = ((start % 360) + 360) % 360;
        let end = ((end % 360) + 360) % 360;

        let mut deg = start;
        loop {
            let (sin_val, cos_val) = sin_cos_deg(deg);
            let px = cx + ((r as i32 * cos_val as i32) >> 8) as i16;
            let py = cy + ((r as i32 * sin_val as i32) >> 8) as i16;
            self.plot(px, py, color);

            if deg == end {
                break;
            }
            deg += 1;
            if deg >= 360 {
                deg = 0;
            }
        }
    }

    /// Thick arc: the ring between radii `ir..=r`, clockwise from `start` to
    /// `end` degrees (`draw_arc` convention). Stacked 1 px arcs, or one
    /// anti-aliased ring sector (with smoothed ends) on blend-capable backends.
    #[allow(clippy::too_many_arguments)]
    pub fn fill_arc(&self, cx: i16, cy: i16, r: i16, ir: i16, start: i16, end: i16, color: u16) {
        if r <= 0 {
            return;
        }
        if B::HAS_ALPHA {
            self.smooth_ring(cx, cy, r, r, ir, ir, Some((start, end)), color, 255);
            return;
        }
        for dr in ir..=r {
            self.draw_arc(cx, cy, dr, start, end, color);
        }
    }

    /// Fill a rectangle with a linear gradient. The gradient is sampled as if this
    /// rect were a window at `(x_off, y_off)` into a larger `full_w × full_h` field
    /// (so adjacent clipped pieces share a continuous gradient). `dir`: 1=horizontal,
    /// else vertical. Screen clipping is the caller's responsibility.
    pub fn fill_gradient_rect(
        &self,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        c1: u16,
        c2: u16,
        dir: u8,
        x_off: u16,
        y_off: u16,
        full_w: u16,
        full_h: u16,
    ) {
        if w == 0 || h == 0 {
            return;
        }
        match dir {
            1 => {
                // Horizontal: one begin_pixels covers the full rect; FPGA auto-advances rows.
                let grad_w = full_w.saturating_sub(1);
                self.begin_pixels(x, y, w, h);
                for _row in 0..h {
                    let mut grad = GradientStep::new(c1, c2, x_off, grad_w);
                    for _ in 0..w {
                        self.write_pixel(grad.next());
                    }
                }
            }
            _ => {
                // Vertical (default): one color per row — efficient full-row fill.
                let grad_h = full_h.saturating_sub(1);
                for row in 0..h {
                    let color = lerp_rgb565(c1, c2, y_off + row, grad_h);
                    self.fill_rect(x, y + row, w, 1, color);
                }
            }
        }
    }

    /// Fill a rounded rectangle with a linear gradient. Same Midpoint Circle algorithm
    /// as `fill_rounded_rect`, computing the gradient color per horizontal span.
    /// `dir`: 1=horizontal, else vertical. Screen clipping is the caller's responsibility.
    pub fn fill_gradient_rounded_rect(
        &self,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        r: u16,
        c1: u16,
        c2: u16,
        dir: u8,
    ) {
        if w == 0 || h == 0 {
            return;
        }
        let r = r.min(w / 2).min(h / 2);
        if r == 0 {
            self.fill_gradient_rect(x, y, w, h, c1, c2, dir, 0, 0, w, h);
            return;
        }

        let xi = x as i16;
        let yi = y as i16;
        let wi = w as i16;
        let hi = h as i16;
        let ri = r as i16;

        let cx_l = xi + ri;
        let cx_r = xi + wi - ri - 1;
        let cy_t = yi + ri;
        let cy_b = yi + hi - ri - 1;

        match dir {
            1 => {
                // Horizontal gradient — per-pixel horizontal spans.
                let grad_w = w.saturating_sub(1);
                for row_y in (y + r)..(y + h - r) {
                    self.hspan_gradient(xi, xi + wi - 1, row_y as i16, xi, grad_w, c1, c2);
                }
                let mut px: i16 = 0;
                let mut py: i16 = ri;
                let mut d: i16 = 1 - ri;
                while px <= py {
                    self.hspan_gradient(cx_l - py, cx_r + py, cy_t - px, xi, grad_w, c1, c2);
                    self.hspan_gradient(cx_l - py, cx_r + py, cy_b + px, xi, grad_w, c1, c2);
                    if px != py {
                        self.hspan_gradient(cx_l - px, cx_r + px, cy_t - py, xi, grad_w, c1, c2);
                        self.hspan_gradient(cx_l - px, cx_r + px, cy_b + py, xi, grad_w, c1, c2);
                    }
                    if d < 0 {
                        d += 2 * px + 3;
                    } else {
                        d += 2 * (px - py) + 5;
                        py -= 1;
                    }
                    px += 1;
                }
            }
            _ => {
                // Vertical gradient — one color per horizontal span row.
                let grad_h = h.saturating_sub(1);
                for row_y in (y + r)..(y + h - r) {
                    let color = lerp_rgb565(c1, c2, row_y - y, grad_h);
                    self.hline_clipped(xi, xi + wi - 1, row_y as i16, color);
                }
                let mut px: i16 = 0;
                let mut py: i16 = ri;
                let mut d: i16 = 1 - ri;
                while px <= py {
                    let c_tp = lerp_rgb565(c1, c2, (cy_t - px - yi).max(0) as u16, grad_h);
                    let c_bp = lerp_rgb565(c1, c2, (cy_b + px - yi).max(0) as u16, grad_h);
                    self.hline_clipped(cx_l - py, cx_r + py, cy_t - px, c_tp);
                    self.hline_clipped(cx_l - py, cx_r + py, cy_b + px, c_bp);
                    if px != py {
                        let c_tq = lerp_rgb565(c1, c2, (cy_t - py - yi).max(0) as u16, grad_h);
                        let c_bq = lerp_rgb565(c1, c2, (cy_b + py - yi).max(0) as u16, grad_h);
                        self.hline_clipped(cx_l - px, cx_r + px, cy_t - py, c_tq);
                        self.hline_clipped(cx_l - px, cx_r + px, cy_b + py, c_bq);
                    }
                    if d < 0 {
                        d += 2 * px + 3;
                    } else {
                        d += 2 * (px - py) + 5;
                        py -= 1;
                    }
                    px += 1;
                }
            }
        }
    }

    /// Draw a horizontal gradient span, writing each pixel with its interpolated
    /// color. Clips to `x >= 0` and the scissor rect (the pixel stream bypasses
    /// `fill_rect`); the caller bounds the right/top/bottom screen edges.
    fn hspan_gradient(&self, x0: i16, x1: i16, y: i16, full_x: i16, grad_w: u16, c1: u16, c2: u16) {
        let (mut x0, mut x1) = (x0, x1);
        if let Some(c) = self.scissor.get() {
            if y < c.y || y >= c.y + c.h as i16 {
                return;
            }
            x0 = x0.max(c.x);
            x1 = x1.min(c.x + c.w as i16 - 1);
        }
        let left = x0.max(0) as u16;
        let right = x1;
        if right < 0 || left > right as u16 {
            return;
        }
        let w = right as u16 - left + 1;
        let x_base = left.saturating_sub(full_x.max(0) as u16);
        self.begin_pixels(left, y as u16, w, 1);
        let mut grad = GradientStep::new(c1, c2, x_base, grad_w);
        for _ in 0..w {
            self.write_pixel(grad.next());
        }
    }
}

// === RGB565 gradient interpolation ===

/// Linear interpolation between two RGB565 colors.
/// `t` is position in [0, `n`]; returns `c1` when `t`=0, `c2` when `t`=`n`.
#[inline]
pub fn lerp_rgb565(c1: u16, c2: u16, t: u16, n: u16) -> u16 {
    if n == 0 {
        return c1;
    }
    lerp_t8(c1, c2, (t.min(n) as u32 * 256) / n as u32)
}

/// Steps `lerp_rgb565(c1, c2, t, n)` for t = t0, t0+1, … without a divide
/// per pixel (ARMv5 has no divide instruction). Keeps the exact quotient
/// `t * 256 / n` as quotient + remainder, so every value matches
/// `lerp_rgb565` bit for bit.
struct GradientStep {
    c1: u16,
    c2: u16,
    /// `min(t, n) * 256 / n` and its remainder.
    t8: u32,
    rem: u32,
    /// Per-step increment of the quotient/remainder (`256 / n`, `256 % n`).
    dq: u32,
    dr: u32,
    n: u32,
    /// Steps left before `t` reaches `n` (then `t8` stays at 256).
    left: u32,
}

impl GradientStep {
    fn new(c1: u16, c2: u16, t0: u16, n: u16) -> Self {
        let n32 = n.max(1) as u32;
        let t = t0.min(n) as u32;
        let num = t * 256;
        Self {
            c1,
            c2,
            t8: num / n32,
            rem: num % n32,
            dq: 256 / n32,
            dr: 256 % n32,
            n: n32,
            left: (n as u32).saturating_sub(t),
        }
    }

    /// Color at the current `t`, then advance `t` by one.
    #[inline]
    fn next(&mut self) -> u16 {
        let color = lerp_t8(self.c1, self.c2, self.t8);
        if self.left > 0 {
            self.left -= 1;
            self.t8 += self.dq;
            self.rem += self.dr;
            if self.rem >= self.n {
                self.rem -= self.n;
                self.t8 += 1;
            }
        }
        color
    }
}

/// `lerp_rgb565` with the 0..=256 weight already computed.
#[inline]
fn lerp_t8(c1: u16, c2: u16, t8: u32) -> u16 {
    let r1 = (c1 >> 11) as u32;
    let g1 = ((c1 >> 5) & 0x3F) as u32;
    let b1 = (c1 & 0x1F) as u32;
    let r2 = (c2 >> 11) as u32;
    let g2 = ((c2 >> 5) & 0x3F) as u32;
    let b2 = (c2 & 0x1F) as u32;
    let nt8 = 256 - t8;
    let r = (r1 * nt8 + r2 * t8) >> 8;
    let g = (g1 * nt8 + g2 * t8) >> 8;
    let b = (b1 * nt8 + b2 * t8) >> 8;
    ((r as u16) << 11) | ((g as u16) << 5) | (b as u16)
}

// === Sin/Cos lookup table (Q8 fixed-point, 0..90 degrees) ===

static SIN_TABLE: [u16; 91] = [
    0, 4, 9, 13, 18, 22, 27, 31, 36, 40, 44, 49, 53, 57, 62, 66, 70, 75, 79, 83, 87, 91, 96, 100,
    104, 108, 112, 116, 120, 124, 128, 131, 135, 139, 143, 146, 150, 154, 157, 161, 164, 167, 171,
    174, 177, 181, 184, 187, 190, 193, 196, 198, 201, 204, 207, 209, 212, 214, 217, 219, 221, 223,
    226, 228, 230, 232, 233, 235, 237, 238, 240, 242, 243, 244, 246, 247, 248, 249, 250, 251, 252,
    252, 253, 254, 254, 255, 255, 255, 256, 256, 256,
];

/// Q8 fixed-point sin/cos for `deg` (0–360).
pub fn sin_cos_deg(deg: i16) -> (i16, i16) {
    let d = ((deg % 360) + 360) % 360;

    let sin_val = match d {
        0..=90 => SIN_TABLE[d as usize] as i16,
        91..=180 => SIN_TABLE[(180 - d) as usize] as i16,
        181..=270 => -(SIN_TABLE[(d - 180) as usize] as i16),
        _ => -(SIN_TABLE[(360 - d) as usize] as i16),
    };

    let cos_val = match d {
        0..=90 => SIN_TABLE[(90 - d) as usize] as i16,
        91..=180 => -(SIN_TABLE[(d - 90) as usize] as i16),
        181..=270 => -(SIN_TABLE[(270 - d) as usize] as i16),
        _ => SIN_TABLE[(d - 270) as usize] as i16,
    };

    (sin_val, cos_val)
}

/// Half-width of the ellipse with radii `rx`/`ry` on row `dy` (|dy| <= ry).
///
/// Measured against radii + ½ (pixel centres), in doubled units to stay in
/// integers: `X = 2rx+1`, `Y = 2ry+1`, `D = 2dy`, half-width =
/// `X·sqrt(Y² − D²) / Y / 2`. With the bare radii the top/bottom rows
/// shrink to a single-pixel nub and the widest row sticks out by one — a
/// stroke in a contrasting color shows them as ticks at the four extremes.
fn ellipse_half_width(rx: i16, ry: i16, dy: i16) -> i16 {
    let x2 = (2 * rx as i64 + 1).pow(2);
    let y2 = (2 * ry as i64 + 1).pow(2);
    let d2 = (2 * dy as i64).pow(2);
    (isqrt((x2 * (y2 - d2) / y2) as i32) / 2) as i16
}

// === Integer sqrt ===

fn isqrt(n: i32) -> u16 {
    let mut op = n as u32;
    let mut res = 0u32;
    let mut one = 1u32 << 30;

    while one > op {
        one >>= 2;
    }

    while one != 0 {
        if op >= res + one {
            op -= res + one;
            res = (res >> 1) + one;
        } else {
            res >>= 1;
        }
        one >>= 2;
    }

    res as u16
}

#[cfg(feature = "mock")]
pub type Lcd = LcdImpl<crate::mock::MockLcd>;

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    use std::vec::Vec;

    const N: usize = 64;

    /// Records which pixels `fill_rect` touched (N×N grid).
    struct Grid(RefCell<Vec<u8>>);
    impl LcdBackend for Grid {
        const WIDTH: u16 = N as u16;
        const HEIGHT: u16 = N as u16;
        fn begin_frame(&mut self) {}
        fn end_frame(&mut self) {}
        fn back_buf(&self) -> u8 {
            0
        }
        fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
            for yy in y as usize..(y as usize + h as usize).min(N) {
                for xx in x as usize..(x as usize + w as usize).min(N) {
                    self.0.borrow_mut()[yy * N + xx] = color as u8;
                }
            }
        }
        fn begin_pixels(&self, _: u16, _: u16, _: u16, _: u16) {}
        fn write_pixel(&self, _: u16) {}
        fn send_command(&self, _: u16) {}
        fn send_data(&self, _: u16) {}
    }

    fn grid() -> LcdImpl<Grid> {
        LcdImpl::with_backend(Grid(RefCell::new(vec![0; N * N])))
    }

    #[test]
    fn fill_circle_matches_fill_ellipse() {
        for r in [1i16, 2, 5, 17, 30] {
            let (c, e) = (grid(), grid());
            c.fill_circle(32, 32, r, 1);
            e.fill_ellipse(32, 32, r, r, 1);
            assert!(*c.backend().0.borrow() == *e.backend().0.borrow(), "r={r}");
        }
    }

    #[test]
    fn stroke_ring_is_solid_and_meets_the_fill() {
        for (rx, ry) in [(30i16, 30i16), (25, 25), (30, 18), (12, 29), (5, 5), (2, 7)] {
            for t in [1i16, 2, 3, 5, 8, 40] {
                // Reference: the outer ellipse filled.
                let full = grid();
                full.fill_ellipse(32, 32, rx, ry, 1);
                // Ring + inner fill must give exactly the same pixel set,
                // and the ring must not overlap the inner fill.
                let ring = grid();
                ring.stroke_ellipse(32, 32, rx, ry, t, 2);
                let inner = grid();
                if rx - t > 0 && ry - t > 0 {
                    inner.fill_ellipse(32, 32, rx - t, ry - t, 1);
                }
                let (f, r, i) = (full.backend().0.borrow(), ring.backend().0.borrow(), inner.backend().0.borrow());
                for p in 0..N * N {
                    assert!(!(r[p] != 0 && i[p] != 0), "overlap rx={rx} ry={ry} t={t} p={p}");
                    assert_eq!(f[p] != 0, r[p] != 0 || i[p] != 0, "gap/excess rx={rx} ry={ry} t={t} p={p}");
                }
            }
        }
    }

    #[test]
    fn gradient_step_matches_lerp() {
        let pairs = [(0x0000u16, 0xFFFFu16), (0xF800, 0x07E0), (0x1234, 0xABCD), (0xFFFF, 0x0000)];
        for &(c1, c2) in &pairs {
            for n in [0u16, 1, 2, 3, 7, 99, 255, 256, 257, 479, 799] {
                for t0 in [0u16, 1, 5, n / 2, n] {
                    let mut g = GradientStep::new(c1, c2, t0, n);
                    // Walk past `n` too: the weight must saturate like `t.min(n)`.
                    for t in t0..t0.saturating_add(n + 20) {
                        assert_eq!(g.next(), lerp_rgb565(c1, c2, t, n), "c1={c1:#x} c2={c2:#x} n={n} t={t}");
                    }
                }
            }
        }
    }
}
