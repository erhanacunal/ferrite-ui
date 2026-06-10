// BSP RTC module — re-exports framework traits, provides epaper type aliases.

pub use ferrite_core::rtc::*;

pub mod epaper;

pub type Rtc = RtcImpl<epaper::EpdRtc>;

pub fn init() -> Rtc {
    Rtc::with_backend(epaper::EpdRtc::new())
}
