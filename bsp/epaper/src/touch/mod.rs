// BSP Touch module (epaper) — re-exports framework traits + free-fn ctor.
// The T5 has no touch screen; BUTTON_1 is user GPIO pin 0 (`user_gpio.rs`).

pub use ferrite_core::touch::*;

pub type Touch = TouchImpl<NoTouch>;

pub fn init() -> Touch {
    Touch::with_backend(NoTouch)
}
