// BSP SDCard module (sim) — re-exports framework traits + free-fn ctor.
// The simulator has no SD boot path; this exists only to satisfy
// `Platform::SdCardB`.

pub use ferrite_core::sdcard::*;

pub mod sim;

pub type SdCard = SdCardImpl<sim::FileSd>;

pub fn from_image(bytes: std::vec::Vec<u8>) -> SdCard {
    SdCard::with_backend(sim::FileSd::new(bytes))
}
