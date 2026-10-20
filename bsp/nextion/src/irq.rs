/// GD32F103 interrupt vector table
///
/// cortex-m-rt `device` feature — `__INTERRUPTS` defined here.
/// Unused slots are reserved (0) — triggers HardFault if fired.
/// Used IRQs are provided in device.x with DefaultHandler fallback.
/// A #[no_mangle] handler overrides the weak PROVIDE symbol.

#[derive(Copy, Clone)]
union Vector {
    handler: unsafe extern "C" fn(),
    _reserved: u32,
}

unsafe extern "C" {
    fn USART0();
    fn EXTI10_15();
}

const V0: Vector = Vector { _reserved: 0 };

/// GD32F103 — 68 IRQ slots (medium/high density)
/// IRQ 37 = USART0, IRQ 40 = EXTI10_15 (touch PENIRQ on PC14)
#[unsafe(link_section = ".vector_table.interrupts")]
#[unsafe(no_mangle)]
pub static __INTERRUPTS: [Vector; 68] = {
    let mut v = [V0; 68];
    v[37] = Vector { handler: USART0 };
    v[40] = Vector { handler: EXTI10_15 };
    v
};
