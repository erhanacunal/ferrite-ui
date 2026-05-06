//! Console backend for LilyGo T5-ePaper-S3 (ESP32-S3).
//!
//! TX writes the ESP32-S3 USB-Serial-JTAG endpoint directly. On LilyGo ESP32-S3
//! boards this is the same console path used by `espflash monitor`.
//!
//! RX still reads UART0 directly; USB-Serial-JTAG RX can be added separately if
//! the ferrite protocol needs to run over the native USB console.

#![cfg(feature = "epaper")]

use core::ptr::{read_volatile, write_volatile};

use super::UsartBackend;

// --- ESP32-S3 USB-Serial-JTAG register map ---
//
// EP1_REG (0x6003_8000): read = pop one byte from OUT FIFO (host→device),
//                        write = push one byte into IN FIFO (device→host)
// EP1_CONF_REG (0x6003_8004):
//   bit 0  WR_DONE                — commit IN buffer, flush to host
//   bit 1  SERIAL_IN_EP_DATA_FREE — IN FIFO has space (safe to write)
//   bit 2  SERIAL_OUT_EP_DATA_AVAIL — OUT FIFO has data (safe to read)

const USB_DEVICE_BASE: u32 = 0x6003_8000;
const USB_EP1:      *mut u32 = (USB_DEVICE_BASE + 0x00) as *mut u32;
const USB_EP1_CONF: *mut u32 = (USB_DEVICE_BASE + 0x04) as *mut u32;

const USB_WR_DONE: u32 = 1 << 0;
const USB_SERIAL_IN_EP_DATA_FREE:  u32 = 1 << 1;
const USB_SERIAL_OUT_EP_DATA_AVAIL: u32 = 1 << 2;

// SYSTEM peripheral clock/reset control. USB_DEVICE is bit 10 in PERIP_*_EN1.
const SYSTEM_BASE: u32 = 0x600C_0000;
const SYSTEM_PERIP_CLK_EN1: *mut u32 = (SYSTEM_BASE + 0x1C) as *mut u32;
const SYSTEM_PERIP_RST_EN1: *mut u32 = (SYSTEM_BASE + 0x24) as *mut u32;
const SYSTEM_USB_DEVICE_BIT: u32 = 1 << 10;

#[inline]
fn init_usb_console() {
    unsafe {
        let clk = read_volatile(SYSTEM_PERIP_CLK_EN1);
        write_volatile(SYSTEM_PERIP_CLK_EN1, clk | SYSTEM_USB_DEVICE_BIT);

        // Do not pulse reset here: resetting USB_DEVICE breaks an active
        // USB-JTAG connection. Just make sure reset is deasserted.
        let rst = read_volatile(SYSTEM_PERIP_RST_EN1);
        write_volatile(SYSTEM_PERIP_RST_EN1, rst & !SYSTEM_USB_DEVICE_BIT);
    }
}

/// Write one packet to USB-Serial-JTAG without waiting forever for a host.
#[inline]
fn write_usb_packet(bytes: &[u8]) {
    unsafe {
        init_usb_console();

        let mut spins = 0u32;
        while read_volatile(USB_EP1_CONF) & USB_SERIAL_IN_EP_DATA_FREE == 0 {
            spins += 1;
            if spins > 100_000 {
                return;
            }
        }
        for &byte in bytes {
            write_volatile(USB_EP1, byte as u32);
        }
        write_volatile(USB_EP1_CONF, USB_WR_DONE);
    }
}

// --- Backend struct ---

pub struct EspUart;

impl EspUart {
    /// Create the UART handle.  Assumes UART0 was already configured by
    /// the platform startup code.
    pub fn init() -> Self {
        EspUart
    }
}

impl UsartBackend for EspUart {
    fn write_byte(&self, byte: u8) {
        write_usb_packet(&[byte]);
    }

    fn flush(&self) {}
}

// --- Free functions (mirrors usart::hw interface) ---

/// Write a byte slice to USB-Serial-JTAG.
pub fn dbg(msg: &[u8]) {
    for chunk in msg.chunks(64) {
        write_usb_packet(chunk);
    }
}

/// Write a decimal `u16` value to UART0.
pub fn dbg_u16(val: u16) {
    let mut buf = [0u8; 5];
    let mut n = val;
    if n == 0 {
        write_usb_packet(b"0");
        return;
    }
    let mut pos = 5usize;
    while n > 0 {
        pos -= 1;
        buf[pos] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    dbg(&buf[pos..]);
}

/// Return `true` if the USB-Serial-JTAG OUT FIFO has at least one byte.
pub fn rx_has_data() -> bool {
    unsafe { read_volatile(USB_EP1_CONF) & USB_SERIAL_OUT_EP_DATA_AVAIL != 0 }
}

/// Read one byte from the USB-Serial-JTAG OUT FIFO, or `None` if empty.
pub fn rx_read_byte() -> Option<u8> {
    unsafe {
        if read_volatile(USB_EP1_CONF) & USB_SERIAL_OUT_EP_DATA_AVAIL == 0 {
            return None;
        }
        Some(read_volatile(USB_EP1 as *const u32) as u8)
    }
}

/// Return 1 if data is available, 0 otherwise (USB OUT EP has no byte count).
pub fn rx_len() -> usize {
    if rx_has_data() { 1 } else { 0 }
}

/// Drain all bytes currently available in the USB-Serial-JTAG OUT FIFO.
pub fn rx_clear() {
    unsafe {
        while read_volatile(USB_EP1_CONF) & USB_SERIAL_OUT_EP_DATA_AVAIL != 0 {
            let _ = read_volatile(USB_EP1 as *const u32);
        }
    }
}
