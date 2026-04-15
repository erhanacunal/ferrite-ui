/// SysTick timer — 1ms tick counter for non-blocking delays
///
/// Cortex-M3 SysTick is a core peripheral (not a GD32 peripheral IRQ).
/// Configured for 1ms ticks at 108MHz SYSCLK.

static mut TICK_MS: u32 = 0;

const SYST_CSR: u32 = 0xE000_E010;
const SYST_RVR: u32 = 0xE000_E014;
const SYST_CVR: u32 = 0xE000_E018;

const CSR_ENABLE: u32 = 1 << 0;
const CSR_TICKINT: u32 = 1 << 1;
const CSR_CLKSOURCE: u32 = 1 << 2;

pub fn init() {
    unsafe {
        core::ptr::write_volatile(SYST_CSR as *mut u32, 0);
        core::ptr::write_volatile(SYST_RVR as *mut u32, 108_000 - 1);
        core::ptr::write_volatile(SYST_CVR as *mut u32, 0);
        core::ptr::write_volatile(
            SYST_CSR as *mut u32,
            CSR_ENABLE | CSR_TICKINT | CSR_CLKSOURCE,
        );
    }
}

#[inline]
pub fn millis() -> u32 {
    unsafe { core::ptr::read_volatile(&raw const TICK_MS) }
}

#[inline]
pub fn delay_ms(ms: u32) {
    let start = millis();
    while millis().wrapping_sub(start) < ms {}
}

#[unsafe(no_mangle)]
extern "C" fn SysTick() {
    unsafe {
        TICK_MS = TICK_MS.wrapping_add(1);
    }
}
