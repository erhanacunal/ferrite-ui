//! ESP32-S3 internal flash backend for ferrite-ui.
//!
//! The Ferrite filesystem keeps its historical logical address layout with
//! `FS_BASE = 0x1000`.  This backend maps that logical range into the ESP-IDF
//! partition named `ferrite`, whose physical offset is defined in
//! `partitions.csv`.

#![cfg(feature = "epaper")]

use core::slice;

use super::FlashBackend;

const LOGICAL_FS_BASE: u32 = 0x1000;
const FERRITE_PARTITION_BASE: u32 = 0x0011_0000;
const FERRITE_PARTITION_SIZE: u32 = 4 * 1024 * 1024;
const SECTOR_SIZE: u32 = 4096;
const WORD_SIZE: usize = 4;
const SCRATCH_WORDS: usize = 64;
const SCRATCH_BYTES: usize = SCRATCH_WORDS * WORD_SIZE;

pub struct EspFlash;

impl EspFlash {
    pub fn new() -> Self {
        EspFlash
    }
}

impl FlashBackend for EspFlash {
    fn read(&self, addr: u32, buf: &mut [u8]) {
        if buf.is_empty() {
            return;
        }

        let Some(phys) = map_addr(addr, buf.len()) else {
            fill_erased(buf);
            return;
        };

        let mut done = 0usize;
        while done < buf.len() {
            let cur = phys + done as u32;
            let prefix = (cur & 0x03) as usize;
            let aligned = cur - prefix as u32;
            let payload = (SCRATCH_BYTES - prefix).min(buf.len() - done);
            let read_len = round_up(prefix + payload, WORD_SIZE);

            let mut scratch = [0u32; SCRATCH_WORDS];
            let ok = unsafe {
                esp_storage::ll::spiflash_read(aligned, scratch.as_mut_ptr(), read_len as u32)
                    .is_ok()
            };
            if !ok {
                fill_erased(&mut buf[done..]);
                return;
            }

            let scratch_bytes = unsafe {
                slice::from_raw_parts(scratch.as_ptr() as *const u8, SCRATCH_BYTES)
            };
            buf[done..done + payload].copy_from_slice(&scratch_bytes[prefix..prefix + payload]);
            done += payload;
        }
    }

    fn write(&self, addr: u32, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let Some(phys) = map_addr(addr, data.len()) else {
            return;
        };

        let _ = unsafe { esp_storage::ll::spiflash_unlock() };

        let mut done = 0usize;
        while done < data.len() {
            let cur = phys + done as u32;
            let prefix = (cur & 0x03) as usize;
            let aligned = cur - prefix as u32;
            let payload = (SCRATCH_BYTES - prefix).min(data.len() - done);
            let write_len = round_up(prefix + payload, WORD_SIZE);

            let mut scratch = [0xFFFF_FFFFu32; SCRATCH_WORDS];
            if prefix != 0 || write_len != payload {
                let _ = unsafe {
                    esp_storage::ll::spiflash_read(aligned, scratch.as_mut_ptr(), write_len as u32)
                };
            }

            let scratch_bytes = unsafe {
                slice::from_raw_parts_mut(scratch.as_mut_ptr() as *mut u8, SCRATCH_BYTES)
            };
            scratch_bytes[prefix..prefix + payload].copy_from_slice(&data[done..done + payload]);

            if unsafe {
                esp_storage::ll::spiflash_write(aligned, scratch.as_ptr(), write_len as u32)
            }
            .is_err()
            {
                return;
            }
            done += payload;
        }
    }

    fn erase_sector(&self, addr: u32) {
        let Some(phys) = map_addr(addr, 1) else {
            return;
        };
        let _ = unsafe { esp_storage::ll::spiflash_unlock() };
        let _ = unsafe { esp_storage::ll::spiflash_erase_sector(phys / SECTOR_SIZE) };
    }

    fn read_id(&self) -> (u8, u8, u8) {
        (0xEF, 0x40, 0x19)
    }
}

fn map_addr(addr: u32, len: usize) -> Option<u32> {
    if addr < LOGICAL_FS_BASE {
        return None;
    }
    let rel = addr - LOGICAL_FS_BASE;
    let len = len as u32;
    if rel.checked_add(len)? > FERRITE_PARTITION_SIZE {
        return None;
    }
    Some(FERRITE_PARTITION_BASE + rel)
}

fn fill_erased(buf: &mut [u8]) {
    for b in buf {
        *b = 0xFF;
    }
}

fn round_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}
