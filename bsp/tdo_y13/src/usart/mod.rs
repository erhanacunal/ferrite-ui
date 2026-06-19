//! USART backend for TDO Y13 — UART2 with interrupt-driven RX ring buffer.
//!
//! UART2 is used (PE7=TX, PE8=RX) so PE0/PE1 stay dedicated to the LCD panel's
//! configuration SPI.

use core::ptr::addr_of;
use ferrite_core::usart::UsartBackend;
use f1c100s::uart::{self, Uart, init_uart2};

static mut UART2: Option<Uart> = None;

const RBUF_SIZE: usize = 4096;
const RBUF_MASK: usize = RBUF_SIZE - 1;

static mut RBUF: [u8; RBUF_SIZE] = [0u8; RBUF_SIZE];
static mut RBUF_HEAD: usize = 0;
static mut RBUF_TAIL: usize = 0;

fn uart_rx_isr(_vector: u32) {
    let uart_ref = unsafe { &*addr_of!(UART2) };
    if let Some(uart) = uart_ref {
        while uart.rx_ready() {
            if let Some(byte) = uart.read_byte() {
                unsafe {
                    let next = (RBUF_HEAD + 1) & RBUF_MASK;
                    if next != RBUF_TAIL {
                        RBUF[RBUF_HEAD] = byte;
                        RBUF_HEAD = next;
                    }
                }
            }
        }
    }
}

pub struct HwUart;

impl UsartBackend for HwUart {
    fn write_byte(&self, byte: u8) {
        if let Some(uart) = unsafe { &*addr_of!(UART2) } {
            uart.write_byte(byte);
        }
    }

    fn flush(&self) {}

    fn rx_read_byte(&self) -> Option<u8> {
        unsafe {
            if RBUF_HEAD == RBUF_TAIL { return None; }
            let byte = RBUF[RBUF_TAIL];
            RBUF_TAIL = (RBUF_TAIL + 1) & RBUF_MASK;
            Some(byte)
        }
    }
}

pub unsafe fn init() {
    use f1c100s::clock::{self, BusGate};
    use f1c100s::gpio::{self, Port, PullMode};
    use f1c100s::interrupt;

    
    let uart = init_uart2();    
    uart.enable_rx_interrupt();

    interrupt::install(interrupt::UART2_INTERRUPT, uart_rx_isr);
    interrupt::unmask(interrupt::UART2_INTERRUPT);

    unsafe { UART2 = Some(uart) };
}

pub fn new() -> HwUart {
    HwUart
}

/// Emit a single raw byte on UART2 for low-level diagnostics (e.g. from an ISR,
/// where the `Ctx` USART handle is not reachable). No-op until `init()` has run.
pub fn dbg_putc(byte: u8) {
    if let Some(uart) = unsafe { &*addr_of!(UART2) } {
        uart.write_byte(byte);
    }
}
