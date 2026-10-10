//! SysTick backend for N32903 — Timer 0 at 1 kHz, interrupt-driven.
//!
//! The HAL's `timer_isr` counts the ticks; `millis` reads that counter and
//! `delay_ms` waits on it.

use ferrite_core::systick::SystickBackend;
use n32903_rs::aic::{self, IntLevel};
use n32903_rs::registers::IRQ_TMR0;
use n32903_rs::timer::{self, TimerChannel, TimerOpMode, TimerPrescaler};

unsafe extern "C" fn timer0_isr() {
    unsafe { timer::timer_isr() };
}

pub struct N32903Systick;

impl SystickBackend for N32903Systick {
    fn init() {
        unsafe {
            aic::aic_install_isr(IntLevel::Irq1, IRQ_TMR0, timer0_isr);
            timer::timer_start(
                TimerChannel::Timer0,
                1000,
                TimerPrescaler::Div1,
                TimerOpMode::Periodic,
            );
            aic::aic_enable_interrupt(IRQ_TMR0);
        }
    }

    fn delay_ms(ms: u32) {
        let start = millis();
        while millis().wrapping_sub(start) < ms {
            core::hint::spin_loop();
        }
    }

    fn millis() -> u32 {
        millis()
    }
}

/// Milliseconds since `init`.
pub fn millis() -> u32 {
    unsafe { timer::timer_get_ticks(TimerChannel::Timer0) }
}

/// Start the 1 ms tick. IRQs must be enabled at the CPU afterwards.
pub fn init() {
    N32903Systick::init();
}

/// True once the tick counter advances (checks the timer IRQ path works).
pub fn is_ticking() -> bool {
    let start = millis();
    for _ in 0..2_000_000u32 {
        if millis() != start {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

pub fn delay_ms(ms: u32) {
    N32903Systick::delay_ms(ms);
}

pub fn handle() -> ferrite_core::systick::SystickImpl<N32903Systick> {
    ferrite_core::systick::SystickImpl::<N32903Systick>::handle()
}
