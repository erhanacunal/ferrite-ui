//! Touch backend for TDO Y13 — CST8xx capacitive touch controller via I²C.
//!
//! The CST8xx (CST816S/CST820/CST826) is paired with the TL021WVC04 480×480
//! panel. It talks over TWI2 (I²C) on the Port-E pins (PE0=SCL, PE1=SDA, func3)
//! at 7-bit address 0x15, with reset on PE10 and IRQ on PE9. Up to 5 points are
//! reported; we use the first. The interrupt-driven TWI transfer requires a
//! running scheduler, so `init()` and all reads happen on the UI thread.

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
        // Return the coordinates whenever a point is present. Press/release is
        // tracked by the touch count (`is_active`), NOT the CST8xx event bits:
        // this controller reports event=None even during a valid contact, so
        // filtering on the event would drop every touch.
        match driver.read_point() {
            Ok(Some(point)) => Some((point.x, point.y)),
            _ => None,
        }
    }
}

/// Initialize the TWI2 bus and CST8xx touch controller.
///
/// # Safety
/// Must be called once at boot, from the UI thread — the interrupt-driven I²C
/// transfer blocks on a scheduler event and cannot complete before the
/// scheduler is running. The reset pin (PE10) is pulsed earlier in
/// `init_gpio_mux()`.
pub unsafe fn init() {
    // The CST8xx is on TWI2 via the Port-E pins (PE0=SCL, PE1=SDA, func3).
    let twi = f1c100s::i2c::twi2_port_e();
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
