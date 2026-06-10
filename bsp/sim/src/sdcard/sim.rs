use std::cell::RefCell;
use std::vec::Vec;

use super::{CardType, SdCardBackend, SdError};

const BLOCK_SIZE: usize = 512;

/// File-backed sim SD card. Read-only for now — no write support is exposed
/// by the firmware driver anyway. Presents as SDHC (block-addressed).
pub struct FileSd {
    image: RefCell<Vec<u8>>,
}

impl FileSd {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            image: RefCell::new(bytes),
        }
    }
}

impl SdCardBackend for FileSd {
    fn card_type(&self) -> CardType {
        CardType::SdHc
    }

    fn read_block(&self, block: u32, buf: &mut [u8; 512]) -> Result<(), SdError> {
        let start = block as usize * BLOCK_SIZE;
        let image = self.image.borrow();
        if start >= image.len() {
            buf.fill(0xFF);
            return Ok(());
        }
        let end = (start + BLOCK_SIZE).min(image.len());
        let n = end - start;
        buf[..n].copy_from_slice(&image[start..end]);
        if n < BLOCK_SIZE {
            buf[n..].fill(0xFF);
        }
        Ok(())
    }

    fn release_bus(&self) {}
    fn acquire_bus(&self) {}
}
