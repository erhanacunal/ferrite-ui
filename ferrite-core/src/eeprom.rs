/// Backend abstraction for the board's small serial EEPROM (user data that
/// survives a power cycle: settings, counters, calibration).
///
/// Addresses run `0..SIZE` on every board. The backend talks to the chip (GD32
/// Nextion: 24C08 on the RTC's I2C bus) and handles its page and block
/// boundaries. [`EepromImpl`] adds the range checks the VM needs and skips
/// chunks whose contents already match, which saves write cycles and wear.
/// Boards without an EEPROM use [`NoEeprom`] (size 0: every access fails).

pub trait EepromBackend {
    /// Capacity in bytes (0 = no EEPROM).
    const SIZE: u16;

    /// Read `buf.len()` bytes starting at `addr` (`addr + buf.len() <= SIZE`).
    /// `false` on a bus error (chip missing / not answering).
    fn read(&self, addr: u16, buf: &mut [u8]) -> bool;

    /// Write `data` starting at `addr` (`addr + data.len() <= SIZE`) and wait
    /// until the chip has stored it. `false` on a bus error or timeout.
    fn write(&self, addr: u16, data: &[u8]) -> bool;
}

/// Backend for boards without an EEPROM.
pub struct NoEeprom;

impl EepromBackend for NoEeprom {
    const SIZE: u16 = 0;

    fn read(&self, _addr: u16, _buf: &mut [u8]) -> bool {
        false
    }

    fn write(&self, _addr: u16, _data: &[u8]) -> bool {
        false
    }
}

/// Bytes compared (and written, when they differ) per step. Matches the
/// 24C08 page size; backends split further if their pages are smaller.
pub const CHUNK: usize = 16;

pub struct EepromImpl<B: EepromBackend> {
    be: B,
}

impl<B: EepromBackend> EepromImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self { be }
    }

    /// Capacity in bytes (`eepromSize()`).
    #[inline]
    pub fn size(&self) -> i32 {
        B::SIZE as i32
    }

    /// `addr` as a backend address if `addr..addr + len` fits the EEPROM.
    #[inline]
    pub fn range(&self, addr: i32, len: usize) -> Option<u16> {
        let end = (addr as i64).checked_add(len as i64)?;
        (addr >= 0 && end <= B::SIZE as i64).then_some(addr as u16)
    }

    /// Read `buf.len()` bytes at `addr`. `false` if the range doesn't fit or
    /// the bus fails.
    pub fn read(&self, addr: i32, buf: &mut [u8]) -> bool {
        match self.range(addr, buf.len()) {
            Some(a) => buf.is_empty() || self.be.read(a, buf),
            None => false,
        }
    }

    /// Write `data` at `addr`, skipping each chunk that already holds the
    /// same bytes. `false` if the range doesn't fit or the bus fails.
    pub fn write(&self, addr: i32, data: &[u8]) -> bool {
        let Some(mut a) = self.range(addr, data.len()) else {
            return false;
        };
        let mut cur = [0u8; CHUNK];
        for chunk in data.chunks(CHUNK) {
            let old = &mut cur[..chunk.len()];
            if !self.be.read(a, old) {
                return false;
            }
            if old != chunk && !self.be.write(a, chunk) {
                return false;
            }
            a += chunk.len() as u16;
        }
        true
    }

    /// `eepromRead(addr)` → byte 0..255, or `None` (bad address / bus error).
    pub fn read_byte(&self, addr: i32) -> Option<i32> {
        let mut b = [0u8; 1];
        self.read(addr, &mut b).then_some(b[0] as i32)
    }

    /// `eepromWrite(addr, value)`: stores the low byte of `value`.
    pub fn write_byte(&self, addr: i32, value: i32) -> bool {
        self.write(addr, &[value as u8])
    }
}

impl EepromImpl<NoEeprom> {
    /// Convenience constructor for boards without an EEPROM.
    pub fn none() -> Self {
        Self { be: NoEeprom }
    }
}

#[cfg(feature = "mock")]
pub type Eeprom = EepromImpl<crate::mock::MockEeprom>;

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::{Cell, RefCell};

    /// 40-byte RAM EEPROM that counts write calls.
    struct Ram {
        mem: RefCell<[u8; 40]>,
        writes: Cell<u32>,
    }
    impl Ram {
        fn new() -> Self {
            Self { mem: RefCell::new([0xFF; 40]), writes: Cell::new(0) }
        }
    }
    impl EepromBackend for Ram {
        const SIZE: u16 = 40;
        fn read(&self, addr: u16, buf: &mut [u8]) -> bool {
            let a = addr as usize;
            buf.copy_from_slice(&self.mem.borrow()[a..a + buf.len()]);
            true
        }
        fn write(&self, addr: u16, data: &[u8]) -> bool {
            let a = addr as usize;
            self.mem.borrow_mut()[a..a + data.len()].copy_from_slice(data);
            self.writes.set(self.writes.get() + 1);
            true
        }
    }

    #[test]
    fn range_checks() {
        let e = EepromImpl::with_backend(Ram::new());
        assert_eq!(e.size(), 40);
        assert_eq!(e.range(0, 40), Some(0));
        assert_eq!(e.range(39, 1), Some(39));
        assert_eq!(e.range(39, 2), None);
        assert_eq!(e.range(-1, 1), None);
        assert_eq!(e.range(i32::MAX, 1), None);
        assert!(e.read_byte(40).is_none());
        assert!(!e.write_byte(-1, 0));
    }

    #[test]
    fn write_skips_unchanged_chunks() {
        let e = EepromImpl::with_backend(Ram::new());
        let data: [u8; 40] = core::array::from_fn(|i| i as u8);
        assert!(e.write(0, &data)); // 3 chunks (16 + 16 + 8), all changed
        assert_eq!(e.be.writes.get(), 3);
        assert!(e.write(0, &data)); // same contents: no write cycles
        assert_eq!(e.be.writes.get(), 3);
        assert!(e.write_byte(20, 0xAA)); // one byte changed: one chunk
        assert_eq!(e.be.writes.get(), 4);
        assert_eq!(e.read_byte(20), Some(0xAA));
        assert_eq!(e.read_byte(21), Some(21));
    }

    #[test]
    fn no_eeprom_fails_everything() {
        let e = EepromImpl::none();
        assert_eq!(e.size(), 0);
        assert!(e.read_byte(0).is_none());
        assert!(!e.write_byte(0, 1));
        let mut empty = [0u8; 0];
        assert!(!e.read(1, &mut empty)); // past the end even when empty
    }
}
