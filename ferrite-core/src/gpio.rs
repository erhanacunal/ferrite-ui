/// Backend abstraction for the board's general-purpose I/O header.
///
/// A board exposes `PIN_COUNT` user pins numbered `0..PIN_COUNT`; the BSP maps
/// each number to its own port/pin (GD32 Nextion: PC2–PC9, N32903: GPD3, GPD2,
/// GPB2–GPB6, GPA0), so Ferrite programs use the same numbers on every board.
/// Boards without a header use [`NoGpio`] (no pins: every access faults).
///
/// Pins keep their power-on state until a program calls `pinMode`.

/// Input, no pull resistor.
pub const MODE_INPUT: u8 = 0;
/// Push-pull output.
pub const MODE_OUTPUT: u8 = 1;
/// Input with the pull-up enabled.
pub const MODE_INPUT_PULLUP: u8 = 2;
/// Input with the pull-down enabled (not every board has one).
pub const MODE_INPUT_PULLDOWN: u8 = 3;

pub trait GpioBackend {
    /// Number of user pins (`0..PIN_COUNT`).
    const PIN_COUNT: u8;

    /// Configure `pin` (< `PIN_COUNT`). Returns `false` if the board does not
    /// support `mode` on that pin.
    fn set_mode(&self, pin: u8, mode: u8) -> bool;

    /// Drive an output pin. A pin that is not an output ignores it.
    fn write(&self, pin: u8, high: bool);

    /// Current pin level (outputs read back what they drive).
    fn read(&self, pin: u8) -> bool;
}

/// Backend for boards without user GPIO.
pub struct NoGpio;

impl GpioBackend for NoGpio {
    const PIN_COUNT: u8 = 0;

    fn set_mode(&self, _pin: u8, _mode: u8) -> bool {
        false
    }

    fn write(&self, _pin: u8, _high: bool) {}

    fn read(&self, _pin: u8) -> bool {
        false
    }
}

pub struct GpioImpl<B: GpioBackend> {
    be: B,
}

impl<B: GpioBackend> GpioImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self { be }
    }

    /// `pin` as a backend pin number, or `None` if the board has no such pin.
    #[inline]
    fn pin(pin: i32) -> Option<u8> {
        (0..B::PIN_COUNT as i32).contains(&pin).then_some(pin as u8)
    }

    /// `pinMode(pin, mode)`. `false` on an unknown pin or unsupported mode.
    pub fn set_mode(&self, pin: i32, mode: i32) -> bool {
        match (Self::pin(pin), u8::try_from(mode)) {
            (Some(p), Ok(m)) if m <= MODE_INPUT_PULLDOWN => self.be.set_mode(p, m),
            _ => false,
        }
    }

    /// `pinWrite(pin, value)`: any non-zero value drives high. `false` on an
    /// unknown pin.
    pub fn write(&self, pin: i32, value: i32) -> bool {
        match Self::pin(pin) {
            Some(p) => {
                self.be.write(p, value != 0);
                true
            }
            None => false,
        }
    }

    /// `pinRead(pin)` → 0 / 1, or `None` on an unknown pin.
    pub fn read(&self, pin: i32) -> Option<i32> {
        Self::pin(pin).map(|p| self.be.read(p) as i32)
    }
}

impl GpioImpl<NoGpio> {
    /// Convenience constructor for boards without user GPIO.
    pub fn none() -> Self {
        Self { be: NoGpio }
    }
}

#[cfg(feature = "mock")]
pub type Gpio = GpioImpl<crate::mock::MockGpio>;
