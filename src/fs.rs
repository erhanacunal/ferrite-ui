/// Flash File System — compact TOC (Table of Contents) on W25Q256
///
/// Sector 0 (0x000000-0x000FFF) is reserved for ConfigStore.
/// FS starts at FS_BASE (0x001000):
///   FS_BASE + 0x00 - 0x0F : Header (16 bytes)
///   FS_BASE + 0x10 - ...  : Resource Table (N × 32 bytes)
///   immediately after     : Resource data (packed)
///
/// Resources are accessed by name (null-terminated ASCII, max 15 chars).
/// Kind: Font=0, Image=1, Program=2, Page=3

use crate::flash::Flash;

// --- Layout constants ---

/// FS base address — sector 0 is reserved for ConfigStore
pub const FS_BASE: u32 = 0x1000;

/// Header starts at FS_BASE
const HEADER_OFFSET: u32 = FS_BASE;

/// Header size in bytes
const HEADER_SIZE: u32 = 16;

/// Resource table starts right after the header
const TABLE_OFFSET: u32 = HEADER_OFFSET + HEADER_SIZE;

/// Magic number: "FERR" (little-endian)
const MAGIC: u32 = 0x5252_4546;

/// Max resource count (limited by u16 in header, practical limit ~1000)
const MAX_RESOURCES: usize = 1000;

/// Resource name max uzunluğu (null-terminator dahil)
const NAME_LEN: usize = 16;

/// Tek bir resource table entry boyutu
const ENTRY_SIZE: usize = 32;

// Expected version
const CURRENT_VERSION: u16 = 2;

// --- Resource tipleri ---

pub const RES_FONT: u8 = 0;
pub const RES_IMAGE: u8 = 1;
pub const RES_PROGRAM: u8 = 2;
pub const RES_PAGE: u8 = 3;

// --- Header (flash'tan okunan, RAM'de cache) ---

/// Flash dosya sistemi — mount edilince header RAM'de tutulur.
/// Resource table flash'ta kalır, ihtiyaç olunca okunur.
pub struct Fs {
    pub version: u16,
    pub screen_w: u16,
    pub screen_h: u16,
    pub resource_count: u16,
    pub checksum: u32,
}

/// Tek bir resource entry (find sonucu).
#[derive(Clone, Copy)]
pub struct ResourceEntry {
    pub kind: u8,
    pub offset: u32,
    pub size: u32,
}

// --- Mount hataları ---

#[derive(Clone, Copy, PartialEq)]
pub enum FsError {
    BadMagic,
    TooManyResources,
    BadVersion,
}

impl Fs {
    /// Flash'taki dosya sistemini mount et.
    /// Header'ı okur, magic'i doğrular, metadata'yı cache'ler.
    pub fn mount(flash: &Flash) -> Result<Self, FsError> {
        let mut buf = [0u8; 16];
        flash.read(HEADER_OFFSET, &mut buf);

        // Magic (4B LE)
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != MAGIC {
            return Err(FsError::BadMagic);
        }

        // Version (2B LE)
        let version = u16::from_le_bytes([buf[4], buf[5]]);
        if version != CURRENT_VERSION {
            return Err(FsError::BadVersion);
        }

        // Screen dimensions (2B + 2B LE)
        let screen_w = u16::from_le_bytes([buf[6], buf[7]]);
        let screen_h = u16::from_le_bytes([buf[8], buf[9]]);

        // Resource count (2B LE)
        let resource_count = u16::from_le_bytes([buf[10], buf[11]]);
        if resource_count as usize > MAX_RESOURCES {
            return Err(FsError::TooManyResources);
        }

        // Checksum (4B LE) — saklanır, doğrulama ayrı yapılır
        let checksum = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);

        Ok(Fs {
            version,
            screen_w,
            screen_h,
            resource_count,
            checksum,
        })
    }

    /// Resource'u isimle bul. Linear scan — max 127 entry, yeterince hızlı.
    /// Bulunamazsa None döner.
    pub fn find(&self, flash: &Flash, name: &[u8]) -> Option<ResourceEntry> {
        let mut entry_buf = [0u8; ENTRY_SIZE];

        for i in 0..self.resource_count as u32 {
            let addr = TABLE_OFFSET + i * ENTRY_SIZE as u32;
            flash.read(addr, &mut entry_buf);

            // İsim karşılaştır (null-terminated)
            if name_eq(&entry_buf[..NAME_LEN], name) {
                let kind = entry_buf[16];
                // [17..20] = padding
                let offset = u32::from_le_bytes([
                    entry_buf[20],
                    entry_buf[21],
                    entry_buf[22],
                    entry_buf[23],
                ]);
                let size = u32::from_le_bytes([
                    entry_buf[24],
                    entry_buf[25],
                    entry_buf[26],
                    entry_buf[27],
                ]);
                // [28..32] = reserved

                return Some(ResourceEntry { kind, offset, size });
            }
        }

        None
    }

    /// Resource verisini oku. entry.offset + local_offset'ten başlar.
    /// Resource sınırını aşmaz.
    pub fn read_resource(
        &self,
        flash: &Flash,
        entry: &ResourceEntry,
        local_offset: u32,
        buf: &mut [u8],
    ) {
        if local_offset >= entry.size {
            return;
        }
        let remaining = (entry.size - local_offset) as usize;
        let read_len = if buf.len() < remaining {
            buf.len()
        } else {
            remaining
        };
        flash.read(entry.offset + local_offset, &mut buf[..read_len]);
    }

    /// Belirli tipteki tüm resource'ları say.
    pub fn count_by_kind(&self, flash: &Flash, kind: u8) -> u16 {
        let mut count: u16 = 0;
        let mut entry_buf = [0u8; ENTRY_SIZE];

        for i in 0..self.resource_count as u32 {
            let addr = TABLE_OFFSET + i * ENTRY_SIZE as u32;
            flash.read(addr, &mut entry_buf);
            if entry_buf[16] == kind {
                count += 1;
            }
        }

        count
    }

    /// Index ile resource entry oku (0-based).
    /// Resource table'ın i. entry'si — tipe bakmaz.
    pub fn get_entry(&self, flash: &Flash, index: u16) -> Option<ResourceEntry> {
        if index >= self.resource_count {
            return None;
        }

        let mut entry_buf = [0u8; ENTRY_SIZE];
        let addr = TABLE_OFFSET + index as u32 * ENTRY_SIZE as u32;
        flash.read(addr, &mut entry_buf);

        let kind = entry_buf[16];
        let offset = u32::from_le_bytes([
            entry_buf[20],
            entry_buf[21],
            entry_buf[22],
            entry_buf[23],
        ]);
        let size = u32::from_le_bytes([
            entry_buf[24],
            entry_buf[25],
            entry_buf[26],
            entry_buf[27],
        ]);

        Some(ResourceEntry { kind, offset, size })
    }

    /// Belirli tipteki n. resource'u bul (0-based).
    /// Örn: 3. PAGE resource'u → `find_nth_by_kind(flash, RES_PAGE, 3)`
    pub fn find_nth_by_kind(
        &self,
        flash: &Flash,
        kind: u8,
        n: u16,
    ) -> Option<ResourceEntry> {
        let mut found: u16 = 0;
        let mut entry_buf = [0u8; ENTRY_SIZE];

        for i in 0..self.resource_count as u32 {
            let addr = TABLE_OFFSET + i * ENTRY_SIZE as u32;
            flash.read(addr, &mut entry_buf);

            if entry_buf[16] == kind {
                if found == n {
                    let offset = u32::from_le_bytes([
                        entry_buf[20],
                        entry_buf[21],
                        entry_buf[22],
                        entry_buf[23],
                    ]);
                    let size = u32::from_le_bytes([
                        entry_buf[24],
                        entry_buf[25],
                        entry_buf[26],
                        entry_buf[27],
                    ]);
                    return Some(ResourceEntry { kind, offset, size });
                }
                found += 1;
            }
        }

        None
    }

    /// Checksum doğrula: resource table üzerinden basit additive sum.
    /// Host tool aynı algoritmayı kullanarak checksum hesaplar.
    pub fn verify_checksum(&self, flash: &Flash) -> bool {
        let table_size = self.resource_count as u32 * ENTRY_SIZE as u32;
        if table_size == 0 {
            return self.checksum == 0;
        }

        let mut sum: u32 = 0;
        let mut buf = [0u8; ENTRY_SIZE];

        for i in 0..self.resource_count as u32 {
            let addr = TABLE_OFFSET + i * ENTRY_SIZE as u32;
            flash.read(addr, &mut buf);
            for &b in &buf {
                sum = sum.wrapping_add(b as u32);
            }
        }

        sum == self.checksum
    }
}

// --- Yardımcı ---

/// Null-terminated isim karşılaştırma.
/// `stored`: flash'tan okunan NAME_LEN byte (null-padded).
/// `query`: aranan isim (null-terminator olmadan).
fn name_eq(stored: &[u8], query: &[u8]) -> bool {
    // stored'daki null-terminator'a kadar olan kısmı karşılaştır
    let mut stored_len = 0;
    for &b in stored {
        if b == 0 {
            break;
        }
        stored_len += 1;
    }

    if stored_len != query.len() {
        return false;
    }

    for i in 0..stored_len {
        if stored[i] != query[i] {
            return false;
        }
    }

    true
}
