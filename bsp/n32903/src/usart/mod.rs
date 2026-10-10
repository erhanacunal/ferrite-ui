//! USART backend for N32903 — UART1 (GPA10 RX / GPA11 TX, 115200 8N1) with an
//! interrupt-driven RX ring buffer.
//!
//! The board gates UART1 TX through a transistor + 74HC86 enabled by GPA7
//! high; without it nothing leaves the board. The NAND loader already sets
//! it, `init` asserts it again.

use core::ptr::addr_of;

use ferrite_core::usart::UsartBackend;
use n32903_rs::aic::{self, IntLevel};
use n32903_rs::registers::*;
use n32903_rs::uart::{Uart, UartConfig};

/// The console UART. Set once by `init`, read-only afterwards.
static mut UART: Option<Uart> = None;

const RBUF_SIZE: usize = 4096;
const RBUF_MASK: usize = RBUF_SIZE - 1;

static mut RBUF: [u8; RBUF_SIZE] = [0u8; RBUF_SIZE];
static mut RBUF_HEAD: usize = 0;
static mut RBUF_TAIL: usize = 0;

/// GPA7 enables the board's TX transistor.
const TX_ENABLE_PIN: u32 = 1 << 7;

fn uart() -> Option<&'static Uart> {
    unsafe { (&*addr_of!(UART)).as_ref() }
}

/// Move everything in the hardware RX FIFO into the ring buffer. Runs in the
/// UART interrupt, and with IRQs masked from `rx_read_byte`.
fn drain_fifo() {
    let Some(uart) = uart() else { return };
    unsafe {
        uart.clear_rx_overflow();
        while let Some(byte) = uart.getchar() {
            let next = (RBUF_HEAD + 1) & RBUF_MASK;
            if next != RBUF_TAIL {
                RBUF[RBUF_HEAD] = byte;
                RBUF_HEAD = next;
            }
        }
    }
}

unsafe extern "C" fn uart_isr() {
    drain_fifo();
}

/// Run `f` with IRQs masked (CPSR I bit), restoring the previous state.
fn without_irq<R>(f: impl FnOnce() -> R) -> R {
    unsafe {
        let was_on = aic::aic_get_ibit_state();
        aic::aic_set_local_interrupt(aic::LocalIntState::DisableIrq);
        let r = f();
        if was_on {
            aic::aic_set_local_interrupt(aic::LocalIntState::EnableIrq);
        }
        r
    }
}

pub struct HwUart;

impl UsartBackend for HwUart {
    fn write_byte(&self, byte: u8) {
        if let Some(uart) = uart() {
            unsafe { uart.put_raw(byte) };
        }
    }

    fn flush(&self) {
        if let Some(uart) = uart() {
            unsafe { uart.flush() };
        }
    }

    fn rx_read_byte(&self) -> Option<u8> {
        // Also pull the FIFO here: bytes below the trigger level that the
        // timeout interrupt has not reported yet are picked up immediately.
        without_irq(|| {
            drain_fifo();
            unsafe {
                if RBUF_HEAD == RBUF_TAIL {
                    return None;
                }
                let byte = RBUF[RBUF_TAIL];
                RBUF_TAIL = (RBUF_TAIL + 1) & RBUF_MASK;
                Some(byte)
            }
        })
    }
}

/// Initialise UART1 and its RX interrupt (IRQ source `IRQ_UART`).
///
/// # Safety
/// Call once at boot, after `aic::aic_initialize`.
pub unsafe fn init() {
    unsafe {
        // TX gate: drive GPA7 high, then make it a GPIO output.
        reg_write32(REG_GPIOA_DOUT, reg_read32(REG_GPIOA_DOUT) | TX_ENABLE_PIN);
        reg_write32(REG_GPAFUN, reg_read32(REG_GPAFUN) & !0x0000_C000);
        reg_write32(REG_GPIOA_OMD, reg_read32(REG_GPIOA_OMD) | TX_ENABLE_PIN);

        // Let the loader's last message leave before the FIFO reset.
        for _ in 0..1_000_000 {
            if reg_read32(REG_UART1_FSR) & TE_FLAG != 0 {
                break;
            }
        }

        UART = Some(Uart::init(&UartConfig {
            rx_fifo_trigger: 8,
            ..UartConfig::default()
        }));

        aic::aic_install_isr(IntLevel::Irq2, IRQ_UART, uart_isr);
        aic::aic_enable_interrupt(IRQ_UART);
        if let Some(uart) = uart() {
            uart.enable_rx_interrupt();
        }
    }
}

pub fn new() -> HwUart {
    HwUart
}

/// Diagnostic text to the console (blocking).
pub fn log(s: &str) {
    log_bytes(s.as_bytes());
}

/// Diagnostic bytes to the console (blocking).
pub fn log_bytes(data: &[u8]) {
    if let Some(uart) = uart() {
        for &b in data {
            unsafe { uart.put_raw(b) };
        }
    }
}
