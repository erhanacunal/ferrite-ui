//! Backlight — GPD0_1 as a plain on/off output (`display::BACKLIGHT`).
//!
//! TODO: PWM. GPD0_1 is TOUT1 (PWM timer 1); dimming needs a PWM timer
//! driver in the HAL. Until then any brightness > 0 is full on.

use core::sync::atomic::{AtomicU8, Ordering};

use exynos_4412_rs::drivers::display;
use ferrite_core::backlight::BacklightBackend;

static LEVEL: AtomicU8 = AtomicU8::new(0);

pub struct GpioBacklight;

impl BacklightBackend for GpioBacklight {
    fn set_brightness(&self, percent: u8) {
        let p = percent.min(100);
        display::set_backlight(p > 0);
        LEVEL.store(p, Ordering::Relaxed);
    }

    fn brightness(&self) -> u8 {
        LEVEL.load(Ordering::Relaxed)
    }
}
