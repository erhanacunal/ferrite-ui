//! USART backend — UART2, the board's console (115200 8N1, set up by U-Boot).
//!
//! UART2 has only a 16-byte RX FIFO, less than one main-loop pass at 115200
//! baud, so RX is interrupt-driven into a ring (`handle_irq` in main.rs calls
//! [`rx_isr`]). TX is polled. The HAL's `println!` uses the same UART: keep it
//! to boot messages (the host tools skip boot-log text).

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use exynos_4412_rs::drivers::uart::{Uart, UartId};
use ferrite_core::usart::UsartBackend;

pub const UART: UartId = UartId::Uart2;

const RING: usize = 16 * 1024;
const MASK: usize = RING - 1;

static mut BUF: [u8; RING] = [0; RING];
// Head is written only by the ISR, tail only by the main loop. Atomics (not
// plain statics) so the compiler orders the ring bytes against the indices.
static HEAD: AtomicUsize = AtomicUsize::new(0);
static TAIL: AtomicUsize = AtomicUsize::new(0);

// DIAG: bytes received, bytes dropped (ring full), and per-kind counts of
// the UERSTAT errors seen: overrun (IRQ came too late, bytes lost), parity,
// frame (line noise / baud mismatch: byte corrupted), break. ISR-written.
pub static RX_TOTAL: AtomicU32 = AtomicU32::new(0);
pub static RX_DROPPED: AtomicU32 = AtomicU32::new(0);
pub static RX_ERR: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];

fn count_errors(bits: u32) {
    for (i, n) in RX_ERR.iter().enumerate() {
        if bits & (1 << i) != 0 {
            n.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Drains the RX FIFO into the ring. IRQ context.
pub fn rx_isr() {
    let uart = Uart::new(UART);
    uart.clear_rx_interrupt();
    count_errors(uart.take_errors());
    let mut head = HEAD.load(Ordering::Relaxed);
    let tail = TAIL.load(Ordering::Acquire);
    while let Some(b) = uart.try_read_byte() {
        RX_TOTAL.fetch_add(1, Ordering::Relaxed);
        let next = (head + 1) & MASK;
        if next != tail {
            // SAFETY: slot `head` is outside [tail, head), which the main
            // loop reads; it sees it only after the Release store below.
            unsafe { (*(&raw mut BUF))[head] = b };
            head = next;
        } else {
            RX_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
    HEAD.store(head, Ordering::Release);
    count_errors(uart.take_errors());
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
        let tail = TAIL.load(Ordering::Relaxed);
        if tail == HEAD.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: [tail, head) was published by the ISR's Release store.
        let b = unsafe { (*(&raw const BUF))[tail] };
        TAIL.store((tail + 1) & MASK, Ordering::Release);
        Some(b)
    }

    fn rx_stats(&self) -> (usize, usize) {
        let (h, t) = (HEAD.load(Ordering::Relaxed), TAIL.load(Ordering::Relaxed));
        ((h.wrapping_sub(t)) & MASK, RING)
    }
}

/// DIAG: the console's real baud rate, from the registers U-Boot left —
/// UBRDIV/UFRACVAL and SCLK_UART2 (CLK_SRC_PERIL0 / CLK_DIV_PERIL0 bits
/// 11:8, Exynos 4412 UM 7.9.1) — so a baud mismatch (RX framing errors)
/// shows in the boot log. Assumes source 6 = SCLK_MPLL_USER_T.
pub fn report_clock() {
    use exynos_4412_rs::drivers::cmu::Cmu;
    use exynos_4412_rs::{println, soc};
    let reg = |off: usize| unsafe { core::ptr::read_volatile((soc::CMU_BASE + off) as *const u32) };
    let c = Uart::new(UART).config();
    let src = (reg(0xC250) >> 8) & 0xF;
    let div = (reg(0xC550) >> 8) & 0xF;
    let mpll = Cmu::new().fout_mpll_hz();
    let sclk = if src == 6 { mpll / (div + 1) } else { 0 };
    // baud = SCLK / (16 * (UBRDIV + 1 + UFRACVAL / 16)), in mBd.
    let denom = 16 * (c.ubrdiv as u64 + 1) + (c.ufracval & 0xF) as u64;
    let mbaud = if denom > 0 { sclk as u64 * 1000 / denom } else { 0 };
    let err = (mbaud as i64 - 115_200_000) * 10_000 / 115_200_000; // 0.01 %
    println!(
        "ferrite: UART2 ulcon {:#x} ucon {:#x} ufcon {:#x} ubrdiv {} ufracval {};          PERIL0 src {} div {}; MPLL {} Hz -> SCLK {} Hz, {}.{:03} Bd ({}{}.{:02} %)",
        c.ulcon, c.ucon, c.ufcon, c.ubrdiv, c.ufracval, src, div, mpll, sclk,
        mbaud / 1000, mbaud % 1000,
        if err < 0 { "-" } else { "+" }, err.abs() / 100, err.abs() % 100
    );
}

/// Unmasks the RX interrupt at the UART; the GIC side is in main.rs.
pub fn init() {
    Uart::new(UART).enable_rx_interrupt();
}
