use crate::gpio::Gpio;

/// FPGA komut kodları (Ghidra reverse engineering)
const CMD_Y_START: u16 = 0x02;
const CMD_X_START: u16 = 0x03;
const CMD_Y_END: u16 = 0x06;
const CMD_X_END: u16 = 0x07;
const CMD_PIXEL_WRITE: u16 = 0x0F;

/// LCD boyutları
pub const WIDTH: u16 = 800;
pub const HEIGHT: u16 = 480;

pub struct Lcd {
    gpio: Gpio,
}

impl Lcd {
    pub fn new(gpio: Gpio) -> Self {
        Self { gpio }
    }

    /// FPGA'ya komut gönder
    #[inline(always)]
    fn send_command(&self, cmd: u16) {
        self.gpio.set_cmd_data(false); // command mode
        self.gpio.write_data_bus(cmd);
        self.gpio.clock_pulse();
    }

    /// FPGA'ya data gönder
    #[inline(always)]
    fn send_data(&self, data: u16) {
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
}
