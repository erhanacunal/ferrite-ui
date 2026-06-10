//! SD card backend stub for TDO Y13.
//!
//! SD card support is not yet implemented for the F1C100s target.
//! This stub always fails initialization. If an SD card slot is
//! present on the board and connected to SPI or SD/MMC pins,
//! implement `SdCardBackend` using the HAL's SPI and FAT modules.

use ferrite_core::sdcard::{CardType, SdCardBackend, SdError};

/// Stub backend — always returns error.
pub struct StubSd;

impl SdCardBackend for StubSd {
    fn card_type(&self) -> CardType {
        CardType::SdV2
    }

    fn read_block(&self, _block: u32, _buf: &mut [u8; 512]) -> Result<(), SdError> {
        Err(SdError::InitFailed)
    }

    fn release_bus(&self) {}
    fn acquire_bus(&self) {}
}

/// Return a new `StubSd` instance.
pub fn new() -> StubSd {
    StubSd
}
