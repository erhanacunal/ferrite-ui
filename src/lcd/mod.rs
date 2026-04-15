pub const WIDTH: u16 = 800;
pub const HEIGHT: u16 = 480;

#[cfg(not(feature = "host"))]
pub mod hw;
#[cfg(feature = "host")]
pub mod sim;

/// Backend abstraction for the LCD primitives.
/// Hardware backend talks to the FPGA over GPIO; host backend writes to a framebuffer.
pub trait LcdBackend {
    fn begin_frame(&mut self);
    fn end_frame(&mut self);
    fn back_buf(&self) -> u8;

    fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16);
    fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16);
    fn write_pixel(&self, color: u16);

    fn send_command(&self, cmd: u16);
    fn send_data(&self, data: u16);

    fn draw_pixel(&self, x: u16, y: u16, color: u16) {
        if x < WIDTH && y < HEIGHT {
            self.fill_rect(x, y, 1, 1, color);
        }
    }
}

pub struct LcdImpl<B: LcdBackend> {
    be: B,
}

impl<B: LcdBackend> LcdImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self { be }
    }

    // --- Primitives (pass-through to backend) ---

    pub fn begin_frame(&mut self) { self.be.begin_frame() }
    pub fn end_frame(&mut self) { self.be.end_frame() }
    pub fn back_buf(&self) -> u8 { self.be.back_buf() }

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
    pub fn send_command(&self, cmd: u16) { self.be.send_command(cmd) }

    #[inline(always)]
    pub fn send_data(&self, data: u16) { self.be.send_data(data) }

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
            if x >= 0 && x < WIDTH as i16 && y >= 0 && y < HEIGHT as i16 {
                self.draw_pixel(x as u16, y as u16, color);
            }
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
            self.circle_points(cx, cy, x, y, color);
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
        let mut x: i16 = 0;
        let mut y: i16 = r;
        let mut d: i16 = 1 - r;

        self.hline_clipped(cx - r, cx + r, cy, color);

        while x <= y {
            x += 1;
            if d < 0 {
                d += 2 * x + 1;
            } else {
                self.hline_clipped(cx - x + 1, cx + x - 1, cy + y, color);
                self.hline_clipped(cx - x + 1, cx + x - 1, cy - y, color);
                y -= 1;
                d += 2 * (x - y) + 1;
            }
            self.hline_clipped(cx - y, cx + y, cy + x, color);
            self.hline_clipped(cx - y, cx + y, cy - x, color);
        }
    }

    fn circle_points(&self, cx: i16, cy: i16, x: i16, y: i16, color: u16) {
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
        if x >= 0 && x < WIDTH as i16 && y >= 0 && y < HEIGHT as i16 {
            self.draw_pixel(x as u16, y as u16, color);
        }
    }

    fn hline_clipped(&self, x0: i16, x1: i16, y: i16, color: u16) {
        if y < 0 || y >= HEIGHT as i16 {
            return;
        }
        let left = x0.max(0) as u16;
        let right = x1.min(WIDTH as i16 - 1);
        if right < 0 || left > right as u16 {
            return;
        }
        let w = right as u16 - left + 1;
        self.fill_rect(left, y as u16, w, 1, color);
    }

    fn vline_clipped(&self, x: i16, y0: i16, y1: i16, color: u16) {
        if x < 0 || x >= WIDTH as i16 {
            return;
        }
        let top = y0.max(0) as u16;
        let bottom = y1.min(HEIGHT as i16 - 1);
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
        let cy_t = yi + ri;
        let cy_b = yi + hi - ri - 1;

        let mut px: i16 = 0;
        let mut py: i16 = ri;
        let mut d: i16 = 1 - ri;

        while px <= py {
            self.hline_clipped(cx_l - py, cx_r + py, cy_t - px, color);
            self.hline_clipped(cx_l - px, cx_r + px, cy_t - py, color);
            self.hline_clipped(cx_l - py, cx_r + py, cy_b + px, color);
            self.hline_clipped(cx_l - px, cx_r + px, cy_b + py, color);

            if d < 0 {
                d += 2 * px + 3;
            } else {
                d += 2 * (px - py) + 5;
                py -= 1;
            }
            px += 1;
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
}

// --- Type alias: pick backend by build feature ---

#[cfg(not(feature = "host"))]
pub type Lcd = LcdImpl<hw::FpgaLcd>;

#[cfg(feature = "host")]
pub type Lcd = LcdImpl<sim::SimLcd>;

#[cfg(not(feature = "host"))]
impl Lcd {
    pub fn new(gpio: crate::gpio::Gpio) -> Self {
        Self::with_backend(hw::FpgaLcd::new(gpio))
    }
}

#[cfg(feature = "host")]
impl Lcd {
    pub fn new(fb: sim::Framebuffer) -> Self {
        Self::with_backend(sim::SimLcd::new(fb))
    }
}

// === Sin/Cos lookup table (Q8 fixed-point, 0..90 degrees) ===

static SIN_TABLE: [u16; 91] = [
    0, 4, 9, 13, 18, 22, 27, 31, 36, 40,
    44, 49, 53, 57, 62, 66, 70, 75, 79, 83,
    87, 91, 96, 100, 104, 108, 112, 116, 120, 124,
    128, 131, 135, 139, 143, 146, 150, 154, 157, 161,
    164, 167, 171, 174, 177, 181, 184, 187, 190, 193,
    196, 198, 201, 204, 207, 209, 212, 214, 217, 219,
    221, 223, 226, 228, 230, 232, 233, 235, 237, 238,
    240, 242, 243, 244, 246, 247, 248, 249, 250, 251,
    252, 252, 253, 254, 254, 255, 255, 255, 256, 256,
    256,
];

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
