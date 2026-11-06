//! Backlight — GPD0_1 (TOUT1), dimmed by PWM timer 1 at 16 kHz through the
//! HAL's `display::set_brightness` (0 = off, 255 = full, as a plain output;
//! anything between runs the PWM).

use core::sync::atomic::{AtomicU8, Ordering};

use exynos_4412_rs::drivers::display;
use ferrite_core::backlight::BacklightBackend;

static LEVEL: AtomicU8 = AtomicU8::new(0);

pub struct GpioBacklight;

impl BacklightBackend for GpioBacklight {
    fn set_brightness(&self, percent: u8) {
        let p = percent.min(100);
        // 0..100 % → 0..255, rounded: 100 % = 255 (full, PWM stopped).
        display::set_brightness(((u16::from(p) * 255 + 50) / 100) as u8);
        LEVEL.store(p, Ordering::Relaxed);
    }

    fn brightness(&self) -> u8 {
        LEVEL.load(Ordering::Relaxed)
    }
}
