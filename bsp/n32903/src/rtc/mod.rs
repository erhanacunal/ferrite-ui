//! RTC backend stub for N32903.
//!
//! The N32903 has an on-chip RTC (the NAND loader unlocks it for the power
//! key), but it is not wired into ferrite yet. This stub returns zeroed time
//! and reports the clock as invalid.

use ferrite_core::rtc::{DateTime, RtcBackend};

/// Stub backend — always returns invalid time.
pub struct StubRtc;

impl RtcBackend for StubRtc {
    fn read_time(&self) -> DateTime {
        DateTime::zero()
    }

    fn set_time(&self, _dt: &DateTime) {}

    fn is_valid(&self) -> bool {
        false
    }

    fn clear_vl_flag(&self) {}
}

/// Return a new `StubRtc` instance.
pub fn new() -> StubRtc {
    StubRtc
}
