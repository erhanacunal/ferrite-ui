//! Backlight backend for TDO Y13 — PWM-driven LCD backlight.
//!
//! Uses the F1C100s PWM0 channel on PA2 to control backlight brightness.
//! 0% = off, 100% = full brightness.

use ferrite_core::backlight::BacklightBackend;
use f1c100s::pwm::{Pwm, PwmChannel, PwmConfig, PwmMode, PwmPolarity};

/// Global PWM instance. Initialized by `init()`.
static mut PWM_INSTANCE: Option<Pwm> = None;

/// Hardware backend wrapping the HAL PWM driver.
pub struct PwmBl;

impl BacklightBackend for PwmBl {
    fn set_brightness(&self, percent: u8) {
        let pwm = unsafe { &*core::ptr::addr_of!(PWM_INSTANCE) }.as_ref().expect("backlight::init() not called");
        let pct = percent.min(100);

        if pct == 0 {
            pwm.disable();
        } else {
            let pulse_ns = (pct as u32) * 10_000;
            let cfg = PwmConfig {
                period_ns: 1_000_000,
                pulse_ns,
                polarity: PwmPolarity::ActiveHigh,
                mode: PwmMode::Continuous,
            };
            let _ = pwm.configure(&cfg);
        }
    }

    fn brightness(&self) -> u8 {
        100
    }
}

/// Initialize PWM0 on PA2 for LCD backlight control.
///
/// # Safety
/// Must be called once at boot, after GPIO mux is configured.
pub unsafe fn init() {
    use f1c100s::pwm::pins::PWM0_PA2;

    let pwm = unsafe { Pwm::new(PwmChannel::Ch0) };
    pwm.init(&PWM0_PA2);
    let _ = pwm.configure(&PwmConfig {
        period_ns: 1_000_000,
        pulse_ns: 10_000,
        polarity: PwmPolarity::ActiveHigh,
        mode: PwmMode::Continuous,
    });

    unsafe { PWM_INSTANCE = Some(pwm) };
}

/// Return a new `PwmBl` instance. Must call `init()` first.
pub fn new() -> PwmBl {
    PwmBl
}
