//! Backlight stub for the LilyGo T5-ePaper-S3.
//!
//! The ED047TC1 e-paper panel has no backlight.  This backend satisfies the
//! `BacklightBackend` interface by keeping a brightness value in a `Cell`
//! (so the VM's backlight read/write round-trips work correctly) without
//! performing any hardware output.


use core::cell::Cell;

use super::BacklightBackend;

/// No-op backlight backend for e-paper panels.
pub struct EpdBacklight {
    brightness: Cell<u8>,
}

impl EpdBacklight {
    pub fn new() -> Self {
        Self {
            brightness: Cell::new(100),
        }
    }
}

impl BacklightBackend for EpdBacklight {
    fn set_brightness(&self, percent: u8) {
        self.brightness.set(percent.min(100));
    }

    fn brightness(&self) -> u8 {
        self.brightness.get()
    }
}
