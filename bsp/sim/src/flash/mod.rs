// BSP Flash module (sim) — re-exports framework traits, provides sim type aliases.

pub use ferrite_core::flash::*;

pub mod sim;

pub type Flash = FlashImpl<sim::FileFlash>;

pub fn from_image(bytes: std::vec::Vec<u8>) -> Flash {
    Flash::with_backend(sim::FileFlash::new(bytes))
}

pub fn new() -> Flash {
    Flash::with_backend(sim::FileFlash::new(std::vec::Vec::new()))
}
