// Systick — free functions from hw backend + a SystickBackend wrapper so the
// framework's `Ctx.systick` (SystickImpl<S>) can drive it.

pub use ferrite_core::systick::*;

pub mod hw;

pub use hw::{delay_ms, init, millis};

/// Zero-sized backend forwarding to the GD32 SysTick free functions.
#[derive(Clone, Copy)]
pub struct Gd32Systick;

impl SystickBackend for Gd32Systick {
    fn init() {
        hw::init();
    }
    fn delay_ms(ms: u32) {
        hw::delay_ms(ms);
    }
    fn millis() -> u32 {
        hw::millis()
    }
}

pub type Systick = SystickImpl<Gd32Systick>;
