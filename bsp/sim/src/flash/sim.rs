use std::cell::RefCell;
use std::vec::Vec;

use super::FlashBackend;

const FLASH_SIZE: usize = 32 * 1024 * 1024; // W25Q256 = 32 MiB
const SECTOR_SIZE: usize = 4096;

// `Clone` so `Platform::FlashB` is satisfied. The VM clones the flash handle
// once at load for flash-execution cache refills; the sim never uses flash-exec,
// so this clone (a full image copy) is not on any hot path.
#[derive(Clone)]
pub struct FileFlash {
    image: RefCell<Vec<u8>>,
}

impl FileFlash {
    /// Wrap the given byte vector. Pads with 0xFF up to 32 MiB; truncates if longer.
    pub fn new(mut bytes: Vec<u8>) -> Self {
        bytes.resize(FLASH_SIZE, 0xFF);
        Self {
            image: RefCell::new(bytes),
        }
    }

    /// Snapshot the current image (e.g. to flush back to disk on exit).
    pub fn snapshot(&self) -> Vec<u8> {
        self.image.borrow().clone()
    }
}

impl FlashBackend for FileFlash {
    fn read(&self, addr: u32, buf: &mut [u8]) {
        let start = addr as usize;
        let image = self.image.borrow();
        if start >= image.len() {
            buf.fill(0xFF);
            return;
        }
        let n = (image.len() - start).min(buf.len());
        buf[..n].copy_from_slice(&image[start..start + n]);
        if n < buf.len() {
            buf[n..].fill(0xFF);
        }
    }

    fn write(&self, addr: u32, data: &[u8]) {
        // NOR semantics: writes can only clear bits (1 → 0). Callers are expected
        // to erase the sector to 0xFF first.
        let start = addr as usize;
        let mut image = self.image.borrow_mut();
        if start >= image.len() {
            return;
        }
        let n = (image.len() - start).min(data.len());
        for i in 0..n {
            image[start + i] &= data[i];
        }
    }

    fn erase_sector(&self, addr: u32) {
        let start = (addr as usize) & !(SECTOR_SIZE - 1);
        let mut image = self.image.borrow_mut();
        if start >= image.len() {
            return;
        }
        let end = (start + SECTOR_SIZE).min(image.len());
        for b in &mut image[start..end] {
            *b = 0xFF;
        }
    }

    fn read_id(&self) -> (u8, u8, u8) {
        // W25Q256 JEDEC ID — match what the real chip reports so fs mount code
        // passes its verification without special-casing the sim.
        (0xEF, 0x40, 0x19)
    }
}
