//! Touch backend for N32903 — 4-wire resistive panel on the on-chip ADC.
//!
//! The HAL driver delivers raw 10-bit ADC readings (median-filtered); the
//! mapping to screen pixels uses ferrite's `CalParams`, so it can be
//! calibrated (`RxEvent::TouchCalibrate`, three crosshairs) and stored in the
//! config sector like on the Nextion GD32 board.
//!
//! The Nextion firmware uses screen X = ADC_YDATA, hence `xy_swap` in the
//! defaults.

use ferrite_core::touch::{CalParams, TouchBackend, TouchImpl};
use n32903_rs::touch::{Calibration, Touch};

use crate::lcd::VpostLcd;
use crate::systick::{delay_ms, millis};

const SCREEN_W: u16 = 800;
const SCREEN_H: u16 = 480;
const RAW_MAX: u16 = 1023;

/// Uncalibrated defaults: full 10-bit range, axes swapped like the firmware.
pub const DEFAULT_CAL: CalParams = CalParams {
    xy_swap: true,
    x_flip: false,
    y_flip: false,
    x_min: 0,
    x_max: RAW_MAX,
    y_min: 0,
    y_max: RAW_MAX,
};

/// HAL driver with an identity calibration: its `raw_x`/`raw_y` are the
/// plain ADC_XDATA/ADC_YDATA values.
static mut TOUCH: Option<Touch> = None;

fn hal() -> &'static mut Touch {
    unsafe { (&mut *(&raw mut TOUCH)).as_mut().expect("touch::init() not called") }
}

/// Raw (ADC_X, ADC_Y) sample, `None` when not pressed or unstable.
fn read_raw() -> Option<(u16, u16)> {
    unsafe { hal().read() }.map(|p| (p.raw_x, p.raw_y))
}

pub fn is_pressed() -> bool {
    hal().is_pressed()
}

/// Hardware backend.
pub struct AdcTouch;

impl TouchBackend for AdcTouch {
    fn is_active(&self) -> bool {
        is_pressed()
    }

    fn read_screen(&self, cal: &CalParams) -> Option<(u16, u16)> {
        let (rx, ry) = read_raw()?;
        Some(to_screen(rx, ry, cal))
    }
}

fn to_screen(rx: u16, ry: u16, cal: &CalParams) -> (u16, u16) {
    let (cx, cy) = if cal.xy_swap { (ry, rx) } else { (rx, ry) };
    let x = apply_cal(cx, cal.x_min, cal.x_max, SCREEN_W, cal.x_flip);
    let y = apply_cal(cy, cal.y_min, cal.y_max, SCREEN_H, cal.y_flip);
    (x, y)
}

/// Map `raw` from [cal_min, cal_max] to [0, size - 1], optionally flipped.
fn apply_cal(raw: u16, cal_min: u16, cal_max: u16, size: u16, flip: bool) -> u16 {
    if cal_max <= cal_min {
        return 0;
    }
    let raw = raw.clamp(cal_min, cal_max);
    let max = (size - 1) as u32;
    let val = ((raw - cal_min) as u32 * max / (cal_max - cal_min) as u32) as u16;
    if flip { max as u16 - val } else { val }
}

// === Calibration (three crosshairs, like the Nextion GD32 BSP) ===

const CAL_P1: (u16, u16) = (60, 60);
const CAL_P2: (u16, u16) = (740, 60);
const CAL_P3: (u16, u16) = (60, 420);

/// Show three crosshairs, sample the raw values and derive `CalParams`.
/// Sets the result on `touch` and returns it.
pub fn run_calibration(
    touch: &mut TouchImpl<AdcTouch>,
    lcd: &ferrite_core::lcd::LcdImpl<VpostLcd>,
) -> CalParams {
    let bg = 0x0000u16;
    let cross_color = 0xF800u16;
    let done_color = 0x07E0u16;

    lcd.fill_rect(0, 0, SCREEN_W, SCREEN_H, bg);
    crate::lcd::flush_cache();

    let mut raw = [(0u16, 0u16); 3];
    for (i, p) in [CAL_P1, CAL_P2, CAL_P3].iter().enumerate() {
        draw_crosshair(lcd, p.0, p.1, cross_color);
        raw[i] = wait_raw_touch();
        draw_crosshair(lcd, p.0, p.1, done_color);
    }
    wait_release();

    let cal = compute_calibration(raw[0], raw[1], raw[2]);
    touch.cal = cal;

    lcd.fill_rect(0, 0, SCREEN_W, SCREEN_H, bg);
    crate::lcd::flush_cache();
    cal
}

fn compute_calibration(raw1: (u16, u16), raw2: (u16, u16), raw3: (u16, u16)) -> CalParams {
    // P1→P2 moves along screen X: the raw axis that changes most is X.
    let xy_swap = raw1.1.abs_diff(raw2.1) > raw1.0.abs_diff(raw2.0);
    let pick = |r: (u16, u16), x_axis: bool| -> i32 {
        (if x_axis != xy_swap { r.0 } else { r.1 }) as i32
    };

    // Extrapolate the raw values at the screen edges from the two points.
    let (sx1, sx2) = (pick(raw1, true), pick(raw2, true));
    let dx = sx2 - sx1;
    let span_x = (CAL_P2.0 - CAL_P1.0) as i32;
    let at_x0 = sx1 - CAL_P1.0 as i32 * dx / span_x;
    let at_xw = sx1 + (SCREEN_W - CAL_P1.0) as i32 * dx / span_x;

    let (sy1, sy3) = (pick(raw1, false), pick(raw3, false));
    let dy = sy3 - sy1;
    let span_y = (CAL_P3.1 - CAL_P1.1) as i32;
    let at_y0 = sy1 - CAL_P1.1 as i32 * dy / span_y;
    let at_yh = sy1 + (SCREEN_H - CAL_P1.1) as i32 * dy / span_y;

    let clamp = |v: i32| v.clamp(0, RAW_MAX as i32) as u16;
    CalParams {
        xy_swap,
        x_flip: at_x0 > at_xw,
        y_flip: at_y0 > at_yh,
        x_min: clamp(at_x0.min(at_xw)),
        x_max: clamp(at_x0.max(at_xw)),
        y_min: clamp(at_y0.min(at_yh)),
        y_max: clamp(at_y0.max(at_yh)),
    }
}

fn draw_crosshair(lcd: &ferrite_core::lcd::LcdImpl<VpostLcd>, x: u16, y: u16, color: u16) {
    lcd.fill_rect(x.saturating_sub(15), y, 31, 1, color);
    lcd.fill_rect(x, y.saturating_sub(15), 1, 31, color);
    crate::lcd::flush_cache();
}

fn wait_release() {
    loop {
        delay_ms(20);
        if !is_pressed() {
            break;
        }
    }
    delay_ms(200);
}

/// Average of 8 stable raw samples of one press.
fn wait_raw_touch() -> (u16, u16) {
    const STABLE_COUNT: u32 = 8;
    wait_release();
    let (mut n, mut ax, mut ay) = (0u32, 0u32, 0u32);
    loop {
        delay_ms(10);
        if !is_pressed() {
            (n, ax, ay) = (0, 0, 0);
            continue;
        }
        if let Some((rx, ry)) = read_raw() {
            ax += rx as u32;
            ay += ry as u32;
            n += 1;
            if n >= STABLE_COUNT {
                return ((ax / n) as u16, (ay / n) as u16);
            }
        }
    }
}

/// True if the panel is held in the top-left 50×50 corner for `hold_ms`.
pub fn check_recovery_touch(cal: &CalParams, hold_ms: u32) -> bool {
    let start = millis();
    while millis().wrapping_sub(start) < hold_ms {
        match read_raw() {
            Some((rx, ry)) => {
                let (x, y) = to_screen(rx, ry, cal);
                if x >= 50 || y >= 50 {
                    return false;
                }
            }
            None => {
                if !is_pressed() {
                    return false;
                }
            }
        }
    }
    true
}

/// Take over the ADC for the touch panel.
///
/// # Safety
/// Call once at boot.
pub unsafe fn init() {
    let identity = Calibration {
        x_min: 0,
        x_max: RAW_MAX,
        y_min: 0,
        y_max: RAW_MAX,
        swap_xy: false,
    };
    unsafe { TOUCH = Some(Touch::init(1024, 1024, identity)) };
}

/// Return a new `AdcTouch` instance. Must call `init()` first.
pub fn new() -> AdcTouch {
    AdcTouch
}
