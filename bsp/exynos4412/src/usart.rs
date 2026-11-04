//! USART backend — UART2, the board's console (115200 8N1, set up by U-Boot).
//!
//! UART2 has only a 16-byte RX FIFO, less than one main-loop pass at 115200
//! baud, so RX is interrupt-driven into a ring (`handle_irq` in main.rs calls
//! [`rx_isr`]). TX is polled. The HAL's `println!` uses the same UART: keep it
//! to boot messages (the host tools skip boot-log text).

use exynos_4412_rs::drivers::uart::{Uart, UartId};
use ferrite_core::usart::UsartBackend;

pub const UART: UartId = UartId::Uart2;

const RING: usize = 16 * 1024;
const MASK: usize = RING - 1;

static mut BUF: [u8; RING] = [0; RING];
// Head is written only by the ISR, tail only by the main loop.
static mut HEAD: usize = 0;
static mut TAIL: usize = 0;

// DIAG: bytes received, bytes dropped (ring full), UERSTAT bits seen
// (bit 0 overrun, 1 parity, 2 frame, 3 break). ISR-written.
pub static mut RX_TOTAL: u32 = 0;
pub static mut RX_DROPPED: u32 = 0;
pub static mut RX_ERRORS: u32 = 0;

/// Drains the RX FIFO into the ring. IRQ context.
pub fn rx_isr() {
    let uart = Uart::new(UART);
    uart.clear_rx_interrupt();
    unsafe { RX_ERRORS |= uart.take_errors() };
    while let Some(b) = uart.try_read_byte() {
        unsafe {
            RX_TOTAL += 1;
            let head = (&raw const HEAD).read_volatile();
            let next = (head + 1) & MASK;
            if next != (&raw const TAIL).read_volatile() {
                (*(&raw mut BUF))[head] = b;
                (&raw mut HEAD).write_volatile(next);
            } else {
                RX_DROPPED += 1;
            }
        }
    }
    unsafe { RX_ERRORS |= uart.take_errors() };
}

pub struct HwUart;

impl UsartBackend for HwUart {
    fn write_byte(&self, byte: u8) {
        Uart::new(UART).write_byte(byte);
    }

    fn flush(&self) {
        Uart::new(UART).flush();
    }

    fn rx_read_byte(&self) -> Option<u8> {
        unsafe {
            let tail = (&raw const TAIL).read_volatile();
            if tail == (&raw const HEAD).read_volatile() {
                return None;
            }
            let b = (*(&raw const BUF))[tail];
            (&raw mut TAIL).write_volatile((tail + 1) & MASK);
            Some(b)
        }
    }

    fn rx_stats(&self) -> (usize, usize) {
        let (h, t) = unsafe { ((&raw const HEAD).read_volatile(), (&raw const TAIL).read_volatile()) };
        ((h.wrapping_sub(t)) & MASK, RING)
    }
}

/// Unmasks the RX interrupt at the UART; the GIC side is in main.rs.
pub fn init() {
    Uart::new(UART).enable_rx_interrupt();
}
