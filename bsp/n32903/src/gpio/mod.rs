//! User GPIO header: pins 0..7 = GPD3, GPD2, GPB2..GPB6, GPA0.
//!
//! A pin keeps its power-on function until the program calls `pinMode`, which
//! switches it to GPIO. The SoC has pull-ups only (no pull-downs).

use ferrite_core::gpio::{GpioBackend, MODE_INPUT, MODE_INPUT_PULLUP, MODE_OUTPUT};
use n32903_rs::gpio::{self, GpioDirection, GpioPort};
use n32903_rs::registers::{REG_GPIOA_OMD, REG_GPIOB_OMD, REG_GPIOD_OMD, reg_read32};

const PINS: [(GpioPort, u8); 8] = [
    (GpioPort::PortD, 3),
    (GpioPort::PortD, 2),
    (GpioPort::PortB, 2),
    (GpioPort::PortB, 3),
    (GpioPort::PortB, 4),
    (GpioPort::PortB, 5),
    (GpioPort::PortB, 6),
    (GpioPort::PortA, 0),
];

pub struct HeaderGpio;

impl GpioBackend for HeaderGpio {
    const PIN_COUNT: u8 = PINS.len() as u8;

    fn set_mode(&self, pin: u8, mode: u8) -> bool {
        let (port, n) = PINS[pin as usize];
        let mask = 1u16 << n;
        let (dir, pull) = match mode {
            MODE_INPUT => (GpioDirection::Input, false),
            MODE_INPUT_PULLUP => (GpioDirection::Input, true),
            MODE_OUTPUT => (GpioDirection::Output, false),
            _ => return false,
        };
        unsafe {
            gpio::gpio_configure_pin(port, n);
            gpio::gpio_set_pull(port, mask, if pull { mask } else { 0 });
            gpio::gpio_set_direction(port, mask, dir);
        }
        true
    }

    fn write(&self, pin: u8, high: bool) {
        let (port, n) = PINS[pin as usize];
        let omd = match port {
            GpioPort::PortA => REG_GPIOA_OMD,
            GpioPort::PortB => REG_GPIOB_OMD,
            _ => REG_GPIOD_OMD,
        };
        let mask = 1u16 << n;
        unsafe {
            if reg_read32(omd) & mask as u32 != 0 {
                gpio::gpio_set_output(port, mask, if high { mask } else { 0 });
            }
        }
    }

    fn read(&self, pin: u8) -> bool {
        let (port, n) = PINS[pin as usize];
        unsafe { gpio::gpio_read_port(port) & (1 << n) != 0 }
    }
}
