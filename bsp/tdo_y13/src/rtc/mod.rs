//! RTC backend for TDO Y13: a software clock.
//!
//! The board has no RTC chip, so the time is kept from the system tick: it
//! starts at 2000-01-01 00:00:00 on every boot and can be set over USART
//! (`ferrite_cli.py settime`). It is lost on reset and is only as accurate as
//! the tick timer.

use crate::systick::F1cSystick;
use ferrite_core::rtc::{DateTime, RtcBackend};
use ferrite_core::systick::SystickBackend;

/// Seconds since 2000-01-01 00:00:00 at tick `BASE_MS`.
static mut BASE_SECS: u32 = 0;
static mut BASE_MS: u32 = 0;
/// Set once the time was set (`is_valid`).
static mut SET: bool = false;

/// Software clock backend.
pub struct SoftRtc;

/// Current seconds since 2000-01-01. Moves the base forward so the 32-bit
/// millisecond tick may wrap (every ~49 days) between reads.
fn now_secs() -> u32 {
    let ms = F1cSystick::millis();
    // SAFETY: only the UI thread reads or sets the clock.
    unsafe {
        let whole = ms.wrapping_sub(BASE_MS) / 1000;
        BASE_SECS = BASE_SECS.wrapping_add(whole);
        BASE_MS = BASE_MS.wrapping_add(whole * 1000);
        BASE_SECS
    }
}

fn is_leap(year: u32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn month_days(year: u32, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 2000-01-01 to the first of `month` in `year` (2000..=2099).
fn days_before(year: u32, month: u32) -> u32 {
    let mut days = 0;
    for y in 2000..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    for m in 1..month {
        days += month_days(year, m);
    }
    days
}

fn to_secs(dt: &DateTime) -> u32 {
    let year = 2000 + dt.year.min(99) as u32;
    let month = dt.month.clamp(1, 12) as u32;
    let day = dt.day.clamp(1, month_days(year, month) as u8) as u32;
    let days = days_before(year, month) + day - 1;
    days * 86_400 + dt.hour.min(23) as u32 * 3600 + dt.minute.min(59) as u32 * 60 + dt.second.min(59) as u32
}

fn from_secs(secs: u32) -> DateTime {
    let mut days = secs / 86_400;
    let rem = secs % 86_400;
    // 2000-01-01 was a Saturday.
    let weekday = ((days + 6) % 7) as u8;
    let mut year = 2000;
    loop {
        let len = if is_leap(year) { 366 } else { 365 };
        if days < len || year == 2099 {
            break;
        }
        days -= len;
        year += 1;
    }
    let mut month = 1;
    while month < 12 && days >= month_days(year, month) {
        days -= month_days(year, month);
        month += 1;
    }
    DateTime {
        year: (year - 2000) as u8,
        month: month as u8,
        day: (days + 1) as u8,
        weekday,
        hour: (rem / 3600) as u8,
        minute: (rem / 60 % 60) as u8,
        second: (rem % 60) as u8,
    }
}

impl RtcBackend for SoftRtc {
    fn read_time(&self) -> DateTime {
        from_secs(now_secs())
    }

    fn set_time(&self, dt: &DateTime) {
        let ms = F1cSystick::millis();
        // SAFETY: as in `now_secs`.
        unsafe {
            BASE_SECS = to_secs(dt);
            BASE_MS = ms;
            SET = true;
        }
    }

    fn is_valid(&self) -> bool {
        unsafe { SET }
    }

    fn clear_vl_flag(&self) {}
}

/// Return a new `SoftRtc` instance.
pub fn new() -> SoftRtc {
    SoftRtc
}
