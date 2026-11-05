//! USART backend — UART2, the board's console (115200 8N1, set up by U-Boot).
//!
//! RX runs on CPU1: the HAL's async `UartRx` takes UART2's interrupt there
//! (its 16-byte FIFO lasts ~1.4 ms at 115200) and [`rx_task`] moves every
//! byte into [`BUF`], which the runtime on CPU0 drains through
//! [`HwUart::rx_read_byte`]. Whatever CPU0 does (vsync waits asleep with IRQs
//! masked, long renders), nothing is lost. If CPU1 doesn't start, the task
//! runs on CPU0 instead (main.rs). TX is polled. The HAL's `println!` uses
//! the same UART: keep it to boot messages (the host tools skip boot-log
//! text).

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::uart::{Uart, UartId};
use exynos_4412_rs::embassy::uart::UartRx;
use ferrite_core::usart::UsartBackend;

pub const UART: UartId = UartId::Uart2;

const RING: usize = 64 * 1024;
const MASK: usize = RING - 1;

static mut BUF: [u8; RING] = [0; RING];
// Single producer (rx_task) / single consumer (the runtime), on different
// cores: the Release/Acquire pairs order the ring bytes against the indices.
static HEAD: AtomicUsize = AtomicUsize::new(0);
static TAIL: AtomicUsize = AtomicUsize::new(0);

// DIAG: bytes received, bytes dropped (a full ring, here or in the HAL's), and
// the UERSTAT errors seen (1 = seen since boot): overrun (bytes lost before
// the interrupt ran), parity, frame (line noise / baud mismatch), break.
pub static RX_TOTAL: AtomicU32 = AtomicU32::new(0);
pub static RX_DROPPED: AtomicU32 = AtomicU32::new(0);
pub static RX_ERR: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];

// UART2 registers (Exynos 4412 UM ch. 28; serial_s3c.h).
const UCON: usize = 0x04;
const UFCON: usize = 0x08;
const UMCON: usize = 0x0C;
const UINTM: usize = 0x38;
/// RX + TX in interrupt/polling mode, RX timeout on, level interrupts, RX
/// error interrupt off (errors are read from UERSTAT). U-Boot leaves 0x3C5.
const UCON_VALUE: u32 = 0b01 | (0b01 << 2) | (1 << 7) | (1 << 8) | (1 << 9);
/// FIFOs on, both reset, RX trigger 8 bytes (16-byte FIFO), TX trigger 0.
const UFCON_VALUE: u32 = 1 | (1 << 1) | (1 << 2) | (1 << 4);
/// No auto flow control (bit 4), RTS inactive: with AFC on, TX stops while
/// the board's CTS input is not asserted, and nothing leaves the UART.
const UMCON_VALUE: u32 = 0;

fn reg(off: usize) -> *mut u32 {
    (UART.base() + off) as *mut u32
}

/// Sets UART2's mode registers instead of relying on what the bootloader
/// left (uart_boot reaches ferrite on several paths: straight from CACHE, or
/// after WiFi/HTTP with its buffered console).
/// Baud registers (UBRDIV/UFRACVAL) and ULCON (8N1) are kept: see
/// [`report_clock`].
fn take_over() {
    // SAFETY: UART2's MMIO block, mapped for good; only this core touches
    // these registers here, before the RX interrupt is on.
    unsafe {
        // Let polled output already queued drain before the FIFO reset.
        Uart::new(UART).flush();
        core::ptr::write_volatile(reg(UINTM), 0xF);
        core::ptr::write_volatile(reg(UMCON), UMCON_VALUE);
        core::ptr::write_volatile(reg(UFCON), UFCON_VALUE);
        core::ptr::write_volatile(reg(UCON), UCON_VALUE);
    }
}

/// Turns on interrupt-driven RX with the interrupt routed to `core`. Call on
/// CPU0 after `Gic::init`, before `core` runs [`rx_task`].
pub fn start(core: usize) -> UartRx {
    take_over();
    let rx = UartRx::new(UART).expect("UART2 has an interrupt");
    if let Some(irq) = UART.irq() {
        Gic::new().set_target(irq, core);
    }
    rx
}

/// Moves received bytes into the ring for the runtime.
#[embassy_executor::task]
pub async fn rx_task(rx: UartRx) {
    let mut chunk = [0u8; 256];
    let mut own_drops = 0u32;
    loop {
        let n = rx.read(&mut chunk).await;
        let mut head = HEAD.load(Ordering::Relaxed);
        let tail = TAIL.load(Ordering::Acquire);
        for &b in &chunk[..n] {
            let next = (head + 1) & MASK;
            if next == tail {
                own_drops += 1;
                continue;
            }
            // SAFETY: slot `head` is outside [tail, head), which the consumer
            // reads; it sees it only after the Release store below.
            unsafe { (*(&raw mut BUF))[head] = b };
            head = next;
        }
        HEAD.store(head, Ordering::Release);
        let st = rx.stats();
        RX_TOTAL.store(st.received, Ordering::Relaxed);
        RX_DROPPED.store(st.dropped + own_drops, Ordering::Relaxed);
        for (i, e) in RX_ERR.iter().enumerate() {
            e.store((st.errors >> i) & 1, Ordering::Relaxed);
        }
    }
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
