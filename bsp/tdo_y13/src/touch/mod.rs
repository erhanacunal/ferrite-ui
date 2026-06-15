//! Touch backend for TDO Y13 — CST8xx capacitive touch controller via I²C.
//!
//! The CST8xx (CST816S/CST820/CST826) is paired with the TL021WVC04 480×480
//! panel. It talks over TWI2 (I²C) on the Port-E pins (PE0=SCL, PE1=SDA, func3)
//! at 7-bit address 0x15, with reset on PE10 and IRQ on PE9. Up to 5 points are
//! reported; we use the first. The interrupt-driven TWI transfer requires a
//! running scheduler, so `init()` and all reads happen on the UI thread.

use alloc::boxed::Box;
use f1c100s::cst8xx::Cst8xx;
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

    fn read_screen(&self, cal: &CalParams) -> Option<(u16, u16)> {
        let driver = unsafe { &*core::ptr::addr_of!(TOUCH_DRIVER) }
            .as_ref().expect("touch::init() not called");
        match driver.read_point() {
            Ok(Some(point)) => {
                let mut x = point.x;
                let mut y = point.y;
                if cal.xy_swap {
                    core::mem::swap(&mut x, &mut y);
                }
                x = apply_orient(x, cal.x_flip, 480);
                y = apply_orient(y, cal.y_flip, 480);
                Some((x, y))
            }
            _ => None,
        }
    }
}

/// Apply a single-axis flip and clamp to screen bounds.
fn apply_orient(val: u16, flip: bool, max: u16) -> u16 {
    if flip {
        max.saturating_sub(1).saturating_sub(val)
    } else {
        val.min(max.saturating_sub(1))
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
    // Orientation is owned by CalParams and applied in `read_screen` (default
    // y_flip=true, plus any saved calibration). Keep the hardware transform at
    // identity so the two don't stack into a double-flip.
    driver.set_transform(0);
    driver.init().ok();

    unsafe { TOUCH_DRIVER = Some(driver) };
}

/// Return a new `CstTouch` instance. Must call `init()` first.
pub fn new() -> CstTouch {
    CstTouch
}
