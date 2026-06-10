pub use ferrite_core::backlight::*;

pub mod hw;

pub type Backlight = BacklightImpl<hw::Pwm>;

// Free-fn constructors: inherent `impl` on the framework's `BacklightImpl`
// alias is illegal here (E0116, foreign type), so wrap `with_backend`.
pub fn init() -> Backlight {
    Backlight::with_backend(hw::Pwm::init())
}
