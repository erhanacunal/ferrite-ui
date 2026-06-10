//! SysTick backend for TDO Y13 — F1C100s hardware timer.
//!
//! Uses TIMER1 as a periodic interrupt source (10 ms ticks) for the
//! preemptive scheduler (`sched_tick`). Also provides `millis()` via
//! a software tick counter incremented in the same ISR.
//!
//! Delay uses the HAL's AVS hardware counter (`delay_us`/`delay_ms`).

use ferrite_core::systick::SystickBackend;
use f1c100s::timer::{self, Timer, TimerChannel, TimerConfig, TimerMode, Prescaler};
use f1c100s::{interrupt, thread};

static mut TICK_MS: u64 = 0;

const SCHED_TICK_MS: u32 = 10;

fn timer1_isr(_vector: u32) {
    unsafe { TICK_MS = (&raw const TICK_MS).read_volatile().wrapping_add(SCHED_TICK_MS as u64); }
    thread::sched_tick();
}

pub struct F1cSystick;

impl SystickBackend for F1cSystick {
    fn init() {
        unsafe {
            timer::avs_init();

            // 24 MHz / prescaler_128 = 187.5 kHz → 1875 ticks per 10ms
            let timer1 = Timer::new(TimerChannel::Ch1);
            let interval = timer::TIMER_SRC_HZ / 128 * SCHED_TICK_MS / 1000;
            let config = TimerConfig {
                prescaler: Prescaler::Div128,
                mode: TimerMode::Periodic,
                interval,
            };
            timer1.configure(&config);

            interrupt::install(interrupt::TIMER1_INTERRUPT, timer1_isr);
            interrupt::unmask(interrupt::TIMER1_INTERRUPT);

            timer1.start(interval, TimerMode::Periodic);
        }
    }

    fn delay_ms(ms: u32) {
        timer::delay_ms(ms);
    }

    fn millis() -> u32 {
        unsafe { TICK_MS as u32 }
    }
}

pub fn init() {
    F1cSystick::init();
}

pub fn handle() -> ferrite_core::systick::SystickImpl<F1cSystick> {
    ferrite_core::systick::SystickImpl::<F1cSystick>::handle()
}
