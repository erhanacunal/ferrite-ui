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

/// Consecutive I²C-read failures, used to trigger bus recovery.
static mut FAIL_STREAK: u32 = 0;

/// After this many consecutive failed reads, hardware-reset the chip. A small
/// streak avoids reacting to a single transient glitch; once the CST816S
/// auto-sleeps and wedges the bus, every read fails so the threshold is reached
/// almost immediately.
const RECOVER_AFTER: u32 = 3;

/// Hardware-reset the CST8xx and re-apply its configuration.
///
/// When the controller auto-sleeps it holds the I²C bus, so `twi_soft_reset`
/// (controller-side only) cannot recover it — every transfer then fast-fails
/// with the bus stuck non-idle, and the UI thread spins on the failing reads,
/// starving the scheduler. Pulsing the reset line (PE10) releases the bus and
/// reboots the chip; re-running `init()` re-disables auto-sleep so it stays
/// awake afterwards.
fn recover_chip() {
    use f1c100s::gpio::{self, Port};
    use f1c100s::timer;

    gpio::set_value(Port::E, 10, false);
    timer::delay_ms(5);
    gpio::set_value(Port::E, 10, true);
    timer::delay_ms(50); // controller boot time

    if let Some(driver) = unsafe { &*core::ptr::addr_of!(TOUCH_DRIVER) }.as_ref() {
        driver.init().ok();
    }
}

/// Run a touch-count read, healing the bus if it has wedged. Returns the live
/// touch count, or `None` while recovery is in progress.
fn read_count_or_recover(driver: &Cst8xx<'static>) -> Option<u8> {
    match driver.touch_count() {
        Ok(n) => {
            unsafe { FAIL_STREAK = 0; }
            Some(n)
        }
        Err(_) => {
            unsafe {
                FAIL_STREAK += 1;
                if FAIL_STREAK >= RECOVER_AFTER {
                    FAIL_STREAK = 0;
                    recover_chip();
                }
            }
            None
        }
    }
}

/// Hardware backend wrapping the CST8xx I²C driver.
pub struct CstTouch;

impl TouchBackend for CstTouch {
    fn is_active(&self) -> bool {
        let driver = unsafe { &*core::ptr::addr_of!(TOUCH_DRIVER) }
            .as_ref().expect("touch::init() not called");
        read_count_or_recover(driver).unwrap_or(0) > 0
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

    // Retry init: a single failed write here previously left the chip with
    // auto-sleep still enabled (the `init()` chain aborts on the first error and
    // `.ok()` swallowed it), so the controller would sleep after a few seconds,
    // wedge the I²C bus, and the UI thread would spin on the failing reads.
    // Keep trying until the full config (including DISABLE_AUTOSLEEP) lands.
    for _ in 0..8 {
        if driver.init().is_ok() {
            break;
        }
        f1c100s::timer::delay_ms(10);
    }

    unsafe { TOUCH_DRIVER = Some(driver) };
}

/// Return a new `CstTouch` instance. Must call `init()` first.
pub fn new() -> CstTouch {
    CstTouch
}
