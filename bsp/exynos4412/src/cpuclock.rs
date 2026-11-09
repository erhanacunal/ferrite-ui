//! The CPU clock at run time (HAL `cpufreq`): HTTP `POST /api/cpu?mhz=N` asks for one of the
//! HAL's operating points (`cmu::ARM_POINTS`, 200..1400 MHz in steps of 100), CPU0 applies it
//! in `frame_end` ([`service`]): `vdd_arm` (PMIC BUCK2 on I2C0, which CPU0 owns) is raised
//! before a faster clock and lowered after a slower one. All four cores share the clock.
//!
//! U-Boot leaves 1 GHz; the firmware keeps it unless asked. The voltages are mainline Linux's
//! (not per chip), and nothing watches the temperature: try higher rates with care.

use core::sync::atomic::{AtomicU32, Ordering};

use exynos_4412_rs::cpufreq;
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::timer::Mct;

/// The rate asked for (MHz), 0 = nothing pending.
static REQUEST: AtomicU32 = AtomicU32::new(0);

/// Asks for `mhz` (applied by CPU0 at the end of the next pass). False if the HAL has no
/// operating point at that rate.
pub fn request(mhz: u32) -> bool {
    if exynos_4412_rs::drivers::cmu::arm_point(mhz).is_none() {
        return false;
    }
    REQUEST.store(mhz, Ordering::Release);
    true
}

/// The CPU clock now, in MHz.
pub fn mhz() -> u32 {
    cpufreq::arm_hz() / 1_000_000
}

/// CPU0, between passes: applies a pending request.
pub fn service() {
    let mhz = REQUEST.swap(0, Ordering::Acquire);
    if mhz == 0 {
        return;
    }
    let pmic = Max77686::new(I2c::new(Bus::I2c0));
    match cpufreq::set_arm_mhz(pmic, Mct::new(), mhz) {
        Ok(c) => {
            exynos_4412_rs::println!(
                "ferrite: CPU clock {} -> {} MHz, vdd_arm {} -> {} mV",
                c.from_hz / 1_000_000,
                c.to_hz / 1_000_000,
                c.from_microvolts / 1000,
                c.to_microvolts / 1000
            );
            // The profiler's and `sysCpuMhz`'s cycle rate.
            #[cfg(feature = "perf")]
            crate::perf::set_hz(c.to_hz);
        }
        Err(e) => exynos_4412_rs::println!("ferrite: CPU clock {} MHz refused: {:?} (now {} MHz)", mhz, e, self::mhz()),
    }
}
