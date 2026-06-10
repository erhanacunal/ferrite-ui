// BSP Flash module — re-exports framework traits, provides epaper type aliases.

pub use ferrite_core::flash::*;

pub mod epaper;

pub use epaper::probe_ferrite_fs_preamble;

pub type Flash = FlashImpl<epaper::EspFlash>;

pub fn new() -> Flash {
    Flash::with_backend(epaper::EspFlash::new())
}
