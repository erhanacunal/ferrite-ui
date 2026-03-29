/// System Config Store — persistent key-value storage in flash sector 0
///
/// Layout (4KB sector at address 0x000000):
///   0x000 - 0x003 : Magic "FCFG" (4 bytes LE)
///   0x004         : Version (u8)
///   0x005         : Entry count (u8)
///   0x006 - 0x007 : Reserved
///   0x008+        : Entries (32 bytes each, max 127)
///
/// Entry format (32 bytes):
///   0x00 - 0x01 : Key ID (u16 LE)
///   0x02        : Data length (u8, max 28)
///   0x03        : Reserved
///   0x04 - 0x1F : Data (28 bytes, zero-padded)

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use crate::flash::Flash;

const CONFIG_BASE: u32 = 0x0000;
const CONFIG_MAGIC: u32 = 0x4746_4346; // "FCFG" LE
const CONFIG_VERSION: u8 = 1;
const HEADER_SIZE: usize = 8;
const ENTRY_SIZE: usize = 32;
const MAX_ENTRIES: u8 = 127;
const MAX_DATA_LEN: usize = 28;

// --- Key IDs ---

pub const KEY_TOUCH_CAL: u16 = 0x0001;

// --- ConfigStore ---

pub struct ConfigStore {
    entry_count: u8,
}

impl ConfigStore {
    /// Mount existing config store from flash sector 0.
    /// Returns None if not formatted (bad magic or version).
    pub fn mount(flash: &Flash) -> Option<Self> {
        let mut buf = [0u8; 8];
        flash.read(CONFIG_BASE, &mut buf);

        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != CONFIG_MAGIC {
            return None;
        }

        if buf[4] != CONFIG_VERSION {
            return None;
        }

        Some(Self { entry_count: buf[5] })
    }

    /// Format sector 0 as a fresh config store (erases existing data).
    pub fn format(flash: &Flash) -> Self {
        flash.erase_sector(CONFIG_BASE);

        let mut header = [0u8; 8];
        header[0..4].copy_from_slice(&CONFIG_MAGIC.to_le_bytes());
        header[4] = CONFIG_VERSION;
        header[5] = 0;

        flash.write(CONFIG_BASE, &header);

        Self { entry_count: 0 }
    }

    /// Mount if formatted, otherwise format fresh.
    pub fn mount_or_format(flash: &Flash) -> Self {
        Self::mount(flash).unwrap_or_else(|| Self::format(flash))
    }

    /// Read entry by key ID. Copies data into buf, returns data length or None.
    pub fn read(&self, flash: &Flash, key: u16, buf: &mut [u8]) -> Option<u8> {
        let mut entry_buf = [0u8; ENTRY_SIZE];

        for i in 0..self.entry_count {
            let addr = CONFIG_BASE + HEADER_SIZE as u32 + i as u32 * ENTRY_SIZE as u32;
            flash.read(addr, &mut entry_buf);

            let k = u16::from_le_bytes([entry_buf[0], entry_buf[1]]);
            if k == key {
                let len = (entry_buf[2] as usize).min(MAX_DATA_LEN);
                let copy_len = len.min(buf.len());
                buf[..copy_len].copy_from_slice(&entry_buf[4..4 + copy_len]);
                return Some(len as u8);
            }
        }

        None
    }

    /// Write entry by key ID. Updates existing or appends new.
    /// Erases and rewrites the entire config sector.
    pub fn write(&mut self, flash: &Flash, key: u16, data: &[u8]) {
        let data_len = data.len().min(MAX_DATA_LEN);

        // Read used portion of sector into heap buffer
        let used = HEADER_SIZE + self.entry_count as usize * ENTRY_SIZE;
        let mut sector: Vec<u8> = vec![0u8; used + ENTRY_SIZE]; // room for one more
        flash.read(CONFIG_BASE, &mut sector[..used]);

        // Search for existing entry
        let mut found = false;
        for i in 0..self.entry_count as usize {
            let off = HEADER_SIZE + i * ENTRY_SIZE;
            let k = u16::from_le_bytes([sector[off], sector[off + 1]]);
            if k == key {
                sector[off + 2] = data_len as u8;
                sector[off + 3] = 0;
                for b in &mut sector[off + 4..off + 4 + MAX_DATA_LEN] {
                    *b = 0;
                }
                sector[off + 4..off + 4 + data_len].copy_from_slice(&data[..data_len]);
                found = true;
                break;
            }
        }

        if !found {
            if self.entry_count >= MAX_ENTRIES {
                return; // full
            }
            let off = HEADER_SIZE + self.entry_count as usize * ENTRY_SIZE;
            // Ensure buffer is big enough
            if sector.len() < off + ENTRY_SIZE {
                sector.resize(off + ENTRY_SIZE, 0);
            }
            sector[off..off + 2].copy_from_slice(&key.to_le_bytes());
            sector[off + 2] = data_len as u8;
            sector[off + 3] = 0;
            for b in &mut sector[off + 4..off + 4 + MAX_DATA_LEN] {
                *b = 0;
            }
            sector[off + 4..off + 4 + data_len].copy_from_slice(&data[..data_len]);
            self.entry_count += 1;
            sector[5] = self.entry_count;
        }

        let total = HEADER_SIZE + self.entry_count as usize * ENTRY_SIZE;

        // Erase and rewrite sector
        flash.erase_sector(CONFIG_BASE);
        let mut offset: usize = 0;
        while offset < total {
            let page_len = (total - offset).min(256);
            flash.write(CONFIG_BASE + offset as u32, &sector[offset..offset + page_len]);
            offset += page_len;
        }
    }
}
