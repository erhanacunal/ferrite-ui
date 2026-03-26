/// SysTick timer — 1ms tick counter for non-blocking delays
///
/// Cortex-M3 SysTick is a core peripheral (not a GD32 peripheral IRQ).
/// Configured for 1ms ticks at 108MHz SYSCLK.
///
/// Usage:
///   systick::init();           // call once at startup
///   let now = systick::millis(); // read current tick

/// Global millisecond counter — incremented by SysTick exception handler
static mut TICK_MS: u32 = 0;

// SysTick registers (Cortex-M3 core)
const SYST_CSR: u32 = 0xE000_E010; // Control and Status
const SYST_RVR: u32 = 0xE000_E014; // Reload Value
const SYST_CVR: u32 = 0xE000_E018; // Current Value

// CSR bits
const CSR_ENABLE: u32 = 1 << 0;    // Counter enable
const CSR_TICKINT: u32 = 1 << 1;   // Exception request enable
const CSR_CLKSOURCE: u32 = 1 << 2; // 1 = processor clock (108MHz)

/// Initialize SysTick for 1ms interrupts.
/// Must be called after system_init() sets up 108MHz clock.
pub fn init() {
    unsafe {
        // Stop SysTick during configuration
        core::ptr::write_volatile(SYST_CSR as *mut u32, 0);

        // Reload value: 108MHz / 1000 = 108_000 ticks per ms
        core::ptr::write_volatile(SYST_RVR as *mut u32, 108_000 - 1);

        // Clear current value
        core::ptr::write_volatile(SYST_CVR as *mut u32, 0);

        // Enable: counter + interrupt + processor clock source
        core::ptr::write_volatile(
            SYST_CSR as *mut u32,
            CSR_ENABLE | CSR_TICKINT | CSR_CLKSOURCE,
        );
    }
}

/// Read current millisecond tick count.
/// Wraps around every ~49.7 days (u32::MAX ms).
#[inline]
pub fn millis() -> u32 {
    unsafe { core::ptr::read_volatile(&raw const TICK_MS) }
}

/// SysTick exception handler — called every 1ms.
/// cortex-m-rt routes the SysTick vector here via the `#[exception]` attribute.
#[unsafe(no_mangle)]
extern "C" fn SysTick() {
    unsafe {
        TICK_MS = TICK_MS.wrapping_add(1);
    }
}
