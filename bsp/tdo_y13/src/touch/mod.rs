//! Touch backend for TDO Y13 — CST8xx capacitive touch controller via I²C.
//!
//! The CST8xx (CST816S/CST820/CST826) is commonly paired with the TL021WVC04
//! 480×480 panel. It communicates over TWI0 (I²C) at address 0x15 and reports
//! up to 5 simultaneous touch points.

use alloc::boxed::Box;
use f1c100s::cst8xx::Cst8xx;
use f1c100s::touch::transform;
use ferrite_core::touch::{CalParams, TouchBackend};

/// Global CST8xx driver instance. Initialized by `init()`.
static mut TOUCH_DRIVER: Option<Cst8xx<'static>> = None;

/// Hardware backend wrapping the CST8xx I²C driver.
pub struct CstTouch;

impl TouchBackend for CstTouch {
    fn is_active(&self) -> bool {
        let driver = unsafe { &*core::ptr::addr_of!(TOUCH_DRIVER) }
            .as_ref().expect("touch::init() not called");
        driver.touch_count().unwrap_or(0) > 0
    }

    fn read_screen(&self, _cal: &CalParams) -> Option<(u16, u16)> {
        let driver = unsafe { &*core::ptr::addr_of!(TOUCH_DRIVER) }
            .as_ref().expect("touch::init() not called");

        match driver.read_point() {
            Ok(Some(point)) => {
                use f1c100s::touch::TouchEvent;
                if matches!(point.event, TouchEvent::Up | TouchEvent::None) {
                    None
                } else {
                    Some((point.x, point.y))
                }
            }
            _ => None,
        }
    }
}

/// Initialize the TWI0 bus and CST8xx touch controller.
///
/// # Safety
/// Must be called once at boot. Must be called from the UI thread.
pub unsafe fn init() {
    let twi = f1c100s::i2c::twi0();
    let twi_ref: &'static f1c100s::i2c::Twi = Box::leak(Box::new(twi));

    let mut driver = Cst8xx::new(twi_ref);
    driver.set_range(480, 480);
    driver.set_transform(transform::Y_REVERSE);
    driver.init().ok();

    unsafe { TOUCH_DRIVER = Some(driver) };
}

/// Return a new `CstTouch` instance. Must call `init()` first.
pub fn new() -> CstTouch {
    CstTouch
}
