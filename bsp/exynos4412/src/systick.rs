//! Time base: the MCT global free-running counter (24 MHz, 64-bit).
//! No interrupt needed: `millis` reads the counter.

use core::sync::atomic::{AtomicU64, Ordering};

use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::soc;
use ferrite_core::systick::SystickBackend;

const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

pub struct MctSystick;

impl SystickBackend for MctSystick {
    fn init() {
        // Started once in main (before the LCD waits on it); starting again is harmless.
        Mct::new().start();
        resync();
    }

    fn delay_ms(ms: u32) {
        delay_ms(ms);
    }

    fn millis() -> u32 {
        millis()
    }
}

/// The last full counter value seen (`now`): the base the low word extends.
static LAST: AtomicU64 = AtomicU64::new(0);

/// `ms = count * MS_MUL >> MS_SHIFT` (no 64-bit division: the A9 has no
/// divide instruction and `__aeabi_uldivmod` is slow). Relative error
/// ~1e-8, and the result is monotonic in `count`.
const MS_SHIFT: u32 = 40;
const MS_MUL: u128 = ((1u128 << MS_SHIFT) + COUNTS_PER_MS as u128 / 2) / COUNTS_PER_MS as u128;

/// The 64-bit counter from one register read (the low word), extended
/// against `LAST`. Exact as long as calls are less than 2^32 counts
/// (~179 s) apart — the main loop calls it every pass; after a longer gap
/// (standby) call [`resync`]. Lock-free and safe on any core: a racing
/// store may leave an older `LAST`, which is still a past value within range.
pub fn now() -> u64 {
    // Profiler: time the register read on CPU0 (its counters are CPU0-only).
    #[cfg(feature = "perf")]
    let t0 = (exynos_4412_rs::cpu::id() == 0).then(crate::perf::cycles);
    let last = LAST.load(Ordering::Relaxed);
    let lo = Mct::new().count_low();
    #[cfg(feature = "perf")]
    if let Some(t0) = t0 {
        ferrite_core::perf::add_sub(ferrite_core::perf::phase::TIMER, crate::perf::cycles().wrapping_sub(t0));
    }
    let now = last + u64::from(lo.wrapping_sub(last as u32));
    LAST.store(now, Ordering::Relaxed);
    now
}

/// Re-reads the full 64-bit counter (three register reads) as the base for
/// [`now`]. For after a gap of more than ~179 s without calls (standby).
pub fn resync() {
    LAST.store(Mct::new().count(), Ordering::Relaxed);
}

pub fn millis() -> u32 {
    ((now() as u128 * MS_MUL) >> MS_SHIFT) as u32
}

pub fn delay_ms(ms: u32) {
    let mct = Mct::new();
    let end = mct.count() + u64::from(ms) * COUNTS_PER_MS;
    while mct.count() < end {
        core::hint::spin_loop();
    }
}
