/// LCD arka ışık PWM kontrolü
///
/// PA8 = TIMER0_CH0 (AF push-pull)
/// TIMER0: Advanced timer — CCHP.POEN gerekli
///
/// PWM freq: 10kHz (PSC=107 → 1MHz tick, CAR=99 → 10kHz)
/// Duty: 0..100 (CH0CV = 0..99)

const TIMER0: u32 = 0x4001_2C00;

const TIMER_CTL0: u32 = TIMER0 + 0x00;
const TIMER_CHCTL0: u32 = TIMER0 + 0x18;
const TIMER_CHCTL2: u32 = TIMER0 + 0x20;
const TIMER_PSC: u32 = TIMER0 + 0x28;
const TIMER_CAR: u32 = TIMER0 + 0x2C;
const TIMER_CH0CV: u32 = TIMER0 + 0x34;
const TIMER_CCHP: u32 = TIMER0 + 0x44;

const RCU_APB2EN: u32 = 0x4002_1018;
const RCU_APB2RST: u32 = 0x4002_100C;

pub struct Backlight;

impl Backlight {
    /// TIMER0 PWM başlat, %100 parlaklıkla aç
    pub fn init() -> Self {
        unsafe {
            // TIMER0 clock enable (APB2 bit 11)
            let val = core::ptr::read_volatile(RCU_APB2EN as *const u32);
            core::ptr::write_volatile(RCU_APB2EN as *mut u32, val | (1 << 11));

            // TIMER0 reset
            let val = core::ptr::read_volatile(RCU_APB2RST as *const u32);
            core::ptr::write_volatile(RCU_APB2RST as *mut u32, val | (1 << 11));
            core::ptr::write_volatile(RCU_APB2RST as *mut u32, val & !(1 << 11));

            // CH0: PWM mode 0 + shadow enable (0x68 = 0110_1000)
            core::ptr::write_volatile(TIMER_CHCTL0 as *mut u32, 0x0068);

            // CH0 output enable
            core::ptr::write_volatile(TIMER_CHCTL2 as *mut u32, 0x0001);

            // PSC = 107 → 108MHz / 108 = 1MHz tick
            core::ptr::write_volatile(TIMER_PSC as *mut u32, 107);

            // CAR = 99 → 1MHz / 100 = 10kHz PWM
            core::ptr::write_volatile(TIMER_CAR as *mut u32, 99);

            // CH0CV = 99 → %100 duty (tam parlaklık)
            core::ptr::write_volatile(TIMER_CH0CV as *mut u32, 99);

            // CCHP: POEN = 1 (bit 15) — advanced timer ana çıkış aktif
            core::ptr::write_volatile(TIMER_CCHP as *mut u32, 1 << 15);

            // Counter enable
            core::ptr::write_volatile(TIMER_CTL0 as *mut u32, 1);
        }

        Backlight
    }

    /// Parlaklık ayarla: 0 = kapalı, 100 = tam parlaklık
    pub fn set_brightness(&self, percent: u8) {
        let duty = if percent > 100 { 99 } else if percent == 0 { 0 } else { (percent as u32) - 1 };
        unsafe {
            core::ptr::write_volatile(TIMER_CH0CV as *mut u32, duty);
        }
    }

    /// Mevcut parlaklık değerini oku (0..100)
    pub fn brightness(&self) -> u8 {
        let duty = unsafe { core::ptr::read_volatile(TIMER_CH0CV as *const u32) };
        (duty + 1) as u8
    }
}
