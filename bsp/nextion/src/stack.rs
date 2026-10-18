//! Stack high-water mark by painting.
//!
//! At boot the unused stack — from `_heap_end` (bottom of the stack reserve,
//! see memory.x) up to just below the live stack pointer — is filled with a
//! known word. The stack grows down, so the lowest word that no longer holds
//! the pattern marks the deepest point the stack has ever reached.
//!
//! An interrupt frame landing in the painted area during `paint` only makes
//! the reported peak larger (conservative). If the stack ever overflowed into
//! the heap, `peak` reports the whole reserve.
use core::ptr::{read_volatile, write_volatile};

const PAINT: u32 = 0xA5A5_A5A5;
/// Bytes left unpainted directly below the live SP when painting.
const GUARD: u32 = 64;

unsafe extern "C" {
    static _stack_start: u8;
}

/// Top of the stack (initial SP, end of RAM).
pub fn top() -> u32 {
    (&raw const _stack_start) as u32
}

/// Bottom of the stack reserve (= end of the heap).
pub fn bottom() -> u32 {
    (&raw const crate::_heap_end) as u32
}

pub fn current_sp() -> u32 {
    let sp: u32;
    unsafe { core::arch::asm!("mov {}, sp", out(reg) sp) };
    sp
}

/// Fill the unused stack with the paint pattern. Call once, early in `main`.
#[inline(never)]
pub fn paint() {
    let end = (current_sp() - GUARD) & !3;
    let mut p = bottom();
    while p < end {
        unsafe { write_volatile(p as *mut u32, PAINT) };
        p += 4;
    }
}

/// Deepest stack usage since boot, in bytes.
pub fn peak() -> u32 {
    let top = top();
    let mut p = bottom();
    while p < top && unsafe { read_volatile(p as *const u32) } == PAINT {
        p += 4;
    }
    top - p
}
