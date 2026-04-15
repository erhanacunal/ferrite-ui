/// LCD backlight PWM control
///
/// PA8 = TIMER0_CH0 (AF push-pull)
/// TIMER0: Advanced timer — CCHP.POEN required for output
///
/// PWM freq: 50Hz (PSC=1079 → 100kHz tick, CAR=1999 → 50Hz)
/// Duty: 0..100% mapped to CH0CV 0..1999
///
/// Init sequence (from original firmware):
///   1. Configure and start timer
///   2. Wait for stabilization
///   3. THEN connect PA8 to timer (AF push-pull)
/// This prevents glitches — pin outputs nothing until timer is running.

use super::BacklightBackend;

const TIMER0: u32 = 0x4001_2C00;

const TIMER_CTL0: u32 = TIMER0 + 0x00;
const TIMER_SWEVG: u32 = TIMER0 + 0x14;
const TIMER_CHCTL0: u32 = TIMER0 + 0x18;
const TIMER_CHCTL2: u32 = TIMER0 + 0x20;
const TIMER_PSC: u32 = TIMER0 + 0x28;
const TIMER_CAR: u32 = TIMER0 + 0x2C;
const TIMER_CH0CV: u32 = TIMER0 + 0x34;
const TIMER_CCHP: u32 = TIMER0 + 0x44;

const RCU_APB2EN: u32 = 0x4002_1018;
const RCU_APB2RST: u32 = 0x4002_100C;

const GPIOA_CTL1: u32 = 0x4001_0804;

pub struct Pwm;

impl Pwm {
    pub fn init() -> Self {
        unsafe {
            let val = core::ptr::read_volatile(RCU_APB2EN as *const u32);
            core::ptr::write_volatile(RCU_APB2EN as *mut u32, val | (1 << 11));

            let val = core::ptr::read_volatile(RCU_APB2RST as *const u32);
            core::ptr::write_volatile(RCU_APB2RST as *mut u32, val | (1 << 11));
            core::ptr::write_volatile(RCU_APB2RST as *mut u32, val & !(1 << 11));

            core::ptr::write_volatile(TIMER_PSC as *mut u32, 1079);
            core::ptr::write_volatile(TIMER_CAR as *mut u32, 1999);
            core::ptr::write_volatile(TIMER_CH0CV as *mut u32, 1);

            let val = core::ptr::read_volatile(TIMER_CHCTL0 as *const u32);
            core::ptr::write_volatile(TIMER_CHCTL0 as *mut u32, (val & 0xFF00) | 0x68);

            let val = core::ptr::read_volatile(TIMER_CHCTL2 as *const u32);
            core::ptr::write_volatile(TIMER_CHCTL2 as *mut u32, (val & 0xFFF0) | 0x01);

            let val = core::ptr::read_volatile(TIMER_CCHP as *const u32);
            core::ptr::write_volatile(TIMER_CCHP as *mut u32, val | (1 << 15));

            core::ptr::write_volatile(TIMER_SWEVG as *mut u32, 1);

            let val = core::ptr::read_volatile(TIMER_CTL0 as *const u32);
            core::ptr::write_volatile(TIMER_CTL0 as *mut u32, val | 1);

            for _ in 0..0x1FF {
                cortex_m::asm::nop();
            }

            let val = core::ptr::read_volatile(GPIOA_CTL1 as *const u32);
            core::ptr::write_volatile(GPIOA_CTL1 as *mut u32, (val & !(0xF)) | 0xB);
        }

        Pwm
    }
}

impl BacklightBackend for Pwm {
    fn set_brightness(&self, percent: u8) {
        let p = if percent > 100 { 100 } else { percent };
        let duty = (p as u32) * 1999 / 100;
        unsafe {
            core::ptr::write_volatile(TIMER_CH0CV as *mut u32, duty);
        }
    }

    fn brightness(&self) -> u8 {
        let duty = unsafe { core::ptr::read_volatile(TIMER_CH0CV as *const u32) };
        ((duty * 100 + 999) / 1999) as u8
    }
}
