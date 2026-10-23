//! User EEPROM for N32903: 1 KB kept in the last 4 KB sector of the board's
//! BY25Q40 SPI NOR flash (512 KB, SPI0 on GPD12–GPD15, CS = SS0), through
//! `n32903_rs::spiflash`. Sector 0 and the rest of the chip are never
//! touched.
//!
//! NOR flash programming only clears bits; setting one back needs a 4 KB
//! sector erase (~100k erase cycles per sector). So the data lives in a RAM
//! copy: `write` updates it, and `flush` (once per main-loop pass) stores it:
//! - only bits cleared (e.g. the first save onto an erased sector): program
//!   the 256-byte pages that changed, no erase;
//! - any bit set: erase the sector once and program the 1 KB.
//! Each commit is read back and compared.
//!
//! A power loss between erase and program leaves the area erased (all
//! 0xFF), which programs treat as "nothing saved". If the chip doesn't
//! answer at init, or a commit fails, every access fails (VM fault 16).

use core::cell::{Cell, RefCell};

use ferrite_core::eeprom::EepromBackend;
use n32903_rs::spiflash;

const SIZE: usize = 1024;
/// Last sector of the 512 KB BY25Q40.
const REGION: u32 = 0x7F000;
const PAGE: usize = spiflash::PAGE_SIZE as usize;

pub struct SpiNorEeprom {
    /// What programs see (includes writes not yet committed).
    ram: RefCell<[u8; SIZE]>,
    /// What the flash holds.
    stored: RefCell<[u8; SIZE]>,
    dirty: Cell<bool>,
    /// Chip answered at init and every commit so far succeeded.
    ok: Cell<bool>,
}

pub fn init() -> SpiNorEeprom {
    let e = SpiNorEeprom {
        ram: RefCell::new([0xFF; SIZE]),
        stored: RefCell::new([0xFF; SIZE]),
        dirty: Cell::new(false),
        ok: Cell::new(false),
    };
    // SAFETY: SPI0 has no other user on this board; runs once at boot.
    let ok = unsafe {
        spiflash::spi_flash_init().is_ok()
            && spiflash::spi_flash_read(REGION, &mut *e.stored.borrow_mut()).is_ok()
    };
    if ok {
        *e.ram.borrow_mut() = *e.stored.borrow();
    }
    e.ok.set(ok);
    e
}

impl SpiNorEeprom {
    /// Store `ram` and read it back. `stored` ends up holding the flash
    /// contents either way.
    fn commit(&self) -> bool {
        let ram = self.ram.borrow();
        let mut stored = self.stored.borrow_mut();
        // Programming can only turn 1s into 0s.
        let needs_erase = ram.iter().zip(stored.iter()).any(|(&new, &old)| old & new != new);
        // SAFETY: SPI0 has no other user on this board; main-loop context.
        unsafe {
            if needs_erase {
                if spiflash::spi_flash_erase_sector(REGION, 1).is_err()
                    || spiflash::spi_flash_write(REGION, &ram[..]).is_err()
                {
                    return false;
                }
            } else {
                for (i, page) in ram.chunks(PAGE).enumerate() {
                    let at = i * PAGE;
                    if page != &stored[at..at + page.len()]
                        && spiflash::spi_flash_write(REGION + at as u32, page).is_err()
                    {
                        return false;
                    }
                }
            }
            if spiflash::spi_flash_read(REGION, &mut stored[..]).is_err() {
                return false;
            }
        }
        *stored == *ram
    }
}

impl EepromBackend for SpiNorEeprom {
    const SIZE: u16 = SIZE as u16;

    fn read(&self, addr: u16, buf: &mut [u8]) -> bool {
        if !self.ok.get() {
            return false;
        }
        let a = addr as usize;
        buf.copy_from_slice(&self.ram.borrow()[a..a + buf.len()]);
        true
    }

    fn write(&self, addr: u16, data: &[u8]) -> bool {
        if !self.ok.get() {
            return false;
        }
        let a = addr as usize;
        self.ram.borrow_mut()[a..a + data.len()].copy_from_slice(data);
        self.dirty.set(true);
        true
    }

    fn flush(&self) -> bool {
        if !self.dirty.get() || !self.ok.get() {
            return self.ok.get();
        }
        self.dirty.set(false);
        let ok = self.commit();
        self.ok.set(ok);
        ok
    }
}
