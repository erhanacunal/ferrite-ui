//! "Flash" backend — a NOR-semantics RAM region, kept in a file on eMMC.
//!
//! ferrite-core reads resources through `FlashBackend` + the TOC `Fs`. Here
//! that is a 64 MB heap buffer, loaded at boot from `FERRITE/FLASH.BIN` on
//! the CACHE partition (`storage.rs`) and written back after changes
//! ([`save_if_dirty`]), as n32903 does with its NAND shadow.
//!
//! Next step: resources come from package files on CACHE through a
//! `BlobReader` that `Fs` is generic over (several apps), and this region
//! goes away.

use alloc::vec;
use alloc::vec::Vec;
use ferrite_core::flash::FlashBackend;

use crate::{storage, systick};

const SIZE: usize = 64 << 20;
const SECTOR: usize = 4096;

static mut SHADOW: Vec<u8> = Vec::new();

/// Bytes of the region worth saving: the loaded file, grown by every write.
static mut USED: usize = 0;
/// Changed since the last save, and when it last changed.
static mut DIRTY: bool = false;
static mut LAST_WRITE_MS: u32 = 0;

/// DIAG: highest address written + 1 since boot (0 = nothing written).
pub static mut WRITE_END: usize = 0;

/// DIAG: zlib-compatible CRC-32 of `[start, end)` of the region.
pub fn crc(start: usize, end: usize) -> u32 {
    let s = shadow();
    let end = end.min(s.len());
    if start >= end {
        return 0;
    }
    !ferrite_core::fs::crc32_update(!0, &s[start..end])
}

fn shadow() -> &'static mut Vec<u8> {
    unsafe { &mut *(&raw mut SHADOW) }
}

/// Allocates the region erased, then fills it from the saved image if there
/// is one. Call after `heap::init` and `storage::init`.
pub fn init() {
    *shadow() = vec![0xFF; SIZE];
    let n = storage::load(shadow());
    unsafe { USED = n };
}

fn touch(end: usize) {
    unsafe {
        USED = USED.max(end);
        DIRTY = true;
        LAST_WRITE_MS = systick::millis();
    }
}

/// Writes the region back to eMMC if it changed: always when `force`
/// (before a restart), else once writes have been quiet for
/// `storage::IDLE_SAVE_MS`. Cheap when nothing changed.
pub fn save_if_dirty(force: bool) {
    unsafe {
        if !DIRTY || !storage::available() {
            return;
        }
        if !force && systick::millis().wrapping_sub(LAST_WRITE_MS) < storage::IDLE_SAVE_MS {
            return;
        }
        if storage::save(&shadow()[..USED]) {
            DIRTY = false;
        }
    }
}

#[derive(Clone)]
pub struct RamFlash;

impl FlashBackend for RamFlash {
    fn read(&self, addr: u32, buf: &mut [u8]) {
        let s = shadow();
        let start = (addr as usize).min(s.len());
        let n = buf.len().min(s.len() - start);
        buf[..n].copy_from_slice(&s[start..start + n]);
        buf[n..].fill(0xFF);
    }

    fn write(&self, addr: u32, data: &[u8]) {
        let s = shadow();
        let start = (addr as usize).min(s.len());
        for (d, &b) in s[start..].iter_mut().zip(data) {
            *d &= b; // NOR program: 1 → 0 only
        }
        let end = (start + data.len()).min(s.len());
        unsafe { WRITE_END = WRITE_END.max(end) };
        touch(end);
    }

    fn erase_sector(&self, addr: u32) {
        let s = shadow();
        let base = (addr as usize & !(SECTOR - 1)).min(s.len());
        let end = (base + SECTOR).min(s.len());
        s[base..end].fill(0xFF);
        // An erase inside the saved part changes it; past it, nothing to save.
        if base < unsafe { USED } {
            touch(0);
        }
    }

    fn read_id(&self) -> (u8, u8, u8) {
        (0xEF, 0x44, 0x12) // "RAM" marker, not a real JEDEC id
    }
}
