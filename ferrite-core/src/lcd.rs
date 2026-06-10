/// Backend abstraction for LCD primitives.
/// Hardware backends implement this trait; the framework uses LcdImpl<B> for all drawing.
use crate::types::Rect;

pub trait LcdBackend {
    /// Panel dimensions in pixels. The framework builds its screen-clip rect
    /// from these instead of a feature-gated module const.
    const WIDTH: u16;
    const HEIGHT: u16;

    fn begin_frame(&mut self);
    fn end_frame(&mut self);
    fn back_buf(&self) -> u8;

    fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16);
    fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16);
    fn write_pixel(&self, color: u16);

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
}

/// Generic LCD wrapper — backend-agnostic drawing primitives.
pub struct LcdImpl<B: LcdBackend> {
    be: B,
}

impl<B: LcdBackend> LcdImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self { be }
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
        self.be.fill_rect(x, y, w, h, color)
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
    pub fn draw_pixel(&self, x: u16, y: u16, color: u16) {
        self.be.draw_pixel(x, y, color)
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

    pub fn draw_circle(&self, cx: i16, cy: i16, r: i16, color: u16) {
        if r <= 0 {
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
        let rr = r as i32 * r as i32;
        for dy in -r..=r {
            let yy = dy as i32 * dy as i32;
            let dx = isqrt(rr - yy) as i16;
            self.hline_clipped(cx - dx, cx + dx, cy + dy, color);
        }
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

    pub fn fill_rounded_rect(&self, x: u16, y: u16, w: u16, h: u16, r: u16, color: u16) {
        if w == 0 || h == 0 {
            return;
        }
        let r = r.min(w / 2).min(h / 2);
        if r == 0 {
            self.fill_rect(x, y, w, h, color);
            return;
        }
        let xi = x as i16;
        let yi = y as i16;
        let wi = w as i16;
        let hi = h as i16;
        let ri = r as i16;

        self.fill_rect(x, y + r, w, h - 2 * r, color);

        let cx_l = xi + ri;
        let cx_r = xi + wi - ri - 1;
        let mut py: i16 = ri;
        let mut px: i16 = 0;
        let mut d: i16 = 1 - ri;
        let mut last_px: i16 = -1;

        while px <= py {
            if px != last_px {
                self.hline_clipped(cx_l - py, cx_l - px, yi + ri - px, color);
                self.hline_clipped(cx_l - py, cx_l - px, yi + ri + px, color);
                self.hline_clipped(cx_r + px, cx_r + py, yi + ri - px, color);
                self.hline_clipped(cx_r + px, cx_r + py, yi + ri + px, color);
                last_px = px;
            }
            if d < 0 {
                d += 2 * px + 3;
            } else {
                d += 2 * (px - py) + 5;
                py -= 1;
            }
            px += 1;
        }

        self.fill_rect(x + r, y, w.saturating_sub(2 * r), r, color);
        self.fill_rect(x + r, y + h - r, w.saturating_sub(2 * r), r, color);
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
        for col in 0..w {
            self.write_pixel(lerp_rgb565(c1, c2, x_base + col, grad_w));
        }
    }

    pub fn draw_arc(&self, cx: i16, cy: i16, r: i16, start: i16, end: i16, color: u16) {
        if r <= 0 {
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
                    for col in 0..w {
                        self.write_pixel(lerp_rgb565(c1, c2, x_off + col, grad_w));
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
    /// color. Clips only to `x >= 0`; the caller bounds the right/top/bottom edges.
    fn hspan_gradient(&self, x0: i16, x1: i16, y: i16, full_x: i16, grad_w: u16, c1: u16, c2: u16) {
        let left = x0.max(0) as u16;
        let right = x1;
        if right < 0 || left > right as u16 {
            return;
        }
        let w = right as u16 - left + 1;
        let x_base = left.saturating_sub(full_x.max(0) as u16);
        self.begin_pixels(left, y as u16, w, 1);
        for col in 0..w {
            self.write_pixel(lerp_rgb565(c1, c2, x_base + col, grad_w));
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
    let r1 = (c1 >> 11) as u32;
    let g1 = ((c1 >> 5) & 0x3F) as u32;
    let b1 = (c1 & 0x1F) as u32;
    let r2 = (c2 >> 11) as u32;
    let g2 = ((c2 >> 5) & 0x3F) as u32;
    let b2 = (c2 & 0x1F) as u32;
    let t8 = (t.min(n) as u32 * 256) / n as u32;
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
