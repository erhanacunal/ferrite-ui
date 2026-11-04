//! RTC backend — the MAX77686 PMIC's battery-backed clock (I2C0).

use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::max77686::Rtc;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::time;
use ferrite_core::rtc::{DateTime, RtcBackend};

pub struct PmicRtc(Rtc);

pub fn new() -> PmicRtc {
    // I2C0 was initialised by main (PMIC bus).
    PmicRtc(Rtc::new(I2c::new(Bus::I2c0)))
}

impl PmicRtc {
    fn now(&self) -> Option<time::DateTime> {
        self.0.read(Mct::new()).ok()?.time()
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
        let _ = self.0.set(t, Mct::new());
    }

    fn is_valid(&self) -> bool {
        self.now().is_some_and(|t| t.is_valid())
    }

    fn clear_vl_flag(&self) {}
}
