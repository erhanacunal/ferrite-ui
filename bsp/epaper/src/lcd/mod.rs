// BSP LCD module (epaper) — re-exports framework traits + free-fn ctor.
//
// The ED047TC1 driver is split across sibling modules that the backend's
// `super::` paths resolve against, so they must be declared here:
//   - `display`  : PSRAM framebuffer, waveform, tainted-row tracking
//   - `ed047tc1` : low-level I8080 / RMT controller
//   - `rmt`/`error` : RMT pulse generation + error type

pub use ferrite_core::lcd::*;

pub(crate) mod display;
pub(crate) mod ed047tc1;
pub(crate) mod error;
pub(crate) mod rmt;

pub mod epaper;

pub type Lcd = LcdImpl<epaper::EpdLcd>;

pub fn new() -> Lcd {
    Lcd::with_backend(epaper::EpdLcd::new())
}
