/// Callback metadata — function table for VM callback dispatch
///
/// Loaded from flash filesystem as "<program>.meta" resource.
///
/// Flash format:
///   Header (16 bytes):
///     [0..2]   func_count:        u16 LE
///     [2..4]   on_program_start:  u16 LE (bytecode offset, 0xFFFF = not defined)
///     [4..6]   on_page_changing:  u16 LE
///     [6..8]   on_page_changed:   u16 LE
///     [8..10]  on_user_message:   u16 LE
///     [10..12] on_touch_down:     u16 LE
///     [12..14] on_touch_up:       u16 LE
///     [14..16] on_touch_move:     u16 LE
///
///   Function table (5 bytes per entry):
///     [0..2] func_id:    u16 LE (1-based, 0 = invalid)
///     [2..4] offset:     u16 LE (byte offset in bytecode)
///     [4]    arg_count:  u8

use crate::flash::Flash;
use crate::fs::Fs;

/// Maximum number of callback functions
const MAX_FUNCS: usize = 16;

/// Metadata header size in bytes
const META_HEADER: usize = 16;

/// Raw function table size: MAX_FUNCS * 5 bytes per entry
const FUNC_TABLE_SIZE: usize = MAX_FUNCS * 5;

/// Sentinel value: callback not defined
pub const NO_CALLBACK: u16 = 0xFFFF;

/// Callback metadata: function table + system callback slots.
/// Function table stored as raw bytes — parsed on demand in find_func().
pub struct CallbackMeta {
    func_raw: [u8; FUNC_TABLE_SIZE],
    func_count: u8,
    pub on_program_start: u16,
    pub on_page_changing: u16,
    pub on_page_changed: u16,
    pub on_user_message: u16,
    pub on_touch_down: u16,
    pub on_touch_up: u16,
    pub on_touch_move: u16,
}

impl CallbackMeta {
    pub const fn new() -> Self {
        Self {
            func_raw: [0; FUNC_TABLE_SIZE],
            func_count: 0,
            on_program_start: NO_CALLBACK,
            on_page_changing: NO_CALLBACK,
            on_page_changed: NO_CALLBACK,
            on_user_message: NO_CALLBACK,
            on_touch_down: NO_CALLBACK,
            on_touch_up: NO_CALLBACK,
            on_touch_move: NO_CALLBACK,
        }
    }

    /// Parse the 16-byte header from a byte slice into system callback fields.
    fn parse_header(hdr: &[u8]) -> (u16, u16, u16, u16, u16, u16, u16, u16) {
        (
            u16::from_le_bytes([hdr[0], hdr[1]]),   // func_count
            u16::from_le_bytes([hdr[2], hdr[3]]),   // on_program_start
            u16::from_le_bytes([hdr[4], hdr[5]]),   // on_page_changing
            u16::from_le_bytes([hdr[6], hdr[7]]),   // on_page_changed
            u16::from_le_bytes([hdr[8], hdr[9]]),   // on_user_message
            u16::from_le_bytes([hdr[10], hdr[11]]), // on_touch_down
            u16::from_le_bytes([hdr[12], hdr[13]]), // on_touch_up
            u16::from_le_bytes([hdr[14], hdr[15]]), // on_touch_move
        )
    }

    /// Load callback metadata from flash filesystem.
    /// Looks for resource named "<name>.meta" (e.g., "main.meta").
    pub fn load(fs: &Fs, flash: &Flash, name: &[u8]) -> Option<Self> {
        let mut meta_name = [0u8; 16];
        let name_len = name.len().min(10);
        meta_name[..name_len].copy_from_slice(&name[..name_len]);
        meta_name[name_len..name_len + 5].copy_from_slice(b".meta");
        let full_len = name_len + 5;

        let entry = fs.find(flash, &meta_name[..full_len])?;
        if entry.size < META_HEADER as u32 {
            return None;
        }

        let mut hdr = [0u8; META_HEADER];
        flash.read(entry.offset, &mut hdr);

        let (fc, ops, opc, opd, oum, otd, otu, otm) = Self::parse_header(&hdr);
        let count = (fc as usize).min(MAX_FUNCS);

        let mut meta = Self {
            func_raw: [0; FUNC_TABLE_SIZE],
            func_count: count as u8,
            on_program_start: ops,
            on_page_changing: opc,
            on_page_changed: opd,
            on_user_message: oum,
            on_touch_down: otd,
            on_touch_up: otu,
            on_touch_move: otm,
        };

        // Read function table directly into raw buffer
        let table_bytes = count * 5;
        if table_bytes > 0 {
            flash.read(
                entry.offset + META_HEADER as u32,
                &mut meta.func_raw[..table_bytes],
            );
        }

        Some(meta)
    }

    /// Parse callback metadata from a byte slice (RAM buffer).
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < META_HEADER {
            return None;
        }

        let (fc, ops, opc, opd, oum, otd, otu, otm) = Self::parse_header(&data[..META_HEADER]);
        let count = (fc as usize).min(MAX_FUNCS);

        let mut meta = Self {
            func_raw: [0; FUNC_TABLE_SIZE],
            func_count: count as u8,
            on_program_start: ops,
            on_page_changing: opc,
            on_page_changed: opd,
            on_user_message: oum,
            on_touch_down: otd,
            on_touch_up: otu,
            on_touch_move: otm,
        };

        // Copy function table raw bytes
        let table_bytes = count * 5;
        let table_end = META_HEADER + table_bytes;
        if data.len() >= table_end && table_bytes > 0 {
            meta.func_raw[..table_bytes].copy_from_slice(&data[META_HEADER..table_end]);
        }

        Some(meta)
    }

    /// Find a function by func_id. Parses raw bytes on demand.
    /// Returns (offset, arg_count) or None.
    pub fn find_func(&self, func_id: u16) -> Option<(u16, u8)> {
        let count = self.func_count as usize;
        let mut i = 0;
        while i < count {
            let off = i * 5;
            let fid = u16::from_le_bytes([self.func_raw[off], self.func_raw[off + 1]]);
            if fid == func_id {
                let foff = u16::from_le_bytes([self.func_raw[off + 2], self.func_raw[off + 3]]);
                let argc = self.func_raw[off + 4];
                return Some((foff, argc));
            }
            i += 1;
        }
        None
    }
}
