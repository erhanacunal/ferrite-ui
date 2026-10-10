//! Backlight backend for N32903 — PWM, pin and polarity by board variant.
//!
//! The variant is read from GPB0 at boot (HAL `detect_variant`): variant A
//! drives PWM1 on GPD1 (active high), variant B PWM0 on GPD0 (active low).
//! GPD0–GPD4 are also the JTAG pins, so JTAG is lost once this runs.

use ferrite_core::backlight::BacklightBackend;
use n32903_rs::backlight::{self, Backlight};

/// SAFETY: only the main loop uses it.
static mut BACKLIGHT: Option<Backlight> = None;

fn hal() -> &'static mut Backlight {
    unsafe { (&mut *(&raw mut BACKLIGHT)).as_mut().expect("backlight::init() not called") }
}

/// Hardware backend wrapping the HAL PWM backlight.
pub struct PwmBl;

impl BacklightBackend for PwmBl {
    fn set_brightness(&self, percent: u8) {
        unsafe { hal().set_brightness(percent.min(100)) };
    }

    fn brightness(&self) -> u8 {
        hal().brightness()
    }
}

/// Detect the board variant and set the PWM up (backlight off).
///
/// # Safety
/// Call once at boot. Takes over GPD0/GPD1 (JTAG).
pub unsafe fn init() {
    unsafe {
        let variant = backlight::detect_variant();
        let mut bl = Backlight::init(variant, backlight::DEFAULT_FREQ_HZ);
        bl.set_brightness(0);
        BACKLIGHT = Some(bl);
    }
}

/// Return a new `PwmBl` instance. Must call `init()` first.
pub fn new() -> PwmBl {
    PwmBl
}
