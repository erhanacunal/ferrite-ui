use std::cell::Cell;

use super::BacklightBackend;

/// In-memory stub. Brightness reads/writes round-trip through a Cell so the
/// firmware's state-observation code behaves correctly; no display effect on host.
pub struct StubBacklight {
    percent: Cell<u8>,
}

impl StubBacklight {
    pub fn new() -> Self {
        Self {
            percent: Cell::new(100),
        }
    }
}

impl BacklightBackend for StubBacklight {
    fn set_brightness(&self, percent: u8) {
        self.percent.set(percent.min(100));
    }

    fn brightness(&self) -> u8 {
        self.percent.get()
    }
}
