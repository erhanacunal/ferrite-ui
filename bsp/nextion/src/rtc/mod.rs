pub use ferrite_core::rtc::*;

pub mod hw;

pub type Rtc = RtcImpl<hw::At8563>;

pub fn init() -> Rtc {
    Rtc::with_backend(hw::At8563::init())
}
