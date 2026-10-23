/// Shared I2C bus — bit-banged on PC0 (SCL) / PC1 (SDA)
///
/// Devices on the bus:
///   0x51       AT8563T RTC (`rtc/hw.rs`)
///   0x54..0x57 24C08 EEPROM, one address per 256-byte block (`eeprom/hw.rs`)
///
/// GPIO configured as open-drain output by init_ports() (CTL0 = 0x77).
/// Both lines idle HIGH (external pull-ups). Initial state set in init_ports().
///
/// ~100kHz (spin delays tuned for 108MHz sysclk). Only main-loop code (VM,
/// runtime) uses the bus, never an ISR, so transactions don't need a lock.
/// ISRs may stretch a clock phase; I2C has no minimum clock rate.

const GPIOC_BASE: u32 = 0x4001_1000;
const GPIO_IDR: u32 = GPIOC_BASE + 0x08;
const GPIO_BOP: u32 = GPIOC_BASE + 0x10;
const GPIO_BC: u32 = GPIOC_BASE + 0x14;

const SCL_PIN: u32 = 0;
const SDA_PIN: u32 = 1;

#[inline]
fn delay() {
    spin(80);
}

#[inline]
fn scl_high() {
    unsafe {
        core::ptr::write_volatile(GPIO_BOP as *mut u32, 1 << SCL_PIN);
    }
}

#[inline]
fn scl_low() {
    unsafe {
        core::ptr::write_volatile(GPIO_BC as *mut u32, 1 << SCL_PIN);
    }
}

#[inline]
fn sda_high() {
    unsafe {
        core::ptr::write_volatile(GPIO_BOP as *mut u32, 1 << SDA_PIN);
    }
}

#[inline]
fn sda_low() {
    unsafe {
        core::ptr::write_volatile(GPIO_BC as *mut u32, 1 << SDA_PIN);
    }
}

#[inline]
fn sda_read() -> bool {
    let idr = unsafe { core::ptr::read_volatile(GPIO_IDR as *const u32) };
    idr & (1 << SDA_PIN) != 0
}

/// START (or repeated START).
pub fn start() {
    sda_high();
    delay();
    scl_high();
    delay();
    sda_low();
    delay();
    scl_low();
    delay();
}

pub fn stop() {
    sda_low();
    delay();
    scl_high();
    delay();
    sda_high();
    delay();
}

/// Clock out one byte, MSB first. Returns `true` if the device ACKed.
pub fn write_byte(byte: u8) -> bool {
    for i in (0..8).rev() {
        if byte & (1 << i) != 0 {
            sda_high();
        } else {
            sda_low();
        }
        delay();
        scl_high();
        delay();
        scl_low();
    }
    sda_high();
    delay();
    scl_high();
    delay();
    let ack = !sda_read();
    scl_low();
    delay();
    ack
}

/// Clock in one byte; `send_ack` = more bytes follow (ACK), else NACK.
pub fn read_byte(send_ack: bool) -> u8 {
    sda_high();
    let mut byte: u8 = 0;
    for _ in 0..8 {
        delay();
        scl_high();
        delay();
        byte <<= 1;
        if sda_read() {
            byte |= 1;
        }
        scl_low();
    }
    if send_ack {
        sda_low();
    } else {
        sda_high();
    }
    delay();
    scl_high();
    delay();
    scl_low();
    sda_high();
    delay();
    byte
}

#[inline]
fn spin(cycles: u32) {
    for _ in 0..cycles {
        cortex_m::asm::nop();
    }
}
