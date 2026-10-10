//! RTC backend for N32903 — AT8563 (PCF8563 compatible) over bit-banged I²C.
//!
//! Same chip as the Nextion GD32 board. Wired to GPB14 (SDA) / GPB13 (SCL).
//! Those are also the N32903 hardware-I²C pins, but the HAL documents them
//! the other way round (GPB13 = SDA) and the Nextion firmware never touches
//! the I²C block, so this driver bit-bangs both lines as GPIOs. At init it
//! probes the chip with the documented wiring and, failing that, with the
//! pins swapped.
//!
//! Open-drain emulation: a line is driven low by enabling its output (DOUT
//! kept 0) and released by switching it back to input; the board's pull-ups
//! (plus the internal ones) pull it high.

use ferrite_core::rtc::{DateTime, RtcBackend};
use n32903_rs::registers::*;

const I2C_ADDR: u8 = 0x51;

const REG_CONTROL1: u8 = 0x00;
const REG_SECONDS: u8 = 0x02;

/// Half an SCL period (~5 µs at 96 MHz → ~100 kHz).
const HALF_PERIOD_SPINS: u32 = 240;

/// The two GPB pins in use; set by `init`.
static mut SDA: u32 = 14;
static mut SCL: u32 = 13;
/// The chip answered at init.
static mut PRESENT: bool = false;

/// Hardware backend.
pub struct At8563;

impl RtcBackend for At8563 {
    fn read_time(&self) -> DateTime {
        let mut buf = [0u8; 7];
        if !present() || !read_regs(REG_SECONDS, &mut buf) {
            return DateTime::zero();
        }
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
        if !present() {
            return;
        }
        let buf = [
            bin_to_bcd(dt.second) & 0x7F, // also clears VL (bit 7)
            bin_to_bcd(dt.minute) & 0x7F,
            bin_to_bcd(dt.hour) & 0x3F,
            bin_to_bcd(dt.day) & 0x3F,
            dt.weekday & 0x07,
            bin_to_bcd(dt.month) & 0x1F,
            bin_to_bcd(dt.year),
        ];
        write_regs(REG_SECONDS, &buf);
    }

    fn is_valid(&self) -> bool {
        // VL (seconds bit 7) set = the clock lost power and is unreliable
        present() && read_reg(REG_SECONDS).is_some_and(|s| s & 0x80 == 0)
    }

    fn clear_vl_flag(&self) {
        if let Some(sec) = read_reg(REG_SECONDS) {
            if sec & 0x80 != 0 {
                write_regs(REG_SECONDS, &[sec & 0x7F]);
            }
        }
    }
}

fn present() -> bool {
    unsafe { PRESENT }
}

fn read_reg(reg: u8) -> Option<u8> {
    let mut v = [0u8; 1];
    read_regs(reg, &mut v).then_some(v[0])
}

/// Register read; false if the chip did not acknowledge.
fn read_regs(reg: u8, buf: &mut [u8]) -> bool {
    start();
    let ok = write_byte(I2C_ADDR << 1) && write_byte(reg) && {
        start(); // repeated start
        write_byte((I2C_ADDR << 1) | 1)
    };
    if ok {
        let n = buf.len();
        for (i, b) in buf.iter_mut().enumerate() {
            *b = read_byte(i + 1 < n);
        }
    }
    stop();
    ok
}

fn write_regs(reg: u8, data: &[u8]) -> bool {
    start();
    let ok = write_byte(I2C_ADDR << 1) && write_byte(reg) && data.iter().all(|&b| write_byte(b));
    stop();
    ok
}

// === Bit-banged I²C on GPB ===

fn release(pin: u32) {
    unsafe { reg_write32(REG_GPIOB_OMD, reg_read32(REG_GPIOB_OMD) & !(1 << pin)) };
}

fn pull_low(pin: u32) {
    unsafe { reg_write32(REG_GPIOB_OMD, reg_read32(REG_GPIOB_OMD) | (1 << pin)) };
}

fn is_high(pin: u32) -> bool {
    unsafe { reg_read32(REG_GPIOB_PIN) & (1 << pin) != 0 }
}

fn sda() -> u32 {
    unsafe { SDA }
}

fn scl() -> u32 {
    unsafe { SCL }
}

fn delay() {
    unsafe {
        core::arch::asm!(
            "1:",
            "subs {0}, {0}, #1",
            "bne 1b",
            inout(reg) HALF_PERIOD_SPINS => _,
            options(nomem, nostack),
        );
    }
}

/// Release SCL and wait (briefly) for a slave holding it low.
fn scl_high() {
    release(scl());
    for _ in 0..1000 {
        if is_high(scl()) {
            break;
        }
    }
}

fn start() {
    release(sda());
    scl_high();
    delay();
    pull_low(sda());
    delay();
    pull_low(scl());
    delay();
}

fn stop() {
    pull_low(sda());
    delay();
    scl_high();
    delay();
    release(sda());
    delay();
}

/// Clock out a byte MSB first; true if the slave acknowledged.
fn write_byte(byte: u8) -> bool {
    for i in (0..8).rev() {
        if byte & (1 << i) != 0 {
            release(sda());
        } else {
            pull_low(sda());
        }
        delay();
        scl_high();
        delay();
        pull_low(scl());
    }
    release(sda());
    delay();
    scl_high();
    delay();
    let ack = !is_high(sda());
    pull_low(scl());
    delay();
    ack
}

/// Clock in a byte; `ack` = acknowledge it (more bytes follow).
fn read_byte(ack: bool) -> u8 {
    let mut v = 0u8;
    release(sda());
    for _ in 0..8 {
        delay();
        scl_high();
        delay();
        v = (v << 1) | is_high(sda()) as u8;
        pull_low(scl());
    }
    if ack {
        pull_low(sda());
    }
    delay();
    scl_high();
    delay();
    pull_low(scl());
    release(sda());
    delay();
    v
}

/// Nine clocks with SDA released, then STOP: frees a slave stuck mid-byte.
fn bus_clear() {
    release(sda());
    for _ in 0..9 {
        scl_high();
        delay();
        pull_low(scl());
        delay();
    }
    stop();
}

fn probe() -> bool {
    bus_clear();
    start();
    let ack = write_byte(I2C_ADDR << 1);
    stop();
    ack
}

/// Make GPB13/14 GPIOs, find the chip and start its clock.
///
/// # Safety
/// Call once at boot. Takes over GPB13/GPB14.
pub unsafe fn init() {
    unsafe {
        reg_write32(REG_GPBFUN, reg_read32(REG_GPBFUN) & !(MF_GPB13 | MF_GPB14));
        let pins = (1 << 13) | (1 << 14);
        reg_write32(REG_GPIOB_DOUT, reg_read32(REG_GPIOB_DOUT) & !pins); // low when driven
        reg_write32(REG_GPIOB_PUEN, reg_read32(REG_GPIOB_PUEN) | pins);
        reg_write32(REG_GPIOB_OMD, reg_read32(REG_GPIOB_OMD) & !pins); // released

        for (sda, scl) in [(14, 13), (13, 14)] {
            SDA = sda;
            SCL = scl;
            if probe() {
                PRESENT = true;
                break;
            }
        }
    }

    if present() {
        // Clear STOP (control 1 bit 5) so the clock runs
        if let Some(c1) = read_reg(REG_CONTROL1) {
            if c1 & 0x20 != 0 {
                write_regs(REG_CONTROL1, &[c1 & !0x20]);
            }
        }
        crate::usart::log_fmt(format_args!(
            "rtc: AT8563 found (SDA GPB{}, SCL GPB{})\r\n",
            sda(),
            scl()
        ));
    } else {
        crate::usart::log("rtc: AT8563 not found on GPB13/GPB14\r\n");
    }
}

fn bcd_to_bin(bcd: u8) -> u8 {
    (bcd >> 4) * 10 + (bcd & 0x0F)
}

fn bin_to_bcd(bin: u8) -> u8 {
    ((bin / 10) << 4) | (bin % 10)
}

/// Return a new `At8563` instance. Must call `init()` first.
pub fn new() -> At8563 {
    At8563
}
