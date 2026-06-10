// BSP LCD module (sim) — re-exports framework traits, provides sim type aliases.

pub use ferrite_core::lcd::*;

pub const WIDTH: u16 = 800;
pub const HEIGHT: u16 = 480;

pub mod sim;

pub type Lcd = LcdImpl<sim::SimLcd>;

pub fn new(fb: sim::Framebuffer) -> Lcd {
    Lcd::with_backend(sim::SimLcd::new(fb))
}
