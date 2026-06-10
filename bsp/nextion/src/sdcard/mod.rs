pub use ferrite_core::sdcard::*;

pub mod hw;

pub type SdCard = SdCardImpl<hw::SpiSd>;

pub fn probe() -> bool {
    hw::probe()
}
pub fn init() -> Result<SdCard, SdError> {
    hw::SpiSd::init().map(SdCard::with_backend)
}
