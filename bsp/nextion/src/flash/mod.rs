pub use ferrite_core::flash::*;

pub mod hw;

pub type Flash = FlashImpl<hw::FpgaFlash>;

pub fn new() -> Flash {
    Flash::with_backend(hw::FpgaFlash::new())
}
pub fn init() -> Flash {
    Flash::with_backend(hw::FpgaFlash::init())
}
