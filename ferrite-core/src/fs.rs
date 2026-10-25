/// Flash File System — compact TOC (Table of Contents) on W25Q256
///
/// The resource flash is partitioned by the board (see `Platform::FS_BASE` /
/// `Platform::CONFIG_BASE`). A 4 KB ConfigStore sector sits below the FS; the
/// FS itself begins at `base` — 0x001000 by default, but pushed past the
/// firmware region on boards that share the flash with firmware (e.g. tdo_y13):
///   base + 0x00 - 0x0F : Header (16 bytes)
///   base + 0x10 - ...  : Resource Table (N × 32 bytes)
///   immediately after  : Resource data (packed)
///
/// Header: magic "FERR" u32, version u16, screen_w u16, screen_h u16,
/// resource_count u16, checksum u32. Version 3 (current): `checksum` is the
/// CRC-32 (IEEE, = Python `zlib.crc32`) of header bytes 0..12 followed by
/// everything from the table to the end of the last resource — the whole
/// image except the checksum field. Version 2 (still mounted): additive
/// byte sum of the table only. See [`Fs::verify`].
///
/// Resources are accessed by name (null-terminated ASCII, max 15 chars).
/// Kind: Font=0, Image=1, Program=2, Page=3, File=4, SysCfg=5, Sound=6
use crate::flash::{FlashBackend, FlashImpl};

// --- Layout constants ---

/// Default FS base address (sector 1) for boards whose firmware lives on a
/// separate chip — sector 0 is reserved for ConfigStore. Overridable per-board
/// via [`crate::platform::Platform::FS_BASE`].
pub const DEFAULT_FS_BASE: u32 = 0x1000;

/// Header size in bytes
const HEADER_SIZE: u32 = 16;

/// Magic number: "FERR" (little-endian)
const MAGIC: u32 = 0x5252_4546;

/// Max resource count (limited by u16 in header, practical limit ~1000)
const MAX_RESOURCES: usize = 1000;

/// Resource name max uzunluğu (null-terminator dahil)
const NAME_LEN: usize = 16;

/// Tek bir resource table entry boyutu
const ENTRY_SIZE: usize = 32;

/// Version written by the build tools: full-image CRC-32.
const CURRENT_VERSION: u16 = 3;
/// Older images (table-only byte sum) — still mounted.
const LEGACY_VERSION: u16 = 2;

/// Largest image [`Fs::verify`] hashes (the biggest supported flash, 32 MB):
/// a corrupt table entry must not make it read for minutes.
const MAX_IMAGE_BYTES: u32 = 32 * 1024 * 1024;

// --- Resource tipleri ---

pub const RES_FONT: u8 = 0;
pub const RES_IMAGE: u8 = 1;
pub const RES_PROGRAM: u8 = 2;
/// Widget page: static widget setup of the program with the same name,
/// run by its OP_W_LOAD blocks (see `vm::Vm::set_page`).
pub const RES_PAGE: u8 = 3;
pub const RES_FILE: u8 = 4;
/// System configuration record (pool sizes) — see [`crate::syscfg`].
pub const RES_SYSCFG: u8 = 5;
/// PCM16 sound (header + samples) — see `audio::SOUND_HEADER` (`audio` feature).
pub const RES_SOUND: u8 = 6;
/// `Fs::find_kind` wildcard.
pub const RES_ANY: u8 = 0xFF;

// --- Header (flash'tan okunan, RAM'de cache) ---

/// Flash dosya sistemi — mount edilince header RAM'de tutulur.
/// Resource table flash'ta kalır, ihtiyaç olunca okunur.
pub struct Fs {
    pub version: u16,
    pub screen_w: u16,
    pub screen_h: u16,
    pub resource_count: u16,
    pub checksum: u32,
    /// Flash address of the FS header (board-specific — see
    /// [`crate::platform::Platform::FS_BASE`]). All table/resource offsets are
    /// relative to it.
    base: u32,
}

/// Tek bir resource entry (find sonucu).
#[derive(Clone, Copy)]
pub struct ResourceEntry {
    pub kind: u8,
    pub flags: u8,
    pub offset: u32,
    pub size: u32,
}

/// Resource flags (stored in entry reserved[0])
pub const RES_FLAG_FLASH_EXEC: u8 = 0x01;

// --- Mount hataları ---

#[derive(Clone, Copy, PartialEq)]
pub enum FsError {
    BadMagic,
    TooManyResources,
    BadVersion,
}

impl Fs {
    /// Resource table starts right after the header.
    #[inline]
    fn table_offset(&self) -> u32 {
        self.base + HEADER_SIZE
    }

    /// Flash'taki dosya sistemini mount et.
    /// Header'ı okur, magic'i doğrular, metadata'yı cache'ler.
    /// `base` is the flash address of the FS header (see `Platform::FS_BASE`).
    #[inline(never)]
    pub fn mount<F: FlashBackend>(flash: &FlashImpl<F>, base: u32) -> Result<Self, FsError> {
        let mut buf = [0u8; 16];
        flash.read(base, &mut buf);

        // Magic (4B LE)
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != MAGIC {
            return Err(FsError::BadMagic);
        }

        // Version (2B LE)
        let version = u16::from_le_bytes([buf[4], buf[5]]);
        if version != CURRENT_VERSION && version != LEGACY_VERSION {
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
            base,
        })
    }

    /// Resource'u isimle bul. Linear scan — max 127 entry, yeterince hızlı.
    /// Bulunamazsa None döner.
    #[inline]
    pub fn find<F: FlashBackend>(
        &self,
        flash: &FlashImpl<F>,
        name: &[u8],
    ) -> Option<ResourceEntry> {
        self.find_kind(flash, name, RES_ANY)
    }

    /// Find a resource by name and kind (`RES_ANY` = first with that name).
    /// A program and its widget page share a name.
    pub fn find_kind<F: FlashBackend>(
        &self,
        flash: &FlashImpl<F>,
        name: &[u8],
        want: u8,
    ) -> Option<ResourceEntry> {
        let mut entry_buf = [0u8; ENTRY_SIZE];

        for i in 0..self.resource_count as u32 {
            let addr = self.table_offset() + i * ENTRY_SIZE as u32;
            flash.read(addr, &mut entry_buf);

            // İsim karşılaştır (null-terminated)
            let kind = entry_buf[16];
            if name_eq(&entry_buf[..NAME_LEN], name) && (want == RES_ANY || kind == want) {
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
                let flags = entry_buf[28]; // reserved[0] = resource flags

                return Some(ResourceEntry {
                    kind,
                    flags,
                    offset,
                    size,
                });
            }
        }

        None
    }

    /// Resource verisini oku. entry.offset + local_offset'ten başlar.
    /// Resource sınırını aşmaz.
    pub fn read_resource<F: FlashBackend>(
        &self,
        flash: &FlashImpl<F>,
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
    pub fn count_by_kind<F: FlashBackend>(&self, flash: &FlashImpl<F>, kind: u8) -> u16 {
        let mut count: u16 = 0;
        let mut entry_buf = [0u8; ENTRY_SIZE];

        for i in 0..self.resource_count as u32 {
            let addr = self.table_offset() + i * ENTRY_SIZE as u32;
            flash.read(addr, &mut entry_buf);
            if entry_buf[16] == kind {
                count += 1;
            }
        }

        count
    }

    /// Index ile resource entry oku (0-based).
    /// Resource table'ın i. entry'si — tipe bakmaz.
    pub fn get_entry<F: FlashBackend>(
        &self,
        flash: &FlashImpl<F>,
        index: u16,
    ) -> Option<ResourceEntry> {
        if index >= self.resource_count {
            return None;
        }

        let mut entry_buf = [0u8; ENTRY_SIZE];
        let addr = self.table_offset() + index as u32 * ENTRY_SIZE as u32;
        flash.read(addr, &mut entry_buf);

        let kind = entry_buf[16];
        let offset =
            u32::from_le_bytes([entry_buf[20], entry_buf[21], entry_buf[22], entry_buf[23]]);
        let size = u32::from_le_bytes([entry_buf[24], entry_buf[25], entry_buf[26], entry_buf[27]]);

        Some(ResourceEntry {
            kind,
            flags: entry_buf[28],
            offset,
            size,
        })
    }

    /// Belirli tipteki n. resource'u bul (0-based).
    /// Örn: 3. PAGE resource'u → `find_nth_by_kind(flash, RES_PAGE, 3)`
    pub fn find_nth_by_kind<F: FlashBackend>(
        &self,
        flash: &FlashImpl<F>,
        kind: u8,
        n: u16,
    ) -> Option<ResourceEntry> {
        let mut found: u16 = 0;
        let mut entry_buf = [0u8; ENTRY_SIZE];

        for i in 0..self.resource_count as u32 {
            let addr = self.table_offset() + i * ENTRY_SIZE as u32;
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
                    return Some(ResourceEntry {
                        kind,
                        flags: entry_buf[28],
                        offset,
                        size,
                    });
                }
                found += 1;
            }
        }

        None
    }

    /// Check the whole image against the header checksum: CRC-32 over the
    /// header, table and resource data (v3), or the table byte sum (v2).
    /// Reads the entire image, so it costs one flash pass — ~8 s per MB on
    /// the bit-banged Nextion SPI, a few ms on RAM-shadowed flash.
    pub fn verify<F: FlashBackend>(&self, flash: &FlashImpl<F>) -> bool {
        if self.version == LEGACY_VERSION {
            return self.verify_checksum(flash);
        }
        let Some(end) = self.image_end(flash) else {
            return false;
        };
        let mut buf = [0u8; 64];
        flash.read(self.base, &mut buf[..12]);
        let mut crc = crc32_update(!0, &buf[..12]);
        let mut addr = self.table_offset();
        let end = self.base + end;
        while addr < end {
            let n = ((end - addr) as usize).min(buf.len());
            flash.read(addr, &mut buf[..n]);
            crc = crc32_update(crc, &buf[..n]);
            addr += n as u32;
        }
        !crc == self.checksum
    }

    /// Image length from `base`: the end of the table or of the furthest
    /// resource. `None` if an entry points outside the image.
    fn image_end<F: FlashBackend>(&self, flash: &FlashImpl<F>) -> Option<u32> {
        let table_end = HEADER_SIZE + self.resource_count as u32 * ENTRY_SIZE as u32;
        let mut end = table_end;
        let mut entry = [0u8; ENTRY_SIZE];
        for i in 0..self.resource_count as u32 {
            flash.read(self.table_offset() + i * ENTRY_SIZE as u32, &mut entry);
            let offset = u32::from_le_bytes([entry[20], entry[21], entry[22], entry[23]]);
            let size = u32::from_le_bytes([entry[24], entry[25], entry[26], entry[27]]);
            let start = offset.checked_sub(self.base)?;
            let stop = start.checked_add(size)?;
            if start < table_end || stop > MAX_IMAGE_BYTES {
                return None;
            }
            end = end.max(stop);
        }
        Some(end)
    }

    /// Version 2 check: additive sum of the resource table bytes.
    pub fn verify_checksum<F: FlashBackend>(&self, flash: &FlashImpl<F>) -> bool {
        let table_size = self.resource_count as u32 * ENTRY_SIZE as u32;
        if table_size == 0 {
            return self.checksum == 0;
        }

        let mut sum: u32 = 0;
        let mut buf = [0u8; ENTRY_SIZE];

        for i in 0..self.resource_count as u32 {
            let addr = self.table_offset() + i * ENTRY_SIZE as u32;
            flash.read(addr, &mut buf);
            for &b in &buf {
                sum = sum.wrapping_add(b as u32);
            }
        }

        sum == self.checksum
    }
}

// --- Yardımcı ---

/// CRC-32 (IEEE 802.3, reflected 0xEDB88320) nibble table — 64 bytes of
/// flash instead of the usual 1 KB byte table.
const CRC_NIBBLE: [u32; 16] = {
    let mut t = [0u32; 16];
    let mut i = 0;
    while i < 16 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 4 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// Feed `data` into a running CRC-32. Start from `!0` and invert the
/// result: `!crc32_update(!0, data)` = `zlib.crc32(data)`.
pub fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc ^= b as u32;
        crc = (crc >> 4) ^ CRC_NIBBLE[(crc & 0xF) as usize];
        crc = (crc >> 4) ^ CRC_NIBBLE[(crc & 0xF) as usize];
    }
    crc
}

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

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use alloc::vec::Vec;

    struct MemFlash(Vec<u8>);
    impl FlashBackend for MemFlash {
        fn read(&self, addr: u32, buf: &mut [u8]) {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = *self.0.get(addr as usize + i).unwrap_or(&0xFF);
            }
        }
        fn write(&self, _: u32, _: &[u8]) {}
        fn erase_sector(&self, _: u32) {}
        fn read_id(&self) -> (u8, u8, u8) {
            (0, 0, 0)
        }
    }

    const BASE: u32 = 0x1000;

    /// Flash holding an image at BASE with two resources, laid out the way
    /// `ferrite_build.build_fs` does it.
    fn image(version: u16) -> Vec<u8> {
        let data: [&[u8]; 2] = [b"hello", b"world!!"];
        let mut table = Vec::new();
        let mut blob = Vec::new();
        for (i, d) in data.iter().enumerate() {
            let mut e = [0u8; ENTRY_SIZE];
            e[0] = b'a' + i as u8;
            e[16] = RES_FILE;
            let off = BASE + HEADER_SIZE + 2 * ENTRY_SIZE as u32 + blob.len() as u32;
            e[20..24].copy_from_slice(&off.to_le_bytes());
            e[24..28].copy_from_slice(&(d.len() as u32).to_le_bytes());
            table.extend_from_slice(&e);
            blob.extend_from_slice(d);
        }
        let mut hdr = Vec::new();
        hdr.extend_from_slice(&MAGIC.to_le_bytes());
        hdr.extend_from_slice(&version.to_le_bytes());
        hdr.extend_from_slice(&800u16.to_le_bytes());
        hdr.extend_from_slice(&480u16.to_le_bytes());
        hdr.extend_from_slice(&2u16.to_le_bytes());
        let sum = if version == LEGACY_VERSION {
            table.iter().map(|&b| b as u32).sum::<u32>()
        } else {
            let c = crc32_update(!0, &hdr);
            let c = crc32_update(c, &table);
            !crc32_update(c, &blob)
        };
        hdr.extend_from_slice(&sum.to_le_bytes());
        let mut flash = alloc::vec![0xFFu8; BASE as usize];
        flash.extend_from_slice(&hdr);
        flash.extend_from_slice(&table);
        flash.extend_from_slice(&blob);
        flash
    }

    fn verify(bytes: Vec<u8>) -> bool {
        let flash = FlashImpl::with_backend(MemFlash(bytes));
        match Fs::mount(&flash, BASE) {
            Ok(fs) => fs.verify(&flash),
            Err(_) => false,
        }
    }

    #[test]
    fn crc32_matches_zlib() {
        assert_eq!(!crc32_update(!0, b"123456789"), 0xCBF4_3926);
        assert_eq!(!crc32_update(!0, b""), 0);
    }

    #[test]
    fn v3_image_verifies() {
        assert!(verify(image(CURRENT_VERSION)));
    }

    #[test]
    fn v3_detects_corrupt_data_table_and_header() {
        let good = image(CURRENT_VERSION);
        let last = good.len() - 1;
        // Last resource byte, a table name byte, the screen width.
        for at in [last, BASE as usize + 16, BASE as usize + 6] {
            let mut bad = good.clone();
            bad[at] ^= 0x01;
            assert!(!verify(bad), "flip at {at:#x} not detected");
        }
    }

    #[test]
    fn v3_rejects_entry_outside_image() {
        let mut bad = image(CURRENT_VERSION);
        // First entry's offset points below the table.
        let at = BASE as usize + 16 + 20;
        bad[at..at + 4].copy_from_slice(&BASE.to_le_bytes());
        assert!(!verify(bad));
    }

    #[test]
    fn v2_image_still_mounts_and_verifies() {
        assert!(verify(image(LEGACY_VERSION)));
        let mut bad = image(LEGACY_VERSION);
        bad[BASE as usize + 16] ^= 1;
        assert!(!verify(bad));
    }
}
