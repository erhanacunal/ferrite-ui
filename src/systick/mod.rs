#[cfg(feature = "firmware")]
pub mod hw;
#[cfg(feature = "host")]
pub mod sim;
#[cfg(feature = "epaper")]
pub mod epaper;

#[cfg(feature = "firmware")]
pub use hw::{delay_ms, init, millis};

#[cfg(feature = "host")]
pub use sim::{delay_ms, init, millis};

#[cfg(feature = "epaper")]
pub use epaper::{delay_ms, init, millis};
