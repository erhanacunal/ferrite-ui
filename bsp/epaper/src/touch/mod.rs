// BSP Touch module (epaper) — re-exports framework traits + free-fn ctor.
// Hit testing lives in the framework (`ferrite_core::touch::hit_test`).

pub use ferrite_core::touch::*;

pub mod epaper;

pub type Touch = TouchImpl<epaper::EpdButtons>;

pub fn init() -> Touch {
    Touch::with_backend(epaper::EpdButtons::new())
}
