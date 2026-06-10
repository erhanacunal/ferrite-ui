//! RTC backend stub for TDO Y13.
//!
//! The F1C100s has no dedicated RTC peripheral in the ferrite sense.
//! This stub returns zeroed time values and treats the clock as invalid.
//! If a real RTC chip (e.g. AT8563 on I²C) is present on the board,
//! replace this stub with a proper driver.

use ferrite_core::rtc::{DateTime, RtcBackend};

/// Stub backend — always returns invalid time.
pub struct StubRtc;

impl RtcBackend for StubRtc {
    fn read_time(&self) -> DateTime {
        DateTime::zero()
    }

    fn set_time(&self, _dt: &DateTime) {
        // No-op: no RTC hardware.
    }

    fn is_valid(&self) -> bool {
        false
    }

    fn clear_vl_flag(&self) {
        // No-op.
    }
}

/// Return a new `StubRtc` instance.
pub fn new() -> StubRtc {
    StubRtc
}
