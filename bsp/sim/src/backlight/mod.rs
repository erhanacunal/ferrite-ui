// BSP Backlight module (sim) — re-exports framework traits, provides sim type aliases.

pub use ferrite_core::backlight::*;

pub mod sim;

pub type Backlight = BacklightImpl<sim::StubBacklight>;

pub fn new() -> Backlight {
    Backlight::with_backend(sim::StubBacklight::new())
}
