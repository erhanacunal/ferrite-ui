pub use ferrite_core::touch::*;

pub mod hw;

pub use hw::{check_recovery_touch, penirq_active_pub, run_calibration};

// Hit testing now lives in the framework (`ferrite_core::touch::hit_test`).

pub type Touch = TouchImpl<hw::XptTouch>;

pub fn init() -> Touch {
    Touch::with_backend(hw::XptTouch::new())
}
