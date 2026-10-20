//! User GPIO header: pins 0..7 = PC2..PC9.
//!
//! `init_ports` leaves them floating inputs with ODR = 0. Only these pins'
//! CTL nibbles are touched (read-modify-write); PC0/PC1 (RTC) and PC13/PC14
//! share the port.

use ferrite_core::gpio::{GpioBackend, MODE_INPUT, MODE_INPUT_PULLDOWN, MODE_INPUT_PULLUP, MODE_OUTPUT};

const GPIOC_BASE: u32 = 0x4001_1000;
const GPIO_CTL0: u32 = GPIOC_BASE + 0x00;
const GPIO_ISTAT: u32 = GPIOC_BASE + 0x08;
const GPIO_BOP: u32 = GPIOC_BASE + 0x10;
const GPIO_BC: u32 = GPIOC_BASE + 0x14;

/// Header pin 0 is PC2.
const FIRST_PIN: u32 = 2;

// CTL nibbles (CTL[1:0] = mode, CTL[3:2] = config)
const CFG_INPUT_FLOATING: u32 = 0x4;
const CFG_INPUT_PULL: u32 = 0x8; // ODR picks the direction: 1 = up, 0 = down
const CFG_OUTPUT_PP_2MHZ: u32 = 0x2;

pub struct HeaderGpio;

/// Port pin number and its CTL register for header pin `pin`.
fn port_pin(pin: u8) -> (u32, *mut u32) {
    let n = FIRST_PIN + pin as u32;
    let ctl = (GPIO_CTL0 + (n / 8) * 4) as *mut u32;
    (n, ctl)
}

fn set_odr(n: u32, high: bool) {
    let reg = if high { GPIO_BOP } else { GPIO_BC };
    unsafe { core::ptr::write_volatile(reg as *mut u32, 1 << n) };
}

impl GpioBackend for HeaderGpio {
    const PIN_COUNT: u8 = 8;

    fn set_mode(&self, pin: u8, mode: u8) -> bool {
        let (n, ctl) = port_pin(pin);
        let cfg = match mode {
            MODE_INPUT => CFG_INPUT_FLOATING,
            MODE_OUTPUT => CFG_OUTPUT_PP_2MHZ,
            MODE_INPUT_PULLUP => {
                set_odr(n, true);
                CFG_INPUT_PULL
            }
            MODE_INPUT_PULLDOWN => {
                set_odr(n, false);
                CFG_INPUT_PULL
            }
            _ => return false,
        };
        let shift = (n % 8) * 4;
        unsafe {
            let v = core::ptr::read_volatile(ctl);
            core::ptr::write_volatile(ctl, (v & !(0xF << shift)) | (cfg << shift));
        }
        true
    }

    fn write(&self, pin: u8, high: bool) {
        let (n, ctl) = port_pin(pin);
        // On an input, ODR selects the pull direction: leave it alone.
        let is_output = unsafe { core::ptr::read_volatile(ctl) } >> ((n % 8) * 4) & 0x3 != 0;
        if is_output {
            set_odr(n, high);
        }
    }

    fn read(&self, pin: u8) -> bool {
        let (n, _) = port_pin(pin);
        unsafe { core::ptr::read_volatile(GPIO_ISTAT as *const u32) & (1 << n) != 0 }
    }
}
