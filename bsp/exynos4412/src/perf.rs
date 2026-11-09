//! Clock for the runtime profiler (`perf` feature): the MCT's global counter (24 MHz).
//!
//! It counts while the CPU sleeps (idle sleep: `ui_task` waits in `wfe`, where the PMU cycle
//! counter used before stopped and the profile's window shrank to the awake time), is the
//! same on every core, and does not change with the CPU clock (`cpuclock`). 42 ns a count.

use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::soc;

/// Counts CPU0 waited for the display since the last report: for the
/// presenter to finish reading the shadow (`lcd::fence`), or for the switch
/// when presents run on CPU0.
static mut WAIT: u64 = 0;

/// The MCT's low word: wraps every ~179 s.
#[inline(always)]
pub fn cycles() -> u32 {
    Mct::new().count_low()
}

/// The rate of [`cycles`].
pub fn hz() -> u32 {
    soc::MCT_CLOCK_HZ
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
