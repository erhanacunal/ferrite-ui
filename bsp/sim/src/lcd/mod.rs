// BSP LCD module (sim) — re-exports framework traits, provides sim type aliases.

pub use ferrite_core::lcd::*;

/// Screen size: the Nextion's 800×480, or the Exynos 4412 tablet's 1280×800
/// with the `tablet` feature (examples/exynos4412).
#[cfg(not(feature = "tablet"))]
pub const WIDTH: u16 = 800;
#[cfg(not(feature = "tablet"))]
pub const HEIGHT: u16 = 480;
#[cfg(feature = "tablet")]
pub const WIDTH: u16 = 1280;
#[cfg(feature = "tablet")]
pub const HEIGHT: u16 = 800;

pub mod sim;

pub type Lcd = LcdImpl<sim::SimLcd>;

pub fn new(fb: sim::Framebuffer) -> Lcd {
    Lcd::with_backend(sim::SimLcd::new(fb))
}
