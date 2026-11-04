//! Time base: the MCT global free-running counter (24 MHz, 64-bit).
//! No interrupt needed: `millis` reads the counter.

use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::soc;
use ferrite_core::systick::SystickBackend;

const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

pub struct MctSystick;

impl SystickBackend for MctSystick {
    fn init() {
        // Started once in main (before the LCD waits on it); starting again is harmless.
        Mct::new().start();
    }

    fn delay_ms(ms: u32) {
        delay_ms(ms);
    }

    fn millis() -> u32 {
        millis()
    }
}

pub fn millis() -> u32 {
    (Mct::new().count() / COUNTS_PER_MS) as u32
}

pub fn delay_ms(ms: u32) {
    let mct = Mct::new();
    let end = mct.count() + u64::from(ms) * COUNTS_PER_MS;
    while mct.count() < end {
        core::hint::spin_loop();
    }
}
