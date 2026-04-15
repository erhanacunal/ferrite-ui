use std::cell::Cell;
use std::rc::Rc;

use super::{CalParams, TouchBackend};

/// Shared mouse state — the sim binary's window loop writes into it each frame,
/// and the `MouseTouch` backend reads from it whenever `Touch::poll()` fires.
#[derive(Clone)]
pub struct MouseState {
    inner: Rc<Cell<Snapshot>>,
}

#[derive(Clone, Copy, Default)]
struct Snapshot {
    x: u16,
    y: u16,
    pressed: bool,
}

impl MouseState {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(Cell::new(Snapshot::default())),
        }
    }

    /// Update from the window event loop.
    pub fn update(&self, x: u16, y: u16, pressed: bool) {
        self.inner.set(Snapshot { x, y, pressed });
    }
}

pub struct MouseTouch {
    state: MouseState,
}

impl MouseTouch {
    pub fn new(state: MouseState) -> Self {
        Self { state }
    }
}

impl TouchBackend for MouseTouch {
    fn is_active(&self) -> bool {
        self.state.inner.get().pressed
    }

    fn read_screen(&self, _cal: &CalParams) -> Option<(u16, u16)> {
        let s = self.state.inner.get();
        if s.pressed { Some((s.x, s.y)) } else { None }
    }
}
