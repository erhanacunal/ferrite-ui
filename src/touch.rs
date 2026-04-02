/// XPT2046 Touch Controller — SPI bit-bang driver
///
/// Pin assignment (Ghidra RE):
///   PA0 = MISO (XPT2046 DATA OUT → MCU input)
///   PA1 = MOSI (MCU → XPT2046 DATA IN, output)
///   PA2 = CS (chip select, active low, output)
///   PA3 = CLK (SPI clock, output)
///
/// Touch detection: double-read debounce (matches proven C implementation).
/// Coordinate: raw ADC → linear calibration → screen pixel.
/// GPIO pin initial state is configured by init_ports().

use crate::lcd::Lcd;
use crate::systick::delay_ms;
use crate::widget::{WidgetId, WidgetTree, FLAG_CLICKABLE, FLAG_VISIBLE};

// --- PENIRQ pin (PC14) — direct GPIO read, no interrupt ---

const GPIOC_IDR: u32 = 0x4001_1008;
const PENIRQ_PIN: u32 = 14;

/// Read PENIRQ pin state. Returns true if pen is DOWN (active low).
#[inline]
fn penirq_active() -> bool {
    unsafe {
        core::ptr::read_volatile(GPIOC_IDR as *const u32) & (1 << PENIRQ_PIN) == 0
    }
}

// --- Hardware addresses ---

const GPIOA_BASE: u32 = 0x4001_0800;
const GPIO_IDR: u32 = GPIOA_BASE + 0x08;
const GPIO_BOP: u32 = GPIOA_BASE + 0x10;
const GPIO_BC: u32 = GPIOA_BASE + 0x14;

// --- Pin definitions (Ghidra RE) ---

const MISO_PIN: u32 = 0; // PA0 — XPT2046 DATA OUT (input)
const MOSI_PIN: u32 = 1; // PA1 — XPT2046 DATA IN (output)
const CS_PIN: u32 = 2;   // PA2 — chip select (output)
const CLK_PIN: u32 = 3;  // PA3 — SPI clock (output)

// --- XPT2046 commands ---

const CMD_X: u8 = 0x90; // 1_001_0_0_00 — Y channel (axis=true in C)
const CMD_Y: u8 = 0xD0; // 1_101_0_0_00 — X channel (axis=false in C)
const CMD_Z1: u8 = 0xB0; // 1_011_0_0_00 — Z1 pressure

// --- Debounce ---

/// Max raw ADC difference between two consecutive reads.
/// If exceeded, the reading is rejected as noise.
/// XPT2046 jitter is ~25 raw units (≈5 screen pixels), so 50 gives margin.
const RAW_ERROR: u16 = 50;

/// Number of consecutive failed reads before a Release event is emitted.
/// Prevents flickering caused by intermittent noisy XPT2046 readings.
const RELEASE_DEBOUNCE: u8 = 5;

/// Z1 pressure threshold. Below this = no touch.
/// Finger touch can produce Z1 as low as 30-50 (wide contact area).
/// Nail/stylus produces Z1 200-4000. Idle noise is near 0.
const Z_THRESHOLD: u16 = 50;

// --- Screen ---

const SCREEN_W: u16 = 800;
const SCREEN_H: u16 = 480;

// --- SPI timing (bit-bang, 108MHz CPU) ---
// Each spin(1) ≈ 9.3ns (one NOP at 108MHz).
// SPI clock = 1 / (2 * SPI_HALF_CLK * 9.3ns).
//   spin(4)  → ~13.5 MHz (too fast, causes jitter on some panels)
//   spin(27) → ~2.0 MHz  (XPT2046 datasheet max at 3.3V)
//   spin(54) → ~1.0 MHz  (recommended for stability)
//
// If touch readings are jittery, increase these values.

/// SPI clock half-period (nop cycles). Controls bit-bang SPI speed.
const SPI_HALF_CLK: u32 = 54;       // ~1 MHz

/// CS setup/hold time (nop cycles).
const SPI_CS_DELAY: u32 = 10;

/// ADC conversion wait (nop cycles). Must be ≥ 600 for 12-bit conversion.
const SPI_ADC_WAIT: u32 = 600;

/// Busy clock high/low time (nop cycles).
const SPI_BUSY_LOW: u32 = 14;
const SPI_BUSY_HIGH: u32 = SPI_HALF_CLK;

// --- Calibration targets ---

const CAL_P1: (u16, u16) = (60, 60);   // top-left
const CAL_P2: (u16, u16) = (740, 60);  // top-right
const CAL_P3: (u16, u16) = (60, 420);  // bottom-left

// --- Calibration parameters ---

#[derive(Clone, Copy)]
pub struct CalParams {
    pub xy_swap: bool,
    pub x_flip: bool,
    pub y_flip: bool,
    /// Raw ADC range mapped to screen X (min < max always)
    pub x_min: u16,
    pub x_max: u16,
    /// Raw ADC range mapped to screen Y (min < max always)
    pub y_min: u16,
    pub y_max: u16,
}

impl CalParams {
    pub fn default() -> Self {
        Self {
            xy_swap: true,
            x_flip: true,
            y_flip: true,
            x_min: 0,
            x_max: 4095,
            y_min: 0,
            y_max: 4095,
        }
    }

    /// Serialize to 9 bytes: flags(1) + x_min(2) + x_max(2) + y_min(2) + y_max(2)
    pub fn to_bytes(&self) -> [u8; 9] {
        let mut buf = [0u8; 9];
        buf[0] = (self.xy_swap as u8) | ((self.x_flip as u8) << 1) | ((self.y_flip as u8) << 2);
        buf[1..3].copy_from_slice(&self.x_min.to_le_bytes());
        buf[3..5].copy_from_slice(&self.x_max.to_le_bytes());
        buf[5..7].copy_from_slice(&self.y_min.to_le_bytes());
        buf[7..9].copy_from_slice(&self.y_max.to_le_bytes());
        buf
    }

    /// Deserialize from 9 bytes.
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < 9 {
            return None;
        }
        Some(Self {
            xy_swap: buf[0] & 0x01 != 0,
            x_flip: buf[0] & 0x02 != 0,
            y_flip: buf[0] & 0x04 != 0,
            x_min: u16::from_le_bytes([buf[1], buf[2]]),
            x_max: u16::from_le_bytes([buf[3], buf[4]]),
            y_min: u16::from_le_bytes([buf[5], buf[6]]),
            y_max: u16::from_le_bytes([buf[7], buf[8]]),
        })
    }
}

// --- Touch state machine ---

#[derive(Clone, Copy, PartialEq)]
enum TouchState {
    Idle,
    Pressed,
    Held,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TouchEventKind {
    Press,
    Hold,
    Release,
}

#[derive(Clone, Copy)]
pub struct TouchEvent {
    pub kind: TouchEventKind,
    pub x: u16,
    pub y: u16,
}

pub struct Touch {
    state: TouchState,
    fail_count: u8,
    last_x: u16,
    last_y: u16,
    pub cal: CalParams,
}

impl Touch {
    /// Initialize touch controller with default calibration.
    pub fn init() -> Self {
        Touch {
            state: TouchState::Idle,
            fail_count: 0,
            last_x: 0,
            last_y: 0,
            cal: CalParams::default(),
        }
    }

    /// Called from main loop. Polls touch state, returns event.
    /// When idle, skips SPI reads if PENIRQ is high (no touch).
    pub fn poll(&mut self) -> Option<TouchEvent> {
        // In idle state, skip SPI read if PENIRQ says no touch
        if self.state == TouchState::Idle && !penirq_active() {
            return None;
        }

        let sample = self.read_calibrated();

        match self.state {
            TouchState::Idle => {
                if let Some((x, y)) = sample {
                    self.last_x = x;
                    self.last_y = y;
                    self.fail_count = 0;
                    self.state = TouchState::Pressed;
                    Some(TouchEvent {
                        kind: TouchEventKind::Press,
                        x,
                        y,
                    })
                } else {
                    // SPI read failed but IRQ fired — keep retrying next poll
                    None
                }
            }

            TouchState::Pressed | TouchState::Held => {
                if let Some((x, y)) = sample {
                    self.last_x = x;
                    self.last_y = y;
                    self.fail_count = 0;
                    self.state = TouchState::Held;
                    Some(TouchEvent {
                        kind: TouchEventKind::Hold,
                        x,
                        y,
                    })
                } else {
                    self.fail_count += 1;
                    if self.fail_count >= RELEASE_DEBOUNCE {
                        self.state = TouchState::Idle;
                        self.fail_count = 0;
                        Some(TouchEvent {
                            kind: TouchEventKind::Release,
                            x: self.last_x,
                            y: self.last_y,
                        })
                    } else {
                        // Noisy read — ignore, keep pressed state
                        None
                    }
                }
            }
        }
    }

    /// Read touch with calibration applied. Returns screen (x, y) or None.
    fn read_calibrated(&self) -> Option<(u16, u16)> {
        let (rx, ry) = read_raw_sample()?;

        let (cx, cy) = if self.cal.xy_swap { (ry, rx) } else { (rx, ry) };

        let x = apply_cal(cx, self.cal.x_min, self.cal.x_max, SCREEN_W, self.cal.x_flip);
        let y = apply_cal(cy, self.cal.y_min, self.cal.y_max, SCREEN_H, self.cal.y_flip);

        Some((x, y))
    }
}

// --- Hit test ---

/// Ekran koordinatında (x, y) en üstteki CLICKABLE widget'ı bul.
/// DFS pre-order = z-order (son eşleşen = en üst).
pub fn hit_test(tree: &WidgetTree, x: u16, y: u16) -> WidgetId {
    let dfs = tree.dfs_order();
    let mut result = WidgetId::NONE;

    for i in 0..dfs.len() {
        let id = dfs[i];
        let w = tree.get(id);
        if w.flags & FLAG_CLICKABLE == 0 || !tree.is_tree_visible(id) {
            continue;
        }
        let abs = tree.absolute_rect(id);
        if (x as i16) >= abs.x
            && (x as i16) < abs.right()
            && (y as i16) >= abs.y
            && (y as i16) < abs.bottom()
        {
            result = id; // Son eşleşen = en üstteki
        }
    }

    result
}

// === Calibration ===

/// Apply calibration to a single axis: raw ADC → screen pixel.
fn apply_cal(raw: u16, cal_min: u16, cal_max: u16, screen_size: u16, flip: bool) -> u16 {
    if cal_max <= cal_min {
        return 0;
    }
    let raw = if raw < cal_min {
        cal_min
    } else if raw > cal_max {
        cal_max
    } else {
        raw
    };
    let val = ((raw - cal_min) as u32 * screen_size as u32 / (cal_max - cal_min) as u32) as u16;
    let val = if val > screen_size { screen_size } else { val };
    if flip { screen_size - val } else { val }
}

/// Run 3-point touch calibration (blocking).
/// Draws crosshairs on LCD, waits for user to touch each one,
/// computes calibration params and applies them.
/// Returns the computed CalParams.
pub fn run_calibration(touch: &mut Touch, lcd: &Lcd) -> CalParams {
    let bg = 0x0000u16; // black
    let cross_color = 0xF800u16; // red
    let done_color = 0x07E0u16; // green

    // Clear screen
    lcd.fill_rect(0, 0, SCREEN_W, SCREEN_H, bg);

    // Point 1: top-left
    draw_crosshair(lcd, CAL_P1.0, CAL_P1.1, cross_color);
    let raw1 = wait_raw_touch();
    draw_crosshair(lcd, CAL_P1.0, CAL_P1.1, done_color);

    // Point 2: top-right
    draw_crosshair(lcd, CAL_P2.0, CAL_P2.1, cross_color);
    let raw2 = wait_raw_touch();
    draw_crosshair(lcd, CAL_P2.0, CAL_P2.1, done_color);

    // Point 3: bottom-left
    draw_crosshair(lcd, CAL_P3.0, CAL_P3.1, cross_color);
    let raw3 = wait_raw_touch();
    draw_crosshair(lcd, CAL_P3.0, CAL_P3.1, done_color);
    wait_release();

    // Compute calibration from collected raw data
    let cal = compute_calibration(raw1, raw2, raw3);

    // Apply calibration
    touch.cal = cal;

    // Clear screen
    lcd.fill_rect(0, 0, SCREEN_W, SCREEN_H, bg);

    cal
}

/// Compute calibration params from 3-point raw readings.
/// P1 at screen (60,60), P2 at screen (740,60), P3 at screen (60,420).
fn compute_calibration(raw1: (u16, u16), raw2: (u16, u16), raw3: (u16, u16)) -> CalParams {
    // P1→P2: screen X changes (60→740), screen Y constant (60)
    // Whichever raw axis changes more maps to screen X
    let dx_rx = abs_diff(raw1.0, raw2.0);
    let dx_ry = abs_diff(raw1.1, raw2.1);
    let xy_swap = dx_ry > dx_rx;

    // After swap: pick which raw values correspond to screen X and Y
    let (sx1, sx2) = if xy_swap {
        (raw1.1, raw2.1)
    } else {
        (raw1.0, raw2.0)
    };
    let (sy1, sy3) = if xy_swap {
        (raw1.0, raw3.0)
    } else {
        (raw1.1, raw3.1)
    };

    // X axis: P1 at screen_x=60, P2 at screen_x=740
    // Extrapolate to screen edges 0 and 800
    let sx1_i = sx1 as i32;
    let sx2_i = sx2 as i32;
    let dx = sx2_i - sx1_i; // raw change for 680 screen pixels
    let raw_at_x0 = sx1_i - 60 * dx / 680;
    let raw_at_x800 = sx1_i + 740 * dx / 680;
    let x_flip = raw_at_x0 > raw_at_x800;
    let (x_min, x_max) = if x_flip {
        (raw_at_x800, raw_at_x0)
    } else {
        (raw_at_x0, raw_at_x800)
    };

    // Y axis: P1 at screen_y=60, P3 at screen_y=420
    let sy1_i = sy1 as i32;
    let sy3_i = sy3 as i32;
    let dy = sy3_i - sy1_i; // raw change for 360 screen pixels
    let raw_at_y0 = sy1_i - 60 * dy / 360;
    let raw_at_y480 = sy1_i + 420 * dy / 360;
    let y_flip = raw_at_y0 > raw_at_y480;
    let (y_min, y_max) = if y_flip {
        (raw_at_y480, raw_at_y0)
    } else {
        (raw_at_y0, raw_at_y480)
    };

    CalParams {
        xy_swap,
        x_flip,
        y_flip,
        x_min: (x_min.max(0) as u16).min(4095),
        x_max: (x_max.max(0) as u16).min(4095),
        y_min: (y_min.max(0) as u16).min(4095),
        y_max: (y_max.max(0) as u16).min(4095),
    }
}

fn draw_crosshair(lcd: &Lcd, x: u16, y: u16, color: u16) {
    let x1 = if x >= 15 { x - 15 } else { 0 };
    let y1 = if y >= 15 { y - 15 } else { 0 };
    lcd.fill_rect(x1, y, 31, 1, color);
    lcd.fill_rect(x, y1, 1, 31, color);
}

/// Consecutive stable readings required to accept a calibration touch.
const STABLE_COUNT: u8 = 8;

/// Check if screen is being touched using Z1 pressure.
fn is_pressed() -> bool {
    let z1 = read_raw_axis_cmd(CMD_Z1);
    z1 > Z_THRESHOLD
}

/// Wait for finger lift using Z-pressure (blocking).
fn wait_release() {
    loop {
        delay_ms(20);
        if !is_pressed() {
            break;
        }
    }
    delay_ms(200);
}

/// Wait for a stable raw touch (blocking). Returns (raw_x, raw_y).
/// Uses Z-pressure to detect real touch, then collects N consistent readings.
fn wait_raw_touch() -> (u16, u16) {
    // 1. Wait for no touch (Z-pressure based)
    wait_release();

    // 2. Wait for press, then collect stable readings
    let mut count: u8 = 0;
    let mut acc_x: u32 = 0;
    let mut acc_y: u32 = 0;

    loop {
        delay_ms(10);
        if is_pressed() {
            if let Some((rx, ry)) = read_raw_sample() {
                acc_x += rx as u32;
                acc_y += ry as u32;
                count += 1;
                if count >= STABLE_COUNT {
                    let x = (acc_x / count as u32) as u16;
                    let y = (acc_y / count as u32) as u16;
                    return (x, y);
                }
            }
        } else {
            // No pressure — reset
            count = 0;
            acc_x = 0;
            acc_y = 0;
        }
    }
}

// === Low-level SPI ===

/// Read raw touch sample with double-read debounce. Returns (raw_x, raw_y) or None.
fn read_raw_sample() -> Option<(u16, u16)> {
    let x1 = read_raw_axis(true);
    let y1 = read_raw_axis(false);
    let x2 = read_raw_axis(true);
    let y2 = read_raw_axis(false);

    if abs_diff(x1, x2) > RAW_ERROR || abs_diff(y1, y2) > RAW_ERROR {
        return None;
    }
    if x1 == 0 || x1 >= 4095 || y1 == 0 || y1 >= 4095 {
        return None;
    }
    Some((x1, y1))
}

/// Read one raw axis value from XPT2046 (12-bit result).
fn read_raw_axis(axis: bool) -> u16 {
    read_raw_axis_cmd(if axis { CMD_X } else { CMD_Y })
}

/// Read XPT2046 channel by command byte (12-bit result).
/// Protocol matches the working C implementation exactly:
///   CS HIGH → CLK LOW → MOSI LOW → CS LOW → write cmd →
///   spin(600) → busy clock → read 16 bits → CS HIGH → shift >> 4
fn read_raw_axis_cmd(cmd: u8) -> u16 {
    pin_high(CS_PIN);
    spin(SPI_CS_DELAY);

    pin_low(CLK_PIN);
    spin(SPI_HALF_CLK);
    pin_low(MOSI_PIN);
    spin(SPI_CS_DELAY);

    pin_low(CS_PIN);
    spin(SPI_CS_DELAY);

    spi_write_byte(cmd);

    // ADC conversion time — critical for 12-bit accuracy
    spin(SPI_ADC_WAIT);

    // Busy clock cycle
    pin_low(CLK_PIN);
    spin(SPI_BUSY_LOW);
    pin_high(CLK_PIN);
    spin(SPI_BUSY_HIGH);
    pin_low(CLK_PIN);

    // Read 16-bit result
    let mut data: u16 = 0;
    for _ in 0..16 {
        data <<= 1;
        pin_low(CLK_PIN);
        spin(SPI_HALF_CLK);
        pin_high(CLK_PIN);
        spin(SPI_HALF_CLK);
        if pin_read(MISO_PIN) {
            data += 1;
        }
    }

    // Deselect
    pin_high(CS_PIN);
    spin(SPI_CS_DELAY);

    // 12-bit result is in upper bits
    data >> 4
}

/// 8-bit SPI write (MSB first). Clock: LOW → HIGH (rising edge).
fn spi_write_byte(byte: u8) {
    let mut data = byte as u32;
    for _ in 0..8 {
        if data & 0x80 != 0 {
            pin_high(MOSI_PIN);
        } else {
            pin_low(MOSI_PIN);
        }
        data <<= 1;
        pin_low(CLK_PIN);
        spin(SPI_HALF_CLK);
        pin_high(CLK_PIN);
        spin(SPI_HALF_CLK);
    }
}

fn abs_diff(a: u16, b: u16) -> u16 {
    if a > b { a - b } else { b - a }
}

// === GPIO yardımcıları ===

#[inline(always)]
fn pin_high(pin: u32) {
    unsafe {
        let bop = GPIO_BOP as *mut u32;
        core::ptr::write_volatile(bop, 1 << pin);
    }
}

#[inline(always)]
fn pin_low(pin: u32) {
    unsafe {
        let bc = GPIO_BC as *mut u32;
        core::ptr::write_volatile(bc, 1 << pin);
    }
}

#[inline(always)]
fn pin_read(pin: u32) -> bool {
    unsafe {
        let idr = GPIO_IDR as *const u32;
        core::ptr::read_volatile(idr) & (1 << pin) != 0
    }
}

/// Short delay for SPI timing.
#[inline(always)]
fn spin(cycles: u32) {
    for _ in 0..cycles {
        cortex_m::asm::nop();
    }
}
