/// GD32F103 interrupt vector table (minimal)
///
/// cortex-m-rt `device` feature aktif — `__INTERRUPTS` burada tanımlanır.
/// Kullanılmayan slot'lar reserved (0) — tetiklenirse HardFault (debug'a yardımcı).
/// Kullanılan IRQ'lar device.x'te PROVIDE ile DefaultHandler'a bağlı,
/// #[no_mangle] handler tanımlanınca linker güçlü sembolü tercih eder.

#[derive(Copy, Clone)]
union Vector {
    handler: unsafe extern "C" fn(),
    _reserved: u32,
}

unsafe extern "C" {
    fn USART0();
}

const V0: Vector = Vector { _reserved: 0 };

/// GD32F103 — 68 IRQ slotu (medium/high density)
/// IRQ 37 = USART0
#[unsafe(link_section = ".vector_table.interrupts")]
#[unsafe(no_mangle)]
pub static __INTERRUPTS: [Vector; 68] = {
    let mut v = [V0; 68];
    v[37] = Vector { handler: USART0 };
    v
};
