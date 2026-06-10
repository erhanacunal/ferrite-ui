// BSP Touch module (sim) — re-exports framework traits + free-fn ctor.
// Hit testing lives in the framework (`ferrite_core::touch::hit_test`).

pub use ferrite_core::touch::*;

pub mod sim;

pub type Touch = TouchImpl<sim::MouseTouch>;

pub fn new(mouse: sim::MouseState) -> Touch {
    Touch::with_backend(sim::MouseTouch::new(mouse))
}
