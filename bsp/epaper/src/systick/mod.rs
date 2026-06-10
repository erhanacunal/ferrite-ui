// Systick — free functions from the epaper backend (u64 millis), plus a
// `SystickBackend` ZST so the framework's `Ctx.systick` (SystickImpl<S>) can
// drive it. Hardware init takes the SYSTIMER peripheral and runs in `main`.

pub use ferrite_core::systick::*;

pub mod epaper;

pub use epaper::{delay_ms, init, millis};

/// Zero-sized backend forwarding to the ESP32-S3 SYSTIMER free functions.
/// `millis`/`delay_ms` narrow the backend's `u64` to the trait's `u32`.
#[derive(Clone, Copy)]
pub struct EpdSystick;

impl SystickBackend for EpdSystick {
    fn init() {
        // The SYSTIMER is initialised in `main` (it needs the peripheral
        // handle); nothing to do here.
    }
    fn delay_ms(ms: u32) {
        epaper::delay_ms(ms as u64);
    }
    fn millis() -> u32 {
        epaper::millis() as u32
    }
}

pub type Systick = SystickImpl<EpdSystick>;
