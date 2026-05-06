#[cfg(feature = "firmware")]
pub mod hw;
#[cfg(feature = "host")]
pub mod sim;
#[cfg(feature = "epaper")]
pub mod epaper;

/// Date and time.
#[derive(Clone, Copy)]
pub struct DateTime {
    pub year: u8,    // 0-99 (offset from 2000)
    pub month: u8,   // 1-12
    pub day: u8,     // 1-31
    pub weekday: u8, // 0-6 (0=Sunday)
    pub hour: u8,    // 0-23
    pub minute: u8,  // 0-59
    pub second: u8,  // 0-59
}

impl DateTime {
    pub const fn zero() -> Self {
        Self {
            year: 0,
            month: 1,
            day: 1,
            weekday: 0,
            hour: 0,
            minute: 0,
            second: 0,
        }
    }
}

pub trait RtcBackend {
    fn read_time(&self) -> DateTime;
    fn set_time(&self, dt: &DateTime);
    fn is_valid(&self) -> bool;
    fn clear_vl_flag(&self);
}

pub struct RtcImpl<B: RtcBackend> {
    be: B,
}

impl<B: RtcBackend> RtcImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self { be }
    }

    #[inline]
    pub fn read_time(&self) -> DateTime {
        self.be.read_time()
    }

    #[inline]
    pub fn set_time(&self, dt: &DateTime) {
        self.be.set_time(dt)
    }

    #[inline]
    pub fn is_valid(&self) -> bool {
        self.be.is_valid()
    }

    #[inline]
    pub fn clear_vl_flag(&self) {
        self.be.clear_vl_flag()
    }
}

#[cfg(feature = "firmware")]
pub type Rtc = RtcImpl<hw::At8563>;

#[cfg(feature = "host")]
pub type Rtc = RtcImpl<sim::SystemRtc>;

#[cfg(feature = "epaper")]
pub type Rtc = RtcImpl<epaper::EpdRtc>;

#[cfg(feature = "firmware")]
impl Rtc {
    pub fn init() -> Self {
        Self::with_backend(hw::At8563::init())
    }
}

#[cfg(feature = "host")]
impl Rtc {
    pub fn init() -> Self {
        Self::with_backend(sim::SystemRtc::new())
    }
}

#[cfg(feature = "epaper")]
impl Rtc {
    pub fn init() -> Self {
        Self::with_backend(epaper::EpdRtc::new())
    }
}
