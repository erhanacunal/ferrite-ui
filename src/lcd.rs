use crate::gpio::Gpio;

/// FPGA command codes (Ghidra reverse engineering)
const CMD_Y_START: u16 = 0x02;
const CMD_X_START: u16 = 0x03;
const CMD_FRONT_SWAP: u16 = 0x04;
const CMD_BACK_SELECT: u16 = 0x05;
const CMD_Y_END: u16 = 0x06;
const CMD_X_END: u16 = 0x07;
const CMD_PIXEL_WRITE: u16 = 0x0F;

pub const WIDTH: u16 = 800;
pub const HEIGHT: u16 = 480;

pub struct Lcd {
    gpio: Gpio,
    /// Front buffer index (what FPGA displays). Tracks CMD4 value.
    lcd4: u8,
    /// Back buffer index (what CPU writes to). Tracks CMD5 value.
    lcd5: u8,
}

impl Lcd {
    pub fn new(gpio: Gpio) -> Self {
        Self { gpio, lcd4: 0, lcd5: 0 }
    }

    /// Begin a new frame: toggle back buffer so CPU writes to the hidden buffer.
    /// No-op if buffers are already swapped (fresh buffer available).
    pub fn begin_frame(&mut self) {
        if self.lcd4 == self.lcd5 {
            self.lcd5 ^= 1;
            self.send_command(CMD_BACK_SELECT);
            self.send_data(self.lcd5 as u16);
        }
    }

    /// Current back buffer index (0 or 1). Used for dual-buffer dirty tracking.
    pub fn back_buf(&self) -> u8 {
        self.lcd5
    }

    /// End frame: swap front buffer to show what was just drawn.
    /// FPGA atomically swaps — no tearing.
    pub fn end_frame(&mut self) {
        self.lcd4 = self.lcd5;
        self.send_command(CMD_FRONT_SWAP);
        self.send_data(self.lcd4 as u16);
    }

    /// FPGA'ya komut gönder
    #[inline(always)]
    pub fn send_command(&self, cmd: u16) {
        self.gpio.set_cmd_data(false); // command mode
        self.gpio.write_data_bus(cmd);
        self.gpio.clock_pulse();
    }

    /// FPGA'ya data gönder
    #[inline(always)]
    pub fn send_data(&self, data: u16) {
        self.gpio.set_cmd_data(true); // data mode
        self.gpio.write_data_bus(data);
        self.gpio.clock_pulse();
    }

    /// Çizim alanını ayarla (x1, y1) → (x2, y2)
    fn set_address(&self, x1: u16, y1: u16, x2: u16, y2: u16) {
        self.send_command(CMD_X_START);
        self.send_data(x1);

        self.send_command(CMD_Y_START);
        self.send_data(y1);

        self.send_command(CMD_X_END);
        self.send_data(x2);

        self.send_command(CMD_Y_END);
        self.send_data(y2);

        self.send_command(CMD_PIXEL_WRITE);
    }

    /// Piksel yazma penceresi aç. Sonraki `write_pixel` çağrıları bu alana yazar.
    /// Font rendering gibi tek tek piksel yazan kodlar için.
    #[inline]
    pub fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16) {
        self.set_address(x, y, x + w - 1, y + h - 1);
    }

    /// Tek piksel yaz (begin_pixels sonrası). Piksel sırayla yazılır (soldan sağa, yukarıdan aşağı).
    #[inline(always)]
    pub fn write_pixel(&self, color: u16) {
        self.send_data(color);
    }

    /// Dikdörtgen doldur — RGB565 renk
    pub fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        let x2 = x + w - 1;
        let y2 = y + h - 1;

        self.set_address(x, y, x2, y2);

        let pixel_count = w as u32 * h as u32;
        for _ in 0..pixel_count {
            self.send_data(color);
        }
    }

    /// Draw rectangle outline (1px border)
    pub fn draw_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        if w == 0 || h == 0 {
            return;
        }
        // Top edge
        self.fill_rect(x, y, w, 1, color);
        // Bottom edge
        if h > 1 {
            self.fill_rect(x, y + h - 1, w, 1, color);
        }
        // Left edge (excluding corners)
        if h > 2 {
            self.fill_rect(x, y + 1, 1, h - 2, color);
        }
        // Right edge (excluding corners)
        if w > 1 && h > 2 {
            self.fill_rect(x + w - 1, y + 1, 1, h - 2, color);
        }
    }

    /// Draw a single pixel at (x, y)
    #[inline]
    pub fn draw_pixel(&self, x: u16, y: u16, color: u16) {
        if x < WIDTH && y < HEIGHT {
            self.set_address(x, y, x, y);
            self.send_data(color);
        }
    }

    /// Draw line using Bresenham's algorithm
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

    /// Draw circle outline using midpoint circle algorithm
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

    /// Draw filled circle using midpoint algorithm with horizontal spans
    pub fn fill_circle(&self, cx: i16, cy: i16, r: i16, color: u16) {
        if r <= 0 {
            return;
        }
        let mut x: i16 = 0;
        let mut y: i16 = r;
        let mut d: i16 = 1 - r;

        // Center horizontal line
        self.hline_clipped(cx - r, cx + r, cy, color);

        while x <= y {
            x += 1;
            if d < 0 {
                d += 2 * x + 1;
            } else {
                // Draw horizontal spans for the previous y before decrementing
                self.hline_clipped(cx - x + 1, cx + x - 1, cy + y, color);
                self.hline_clipped(cx - x + 1, cx + x - 1, cy - y, color);
                y -= 1;
                d += 2 * (x - y) + 1;
            }
            self.hline_clipped(cx - y, cx + y, cy + x, color);
            self.hline_clipped(cx - y, cx + y, cy - x, color);
        }
    }

    /// Plot 8 symmetric circle points
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

    /// Plot pixel with bounds check (signed coordinates)
    #[inline]
    fn plot(&self, x: i16, y: i16, color: u16) {
        if x >= 0 && x < WIDTH as i16 && y >= 0 && y < HEIGHT as i16 {
            self.draw_pixel(x as u16, y as u16, color);
        }
    }

    /// Draw clipped horizontal line from x0 to x1 (inclusive) at y
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

    // --- Rounded rectangles ---

    /// Draw rounded rectangle outline (1px border, quarter-circle corners)
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

        // Straight edges (between corners)
        self.hline_clipped(xi + ri, xi + wi - ri - 1, yi, color);           // top
        self.hline_clipped(xi + ri, xi + wi - ri - 1, yi + hi - 1, color);  // bottom
        self.vline_clipped(xi, yi + ri, yi + hi - ri - 1, color);           // left
        self.vline_clipped(xi + wi - 1, yi + ri, yi + hi - ri - 1, color);  // right

        // Quarter-circle corners (midpoint algorithm)
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
            // Top-left corner (octants 5,6)
            self.plot(cx_tl - py, cy_tl - px, color);
            self.plot(cx_tl - px, cy_tl - py, color);
            // Top-right corner (octants 7,8)
            self.plot(cx_tr + py, cy_tr - px, color);
            self.plot(cx_tr + px, cy_tr - py, color);
            // Bottom-left corner (octants 3,4)
            self.plot(cx_bl - py, cy_bl + px, color);
            self.plot(cx_bl - px, cy_bl + py, color);
            // Bottom-right corner (octants 1,2)
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

    /// Draw filled rounded rectangle
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

        // Center band (full width, between top and bottom rounded areas)
        self.fill_rect(x, y + r, w, h - 2 * r, color);

        // Top and bottom strips (between corners, within rounded area)
        // Filled by the quarter-circle spans below

        // Quarter-circle fills using horizontal spans
        let cx_l = xi + ri;
        let cx_r = xi + wi - ri - 1;
        let cy_t = yi + ri;
        let cy_b = yi + hi - ri - 1;

        let mut px: i16 = 0;
        let mut py: i16 = ri;
        let mut d: i16 = 1 - ri;

        while px <= py {
            // Top half: horizontal spans connecting left and right corners
            self.hline_clipped(cx_l - py, cx_r + py, cy_t - px, color);
            self.hline_clipped(cx_l - px, cx_r + px, cy_t - py, color);
            // Bottom half
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

    /// Draw clipped vertical line from y0 to y1 (inclusive) at x
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

    // --- Arc ---

    /// Draw arc (portion of a circle outline).
    /// Angles in degrees: 0 = right (3 o'clock), 90 = down, counter-clockwise.
    /// start/end: 0..360, handles wrap-around (e.g. start=350, end=10).
    pub fn draw_arc(&self, cx: i16, cy: i16, r: i16, start: i16, end: i16, color: u16) {
        if r <= 0 {
            return;
        }

        // Normalize angles to 0..359
        let start = ((start % 360) + 360) % 360;
        let end = ((end % 360) + 360) % 360;

        // Iterate degree by degree using sin/cos lookup
        let mut deg = start;
        loop {
            let (sin_val, cos_val) = sin_cos_deg(deg);
            // Fixed-point Q8: multiply by r, shift right 8
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

// === Sin/Cos lookup table (Q8 fixed-point, 0..90 degrees) ===

/// Sin values for 0..90 degrees, scaled to 0..256 (Q8 fixed-point).
/// sin(0°) = 0, sin(90°) = 256.
/// Full circle via symmetry: sin(90+x) = cos(x), etc.
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

/// Return (sin, cos) in Q8 fixed-point for angle in degrees (0..359).
/// Positive sin = downward, positive cos = rightward (screen coordinates).
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
