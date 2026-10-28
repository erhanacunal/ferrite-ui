//! User GPIO for the LilyGo T5-ePaper-S3: pin 0 = BUTTON_1 (GPIO21).
//!
//! The board has no free header pins, so the only user pin is the button. It
//! is wired to ground (pressed = 0) and is input-only: an output would short
//! the pad when the button is pressed. `main::init_gpio` sets it up as an
//! input with the pull-up on, and keeps the esp-hal `Input` alive so nothing
//! else claims the pad; `pinMode` only switches the pulls in IO_MUX.

use ferrite_core::gpio::{GpioBackend, MODE_INPUT, MODE_INPUT_PULLDOWN, MODE_INPUT_PULLUP};

/// GPIO_IN register (bank 0, GPIO0–31).
const GPIO_IN: *const u32 = 0x6000_403C as *const u32;
/// IO_MUX_GPIOn_REG = IO_MUX base + 4 + 4 × n.
const IO_MUX_BASE: u32 = 0x6000_9000;

const FUN_WPD: u32 = 1 << 7;
const FUN_WPU: u32 = 1 << 8;

/// Pad number of each user pin.
const PADS: [u32; 1] = [21];

pub struct EpdGpio;

impl GpioBackend for EpdGpio {
    const PIN_COUNT: u8 = PADS.len() as u8;

    fn set_mode(&self, pin: u8, mode: u8) -> bool {
        let pulls = match mode {
            MODE_INPUT => 0,
            MODE_INPUT_PULLUP => FUN_WPU,
            MODE_INPUT_PULLDOWN => FUN_WPD,
            _ => return false,
        };
        let reg = (IO_MUX_BASE + 4 + 4 * PADS[pin as usize]) as *mut u32;
        unsafe {
            let v = core::ptr::read_volatile(reg);
            core::ptr::write_volatile(reg, (v & !(FUN_WPU | FUN_WPD)) | pulls);
        }
        true
    }

    // Input-only pins: nothing to drive.
    fn write(&self, _pin: u8, _high: bool) {}

    fn read(&self, pin: u8) -> bool {
        unsafe { core::ptr::read_volatile(GPIO_IN) & (1 << PADS[pin as usize]) != 0 }
    }
}
