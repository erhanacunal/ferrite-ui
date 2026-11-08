//! RTC backend — the MAX77686 PMIC's battery-backed clock (I2C0).
//!
//! A read of the chip asks it to copy its counters to the readable
//! registers and waits for that (`RTC_UPDATE_US`, 16 ms). The tablet reads
//! the time in every `loop()` (every 30–50 ms): that was a 16.6 ms stall per
//! read, a third of the main loop, and a dropped frame each time. So the
//! chip is read once, then the time runs on the MCT ([`systick::millis`])
//! and the chip is read again every [`RESYNC_MS`] and after `set_time`.

use core::cell::Cell;

use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::max77686::Rtc;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::time;
use ferrite_core::rtc::{DateTime, RtcBackend};

use crate::systick;

/// How long the time runs on the MCT before the chip is read again (one
/// 16 ms stall per period; the two clocks drift apart by far less).
const RESYNC_MS: u32 = 10 * 60 * 1000;

pub struct PmicRtc {
    rtc: Rtc,
    /// Unix seconds the chip last gave, and `systick::millis` then.
    base: Cell<Option<(i64, u32)>>,
}

pub fn new() -> PmicRtc {
    // I2C0 was initialised by main (PMIC bus).
    PmicRtc { rtc: Rtc::new(I2c::new(Bus::I2c0)), base: Cell::new(None) }
}

impl PmicRtc {
    /// The chip's time (a 16 ms wait).
    fn read_chip(&self) -> Option<time::DateTime> {
        self.rtc.read(Mct::new()).ok()?.time()
    }

    /// The current time: the chip's last reading plus the MCT time since,
    /// or a new reading when there is none or it is [`RESYNC_MS`] old.
    fn now(&self) -> Option<time::DateTime> {
        let ms = systick::millis();
        if let Some((secs, at)) = self.base.get() {
            let age = ms.wrapping_sub(at);
            if age < RESYNC_MS {
                return Some(time::DateTime::from_unix(secs + (age / 1000) as i64));
            }
        }
        let t = self.read_chip();
        // An invalid reading is not kept: the next call reads again.
        self.base.set(t.filter(|t| t.is_valid()).map(|t| (t.to_unix(), ms)));
        t
    }
}

impl RtcBackend for PmicRtc {
    fn read_time(&self) -> DateTime {
        let Some(t) = self.now() else {
            return DateTime { year: 0, month: 1, day: 1, weekday: 6, hour: 0, minute: 0, second: 0 };
        };
        // 1970-01-01 was a Thursday (4).
        let weekday = ((t.to_unix().div_euclid(86_400) + 4).rem_euclid(7)) as u8;
        DateTime {
            year: t.year.saturating_sub(2000).min(99) as u8,
            month: t.month,
            day: t.day,
            weekday,
            hour: t.hour,
            minute: t.minute,
            second: t.second,
        }
    }

    fn set_time(&self, dt: &DateTime) {
        let t = time::DateTime {
            year: 2000 + dt.year as u16,
            month: dt.month,
            day: dt.day,
            hour: dt.hour,
            minute: dt.minute,
            second: dt.second,
        };
        let _ = self.rtc.set(t, Mct::new());
        // Read it back on the next call.
        self.base.set(None);
    }

    fn is_valid(&self) -> bool {
        self.now().is_some_and(|t| t.is_valid())
    }

    fn clear_vl_flag(&self) {}
}
