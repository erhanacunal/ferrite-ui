//! Battery syscalls (`lib/battery.fl`): the MAX17040 fuel gauge on `i2c_3` (voltage, charge)
//! and the MAX8903 charger's pins (HAL `drivers::max17040`, `drivers::max8903`).
//!
//! The bus and the pins are set up on the first call. A gauge reading (two I2C reads at
//! 100 kHz, ~1 ms) is kept for [`KEEP_MS`]: the gauge itself changes slowly, and a program
//! polling every pass would otherwise spend its frames on the bus. CPU0 only (syscalls).

use core::sync::atomic::{AtomicI32, Ordering};

use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::max17040::{self, Max17040};
use exynos_4412_rs::drivers::max8903::{Max8903, State};

use crate::systick;

/// `() → millivolts`, -1 if the gauge does not answer. Same id as on epaper.
pub const SYS_BATTERY_MV: u8 = 0x05;
/// `() → 0..100 %`, the gauge's estimate; -1 if it does not answer.
pub const SYS_BATTERY_PERCENT: u8 = 0x08;
/// `() → CHARGER_*`: 0 on battery, 1 charging, 2 powered but not charging (full), 3 fault.
pub const SYS_CHARGER: u8 = 0x09;

/// How long a gauge reading is kept.
const KEEP_MS: u32 = 2000;

/// The last reading, for `/api/status` (CPU3 reads them): mV, % (-1 = none yet / failed).
pub static LAST_MV: AtomicI32 = AtomicI32::new(-1);
pub static LAST_PERCENT: AtomicI32 = AtomicI32::new(-1);
/// The last charger state (`SYS_CHARGER`'s value), -1 = not read yet.
pub static LAST_CHARGER: AtomicI32 = AtomicI32::new(-1);

struct Gauge {
    /// None until the first call; Some(None) if the bus could not be set up.
    gauge: Option<Option<Max17040>>,
    charger: Option<Max8903>,
    read_at: Option<u32>,
}

static mut GAUGE: Gauge = Gauge { gauge: None, charger: None, read_at: None };

fn state() -> &'static mut Gauge {
    // SAFETY: syscalls run on CPU0 only, never reentrant.
    unsafe { &mut *(&raw mut GAUGE) }
}

/// Reads the gauge unless the last reading is recent; updates [`LAST_MV`] / [`LAST_PERCENT`].
fn refresh() {
    let st = state();
    let now = systick::millis();
    if st.read_at.is_some_and(|t| now.wrapping_sub(t) < KEEP_MS) {
        return;
    }
    st.read_at = Some(now);
    let gauge = *st.gauge.get_or_insert_with(|| {
        let i2c = I2c::new(Bus::I2c3);
        match i2c.init(max17040::BUS_HZ, max17040::BUS_SDA_DELAY_NS) {
            Ok(_) => Some(Max17040::new(i2c)),
            Err(e) => {
                exynos_4412_rs::println!("ferrite: fuel gauge bus ERROR {:?}", e);
                None
            }
        }
    });
    let (mv, percent) = match gauge.map(|g| g.reading()) {
        Some(Ok(r)) => (i32::from(r.millivolts), i32::from(r.percent())),
        _ => (-1, -1),
    };
    LAST_MV.store(mv, Ordering::Relaxed);
    LAST_PERCENT.store(percent, Ordering::Relaxed);
}

/// The charger's state now (its pins are read at once).
fn charger() -> i32 {
    let c = *state().charger.get_or_insert_with(Max8903::new);
    let s = c.status();
    let v = if s.fault {
        3
    } else {
        match s.state() {
            State::Discharging => 0,
            State::Charging => 1,
            State::NotCharging => 2,
        }
    };
    LAST_CHARGER.store(v, Ordering::Relaxed);
    v
}

/// The battery syscalls; None for other ids.
pub fn syscall(id: u8) -> Option<i32> {
    match id {
        SYS_BATTERY_MV => {
            refresh();
            Some(LAST_MV.load(Ordering::Relaxed))
        }
        SYS_BATTERY_PERCENT => {
            refresh();
            Some(LAST_PERCENT.load(Ordering::Relaxed))
        }
        SYS_CHARGER => Some(charger()),
        _ => None,
    }
}
