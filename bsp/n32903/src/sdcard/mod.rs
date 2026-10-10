//! SD card backend stub for N32903 (no SD slot used on this board).

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
