//! SD card backend for N32903 — the slot on SD port 0 (GPE2–GPE7), driven by
//! the HAL's `sd` module (a port of Nuvoton's SD driver).
//!
//! There is no card-detect line: a missing card shows up as a timeout, so
//! `init` doubles as the probe. The SD host shares the FMI with the NAND
//! driver; both only run from the main loop.

use ferrite_core::sdcard::{CardType, SdCardBackend, SdCardImpl, SdError};
use n32903_rs::sd::{self, SdCardType, SdInfo, SdPort};

pub type SdCard = SdCardImpl<SicSd>;

/// DMA target for one sector (`read_block` callers' buffers aren't
/// cache-line aligned).
#[repr(C, align(32))]
struct DmaBuf([u8; 512]);
static mut DMA_BUF: DmaBuf = DmaBuf([0; 512]);

pub struct SicSd {
    info: SdInfo,
}

fn map_err(e: sd::SdError) -> SdError {
    match e {
        sd::SdError::Timeout => SdError::Timeout,
        sd::SdError::Crc | sd::SdError::BadBuffer => SdError::ReadError,
        sd::SdError::Unsupported => SdError::InitFailed,
    }
}

/// Bring up the card in the slot. `Err` when there is no (usable) card.
pub fn init() -> Result<SdCard, SdError> {
    let info = unsafe { sd::sd_init_device(SdPort::Port0) }.map_err(|_| SdError::InitFailed)?;
    Ok(SdCard::with_backend(SicSd { info }))
}

impl SdCardBackend for SicSd {
    fn card_type(&self) -> CardType {
        match self.info.card_type {
            SdCardType::SdHigh => CardType::SdHc,
            SdCardType::SdLow => CardType::SdV2,
        }
    }

    fn read_block(&self, block: u32, buf: &mut [u8; 512]) -> Result<(), SdError> {
        // SAFETY: main loop only; nothing else uses DMA_BUF.
        unsafe {
            let dma = &raw mut DMA_BUF;
            sd::sd_read(&self.info, block, 1, (*dma).0.as_mut_ptr()).map_err(map_err)?;
            buf.copy_from_slice(&(*dma).0);
        }
        Ok(())
    }

    fn release_bus(&self) {}
    fn acquire_bus(&self) {}
}
