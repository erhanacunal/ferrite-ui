//! 32-bit integer division for ARM cores without `udiv`/`sdiv` (ARMv5TE:
//! the ARM926EJ-S of n32903 and tdo_y13). Enabled by the `arm_div` feature.
//!
//! Those BSPs build core/alloc/compiler_builtins themselves (`-Zbuild-std`,
//! custom target specs). That compiler_builtins has only its Rust division:
//! `__aeabi_uidivmod` goes through `__udivmodsi4` with the remainder in
//! memory, a bit-by-bit loop (ferrite-ui-xl measured -9 % VM instructions/s
//! on `x % 13` against compiler-rt's assembly). These are strong definitions
//! of the same symbols, so they replace compiler_builtins' weak ones at link
//! time. Copied from ferrite-ui-xl's `bsp/exynos4412/src/divide.rs` (checked
//! there under unicorn on 194 K pairs); every instruction used (`clz`,
//! conditional ARM data processing, `bx`) is ARMv5TE.
//!
//! The algorithm is compiler-rt's `arm/udivsi3.S` (Apache-2.0 WITH
//! LLVM-exception): the shift distance between divisor and dividend from
//! `clz`, then a jump into an unrolled run of compare/subtract steps, one
//! step a quotient bit, 3 ARM instructions (12 bytes) each. Rust checks for
//! a zero divisor (and `i32::MIN / -1`) before it calls these; a zero divisor
//! returns the dividend instead of jumping out of the table.
//!
//! Register use: r0, r1, r2 (signed only), r3, r12; no stack. ARM state only:
//! the callers are ARM code (these targets don't build Thumb).
//!
//! Not for the tdo_y13 SPL (`.text.spl`): these live in plain `.text`, which
//! the BROM doesn't load (see f1c100s_rs `cpu.rs`).

core::arch::global_asm!(
    r#"
    .macro UDIVMOD_CORE
    @ in: r0 = n, r1 = d. out: r0 = n / d, r1 = n % d. uses r3, r12.
    cmp     r0, r1
    bcc     2f                      @ n < d: q = 0, r = n
    cmp     r1, #0
    beq     3f
    clz     r12, r0
    clz     r3, r1
    sub     r3, r3, r12             @ highest quotient bit (n >= d)
    adr     r12, 1f
    sub     r12, r12, r3, lsl #2
    sub     r12, r12, r3, lsl #3    @ 1f - 12 * that bit: its step
    mov     r3, #0
    bx      r12
    .irp s, 31,30,29,28,27,26,25,24,23,22,21,20,19,18,17,16,15,14,13,12,11,10,9,8,7,6,5,4,3,2,1
    cmp     r0, r1, lsl #\s
    addhs   r3, r3, #(1 << \s)
    subhs   r0, r0, r1, lsl #\s
    .endr
1:
    cmp     r0, r1
    addhs   r3, r3, #1
    subhs   r0, r0, r1
    mov     r1, r0
    mov     r0, r3
    b       3f
2:
    mov     r1, r0
    mov     r0, #0
3:
    .endm

    .section .text.__aeabi_uidivmod, "ax", %progbits
    .arm
    .p2align 2
    .global __aeabi_uidivmod
    .global __aeabi_uidiv
    .global __udivsi3
    .type __aeabi_uidivmod, %function
    .type __aeabi_uidiv, %function
    .type __udivsi3, %function
__aeabi_uidivmod:
__aeabi_uidiv:
__udivsi3:
    UDIVMOD_CORE
    bx      lr
    .size __aeabi_uidivmod, . - __aeabi_uidivmod
    .size __aeabi_uidiv, . - __aeabi_uidiv
    .size __udivsi3, . - __udivsi3

    .section .text.__aeabi_idivmod, "ax", %progbits
    .arm
    .p2align 2
    .global __aeabi_idivmod
    .global __aeabi_idiv
    .global __divsi3
    .type __aeabi_idivmod, %function
    .type __aeabi_idiv, %function
    .type __divsi3, %function
__aeabi_idivmod:
__aeabi_idiv:
__divsi3:
    @ r2 bit 0: the quotient is negative; bit 31: the remainder is (n < 0).
    eor     r2, r0, r1
    lsr     r2, r2, #31
    cmp     r0, #0
    rsblt   r0, r0, #0
    orrlt   r2, r2, #0x80000000
    cmp     r1, #0
    rsblt   r1, r1, #0
    UDIVMOD_CORE
    tst     r2, #1
    rsbne   r0, r0, #0
    tst     r2, #0x80000000
    rsbne   r1, r1, #0
    bx      lr
    .size __aeabi_idivmod, . - __aeabi_idivmod
    .size __aeabi_idiv, . - __aeabi_idiv
    .size __divsi3, . - __divsi3
"#
);
