/// AT8563T Real-Time Clock — on the shared I2C bus (`crate::i2c`)
///
/// PCF8563 compatible. I2C address: 0x51 (7-bit).
use super::{DateTime, RtcBackend};
use crate::i2c;

const I2C_ADDR: u8 = 0x51;

const REG_CONTROL1: u8 = 0x00;
#[allow(dead_code)]
const REG_CONTROL2: u8 = 0x01;
const REG_SECONDS: u8 = 0x02;
#[allow(dead_code)]
const REG_MINUTES: u8 = 0x03;
#[allow(dead_code)]
const REG_HOURS: u8 = 0x04;
#[allow(dead_code)]
const REG_DAYS: u8 = 0x05;
#[allow(dead_code)]
const REG_WEEKDAYS: u8 = 0x06;
#[allow(dead_code)]
const REG_MONTHS: u8 = 0x07;
#[allow(dead_code)]
const REG_YEARS: u8 = 0x08;

pub struct At8563;

impl At8563 {
    pub fn init() -> Self {
        let rtc = Self;
        let ctrl1 = rtc.read_reg(REG_CONTROL1);
        if ctrl1 & 0x20 != 0 {
            rtc.write_reg(REG_CONTROL1, ctrl1 & !0x20);
        }
        rtc
    }

    fn read_reg(&self, reg: u8) -> u8 {
        let mut val = [0u8; 1];
        self.read_regs(reg, &mut val);
        val[0]
    }

    fn write_reg(&self, reg: u8, val: u8) {
        self.write_regs(reg, &[val]);
    }

    fn read_regs(&self, reg: u8, buf: &mut [u8]) {
        i2c::start();
        i2c::write_byte((I2C_ADDR << 1) | 0);
        i2c::write_byte(reg);
        i2c::start();
        i2c::write_byte((I2C_ADDR << 1) | 1);
        for i in 0..buf.len() {
            let last = i == buf.len() - 1;
            buf[i] = i2c::read_byte(!last);
        }
        i2c::stop();
    }

    fn write_regs(&self, reg: u8, data: &[u8]) {
        i2c::start();
        i2c::write_byte((I2C_ADDR << 1) | 0);
        i2c::write_byte(reg);
        for &b in data {
            i2c::write_byte(b);
        }
        i2c::stop();
    }
}

impl RtcBackend for At8563 {
    fn read_time(&self) -> DateTime {
        let mut buf = [0u8; 7];
        self.read_regs(REG_SECONDS, &mut buf);

        DateTime {
            second: bcd_to_bin(buf[0] & 0x7F),
            minute: bcd_to_bin(buf[1] & 0x7F),
            hour: bcd_to_bin(buf[2] & 0x3F),
            day: bcd_to_bin(buf[3] & 0x3F),
            weekday: buf[4] & 0x07,
            month: bcd_to_bin(buf[5] & 0x1F),
            year: bcd_to_bin(buf[6]),
        }
    }

    fn set_time(&self, dt: &DateTime) {
        let buf = [
            bin_to_bcd(dt.second) & 0x7F,
            bin_to_bcd(dt.minute) & 0x7F,
            bin_to_bcd(dt.hour) & 0x3F,
            bin_to_bcd(dt.day) & 0x3F,
            dt.weekday & 0x07,
            bin_to_bcd(dt.month) & 0x1F,
            bin_to_bcd(dt.year),
        ];
        self.write_regs(REG_SECONDS, &buf);
    }

    fn is_valid(&self) -> bool {
        let sec = self.read_reg(REG_SECONDS);
        sec & 0x80 == 0
    }

    fn clear_vl_flag(&self) {
        let sec = self.read_reg(REG_SECONDS);
        if sec & 0x80 != 0 {
            self.write_reg(REG_SECONDS, sec & 0x7F);
        }
    }
}

#[inline]
fn bcd_to_bin(bcd: u8) -> u8 {
    (bcd >> 4) * 10 + (bcd & 0x0F)
}

#[inline]
fn bin_to_bcd(bin: u8) -> u8 {
    ((bin / 10) << 4) | (bin % 10)
}
