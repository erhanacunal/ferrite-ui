/// Callback metadata — function table for VM callback dispatch
///
/// Loaded from flash filesystem as "<program>.meta" resource.
///
/// Flash format:
///   Header (8 bytes):
///     [0..2] func_count:        u16 LE
///     [2..4] on_program_start:  u16 LE (bytecode offset, 0xFFFF = not defined)
///     [4..6] on_page_changing:  u16 LE
///     [6..8] on_page_changed:   u16 LE
///
///   Function table (5 bytes per entry):
///     [0..2] func_id:    u16 LE (1-based, 0 = invalid)
///     [2..4] offset:     u16 LE (byte offset in bytecode)
///     [4]    arg_count:  u8
///
///   Extended system callbacks (after function table, optional):
///     [+0..+2] on_user_message: u16 LE

use crate::flash::Flash;
use crate::fs::Fs;

/// Maximum number of callback functions
const MAX_FUNCS: usize = 16;

/// Metadata header size in bytes
const META_HEADER: usize = 8;

/// Sentinel value: callback not defined
pub const NO_CALLBACK: u16 = 0xFFFF;

/// Single function entry in the callback table
#[derive(Clone, Copy)]
struct FuncEntry {
    func_id: u16,
    offset: u16,
    arg_count: u8,
}

impl FuncEntry {
    const fn empty() -> Self {
        Self {
            func_id: 0,
            offset: 0,
            arg_count: 0,
        }
    }
}

/// Callback metadata: function table + system callback slots.
/// Loaded from flash, used to dispatch widget and system callbacks.
pub struct CallbackMeta {
    funcs: [FuncEntry; MAX_FUNCS],
    func_count: u8,
    pub on_program_start: u16,
    pub on_page_changing: u16,
    pub on_page_changed: u16,
    pub on_user_message: u16,
}

impl CallbackMeta {
    pub const fn new() -> Self {
        Self {
            funcs: [FuncEntry::empty(); MAX_FUNCS],
            func_count: 0,
            on_program_start: NO_CALLBACK,
            on_page_changing: NO_CALLBACK,
            on_page_changed: NO_CALLBACK,
            on_user_message: NO_CALLBACK,
        }
    }

    /// Load callback metadata from flash filesystem.
    /// Looks for resource named "<name>.meta" (e.g., "main.meta").
    pub fn load(fs: &Fs, flash: &Flash, name: &[u8]) -> Option<Self> {
        // Build the meta resource name: name + ".meta"
        let mut meta_name = [0u8; 16];
        let name_len = name.len().min(10); // leave room for ".meta\0"
        meta_name[..name_len].copy_from_slice(&name[..name_len]);
        meta_name[name_len..name_len + 5].copy_from_slice(b".meta");
        let full_len = name_len + 5;

        let entry = fs.find(flash, &meta_name[..full_len])?;
        if entry.size < META_HEADER as u32 {
            return None;
        }

        // Read header
        let mut hdr = [0u8; META_HEADER];
        flash.read(entry.offset, &mut hdr);

        let func_count = u16::from_le_bytes([hdr[0], hdr[1]]);
        let on_program_start = u16::from_le_bytes([hdr[2], hdr[3]]);
        let on_page_changing = u16::from_le_bytes([hdr[4], hdr[5]]);
        let on_page_changed = u16::from_le_bytes([hdr[6], hdr[7]]);

        let count = (func_count as usize).min(MAX_FUNCS);
        let mut meta = CallbackMeta {
            funcs: [FuncEntry::empty(); MAX_FUNCS],
            func_count: count as u8,
            on_program_start,
            on_page_changing,
            on_page_changed,
            on_user_message: NO_CALLBACK,
        };

        // Read function entries (5 bytes each)
        let mut buf = [0u8; 5];
        for i in 0..count {
            let addr = entry.offset + META_HEADER as u32 + (i * 5) as u32;
            flash.read(addr, &mut buf);
            meta.funcs[i] = FuncEntry {
                func_id: u16::from_le_bytes([buf[0], buf[1]]),
                offset: u16::from_le_bytes([buf[2], buf[3]]),
                arg_count: buf[4],
            };
        }

        // Extended system callbacks (after function table, backwards compatible)
        let ext_offset = META_HEADER as u32 + (count * 5) as u32;
        if entry.size >= ext_offset + 2 {
            let mut ext = [0u8; 2];
            flash.read(entry.offset + ext_offset, &mut ext);
            meta.on_user_message = u16::from_le_bytes(ext);
        }

        Some(meta)
    }

    /// Find a function by func_id. Returns (offset, arg_count) or None.
    pub fn find_func(&self, func_id: u16) -> Option<(u16, u8)> {
        for i in 0..self.func_count as usize {
            if self.funcs[i].func_id == func_id {
                return Some((self.funcs[i].offset, self.funcs[i].arg_count));
            }
        }
        None
    }
}
