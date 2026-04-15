#[cfg(not(feature = "host"))]
pub mod hw;
#[cfg(feature = "host")]
pub mod sim;

#[cfg(not(feature = "host"))]
pub use hw::{delay_ms, init, millis};

#[cfg(feature = "host")]
pub use sim::{delay_ms, init, millis};
