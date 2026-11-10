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
//!
//! While a network client holds the protocol (`bridge.rs`: the TCP bridge
//! port, or an HTTP API request), RX and TX go through the bridge's rings
//! instead.
//!
//! In standby ([`suspend`] / [`resume`]) CPU1 is stopped and UART2 shut:
//! interrupts masked, receiver and transmitter off, clock gate closed. Bytes
//! sent to the board meanwhile are lost; the ring keeps what arrived before.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::uart::{Uart, UartId};
use exynos_4412_rs::embassy::uart::UartRx;
use exynos_4412_rs::{cpu, embassy, println, smp, soc};
use ferrite_core::usart::UsartBackend;

use crate::hotplug::Group;

/// The core RX runs on.
const CORE: usize = 1;
/// How long [`suspend`] waits for CPU1 to lose power.
const STOP_MS: u64 = 100;
/// `CLK_GATE_IP_PERIL` (Exynos 4412 UM 7.9.1): bit n = UARTn's clock.
const GATE_IP_PERIL: usize = 0xC950;

/// CPU1's task ([`rx_task`]), ended for standby.
static GROUP: Group = Group::new();
/// RX runs on CPU1 (else on CPU0, which never stops: [`suspend`] leaves UART2 alone).
static ON_CPU1: AtomicBool = AtomicBool::new(false);
/// The RX handle for CPU1's task, set before CPU1 starts.
static mut RX: Option<UartRx> = None;

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

/// CPU0: starts RX on CPU1 with `rx` (from [`start`]). False if CPU1 did not
/// start: the caller runs [`rx_task`] on CPU0 then.
pub fn start_core(rx: UartRx) -> bool {
    // SAFETY: CPU1 is not running; it reads this only after its start.
    unsafe { *(&raw mut RX) = Some(rx) };
    GROUP.arm();
    match smp::start_cpu(CORE, cpu1) {
        Ok(()) => {
            ON_CPU1.store(true, Ordering::Release);
            true
        }
        Err(e) => {
            println!("ferrite: CPU{} ERROR {:?}, UART RX on CPU0", CORE, e);
            if let Some(id) = UART.irq() {
                Gic::new().set_target(id, 0);
            }
            false
        }
    }
}

/// CPU1: UART2 RX until [`suspend`], then the core stops.
fn cpu1(_core: usize) -> ! {
    Gic::new().init_this_cpu();
    cpu::enable_irq();
    // SAFETY: written by CPU0 before it started this core.
    let rx = unsafe { (*(&raw const RX)).expect("RX set before CPU1 starts") };
    embassy::run_until(
        |spawner| {
            GROUP.begin(1);
            spawner.spawn(rx_task_stoppable(rx).unwrap());
        },
        || GROUP.done(),
        // No wake-up from UART2 for the ended task: its interrupt off, from this side too.
        || {
            if let Some(id) = UART.irq() {
                Gic::new().disable(id);
            }
        },
    );
    smp::stop_this_cpu()
}

#[embassy_executor::task]
async fn rx_task_stoppable(rx: UartRx) {
    GROUP.run(rx_loop(rx)).await
}

fn set_gate(open: bool) {
    let address = (soc::CMU_BASE + GATE_IP_PERIL) as *mut u32;
    let bit = 1u32 << UART as u32;
    let masked = cpu::mask_irq();
    // SAFETY: the CMU's peripheral clock gates, mapped for good; only UART2's bit changes.
    unsafe {
        let v = core::ptr::read_volatile(address);
        core::ptr::write_volatile(address, if open { v | bit } else { v & !bit });
    }
    cpu::restore_irq(masked);
}

/// CPU0, entering standby: stops CPU1 and shuts UART2. True if it did (then
/// [`resume`] at the wake); false if RX runs on CPU0 or CPU1 did not stop.
pub fn suspend() -> bool {
    if !ON_CPU1.load(Ordering::Acquire) {
        return false;
    }
    GROUP.stop();
    if let Err(status) = smp::wait_offline(CORE, STOP_MS) {
        println!("ferrite: CPU{} did not stop (status {:#x}), UART left on", CORE, status);
        return false;
    }
    ON_CPU1.store(false, Ordering::Release);
    // What was printed goes out first (the console's ring too, when it is this UART).
    exynos_4412_rs::console::flush();
    Uart::new(UART).flush();
    if let Some(id) = UART.irq() {
        Gic::new().disable(id);
    }
    // SAFETY: UART2's registers, mapped for good; its interrupt is off and no
    // core reads it now.
    unsafe {
        core::ptr::write_volatile(reg(UINTM), 0xF);
        core::ptr::write_volatile(reg(UCON), 0);
    }
    set_gate(false);
    true
}

/// CPU0, at the wake after a [`suspend`] that returned true: UART2 back as
/// [`start`] sets it, and RX on CPU1 again.
pub fn resume() {
    set_gate(true);
    let rx = start(CORE);
    if !start_core(rx) {
        // RX on CPU0 needs a task there, which only boot spawns: none now.
        println!("ferrite: UART RX off until a restart");
    }
}

/// Moves received bytes into the ring for the runtime.
#[embassy_executor::task]
pub async fn rx_task(rx: UartRx) {
    rx_loop(rx).await
}

async fn rx_loop(rx: UartRx) {
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
        if crate::bridge::active() {
            crate::bridge::tx_push(byte, crate::systick::millis);
        } else {
            Uart::new(UART).write_byte(byte);
        }
    }

    fn flush(&self) {
        if crate::bridge::active() {
            crate::bridge::tx_drain(500, crate::systick::millis);
        } else {
            Uart::new(UART).flush();
        }
    }

    fn rx_read_byte(&self) -> Option<u8> {
        // A network session (bridge.rs) takes the protocol over; UART2's
        // bytes wait in their ring meanwhile.
        if crate::bridge::active() {
            return crate::bridge::rx_pop();
        }
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
        if crate::bridge::active() {
            return (crate::bridge::RX.len(), crate::bridge::RX.capacity());
        }
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
