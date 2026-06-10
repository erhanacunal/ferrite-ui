/// Backend abstraction for system tick timer.
/// BSPs implement init, delay_ms, and millis since boot.
use core::marker::PhantomData;

pub trait SystickBackend {
    /// Initialize the system tick timer.
    fn init();

    /// Blocking delay in milliseconds.
    fn delay_ms(ms: u32);

    /// Milliseconds since boot (wraps at ~49 days on 32-bit).
    fn millis() -> u32;
}

/// Zero-sized wrapper giving the framework a `P`-typed handle to the system tick.
/// All backend methods are associated functions, so this just forwards to `S::…`.
pub struct SystickImpl<S: SystickBackend> {
    _marker: PhantomData<S>,
}

impl<S: SystickBackend> SystickImpl<S> {
    /// Initialize the timer and return a handle.
    pub fn init() -> Self {
        S::init();
        Self {
            _marker: PhantomData,
        }
    }

    /// Construct a handle without (re)initializing the timer.
    pub const fn handle() -> Self {
        Self {
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn millis(&self) -> u32 {
        S::millis()
    }

    #[inline]
    pub fn delay_ms(&self, ms: u32) {
        S::delay_ms(ms)
    }
}
