//! The CPU clock at run time (HAL `cpufreq`), with thermal throttling and idle sleep.
//!
//! **Rate.** HTTP `POST /api/cpu?mhz=N` asks for one of the HAL's operating points
//! (`cmu::ARM_POINTS`, 200..1400 MHz in steps of 100): the *wanted* rate. U-Boot leaves 1 GHz,
//! kept unless asked. All four cores share the clock.
//!
//! **Throttling.** The TMU (HAL `drivers::tmu`, the chip's one sensor, CPU and GPU together)
//! is read every [`READ_MS`]; a *cap* follows it, a step-wise rule like Linux's thermal
//! governor: at [`HOT_C`] or more it drops 100 MHz a second (down to [`FLOOR_MHZ`]), at
//! [`CRITICAL_C`] or more straight to [`CRITICAL_MHZ`], at [`COOL_C`] or less it climbs back
//! 100 MHz every [`RAISE_MS`]. The CPU runs at min(wanted, cap). The voltages are mainline
//! Linux's (not per chip); the TMU's own hardware trip is left as U-Boot left it (off).
//!
//! CPU0 applies everything in `frame_end` ([`service`]): `vdd_arm` (PMIC BUCK2 on I2C0, which
//! CPU0 owns) is raised before a faster clock and lowered after a slower one.
//!
//! **Idle sleep.** When the programs do not need the CPU (`Runtime::busy`), `ui_task` sleeps
//! between passes instead of spinning ([`IDLE_SLEEP`], HTTP `POST /api/idle?sleep=0|1`): CPU0
//! waits in the executor. [`IDLE_SLEEP_US`] while something needs passes (`Runtime::idle_ms`
//! = 0: an animation, a touch down, a render or a present in flight); otherwise until the next
//! `delay` ends, at most [`IDLE_MAX_MS`], or a touch (the controller's CHG interrupt,
//! `touch::sleep_until_touch`), like SurfaceFlinger's vsync on demand.

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use exynos_4412_rs::cpufreq;
use exynos_4412_rs::drivers::cmu;
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::drivers::tmu::Tmu;

use crate::systick;

/// Temperature reads, ms apart.
const READ_MS: u32 = 250;
/// At or above: the cap drops [`STEP_MHZ`] a [`LOWER_MS`].
pub const HOT_C: i32 = 85;
/// At or above: the cap drops to [`CRITICAL_MHZ`] at once.
pub const CRITICAL_C: i32 = 95;
/// At or below: the cap climbs back [`STEP_MHZ`] every [`RAISE_MS`].
pub const COOL_C: i32 = 75;
const STEP_MHZ: u32 = 100;
const LOWER_MS: u32 = 1000;
const RAISE_MS: u32 = 3000;
/// The lowest cap the throttling sets.
const FLOOR_MHZ: u32 = 600;
const CRITICAL_MHZ: u32 = 800;
/// The highest operating point.
const MAX_MHZ: u32 = 1400;

/// Idle sleep on (default) or off.
pub static IDLE_SLEEP: AtomicBool = AtomicBool::new(true);
/// How long `ui_task` waits between idle passes while something needs passes
/// (`Runtime::idle_ms` = 0: an animation, a touch down, a render or present in
/// flight).
pub const IDLE_SLEEP_US: u64 = 1000;
/// The longest idle sleep when nothing time-based is pending (until a `delay`
/// ends, else this): CPU0 also wakes for a touch (the controller's CHG
/// interrupt), and polls its own work this often (power keys, UART and
/// network messages from the other cores, the thermal reading).
pub const IDLE_MAX_MS: u32 = 20;

/// A rate asked for over HTTP (MHz), 0 = nothing pending.
static REQUEST: AtomicU32 = AtomicU32::new(0);
/// The wanted rate (MHz).
static WANT: AtomicU32 = AtomicU32::new(0);
/// The thermal cap (MHz).
static CAP: AtomicU32 = AtomicU32::new(MAX_MHZ);
/// The rate set last (MHz).
static APPLIED: AtomicU32 = AtomicU32::new(0);
/// The last temperature (°C), `i32::MIN` = no sensor.
static TEMP_C: AtomicI32 = AtomicI32::new(i32::MIN);

/// CPU0-only state of [`service`].
struct State {
    tmu: Option<Tmu>,
    read_at: u32,
    step_at: u32,
}

static mut STATE: State = State { tmu: None, read_at: 0, step_at: 0 };

/// At boot, CPU0, MCT running: starts the TMU and takes the rate U-Boot left as the wanted one.
pub fn init(mct: Mct) {
    let now = mhz();
    WANT.store(now, Ordering::Relaxed);
    APPLIED.store(now, Ordering::Relaxed);
    let tmu = match Tmu::new(mct) {
        Ok(t) => {
            exynos_4412_rs::println!("ferrite: CPU {} MHz, {} C", now, t.celsius());
            Some(t)
        }
        Err(e) => {
            exynos_4412_rs::println!("ferrite: no temperature ({:?}): no thermal throttling", e);
            None
        }
    };
    // SAFETY: CPU0, before the runtime starts.
    unsafe { (*(&raw mut STATE)).tmu = tmu };
}

/// Asks for `mhz` (applied by CPU0 at the end of the next pass, under the thermal cap). False
/// if the HAL has no operating point at that rate.
pub fn request(mhz: u32) -> bool {
    if cmu::arm_point(mhz).is_none() {
        return false;
    }
    REQUEST.store(mhz, Ordering::Release);
    true
}

/// The CPU clock now, in MHz (rounded to the operating points' 100s).
pub fn mhz() -> u32 {
    (cpufreq::arm_hz() + 50_000_000) / 100_000_000 * 100
}

/// (wanted, cap) in MHz, and the last temperature (None without a sensor).
pub fn status() -> (u32, u32, Option<i32>) {
    let t = TEMP_C.load(Ordering::Relaxed);
    (WANT.load(Ordering::Relaxed), CAP.load(Ordering::Relaxed), (t != i32::MIN).then_some(t))
}

/// CPU0, between passes: takes a request, reads the temperature, moves the cap, and sets the
/// clock to min(wanted, cap) if that changed.
pub fn service() {
    // SAFETY: CPU0 only (frame_end), never reentrant.
    let st = unsafe { &mut *(&raw mut STATE) };
    let now = systick::millis();
    let asked = REQUEST.swap(0, Ordering::Acquire);
    if asked != 0 {
        WANT.store(asked, Ordering::Relaxed);
    }
    if let Some(tmu) = st.tmu.as_ref()
        && now.wrapping_sub(st.read_at) >= READ_MS
    {
        st.read_at = now;
        let t = tmu.celsius() as i32;
        TEMP_C.store(t, Ordering::Relaxed);
        let cap = CAP.load(Ordering::Relaxed);
        let since = now.wrapping_sub(st.step_at);
        let new = if t >= CRITICAL_C {
            cap.min(CRITICAL_MHZ)
        } else if t >= HOT_C && since >= LOWER_MS {
            cap.saturating_sub(STEP_MHZ).max(FLOOR_MHZ)
        } else if t <= COOL_C && since >= RAISE_MS {
            (cap + STEP_MHZ).min(MAX_MHZ)
        } else {
            cap
        };
        if new != cap {
            st.step_at = now;
            CAP.store(new, Ordering::Relaxed);
        }
    }
    let target = WANT.load(Ordering::Relaxed).min(CAP.load(Ordering::Relaxed));
    let applied = APPLIED.load(Ordering::Relaxed);
    if target == applied {
        return;
    }
    let pmic = Max77686::new(I2c::new(Bus::I2c0));
    match cpufreq::set_arm_mhz(pmic, Mct::new(), target) {
        Ok(c) => {
            APPLIED.store(target, Ordering::Relaxed);
            exynos_4412_rs::println!(
                "ferrite: CPU clock {} -> {} MHz (wanted {}, thermal cap {}, {} C), vdd_arm {} -> {} mV",
                c.from_hz / 1_000_000,
                c.to_hz / 1_000_000,
                WANT.load(Ordering::Relaxed),
                CAP.load(Ordering::Relaxed),
                TEMP_C.load(Ordering::Relaxed),
                c.from_microvolts / 1000,
                c.to_microvolts / 1000
            );
        }
        Err(e) => {
            // Not retried every pass: a request is dropped, a thermal step comes again after
            // its interval.
            exynos_4412_rs::println!("ferrite: CPU clock {} MHz refused: {:?} (now {} MHz)", target, e, mhz());
            let now_mhz = mhz();
            APPLIED.store(now_mhz, Ordering::Relaxed);
            if asked != 0 {
                WANT.store(now_mhz, Ordering::Relaxed);
            }
            CAP.store(CAP.load(Ordering::Relaxed).max(now_mhz), Ordering::Relaxed);
        }
    }
}
