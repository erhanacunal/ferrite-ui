//! Touch backend for N32903 — 4-wire resistive panel on the on-chip ADC,
//! sampled in interrupts.
//!
//! The main loop never touches the ADC:
//! - idle: the ADC waits for a trigger with the pen-down interrupt
//!   (`IRQ_ADC`) armed, so an untouched panel costs nothing;
//! - pen down: the ADC ISR disarms it and starts Timer 1 at `SAMPLE_HZ`
//!   (the ADC has no pen-up interrupt, so lift-off has to be sampled);
//! - each Timer 1 tick takes one HAL reading (raw 10-bit, median-filtered),
//!   runs the steadiness filter and queues the raw point in a ring buffer;
//! - after `UP_SAMPLES` pen-up ticks in a row it queues `UP`, stops the
//!   timer and re-arms the pen-down interrupt.
//!
//! `AdcTouch` drains the ring once per frame. The mapping to screen pixels
//! uses ferrite's `CalParams`, so it can be calibrated
//! (`RxEvent::TouchCalibrate`, three crosshairs) and stored in the config
//! sector like on the Nextion GD32 board.
//!
//! The Nextion firmware uses screen X = ADC_YDATA, hence `xy_swap` in the
//! defaults.

use core::cell::Cell;
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};

use ferrite_core::touch::{CalParams, TouchBackend, TouchImpl};
use n32903_rs::aic::{self, IntLevel, LocalIntState};
use n32903_rs::registers::{IRQ_ADC, IRQ_TMR1, REG_TISR, TIF1, reg_write32};
use n32903_rs::timer::{self, TimerChannel, TimerOpMode, TimerPrescaler};
use n32903_rs::touch::{Calibration, Touch};

use crate::lcd::VpostLcd;
use crate::systick::{delay_ms, millis};

const SCREEN_W: u16 = 800;
const SCREEN_H: u16 = 480;
const RAW_MAX: u16 = 1023;

/// Timer 1 sampling rate while the pen is down.
const SAMPLE_HZ: u32 = 100;

/// Pen-up samples in a row that end a contact (~30 ms): a resistive panel
/// bounces at lift-off, and each bounce would otherwise be a new tap.
const UP_SAMPLES: u8 = 3;

// === Ring buffer (Timer 1 ISR → main loop) ===
//
// Single producer (ISR) owns the head, single consumer (main loop) owns the
// tail. Entries are raw points `(raw_x << 16) | raw_y`, or `UP`. Shared
// words use volatile access, not atomics: this target has no native atomic
// instructions (they lower to missing libcalls), and on a single core an
// aligned word access with one writer is atomic anyway. No fences either
// (they lower to a missing libcall too): the compiler keeps volatile
// accesses in program order, and there is one core.

const RING_SIZE: usize = 32;
const RING_MASK: usize = RING_SIZE - 1;

/// End of a contact (raw values are 10-bit, so no point encodes to this).
const UP: u32 = u32::MAX;

static mut RING: [u32; RING_SIZE] = [0; RING_SIZE];
static mut RING_HEAD: usize = 0;
static mut RING_TAIL: usize = 0;

/// True from the pen-down interrupt until the debounced release.
static mut PEN_DOWN: bool = false;

fn head() -> usize {
    unsafe { read_volatile(addr_of!(RING_HEAD)) }
}

fn tail() -> usize {
    unsafe { read_volatile(addr_of!(RING_TAIL)) }
}

/// Queue `v` if more than `reserve` slots stay free. Points keep one slot
/// in reserve so the `UP` that closes a contact is never dropped (a lost
/// `UP` would leave the UI pressed). ISR only.
fn ring_push(v: u32, reserve: usize) -> bool {
    let head = head();
    let used = head.wrapping_sub(tail()) & RING_MASK;
    if RING_SIZE - 1 - used <= reserve {
        return false;
    }
    unsafe {
        write_volatile(addr_of_mut!(RING[head]), v);
        write_volatile(addr_of_mut!(RING_HEAD), (head + 1) & RING_MASK);
    }
    true
}

/// Oldest queued entry, without removing it. Main loop only.
fn ring_peek() -> Option<u32> {
    let tail = tail();
    if tail == head() {
        return None;
    }
    Some(unsafe { read_volatile(addr_of!(RING[tail])) })
}

/// Drop the entry returned by `ring_peek`. Main loop only.
fn ring_pop() {
    let tail = tail();
    unsafe { write_volatile(addr_of_mut!(RING_TAIL), (tail + 1) & RING_MASK) };
}

fn set_pen_down(down: bool) {
    unsafe { write_volatile(addr_of_mut!(PEN_DOWN), down) };
}

fn next_entry() -> Option<u32> {
    let v = ring_peek()?;
    ring_pop();
    Some(v)
}

fn drain() {
    while next_entry().is_some() {}
}

fn encode(p: (u16, u16)) -> u32 {
    ((p.0 as u32) << 16) | p.1 as u32
}

fn decode(v: u32) -> (u16, u16) {
    ((v >> 16) as u16, v as u16)
}

// === Interrupt side ===

/// HAL driver with an identity calibration: its `raw_x`/`raw_y` are the
/// plain ADC_XDATA/ADC_YDATA values. Used only by the ISRs (and `init`).
static mut TOUCH: Option<Touch> = None;

fn hal() -> &'static mut Touch {
    unsafe { (&mut *(&raw mut TOUCH)).as_mut().expect("touch::init() not called") }
}

/// Largest raw distance (per axis) between two consecutive readings that
/// still counts as the same steady contact (~10 px on this panel). Confirms
/// the first point of a touch, and steadiness during calibration.
const STEADY_TOL: u16 = 12;

/// Largest raw step (per axis) between two consecutive readings of a contact
/// that is already reported. Samples are 1/`SAMPLE_HZ` apart, so a finger
/// moves far between them: 80 raw is ~60 px on X / ~38 px on Y per 10 ms,
/// i.e. swipes up to ~4000 px/s. Bigger steps are glitches.
const MOVE_TOL: u16 = 80;

/// Per-contact filter state, owned by the Timer 1 ISR.
///
/// A resistive panel reads wrong while the contact pressure is low: the
/// first reading of a touch and the last one before lift-off drift away
/// from the real point. A bad press point hits the wrong widget; a bad
/// release point drops the click or turns it into a swipe. So:
/// - a touch is queued only once a second reading confirms the first
///   (within `STEADY_TOL`); once it moves, steps up to `MOVE_TOL` pass;
/// - after that, readings are queued one sample late, so the final one
///   (taken while lifting off) is never queued;
/// - a jump or an unstable reading queues nothing; the consumer keeps
///   reporting the last point until `UP`.
struct Sampler {
    /// Previous reading of the current contact (not yet queued).
    prev: Option<(u16, u16)>,
    /// Last reading accepted for the current contact.
    last_out: Option<(u16, u16)>,
    /// A point of this contact made it into the ring (so `UP` must follow).
    queued: bool,
    /// Consecutive pen-up samples.
    up_count: u8,
}

impl Sampler {
    const fn new() -> Self {
        Self { prev: None, last_out: None, queued: false, up_count: 0 }
    }

    fn feed(&mut self, cur: (u16, u16)) {
        let tol = if self.last_out.is_none() { STEADY_TOL } else { MOVE_TOL };
        let out = match self.prev.replace(cur) {
            Some(p) if within(p, cur, tol) => {
                // First report of a touch uses the newer (firmer) reading;
                // later ones lag by one so the lift-off reading is dropped.
                if self.last_out.is_none() { cur } else { p }
            }
            // First reading of a touch, or a jump: wait for confirmation.
            _ => return,
        };
        self.last_out = Some(out);
        if ring_push(encode(out), 1) {
            self.queued = true;
        }
    }
}

static mut SAMPLER: Sampler = Sampler::new();

fn is_near(a: (u16, u16), b: (u16, u16)) -> bool {
    within(a, b, STEADY_TOL)
}

fn within(a: (u16, u16), b: (u16, u16), tol: u16) -> bool {
    a.0.abs_diff(b.0) <= tol && a.1.abs_diff(b.1) <= tol
}

/// Pen went down: stop listening for it and start sampling.
///
/// # Safety
/// ISR context, or IRQs masked.
unsafe fn start_contact() {
    unsafe {
        hal().disarm_pen_irq();
        set_pen_down(true);
        timer::timer_start(TimerChannel::Timer1, SAMPLE_HZ, TimerPrescaler::Div1, TimerOpMode::Periodic);
    }
}

/// Pen is up: close the contact, stop sampling, wait for the next pen-down.
///
/// # Safety
/// Timer 1 ISR context.
unsafe fn end_contact(s: &mut Sampler) {
    if s.queued {
        ring_push(UP, 0);
    }
    *s = Sampler::new();
    unsafe {
        timer::timer_stop(TimerChannel::Timer1);
        set_pen_down(false);
        hal().arm_pen_irq();
        // Pressed again while the interrupt was off: it won't fire for that.
        if hal().is_pressed() {
            start_contact();
        }
    }
}

/// `IRQ_ADC`: waiting-for-trigger (pen-down) interrupt.
unsafe extern "C" fn pen_down_isr() {
    unsafe { start_contact() };
}

/// `IRQ_TMR1`: one sample of the current contact.
unsafe extern "C" fn sample_isr() {
    unsafe { reg_write32(REG_TISR, TIF1) };
    let s = unsafe { &mut *(&raw mut SAMPLER) };
    match unsafe { hal().read() } {
        Some(p) => {
            s.up_count = 0;
            s.feed((p.raw_x, p.raw_y));
        }
        // Pen down but the reading was unstable: start a new confirmation.
        None if hal().is_pressed() => {
            s.prev = None;
            s.up_count = 0;
        }
        None => {
            // The held-back reading was taken while lifting off: drop it.
            s.prev = None;
            s.up_count += 1;
            if s.up_count >= UP_SAMPLES {
                unsafe { end_contact(s) };
            }
        }
    }
}

// === Main-loop side ===

/// Pen is down (from the pen-down interrupt to the debounced release).
pub fn is_pressed() -> bool {
    unsafe { read_volatile(addr_of!(PEN_DOWN)) }
}

/// Hardware backend: replays the ISR's queue.
pub struct AdcTouch {
    /// Last raw point of the current contact, `None` between contacts.
    contact: Cell<Option<(u16, u16)>>,
}

impl AdcTouch {
    /// Consume the queue up to the next `UP`: the latest point if any moved
    /// in, else the last one of the contact (pen down, nothing new), else
    /// `None` once `UP` is reached.
    fn current_raw(&self) -> Option<(u16, u16)> {
        let mut moved = false;
        while let Some(v) = ring_peek() {
            if v == UP {
                // Report the final point first; the release on the next poll.
                if !moved {
                    ring_pop();
                    self.contact.set(None);
                }
                break;
            }
            ring_pop();
            self.contact.set(Some(decode(v)));
            moved = true;
        }
        self.contact.get()
    }
}

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

impl TouchBackend for AdcTouch {
    fn is_active(&self) -> bool {
        ring_peek().is_some()
    }

    fn read_screen(&self, cal: &CalParams) -> Option<(u16, u16)> {
        let (rx, ry) = self.current_raw()?;
        Some(to_screen(rx, ry, cal))
    }

    /// `UP` is already debounced in the ISR: release on the first `None`.
    fn release_debounce(&self) -> u8 {
        1
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

/// Wait until the pen is up, then discard everything queued meanwhile.
fn wait_release() {
    loop {
        drain();
        if !is_pressed() {
            break;
        }
        delay_ms(20);
    }
    delay_ms(200);
    drain();
}

/// Average of 8 consecutive steady raw samples of one press, skipping the
/// first few (low contact pressure reads off-point).
fn wait_raw_touch() -> (u16, u16) {
    const SETTLE_COUNT: u32 = 3;
    const STABLE_COUNT: u32 = 8;
    wait_release();
    let (mut seen, mut n, mut ax, mut ay) = (0u32, 0u32, 0u32, 0u32);
    let mut first = (0u16, 0u16);
    loop {
        let Some(v) = next_entry() else {
            delay_ms(5);
            continue;
        };
        if v == UP {
            (seen, n, ax, ay) = (0, 0, 0, 0);
            continue;
        }
        let s = decode(v);
        seen += 1;
        if seen <= SETTLE_COUNT {
            continue;
        }
        if n == 0 {
            first = s;
        } else if !is_near(first, s) {
            // Finger moved: start over from this sample.
            (n, ax, ay) = (0, 0, 0);
            first = s;
        }
        ax += s.0 as u32;
        ay += s.1 as u32;
        n += 1;
        if n >= STABLE_COUNT {
            return ((ax / n) as u16, (ay / n) as u16);
        }
    }
}

/// True if the panel is held in the top-left 50×50 corner for `hold_ms`.
pub fn check_recovery_touch(cal: &CalParams, hold_ms: u32) -> bool {
    let start = millis();
    while millis().wrapping_sub(start) < hold_ms {
        match next_entry() {
            Some(UP) => return false,
            Some(v) => {
                let (rx, ry) = decode(v);
                let (x, y) = to_screen(rx, ry, cal);
                if x >= 50 || y >= 50 {
                    return false;
                }
            }
            None => {
                if !is_pressed() {
                    return false;
                }
                core::hint::spin_loop();
            }
        }
    }
    true
}

/// Take over the ADC for the touch panel and start listening for pen-down.
///
/// # Safety
/// Call once at boot, after `aic_initialize`.
pub unsafe fn init() {
    let identity = Calibration {
        x_min: 0,
        x_max: RAW_MAX,
        y_min: 0,
        y_max: RAW_MAX,
        swap_xy: false,
    };
    unsafe {
        TOUCH = Some(Touch::init(1024, 1024, identity));

        aic::aic_install_isr(IntLevel::Irq7, IRQ_TMR1, sample_isr);
        aic::aic_install_isr(IntLevel::Irq7, IRQ_ADC, pen_down_isr);
        aic::aic_enable_interrupt(IRQ_TMR1);

        // Already pressed at boot (recovery gesture): the pen-down interrupt
        // won't fire for a pen that is down before it is armed.
        let was_on = aic::aic_get_ibit_state();
        aic::aic_set_local_interrupt(LocalIntState::DisableIrq);
        hal().arm_pen_irq();
        if hal().is_pressed() {
            start_contact();
        }
        aic::aic_enable_interrupt(IRQ_ADC);
        if was_on {
            aic::aic_set_local_interrupt(LocalIntState::EnableIrq);
        }
    }
}

/// Return a new `AdcTouch` instance. Must call `init()` first.
pub fn new() -> AdcTouch {
    AdcTouch { contact: Cell::new(None) }
}
