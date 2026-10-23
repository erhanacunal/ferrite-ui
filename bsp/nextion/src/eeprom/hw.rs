/// 24C08 1 KB serial EEPROM — on the shared I2C bus (`crate::i2c`)
///
/// The 24C08 answers on four I2C addresses, one per 256-byte block:
/// `1010 A2 P1 P0`. Its A2 pin is wired to VCC, so the blocks are 0x54..0x57.
/// A1/A0 are tied to GND but unused, because on the 24C08 those bits are the
/// block number. The RTC (0x51) shares the bus.
///
/// - Read: random-address read, then sequential bytes, split at 256-byte
///   block boundaries.
/// - Write: 16-byte page writes, split at page boundaries. After each page,
///   ACK polling waits out the internal write cycle (5 ms max), bounded by
///   `POLL_MAX` so a missing chip reports an error instead of hanging.
use super::EepromBackend;
use crate::i2c;

const BASE_ADDR: u8 = 0x54; // 0x50 | A2 (VCC) << 2
const SIZE: u16 = 1024;
const BLOCK: u16 = 256;
const PAGE: u16 = 16;

/// ACK polls after a page write. One poll (START + address byte + STOP) takes
/// roughly 30 µs, so this allows ~30 ms against the 5 ms the chip needs.
const POLL_MAX: u32 = 1000;

pub struct At24c08;

impl At24c08 {
    pub fn init() -> Self {
        Self
    }
}

/// 8-bit write address of the block holding `addr`.
#[inline]
fn dev(addr: u16) -> u8 {
    (BASE_ADDR | ((addr / BLOCK) as u8 & 0x03)) << 1
}

/// Address the chip at `addr` (device + word address) for a write or the
/// dummy write before a read. Leaves the bus mid-transaction on success;
/// sends STOP and returns `false` if the chip doesn't ACK.
fn select(addr: u16) -> bool {
    i2c::start();
    if !i2c::write_byte(dev(addr)) || !i2c::write_byte(addr as u8) {
        i2c::stop();
        return false;
    }
    true
}

/// Wait for the internal write cycle: the chip NACKs its address until done.
fn wait_ready(addr: u16) -> bool {
    for _ in 0..POLL_MAX {
        i2c::start();
        let ack = i2c::write_byte(dev(addr));
        i2c::stop();
        if ack {
            return true;
        }
    }
    false
}

impl EepromBackend for At24c08 {
    const SIZE: u16 = SIZE;

    fn read(&self, mut addr: u16, mut buf: &mut [u8]) -> bool {
        while !buf.is_empty() {
            let n = buf.len().min((BLOCK - addr % BLOCK) as usize);
            if !select(addr) {
                return false;
            }
            i2c::start();
            if !i2c::write_byte(dev(addr) | 1) {
                i2c::stop();
                return false;
            }
            let (part, rest) = buf.split_at_mut(n);
            for (i, b) in part.iter_mut().enumerate() {
                *b = i2c::read_byte(i + 1 < n);
            }
            i2c::stop();
            addr += n as u16;
            buf = rest;
        }
        true
    }

    fn write(&self, mut addr: u16, mut data: &[u8]) -> bool {
        while !data.is_empty() {
            let n = data.len().min((PAGE - addr % PAGE) as usize);
            if !select(addr) {
                return false;
            }
            let (part, rest) = data.split_at(n);
            for &b in part {
                if !i2c::write_byte(b) {
                    i2c::stop();
                    return false;
                }
            }
            i2c::stop();
            if !wait_ready(addr) {
                return false;
            }
            addr += n as u16;
            data = rest;
        }
        true
    }
}
