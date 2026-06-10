// BSP Backlight module — re-exports framework traits, provides epaper type aliases.

pub use ferrite_core::backlight::*;

pub mod epaper;

pub type Backlight = BacklightImpl<epaper::EpdBacklight>;

// Free-fn constructor: an inherent `impl` on the framework's `BacklightImpl`
// alias is illegal here (E0116, foreign type), so wrap `with_backend`.
pub fn init() -> Backlight {
    Backlight::with_backend(epaper::EpdBacklight::new())
}
