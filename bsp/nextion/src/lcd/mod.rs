// BSP LCD module — re-exports framework traits, provides type aliases, redirects backend.

pub use ferrite_core::lcd::*;

pub const WIDTH: u16 = 800;
pub const HEIGHT: u16 = 480;

pub mod hw;

pub type Lcd = LcdImpl<hw::FpgaLcd>;

pub fn new(gpio: crate::gpio::Gpio) -> Lcd {
    Lcd::with_backend(hw::FpgaLcd::new(gpio))
}
