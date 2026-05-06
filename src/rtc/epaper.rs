//! PCF8563 RTC backend for the LilyGo T5-ePaper-S3.
//!
//! Pin assignment:
//!   GPIO17 = SCL
//!   GPIO18 = SDA
//!
//! The pins are driven by the ESP32-S3 I2C master through `esp-hal`.

#![cfg(feature = "epaper")]

use core::cell::RefCell;

use critical_section::Mutex;
use esp_hal::{
    Blocking,
    i2c::master::{Config as I2cConfig, I2c},
    peripherals,
    time::Rate,
};

use super::{DateTime, RtcBackend};

const I2C_ADDR: u8 = 0x51;

const REG_CONTROL1: u8 = 0x00;
const REG_SECONDS: u8 = 0x02;

const CONTROL1_STOP: u8 = 0x20;
const SECONDS_VL: u8 = 0x80;

static RTC_BUS: Mutex<RefCell<Option<RtcBus>>> = Mutex::new(RefCell::new(None));

/// Install the PCF8563 I2C pins.
pub fn init(
    i2c: peripherals::I2C0<'static>,
    scl: peripherals::GPIO17<'static>,
    sda: peripherals::GPIO18<'static>,
) {
    if let Ok(mut bus) = RtcBus::new(i2c, scl, sda) {
        bus.start_clock();
        critical_section::with(|cs| {
            RTC_BUS.borrow_ref_mut(cs).replace(bus);
        });
    }
}

/// PCF8563-compatible RTC on the e-paper board.
pub struct EpdRtc;

impl EpdRtc {
    pub fn new() -> Self {
        EpdRtc
    }

    fn read_reg(&self, reg: u8) -> u8 {
        let mut val = [0u8; 1];
        if self.read_regs(reg, &mut val) {
            val[0]
        } else {
            0
        }
    }

    fn write_reg(&self, reg: u8, val: u8) {
        self.write_regs(reg, &[val]);
    }

    fn read_regs(&self, reg: u8, buf: &mut [u8]) -> bool {
        critical_section::with(|cs| {
            if let Some(bus) = RTC_BUS.borrow_ref_mut(cs).as_mut() {
                bus.read_regs(reg, buf)
            } else {
                false
            }
        })
    }

    fn write_regs(&self, reg: u8, data: &[u8]) -> bool {
        critical_section::with(|cs| {
            if let Some(bus) = RTC_BUS.borrow_ref_mut(cs).as_mut() {
                bus.write_regs(reg, data)
            } else {
                false
            }
        })
    }
}

impl RtcBackend for EpdRtc {
    fn read_time(&self) -> DateTime {
        let mut buf = [0u8; 7];
        if !self.read_regs(REG_SECONDS, &mut buf) {
            return DateTime::zero();
        }

        decode_datetime(&buf).unwrap_or_else(DateTime::zero)
    }

    fn set_time(&self, dt: &DateTime) {
        let dt = sanitize(*dt);
        let buf = [
            bin_to_bcd(dt.second) & 0x7F,
            bin_to_bcd(dt.minute) & 0x7F,
            bin_to_bcd(dt.hour) & 0x3F,
            bin_to_bcd(dt.day) & 0x3F,
            get_day_of_week(dt.day as u32, dt.month as u32, (dt.year as u32 + 2000) as u32) as u8,
            (bin_to_bcd(dt.month) & 0x1F) | 0x80,
            bin_to_bcd(dt.year % 100),
        ];
        self.write_regs(REG_SECONDS, &buf);
    }

    fn is_valid(&self) -> bool {
        self.read_reg(REG_SECONDS) & SECONDS_VL == 0
    }

    fn clear_vl_flag(&self) {
        let sec = self.read_reg(REG_SECONDS);
        if sec & SECONDS_VL != 0 {
            self.write_reg(REG_SECONDS, sec & !SECONDS_VL);
        }
    }
}

fn get_day_of_week(day: u32, mut month: u32, mut year: u32) -> u32 {
    let mut val: u32;
    if month < 3 {
        month = 12u32 + month;
        year -= 1u32;
    }
    val = (day
        + (((month + 1u32) * 26u32) / 10u32)
        + year
        + (year / 4u32)
        + (6u32 * (year / 100u32))
        + (year / 400u32))
        % 7u32;
    if val == 0u32 {
        val = 7;
    }
    return val - 1u32;
}

fn sanitize(mut dt: DateTime) -> DateTime {
    dt.second = dt.second.min(59);
    dt.minute = dt.minute.min(59);
    dt.hour = dt.hour.min(23);
    dt.month = dt.month.clamp(1, 12);
    let max_day = month_days(2000 + dt.year as u16, dt.month);
    dt.day = dt.day.clamp(1, max_day);
    dt.weekday %= 7;
    dt
}

fn is_leap(year: u16) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn month_days(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        _ => 31,
    }
}

#[inline]
fn bcd_to_bin(bcd: u8) -> u8 {
    (bcd / 16 * 10) + (bcd % 16)
}

#[inline]
fn bin_to_bcd(bin: u8) -> u8 {
    ((bin / 10) * 16) + (bin % 10)
}

struct RtcBus {
    i2c: I2c<'static, Blocking>,
}

impl RtcBus {
    fn new(
        i2c: peripherals::I2C0<'static>,
        scl: peripherals::GPIO17<'static>,
        sda: peripherals::GPIO18<'static>,
    ) -> Result<Self, esp_hal::i2c::master::ConfigError> {
        let config = I2cConfig::default().with_frequency(Rate::from_khz(100));
        let i2c = I2c::new(i2c, config)?.with_sda(sda).with_scl(scl);
        Ok(Self { i2c })
    }

    fn start_clock(&mut self) {
        let ctrl1 = self.read_reg(REG_CONTROL1);
        if ctrl1 & CONTROL1_STOP != 0 {
            self.write_reg(REG_CONTROL1, ctrl1 & !CONTROL1_STOP);
        }
    }

    fn read_reg(&mut self, reg: u8) -> u8 {
        let mut val = [0u8; 1];
        if self.read_regs(reg, &mut val) {
            val[0]
        } else {
            0
        }
    }

    fn write_reg(&mut self, reg: u8, val: u8) {
        self.write_regs(reg, &[val]);
    }

    fn read_regs(&mut self, reg: u8, buf: &mut [u8]) -> bool {
        self.i2c.write_read(I2C_ADDR, &[reg], buf).is_ok()
    }

    fn write_regs(&mut self, reg: u8, data: &[u8]) -> bool {
        let mut buf = [0u8; 8];
        if data.len() > buf.len() - 1 {
            return false;
        }
        buf[0] = reg;
        buf[1..=data.len()].copy_from_slice(data);
        self.i2c.write(I2C_ADDR, &buf[..data.len() + 1]).is_ok()
    }
}

fn decode_datetime(buf: &[u8; 7]) -> Option<DateTime> {
    let second = decode_bcd(buf[0] & 0x7F, 59)?;
    let minute = decode_bcd(buf[1] & 0x7F, 59)?;
    let hour = decode_bcd(buf[2] & 0x3F, 23)?;
    let day = decode_bcd(buf[3] & 0x3F, 31)?;
    let weekday = buf[4] & 0x07;
    let month = decode_bcd(buf[5] & 0x1F, 12)?;
    let year = decode_bcd(buf[6], 99)?;

    if day == 0 || month == 0 {
        return None;
    }

    let max_day = month_days(2000 + year as u16, month);
    if day > max_day {
        return None;
    }

    Some(DateTime {
        year,
        month,
        day,
        weekday,
        hour,
        minute,
        second,
    })
}

fn decode_bcd(bcd: u8, max: u8) -> Option<u8> {
    let val = bcd_to_bin(bcd);
    if val <= max { Some(val) } else { None }
}
