//! Millisecond timer for the ESP32-S3 using the SYSTIMER peripheral.
//!
//! SYSTIMER UNIT0 is a 52-bit counter that increments at 16 MHz from boot.
//!   millis = UNIT0_VALUE / 16_000
//!
//! Reading procedure (from the TRM):
//!   1. Write bit 30 of SYSTIMER_UNIT0_OP to latch the current count.
//!   2. Read SYSTIMER_UNIT0_VALUE_LO.
//!   3. Read SYSTIMER_UNIT0_VALUE_HI for the high 20 bits (ignored here,
//!      wraps after ~77 hours which is fine for a u32 millisecond counter).


use esp_hal::{peripherals, timer::systimer::SystemTimer, timer::systimer::Unit};

static mut SYSTIMER: Option<SystemTimer<'static>> = None;

static mut TICKS_PER_MS: u64 = 16_000;

/// Initialise the systick subsystem.
///
/// SYSTIMER runs from boot on ESP32-S3 — nothing to configure.
pub fn init(systimer: peripherals::SYSTIMER<'static>) {
    unsafe {
        SYSTIMER = Some(SystemTimer::new(systimer));
        let timer_hz = SystemTimer::ticks_per_second();
        TICKS_PER_MS = timer_hz / 1_000;
    }
}

/// Return a millisecond timestamp.
///
/// The 32-bit result wraps after ~49 days.  `delay_ms` and all callers
/// use wrapping arithmetic so this is safe.
#[inline]
pub fn millis() -> u64 {
    unsafe { SystemTimer::unit_value(Unit::Unit0) / TICKS_PER_MS }
}

/// Blocking millisecond delay using `millis()`.
pub fn delay_ms(ms: u64) {
    let start = millis();
    while millis().wrapping_sub(start) < ms {}
}
