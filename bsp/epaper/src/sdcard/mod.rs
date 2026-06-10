// BSP SDCard module — re-exports framework traits, provides epaper type aliases.

pub use ferrite_core::sdcard::*;

pub mod epaper;

pub type SdCard = SdCardImpl<epaper::EspSdCard>;

pub fn probe() -> bool {
    epaper::probe()
}

pub fn init() -> Result<SdCard, SdError> {
    epaper::EspSdCard::init().map(SdCard::with_backend)
}
