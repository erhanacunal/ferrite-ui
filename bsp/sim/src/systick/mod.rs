// Systick — free functions from the sim backend plus a `SystickBackend` ZST so
// the framework's `Ctx.systick` (SystickImpl<S>) can drive it.

pub use ferrite_core::systick::*;

pub mod sim;

pub use sim::{delay_ms, init, millis};

/// Zero-sized backend forwarding to the host `Instant`-based free functions.
#[derive(Clone, Copy)]
pub struct SimSystick;

impl SystickBackend for SimSystick {
    fn init() {
        sim::init();
    }
    fn delay_ms(ms: u32) {
        sim::delay_ms(ms);
    }
    fn millis() -> u32 {
        sim::millis()
    }
}

pub type Systick = SystickImpl<SimSystick>;
