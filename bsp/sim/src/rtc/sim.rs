use std::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{DateTime, RtcBackend};

/// Host RTC backed by `SystemTime::now()`. `set_time` shifts an offset so
/// subsequent reads reflect the requested clock, emulating RTC write/readback.
pub struct SystemRtc {
    offset_secs: Cell<i64>,
}

impl SystemRtc {
    pub fn new() -> Self {
        Self {
            offset_secs: Cell::new(0),
        }
    }

    fn current_unix_secs(&self) -> u64 {
        let real = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0) as i64;
        (real + self.offset_secs.get()).max(0) as u64
    }
}

impl RtcBackend for SystemRtc {
    fn read_time(&self) -> DateTime {
        unix_to_datetime(self.current_unix_secs())
    }

    fn set_time(&self, dt: &DateTime) {
        let target = datetime_to_unix(dt) as i64;
        let real = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0) as i64;
        self.offset_secs.set(target - real);
    }

    fn is_valid(&self) -> bool {
        true
    }

    fn clear_vl_flag(&self) {}
}

fn is_leap(y: u32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn month_days(y: u32, m: u8) -> u8 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn unix_to_datetime(secs: u64) -> DateTime {
    let mut rem = secs;
    let s = (rem % 60) as u8;
    rem /= 60;
    let mi = (rem % 60) as u8;
    rem /= 60;
    let h = (rem % 24) as u8;
    rem /= 24;
    let mut days = rem;

    // 1970-01-01 was a Thursday. DateTime::weekday uses 0=Sunday.
    let weekday = ((days + 4) % 7) as u8;

    let mut year: u32 = 1970;
    loop {
        let dy = if is_leap(year) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        year += 1;
    }

    let mut month: u8 = 1;
    loop {
        let md = month_days(year, month) as u64;
        if days < md {
            break;
        }
        days -= md;
        month += 1;
    }

    let day = (days + 1) as u8;

    let year_byte = if year >= 2000 && year <= 2255 {
        (year - 2000) as u8
    } else {
        0
    };

    DateTime {
        year: year_byte,
        month,
        day,
        weekday,
        hour: h,
        minute: mi,
        second: s,
    }
}

fn datetime_to_unix(dt: &DateTime) -> u64 {
    let year = 2000u32 + dt.year as u32;
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    for m in 1..dt.month {
        days += month_days(year, m) as u64;
    }
    days += (dt.day.saturating_sub(1)) as u64;

    days * 86400 + (dt.hour as u64) * 3600 + (dt.minute as u64) * 60 + dt.second as u64
}
