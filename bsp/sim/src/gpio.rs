//! Virtual user GPIO header for the simulator: 8 pins that remember their
//! mode and output level and log changes to stdout. Inputs read their pull
//! level (a floating input reads low).

use core::cell::Cell;
use ferrite_core::gpio::{GpioBackend, MODE_INPUT_PULLUP, MODE_OUTPUT};

#[derive(Default)]
pub struct SimGpio {
    mode: [Cell<u8>; 8],
    level: [Cell<bool>; 8],
}

impl GpioBackend for SimGpio {
    const PIN_COUNT: u8 = 8;

    fn set_mode(&self, pin: u8, mode: u8) -> bool {
        println!("gpio: pin {pin} mode {mode}");
        self.mode[pin as usize].set(mode);
        true
    }

    fn write(&self, pin: u8, high: bool) {
        let p = pin as usize;
        if self.mode[p].get() == MODE_OUTPUT && self.level[p].get() != high {
            println!("gpio: pin {pin} = {}", high as u8);
            self.level[p].set(high);
        }
    }

    fn read(&self, pin: u8) -> bool {
        let p = pin as usize;
        match self.mode[p].get() {
            MODE_OUTPUT => self.level[p].get(),
            m => m == MODE_INPUT_PULLUP,
        }
    }
}
