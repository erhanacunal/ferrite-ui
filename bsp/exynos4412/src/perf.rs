//! Cycle counter for the runtime profiler (`perf` feature): the Cortex-A9
//! PMU cycle counter (PMCCNTR) of CPU0, where the runtime runs.
//!
//! The counter runs at the CPU clock. [`init`] measures that clock against
//! the MCT (24 MHz) once at boot, so the report needs no APLL decoding and
//! shows the real rate the firmware got.

use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::soc;

/// PMCR.E: counters on. PMCR.C: cycle counter reset. PMCR.D (bit 3, ÷64) stays clear.
const PMCR_E: u32 = 1 << 0;
const PMCR_C: u32 = 1 << 2;
/// PMCNTENSET bit 31: cycle counter enable.
const CNTEN_CYCLES: u32 = 1 << 31;

/// Calibration length (MCT counts): 10 ms.
const CAL_MCT: u64 = soc::MCT_CLOCK_HZ as u64 / 100;

/// Measured cycle counter rate (Hz).
static mut HZ: u32 = 0;
/// Cycles CPU0 waited for the display since the last report: for the
/// presenter to finish reading the shadow (`lcd::fence`), or for the switch
/// when presents run on CPU0.
static mut WAIT: u64 = 0;

/// Turns the cycle counter on and measures its rate. CPU0, with the MCT
/// running. Prints the CPU clock.
pub fn init(mct: &Mct) {
    enable_counter();
    let start = mct.count();
    while mct.count() == start {}
    let (m0, c0) = (mct.count(), cycles());
    while mct.count() - m0 < CAL_MCT {}
    let (m1, c1) = (mct.count(), cycles());
    let hz = c1.wrapping_sub(c0) as u64 * soc::MCT_CLOCK_HZ as u64 / (m1 - m0);
    unsafe { HZ = hz as u32 };
    exynos_4412_rs::println!("ferrite: CPU0 clock {} MHz (PMU vs MCT)", hz / 1_000_000);
}

/// Turns this core's PMU cycle counter on (every core has its own; they
/// all run at the CPU clock).
pub fn enable_counter() {
    // SAFETY: CP15 PMU registers of this core; nothing else uses the PMU.
    unsafe {
        let mut pmcr: u32;
        core::arch::asm!("mrc p15, 0, {}, c9, c12, 0", out(reg) pmcr, options(nomem, nostack));
        pmcr = (pmcr | PMCR_E | PMCR_C) & !(1 << 3);
        core::arch::asm!("mcr p15, 0, {}, c9, c12, 0", in(reg) pmcr, options(nomem, nostack));
        core::arch::asm!("mcr p15, 0, {}, c9, c12, 1", in(reg) CNTEN_CYCLES, options(nomem, nostack));
    }
}

/// PMCCNTR: CPU cycles, wraps every ~3 s at 1.4 GHz.
#[inline(always)]
pub fn cycles() -> u32 {
    let c: u32;
    // SAFETY: reads a CP15 register.
    unsafe { core::arch::asm!("mrc p15, 0, {}, c9, c13, 0", out(reg) c, options(nomem, nostack)) };
    c
}

pub fn hz() -> u32 {
    unsafe { HZ }
}

/// Adds a vsync wait (CPU0 only).
pub fn add_wait(c: u32) {
    unsafe { WAIT += c as u64 };
}

/// Vsync wait since the last call, then 0. Also hands the presenter's copy
/// statistics to the profiler (`PRESENT` row).
pub fn take_wait() -> u64 {
    let (cycles, n, bytes) = crate::lcd::take_present_stats();
    if n != 0 {
        ferrite_core::perf::add_sub_n(ferrite_core::perf::phase::PRESENT, cycles as u64, n);
    }
    // Bytes also come from layer copies, with no present (`lcd_layers`):
    // counted whenever there are some, the remainder kept.
    static mut KB_REM: u32 = 0;
    // SAFETY: CPU0 only.
    let rem = unsafe { &mut *(&raw mut KB_REM) };
    let total = *rem + bytes;
    if total >= 1024 {
        ferrite_core::perf::add_count(ferrite_core::perf::count::PRESENT_KB, total >> 10);
    }
    *rem = total & 1023;
    unsafe { core::mem::replace(&mut *(&raw mut WAIT), 0) }
}

/// Pixel counts (`count::G2D_KPX`, `count::CPU_BLEND_KPX`) are reported in
/// kilopixels: the remainders carry over (CPU0 only).
static mut PX_REM: [u32; 2] = [0; 2];

/// Adds `px` pixels to kilopixel count `id` (`G2D_KPX` or `CPU_BLEND_KPX`).
pub fn add_px(id: u8, px: usize) {
    let slot = if id == ferrite_core::perf::count::G2D_KPX { 0 } else { 1 };
    let rem = unsafe { &mut (*(&raw mut PX_REM))[slot] };
    let total = *rem as usize + px;
    *rem = (total & 1023) as u32;
    if total >= 1024 {
        ferrite_core::perf::add_count(id, (total >> 10) as u32);
    }
}
