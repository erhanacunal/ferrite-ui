// BSP RTC module (sim) — re-exports framework traits + free-fn ctor.

pub use ferrite_core::rtc::*;

pub mod sim;

pub type Rtc = RtcImpl<sim::SystemRtc>;

pub fn init() -> Rtc {
    Rtc::with_backend(sim::SystemRtc::new())
}
