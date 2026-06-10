//! Touch/input backend for the LilyGo T5-ePaper-S3.
//!
//! The board has no touch screen.  BUTTON_1 (GPIO21, active-low) is mapped
//! as a synthetic touch event at screen coordinate (0, 0).  The `is_active`
//! fast-path allows the `TouchImpl` state machine to skip SPI sampling when
//! the button is not pressed.


use super::{CalParams, TouchBackend};

/// GPIO_IN register (ESP32-S3 bank0, covers GPIOs 0-31).
const GPIO_IN: *const u32 = 0x6000_403Cu32 as *const u32;

/// BUTTON_1 = GPIO21 (active low).
const BUTTON1_BIT: u32 = 1 << 21;

/// Single-button input backend for the T5-ePaper-S3.
pub struct EpdButtons;

impl EpdButtons {
    pub fn new() -> Self {
        EpdButtons
    }
}

impl TouchBackend for EpdButtons {
    /// Return `true` when BUTTON_1 is pressed (pin driven low).
    fn is_active(&self) -> bool {
        unsafe { core::ptr::read_volatile(GPIO_IN) & BUTTON1_BIT == 0 }
    }

    /// Return `Some((0, 0))` while BUTTON_1 is held, `None` otherwise.
    ///
    /// The `cal` parameter is unused — the button produces a fixed
    /// screen-space coordinate.
    fn read_screen(&self, _cal: &CalParams) -> Option<(u16, u16)> {
        if self.is_active() { Some((0, 0)) } else { None }
    }
}
