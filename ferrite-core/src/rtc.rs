/// Backend abstraction for real-time clock.
/// Hardware backends talk to AT8563 (I2C) or ESP32 built-in RTC; sim uses system clock.

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

#[cfg(feature = "mock")]
pub type Rtc = RtcImpl<crate::mock::MockRtc>;
