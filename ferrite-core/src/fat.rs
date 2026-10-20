/// FAT16/FAT32 read-only filesystem driver for SD card
///
/// Reads MBR → BPB → FAT → directory entries → file data.
/// Heap-allocated sector buffer (512B). FAT sectors cached one at a time.
///
/// Usage:
///   let sd = SdCard::init()?;
///   let mut fat = Fat::mount(&sd)?;
///   if let Some(entry) = fat.find(&sd, b"DATA    TXT") {
///       fat.read_file(&sd, &entry, offset, &mut buf);
///   }
///   sd.release_bus();
extern crate alloc;

use crate::sdcard::{SdCard, SdError};
use alloc::boxed::Box;

/// Sector size (always 512 for SD cards in SPI mode).
const SECTOR_SIZE: usize = 512;

/// Maximum directory entries to scan per call.
const MAX_DIR_SCAN: usize = 512;

#[derive(Clone, Copy, PartialEq)]
enum FatType {
    Fat16,
    Fat32,
}

/// FAT filesystem state.
pub struct Fat {
    fat_type: FatType,
    /// First sector of the FAT table.
    fat_start: u32,
    /// Number of sectors per FAT.
    fat_sectors: u32,
    /// First sector of the data region.
    data_start: u32,
    /// Sectors per cluster.
    sectors_per_cluster: u8,
    /// Root directory: first cluster (FAT32) or first sector (FAT16).
    root_cluster: u32,
    /// FAT16: number of root directory entries (fixed area).
    root_entry_count: u16,
    /// FAT16: first sector of the root directory area.
    root_dir_start: u32,
    /// Heap-allocated 512-byte sector buffer.
    buf: Box<[u8; SECTOR_SIZE]>,
    /// Currently cached FAT sector number (0xFFFFFFFF = none).
    cached_fat_sector: u32,
}

/// A directory entry found by scanning.
#[derive(Clone, Copy)]
pub struct DirEntry {
    /// 8.3 filename (8 name + 3 ext, space-padded, uppercase).
    pub name: [u8; 11],
    /// File attributes.
    pub attr: u8,
    /// First cluster (high 16 bits from FAT32 entry, low 16 bits).
    pub cluster: u32,
    /// File size in bytes.
    pub size: u32,
}

/// FAT error.
#[derive(Clone, Copy)]
pub enum FatError {
    Sd(SdError),
    BadMbr,
    BadBpb,
    NotSupported,
}

impl From<SdError> for FatError {
    fn from(e: SdError) -> Self {
        FatError::Sd(e)
    }
}

// Directory entry attributes
const ATTR_READ_ONLY: u8 = 0x01;
const ATTR_HIDDEN: u8 = 0x02;
const ATTR_SYSTEM: u8 = 0x04;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LFN: u8 = 0x0F;

impl Fat {
    /// Mount a FAT filesystem from the SD card.
    /// Reads MBR to find the first partition, then parses the BPB.
    pub fn mount(sd: &SdCard) -> Result<Self, FatError> {
        let mut buf = Box::new([0u8; SECTOR_SIZE]);

        // Read MBR (sector 0)
        sd.read_block(0, &mut buf)?;

        // Check MBR signature
        if buf[510] != 0x55 || buf[511] != 0xAA {
            return Err(FatError::BadMbr);
        }

        // Find first partition — check if sector 0 is MBR or BPB directly
        let partition_start = if buf[0] == 0xEB || buf[0] == 0xE9 {
            // No MBR — BPB at sector 0 (superfloppy)
            0u32
        } else {
            // MBR partition table at offset 446
            let p = &buf[446..462];
            let part_type = p[4];
            if part_type == 0 {
                return Err(FatError::BadMbr);
            }
            u32::from_le_bytes([p[8], p[9], p[10], p[11]])
        };

        // Read BPB (Boot Parameter Block)
        sd.read_block(partition_start, &mut buf)?;

        // Validate BPB signature
        if buf[510] != 0x55 || buf[511] != 0xAA {
            return Err(FatError::BadBpb);
        }

        let bytes_per_sector = u16::from_le_bytes([buf[11], buf[12]]) as u32;
        if bytes_per_sector != 512 {
            return Err(FatError::NotSupported);
        }

        let sectors_per_cluster = buf[13];
        let reserved_sectors = u16::from_le_bytes([buf[14], buf[15]]) as u32;
        let num_fats = buf[16] as u32;
        let root_entry_count = u16::from_le_bytes([buf[17], buf[18]]);
        let total_sectors_16 = u16::from_le_bytes([buf[19], buf[20]]) as u32;
        let fat_sectors_16 = u16::from_le_bytes([buf[22], buf[23]]) as u32;
        let total_sectors_32 = u32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]);
        let fat_sectors_32 = u32::from_le_bytes([buf[36], buf[37], buf[38], buf[39]]);

        let fat_sectors = if fat_sectors_16 != 0 {
            fat_sectors_16
        } else {
            fat_sectors_32
        };
        let total_sectors = if total_sectors_16 != 0 {
            total_sectors_16
        } else {
            total_sectors_32
        };

        let fat_start = partition_start + reserved_sectors;
        let root_dir_sectors = ((root_entry_count as u32) * 32 + 511) / 512;
        let root_dir_start = fat_start + num_fats * fat_sectors;
        let data_start = root_dir_start + root_dir_sectors;

        let data_sectors =
            total_sectors - (reserved_sectors + num_fats * fat_sectors + root_dir_sectors);
        let cluster_count = data_sectors / sectors_per_cluster as u32;

        let fat_type = if cluster_count < 4085 {
            return Err(FatError::NotSupported); // FAT12
        } else if cluster_count < 65525 {
            FatType::Fat16
        } else {
            FatType::Fat32
        };

        let root_cluster = if fat_type == FatType::Fat32 {
            u32::from_le_bytes([buf[44], buf[45], buf[46], buf[47]])
        } else {
            0
        };

        Ok(Fat {
            fat_type,
            fat_start,
            fat_sectors,
            data_start,
            sectors_per_cluster,
            root_cluster,
            root_entry_count,
            root_dir_start,
            buf,
            cached_fat_sector: 0xFFFFFFFF,
        })
    }

    /// Find a file in the root directory by 8.3 name.
    ///
    /// `name` must be exactly 11 bytes: 8 name + 3 extension, space-padded, uppercase.
    /// Example: `b"DATA    TXT"` for "DATA.TXT", `b"README  MD "` for "README.MD".
    pub fn find(&mut self, sd: &SdCard, name: &[u8; 11]) -> Option<DirEntry> {
        if self.fat_type == FatType::Fat32 {
            self.find_in_cluster_chain(sd, self.root_cluster, name)
        } else {
            self.find_in_fat16_root(sd, name)
        }
    }

    /// Find a file by a regular filename string (e.g. "data.txt").
    /// Converts to 8.3 format internally.
    pub fn find_by_name(&mut self, sd: &SdCard, filename: &[u8]) -> Option<DirEntry> {
        let name83 = to_83_name(filename)?;
        self.find(sd, &name83)
    }

    /// Read file data starting at `offset` into `buf`.
    /// Returns the number of bytes actually read.
    pub fn read_file(
        &mut self,
        sd: &SdCard,
        entry: &DirEntry,
        offset: u32,
        out: &mut [u8],
    ) -> usize {
        if offset >= entry.size || out.is_empty() {
            return 0;
        }

        let max_read = (entry.size - offset) as usize;
        let to_read = out.len().min(max_read);

        let cluster_bytes = self.sectors_per_cluster as u32 * SECTOR_SIZE as u32;
        let mut cluster = entry.cluster;
        let mut pos = offset;
        let mut written = 0usize;

        // Skip clusters before the offset
        let clusters_to_skip = pos / cluster_bytes;
        for _ in 0..clusters_to_skip {
            match self.next_cluster(sd, cluster) {
                Some(c) => cluster = c,
                None => return written,
            }
        }
        pos -= clusters_to_skip * cluster_bytes;

        while written < to_read {
            let cluster_offset = pos % cluster_bytes;
            let sector_in_cluster = (cluster_offset / SECTOR_SIZE as u32) as u32;
            let byte_in_sector = (cluster_offset % SECTOR_SIZE as u32) as usize;

            let sector = self.cluster_to_sector(cluster) + sector_in_cluster;
            if sd.read_block(sector, &mut self.buf).is_err() {
                return written;
            }

            let available = SECTOR_SIZE - byte_in_sector;
            let remaining = to_read - written;
            let n = available.min(remaining);

            out[written..written + n]
                .copy_from_slice(&self.buf[byte_in_sector..byte_in_sector + n]);
            written += n;
            pos += n as u32;

            // Move to next sector/cluster
            if pos % cluster_bytes == 0 {
                match self.next_cluster(sd, cluster) {
                    Some(c) => cluster = c,
                    None => return written,
                }
            }
        }

        written
    }

    // --- Internal ---

    /// Convert cluster number to first sector number.
    fn cluster_to_sector(&self, cluster: u32) -> u32 {
        self.data_start + (cluster - 2) * self.sectors_per_cluster as u32
    }

    /// Read the next cluster from the FAT table. Returns None at end-of-chain.
    fn next_cluster(&mut self, sd: &SdCard, cluster: u32) -> Option<u32> {
        match self.fat_type {
            FatType::Fat16 => {
                let offset = cluster * 2;
                let fat_sector = self.fat_start + offset / SECTOR_SIZE as u32;
                let fat_index = (offset % SECTOR_SIZE as u32) as usize;

                if self.cached_fat_sector != fat_sector {
                    if sd.read_block(fat_sector, &mut self.buf).is_err() {
                        return None;
                    }
                    self.cached_fat_sector = fat_sector;
                }

                let next =
                    u16::from_le_bytes([self.buf[fat_index], self.buf[fat_index + 1]]) as u32;

                if next >= 0xFFF8 { None } else { Some(next) }
            }
            FatType::Fat32 => {
                let offset = cluster * 4;
                let fat_sector = self.fat_start + offset / SECTOR_SIZE as u32;
                let fat_index = (offset % SECTOR_SIZE as u32) as usize;

                if self.cached_fat_sector != fat_sector {
                    if sd.read_block(fat_sector, &mut self.buf).is_err() {
                        return None;
                    }
                    self.cached_fat_sector = fat_sector;
                }

                let next = u32::from_le_bytes([
                    self.buf[fat_index],
                    self.buf[fat_index + 1],
                    self.buf[fat_index + 2],
                    self.buf[fat_index + 3],
                ]) & 0x0FFFFFFF;

                if next >= 0x0FFFFFF8 { None } else { Some(next) }
            }
        }
    }

    /// Search FAT16 fixed root directory area.
    fn find_in_fat16_root(&mut self, sd: &SdCard, name: &[u8; 11]) -> Option<DirEntry> {
        let root_sectors = ((self.root_entry_count as u32) * 32 + 511) / 512;
        for s in 0..root_sectors {
            if sd
                .read_block(self.root_dir_start + s, &mut self.buf)
                .is_err()
            {
                return None;
            }
            if let Some(entry) = scan_dir_sector(&self.buf, name) {
                return Some(entry);
            }
        }
        None
    }

    /// Search a cluster chain for a directory entry (FAT32 root or subdirectory).
    fn find_in_cluster_chain(
        &mut self,
        sd: &SdCard,
        start_cluster: u32,
        name: &[u8; 11],
    ) -> Option<DirEntry> {
        let mut cluster = start_cluster;
        let mut scanned = 0usize;

        loop {
            for s in 0..self.sectors_per_cluster as u32 {
                let sector = self.cluster_to_sector(cluster) + s;
                if sd.read_block(sector, &mut self.buf).is_err() {
                    return None;
                }
                if let Some(entry) = scan_dir_sector(&self.buf, name) {
                    return Some(entry);
                }
                scanned += 1;
                if scanned > MAX_DIR_SCAN {
                    return None;
                }
            }
            match self.next_cluster(sd, cluster) {
                Some(c) => cluster = c,
                None => return None,
            }
        }
    }
}

/// Scan one 512-byte sector for a directory entry matching `name`.
fn scan_dir_sector(buf: &[u8; SECTOR_SIZE], name: &[u8; 11]) -> Option<DirEntry> {
    for i in 0..16 {
        let off = i * 32;
        let first = buf[off];

        if first == 0x00 {
            return None; // no more entries
        }
        if first == 0xE5 {
            continue; // deleted entry
        }

        let attr = buf[off + 11];
        if attr == ATTR_LFN {
            continue; // skip LFN entries
        }
        if attr & ATTR_VOLUME_ID != 0 {
            continue; // skip volume label
        }

        // Compare 8.3 name
        if &buf[off..off + 11] == name {
            let cluster_hi = u16::from_le_bytes([buf[off + 20], buf[off + 21]]) as u32;
            let cluster_lo = u16::from_le_bytes([buf[off + 26], buf[off + 27]]) as u32;
            let cluster = (cluster_hi << 16) | cluster_lo;
            let size =
                u32::from_le_bytes([buf[off + 28], buf[off + 29], buf[off + 30], buf[off + 31]]);

            let mut entry_name = [0u8; 11];
            entry_name.copy_from_slice(&buf[off..off + 11]);

            return Some(DirEntry {
                name: entry_name,
                attr,
                cluster,
                size,
            });
        }
    }
    None
}

/// Convert a regular filename (e.g. "data.txt") to 8.3 format.
/// Returns None if the name is invalid.
fn to_83_name(filename: &[u8]) -> Option<[u8; 11]> {
    let mut name = [b' '; 11];

    // Find dot position
    let dot_pos = filename.iter().position(|&b| b == b'.');
    let (base, ext) = match dot_pos {
        Some(pos) => (&filename[..pos], &filename[pos + 1..]),
        None => (filename, &[] as &[u8]),
    };

    if base.is_empty() || base.len() > 8 || ext.len() > 3 {
        return None;
    }

    for (i, &b) in base.iter().enumerate() {
        name[i] = b.to_ascii_uppercase();
    }
    for (i, &b) in ext.iter().enumerate() {
        name[8 + i] = b.to_ascii_uppercase();
    }

    Some(name)
}
