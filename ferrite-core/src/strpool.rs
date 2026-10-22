/// String pool for VM string operations.
///
/// The buffer, the string metadata and the `smart_clear` scratch table are
/// heap slices allocated once — at construction with the default capacity,
/// then again by [`StringPool::configure`] when the image's `RES_SYSCFG`
/// requests a different size. Strings never allocate individually, so the
/// pool causes no fragmentation at runtime.
/// Compacting smart_clear preserves widget text while reclaiming temporaries.
///
/// The default capacity is selected at compile time (2 KB buffer / 64 strings)
/// and suits small RAM targets; BSPs with plenty of RAM enable the
/// `strpool_large` feature for a 1 MB buffer / 256 strings. The string-metadata word width (`Off`)
/// follows the capacity: `u16` (4-byte `StrMeta`) on the small profile, `u32`
/// (8-byte `StrMeta`, strings/buffer past 64 KB) on the large profile.
///
/// RAM cost (heap): pool bytes + slots × (size_of::<StrMeta>() + 2).
extern crate alloc;

use alloc::boxed::Box;

use crate::syscfg::try_alloc_slice;
use crate::widget::{WidgetId, WidgetTree};

#[cfg(not(feature = "strpool_large"))]
const DEFAULT_POOL_SIZE: usize = 2048;
#[cfg(not(feature = "strpool_large"))]
const DEFAULT_MAX_STRINGS: usize = 64;
/// Offset/length word for the small profile — buffer ≤ 64 KB.
#[cfg(not(feature = "strpool_large"))]
type Off = u16;

#[cfg(feature = "strpool_large")]
const DEFAULT_POOL_SIZE: usize = 1024 * 1024;
#[cfg(feature = "strpool_large")]
const DEFAULT_MAX_STRINGS: usize = 256;
/// Offset/length word for the large profile — buffer past 64 KB.
#[cfg(feature = "strpool_large")]
type Off = u32;

const STR_NONE: u16 = 0xFFFF;
const FMT_BUF_SIZE: usize = 32;

/// Largest pool buffer the metadata word can address.
const MAX_POOL_SIZE: usize = Off::MAX as usize;
/// Largest slot count: ids are `u16` and `STR_NONE` is reserved.
const MAX_SLOTS: usize = STR_NONE as usize;

#[derive(Clone, Copy)]
struct StrMeta {
    offset: Off,
    len: Off,
}

impl StrMeta {
    const fn empty() -> Self {
        Self { offset: 0, len: 0 }
    }
}

pub struct StringPool {
    buf: Box<[u8]>,
    meta: Box<[StrMeta]>,
    /// smart_clear scratch: keep marker + old→new id remap, one per slot.
    remap: Box<[u16]>,
    /// One bit per slot: set for literals from `intern`. Those may be shared
    /// by several users, so they are never freed or edited in place. `u16`
    /// words so the allocation shares `remap`'s `try_alloc_slice::<u16>`.
    interned: Box<[u16]>,
    count: u16,
    next: Off,
}

impl StringPool {
    /// Pool with the compile-time default capacity.
    pub fn new() -> Self {
        let mut pool = Self::empty();
        // Boot-time allocation on an empty heap; failing here is fatal anyway.
        assert!(pool.alloc_storage(DEFAULT_POOL_SIZE, DEFAULT_MAX_STRINGS));
        pool
    }

    fn empty() -> Self {
        Self {
            buf: Box::new([]),
            meta: Box::new([]),
            remap: Box::new([]),
            interned: Box::new([]),
            count: 0,
            next: 0,
        }
    }

    /// Resize the pool to `bytes` / `strings` (0 = compile-time default),
    /// clamped to what the metadata width can address. Clears all strings.
    /// Returns false if the heap cannot hold the requested size; the pool then
    /// falls back to the default capacity (or stays empty if even that fails).
    pub fn configure(&mut self, bytes: usize, strings: usize) -> bool {
        let bytes = if bytes == 0 { DEFAULT_POOL_SIZE } else { bytes.min(MAX_POOL_SIZE) };
        let strings = if strings == 0 { DEFAULT_MAX_STRINGS } else { strings.min(MAX_SLOTS) };
        self.clear();
        if bytes == self.buf.len() && strings == self.meta.len() {
            return true;
        }
        // Release the old storage first so the allocator can reuse it.
        *self = Self::empty();
        if self.alloc_storage(bytes, strings) {
            return true;
        }
        self.alloc_storage(DEFAULT_POOL_SIZE, DEFAULT_MAX_STRINGS);
        false
    }

    fn alloc_storage(&mut self, bytes: usize, strings: usize) -> bool {
        let (Some(buf), Some(meta), Some(remap), Some(interned)) = (
            try_alloc_slice(bytes, 0u8),
            try_alloc_slice(strings, StrMeta::empty()),
            try_alloc_slice(strings, STR_NONE),
            try_alloc_slice(strings.div_ceil(16), 0u16),
        ) else {
            return false;
        };
        self.buf = buf;
        self.meta = meta;
        self.remap = remap;
        self.interned = interned;
        true
    }

    #[inline(never)]
    fn is_interned(&self, id: u16) -> bool {
        self.interned[id as usize / 16] & (1 << (id % 16)) != 0
    }

    #[inline(never)]
    fn set_interned(&mut self, id: u16, on: bool) {
        let word = &mut self.interned[id as usize / 16];
        let bit = 1 << (id % 16);
        if on {
            *word |= bit;
        } else {
            *word &= !bit;
        }
    }

    /// Pool buffer size in bytes.
    pub fn capacity_bytes(&self) -> usize {
        self.buf.len()
    }

    /// Maximum number of live strings.
    pub fn capacity_strings(&self) -> usize {
        self.meta.len()
    }

    /// Pool bytes in use (bump pointer — what `alloc` checks against).
    pub fn used_bytes(&self) -> usize {
        self.next as usize
    }

    /// String slots in use.
    pub fn used_strings(&self) -> usize {
        self.count as usize
    }

    /// Heap bytes held by the pool (buffer + per-string tables).
    pub fn heap_bytes(&self) -> usize {
        use core::mem::size_of;
        self.buf.len()
            + self.meta.len() * size_of::<StrMeta>()
            + self.remap.len() * size_of::<u16>()
            + self.interned.len() * size_of::<u16>()
    }

    /// Allocate a string from a byte slice. Returns string ID (u16) or None if full.
    pub fn alloc(&mut self, data: &[u8]) -> Option<u16> {
        let len = data.len();
        if self.count as usize >= self.meta.len() || self.next as usize + len > self.buf.len() {
            return None;
        }
        let buf = &mut self.buf;
        let id = self.count;
        let offset = self.next;
        buf[offset as usize..offset as usize + len].copy_from_slice(data);
        self.meta[id as usize] = StrMeta {
            offset,
            len: len as Off,
        };
        self.next += len as Off;
        self.count += 1;
        self.set_interned(id, false);
        Some(id as u16)
    }

    /// Allocate a string literal, or return the id of an identical literal
    /// that is still live. Without this every run of a `set(text, "…")` line
    /// took a new slot, and a handler without `strClear()` filled the pool.
    /// The result may be shared: `free` ignores it and `insert_byte` /
    /// `delete_byte` edit a private copy. `smart_clear` reclaims it as usual.
    pub fn intern(&mut self, data: &[u8]) -> Option<u16> {
        for id in 0..self.count {
            if self.is_interned(id) && self.get(id) == data {
                return Some(id);
            }
        }
        let id = self.alloc(data)?;
        self.set_interned(id, true);
        Some(id)
    }

    /// Id that can be edited in place: `id` itself, or a fresh private copy
    /// when `id` is an interned (possibly shared) literal.
    fn unshare(&mut self, id: u16) -> Option<u16> {
        if self.is_interned(id) {
            // concat with "nothing" = copy into a fresh, non-interned slot.
            self.concat(id, STR_NONE)
        } else {
            Some(id)
        }
    }

    /// Mark a string as freed (zero-length). Reclaimed on next smart_clear().
    /// Interned literals may be shared, so they are left alone.
    pub fn free(&mut self, id: u16) {
        if (id as usize) < self.count as usize && !self.is_interned(id) {
            self.meta[id as usize].len = 0;
        }
    }

    /// Get string bytes by ID.
    pub fn get(&self, id: u16) -> &[u8] {
        if id as usize >= self.count as usize {
            return &[];
        }
        let m = &self.meta[id as usize];
        let buf = &self.buf;
        &buf[m.offset as usize..m.offset as usize + m.len as usize]
    }

    /// Get string length by ID. Returns `u32` so it covers both metadata
    /// profiles (the large pool allows strings past 64 KB).
    pub fn len(&self, id: u16) -> u32 {
        if id as usize >= self.count as usize {
            return 0;
        }
        self.meta[id as usize].len as u32
    }

    /// Compare two strings lexicographically. Returns -1, 0, or 1.
    pub fn compare(&self, a: u16, b: u16) -> i32 {
        let a_data = self.get(a);
        let b_data = self.get(b);
        match a_data.cmp(b_data) {
            core::cmp::Ordering::Less => -1,
            core::cmp::Ordering::Equal => 0,
            core::cmp::Ordering::Greater => 1,
        }
    }

    /// Concatenate two strings. Returns new string ID or None if full.
    /// An id that names no string (e.g. `0xFFFF` from `get(text)` on an
    /// emptied input) counts as an empty string, as in `get` / `len`.
    pub fn concat(&mut self, a: u16, b: u16) -> Option<u16> {
        let meta = |id: u16| match self.meta.get(id as usize) {
            Some(&m) if (id as usize) < self.count as usize => m,
            _ => StrMeta::empty(),
        };
        let a_meta = meta(a);
        let b_meta = meta(b);
        let total = a_meta.len as usize + b_meta.len as usize;
        if self.count as usize >= self.meta.len() || self.next as usize + total > self.buf.len() {
            return None;
        }
        let buf = &mut self.buf;
        let id = self.count;
        let offset = self.next as usize;
        // Copy a (byte-by-byte — src and dst in the same buffer)
        let a_start = a_meta.offset as usize;
        let a_len = a_meta.len as usize;
        for i in 0..a_len {
            buf[offset + i] = buf[a_start + i];
        }
        // Copy b
        let b_start = b_meta.offset as usize;
        let b_len = b_meta.len as usize;
        for i in 0..b_len {
            buf[offset + a_len + i] = buf[b_start + i];
        }
        self.meta[id as usize] = StrMeta {
            offset: self.next,
            len: total as Off,
        };
        self.next += total as Off;
        self.count += 1;
        self.set_interned(id, false);
        Some(id as u16)
    }

    /// Reset the entire pool. All string IDs become invalid.
    pub fn clear(&mut self) {
        self.count = 0;
        self.next = 0;
    }

    /// Insert a byte at `pos` in string `id`. Copies to end of pool.
    /// Old space becomes a gap, reclaimed by smart_clear().
    /// Returns the id now holding the text — a new one when `id` was a shared
    /// literal, so the caller must store it — or None if nothing changed.
    pub fn insert_byte(&mut self, id: u16, pos: usize, byte: u8, max_len: u8) -> Option<u16> {
        if id as usize >= self.count as usize {
            return None;
        }
        // Enforce max_length (0 = no limit)
        if max_len > 0 && self.meta[id as usize].len as usize >= max_len as usize {
            return None;
        }
        let id = self.unshare(id)?;
        let m = self.meta[id as usize];
        let old_len = m.len as usize;
        let pos = pos.min(old_len);
        let new_len = old_len + 1;
        if self.next as usize + new_len > self.buf.len() {
            return None;
        }
        let buf = &mut self.buf;
        let src = m.offset as usize;
        let dst = self.next as usize;
        // Copy bytes before pos
        for i in 0..pos {
            buf[dst + i] = buf[src + i];
        }
        // Insert new byte
        buf[dst + pos] = byte;
        // Copy bytes after pos
        for i in pos..old_len {
            buf[dst + 1 + i] = buf[src + i];
        }
        self.meta[id as usize] = StrMeta {
            offset: self.next,
            len: new_len as Off,
        };
        self.next += new_len as Off;
        Some(id)
    }

    /// Delete byte at `pos` in string `id`. Copies to end of pool.
    /// Old space becomes a gap, reclaimed by smart_clear().
    /// Returns the id now holding the text (see `insert_byte`).
    pub fn delete_byte(&mut self, id: u16, pos: usize) -> Option<u16> {
        if id as usize >= self.count as usize {
            return None;
        }
        if pos >= self.meta[id as usize].len as usize {
            return None;
        }
        let id = self.unshare(id)?;
        let m = self.meta[id as usize];
        let old_len = m.len as usize;
        let new_len = old_len - 1;
        if new_len == 0 {
            self.meta[id as usize].len = 0;
            return Some(id);
        }
        if self.next as usize + new_len > self.buf.len() {
            return None;
        }
        let buf = &mut self.buf;
        let src = m.offset as usize;
        let dst = self.next as usize;
        // Copy bytes before pos
        for i in 0..pos {
            buf[dst + i] = buf[src + i];
        }
        // Copy bytes after pos (skip deleted byte)
        for i in (pos + 1)..old_len {
            buf[dst + i - 1] = buf[src + i];
        }
        self.meta[id as usize] = StrMeta {
            offset: self.next,
            len: new_len as Off,
        };
        self.next += new_len as Off;
        Some(id)
    }

    /// Smart clear: keep strings referenced by widget text_id fields,
    /// compact survivors to the front, remap widget references.
    /// Zero heap allocations — uses the preallocated `remap` scratch table.
    pub fn smart_clear(&mut self, tree: &mut WidgetTree) {
        if self.count == 0 {
            return;
        }

        let wcount = tree.count();

        // Phase 1: Build keep set from widget text_ids. Skip destroyed
        // widgets — their text references are about to be reclaimed.
        // `remap[id] == KEEP` marks a survivor; everything else is STR_NONE.
        const KEEP: u16 = 0;
        let old_count = self.count as usize;
        let remap = &mut self.remap;
        remap[..old_count].fill(STR_NONE);
        let mut any_kept = false;
        for i in 0..wcount {
            let wid = WidgetId(i as u16);
            if !tree.is_live(wid) {
                continue;
            }
            let text_id = tree.text_id(wid);
            if text_id != STR_NONE && (text_id as usize) < old_count {
                remap[text_id as usize] = KEEP;
                any_kept = true;
            }
        }

        // If nothing to keep, just clear
        if !any_kept {
            self.count = 0;
            self.next = 0;
            return;
        }

        // Phase 2: Compact survivors to front with new sequential IDs.
        // A survivor's remap entry is overwritten with its new id; dropped
        // strings (and empty survivors) go back to STR_NONE.
        let mut new_id: u16 = 0;
        let mut write_pos: Off = 0;

        for old_id in 0..old_count {
            if self.remap[old_id] != KEEP {
                continue;
            }
            let m = self.meta[old_id];
            if m.len == 0 {
                self.remap[old_id] = STR_NONE;
                continue;
            }
            let src = m.offset as usize;
            // Move bytes forward (src >= dst always since we compact left)
            self.buf.copy_within(src..src + m.len as usize, write_pos as usize);
            self.meta[new_id as usize] = StrMeta {
                offset: write_pos,
                len: m.len,
            };
            // Carry the interned bit. Ids only move down and are visited in
            // order, so `new_id`'s old bit was already consumed.
            let interned = self.is_interned(old_id as u16);
            self.set_interned(new_id, interned);
            self.remap[old_id] = new_id;
            write_pos += m.len;
            new_id += 1;
        }

        self.count = new_id;
        self.next = write_pos;

        // Phase 3: Remap widget text_id references (live widgets only).
        for i in 0..wcount {
            let wid = WidgetId(i as u16);
            if !tree.is_live(wid) {
                continue;
            }
            let text_id = tree.text_id(wid);
            if text_id != STR_NONE && (text_id as usize) < old_count {
                let new = self.remap[text_id as usize];
                if let Some(ext) = tree.ext_mut(wid) {
                    ext.text_id = new;
                }
            }
        }
    }
}

// === Formatting: i32 to string ===

/// Format an i32 into a byte buffer. Returns the number of bytes written.
/// Buffer must be at least 12 bytes (sign + 10 digits + null safety).
pub fn format_i32(val: i32, buf: &mut [u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }

    if val == 0 {
        buf[0] = b'0';
        return 1;
    }

    let mut pos = 0;
    let mut n = val;
    let negative = n < 0;

    if negative {
        buf[pos] = b'-';
        pos += 1;
        // Handle i32::MIN carefully
        if n == i32::MIN {
            let s = b"-2147483648";
            let len = s.len().min(buf.len());
            buf[..len].copy_from_slice(&s[..len]);
            return len;
        }
        n = -n;
    }

    // Count digits
    let mut tmp = n;
    let mut digits = 0u8;
    while tmp > 0 {
        digits += 1;
        tmp /= 10;
    }

    let end = pos + digits as usize;
    if end > buf.len() {
        return 0;
    }

    // Write digits right-to-left
    let mut i = end;
    let mut rem = n;
    while rem > 0 {
        i -= 1;
        buf[i] = b'0' + (rem % 10) as u8;
        rem /= 10;
    }

    end
}

/// Convert i32 to string in the pool. Returns string ID.
pub fn itos(pool: &mut StringPool, val: i32) -> Option<u16> {
    let mut tmp = [0u8; FMT_BUF_SIZE];
    let len = format_i32(val, &mut tmp);
    pool.alloc(&tmp[..len])
}

// === Formatting: f32 to string ===

/// Format an f32 into a byte buffer with fixed 2 decimal places.
/// Returns the number of bytes written.
pub fn format_f32(val: f32, buf: &mut [u8]) -> usize {
    if buf.len() < 8 {
        return 0;
    }

    // Handle special cases
    if val != val {
        // NaN
        let s = b"NaN";
        buf[..3].copy_from_slice(s);
        return 3;
    }
    if val == f32::INFINITY {
        let s = b"inf";
        buf[..3].copy_from_slice(s);
        return 3;
    }
    if val == f32::NEG_INFINITY {
        let s = b"-inf";
        buf[..4].copy_from_slice(s);
        return 4;
    }

    let mut pos = 0;
    let mut v = val;

    if v < 0.0 {
        buf[pos] = b'-';
        pos += 1;
        v = -v;
    }

    // Integer part
    let int_part = v as u32;
    let int_len = format_i32(int_part as i32, &mut buf[pos..]);
    pos += int_len;

    // Decimal point
    if pos >= buf.len() - 3 {
        return pos;
    }
    buf[pos] = b'.';
    pos += 1;

    // Fractional part (2 decimal places)
    let frac = v - (int_part as f32);
    let frac_100 = ((frac * 100.0) + 0.5) as u32; // round
    let d1 = (frac_100 / 10) % 10;
    let d2 = frac_100 % 10;
    buf[pos] = b'0' + d1 as u8;
    pos += 1;
    buf[pos] = b'0' + d2 as u8;
    pos += 1;

    pos
}

/// Convert f32 to string in the pool. Returns string ID.
pub fn ftos(pool: &mut StringPool, bits: u32) -> Option<u16> {
    let val = f32::from_bits(bits);
    let mut tmp = [0u8; FMT_BUF_SIZE];
    let len = format_f32(val, &mut tmp);
    pool.alloc(&tmp[..len])
}

// === Parsing: string to number ===

/// Parse a string (by ID) as i32. Returns 0 on failure.
pub fn parse_int(pool: &StringPool, id: u16) -> i32 {
    let data = pool.get(id);
    if data.is_empty() {
        return 0;
    }

    let mut pos = 0;
    let negative = data[0] == b'-';
    if negative || data[0] == b'+' {
        pos += 1;
    }

    // Skip 0x prefix
    let hex = pos + 1 < data.len()
        && data[pos] == b'0'
        && (data[pos + 1] == b'x' || data[pos + 1] == b'X');
    if hex {
        pos += 2;
    }

    let mut result: i32 = 0;
    while pos < data.len() {
        let ch = data[pos];
        let digit = if hex {
            match ch {
                b'0'..=b'9' => (ch - b'0') as i32,
                b'a'..=b'f' => (ch - b'a' + 10) as i32,
                b'A'..=b'F' => (ch - b'A' + 10) as i32,
                _ => break,
            }
        } else {
            if ch < b'0' || ch > b'9' {
                break;
            }
            (ch - b'0') as i32
        };
        result = result
            .wrapping_mul(if hex { 16 } else { 10 })
            .wrapping_add(digit);
        pos += 1;
    }

    if negative { -result } else { result }
}

// === Formatting helpers used by sprintf ===

/// Format u32 as decimal into buf. Returns bytes written.
fn format_u32(n: u32, buf: &mut [u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    if n == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 10];
    let mut pos = tmp.len();
    let mut rem = n;
    while rem > 0 {
        pos -= 1;
        tmp[pos] = b'0' + (rem % 10) as u8;
        rem /= 10;
    }
    let digits = &tmp[pos..];
    let count = digits.len().min(buf.len());
    buf[..count].copy_from_slice(&digits[..count]);
    count
}

/// Format u32 as hex into buf (lower or upper case). Returns bytes written.
fn format_hex(n: u32, upper: bool, buf: &mut [u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    if n == 0 {
        buf[0] = b'0';
        return 1;
    }
    let digits = if upper {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut tmp = [0u8; 8];
    let mut pos = tmp.len();
    let mut rem = n;
    while rem > 0 {
        pos -= 1;
        tmp[pos] = digits[(rem & 0xF) as usize];
        rem >>= 4;
    }
    let hex = &tmp[pos..];
    let count = hex.len().min(buf.len());
    buf[..count].copy_from_slice(&hex[..count]);
    count
}

/// Format f32 with configurable decimal places into buf. Returns bytes written.
fn format_f32_prec(val: f32, decimals: u8, buf: &mut [u8]) -> usize {
    if buf.len() < 4 {
        return 0;
    }
    if val != val {
        let s = b"NaN";
        let n = s.len().min(buf.len());
        buf[..n].copy_from_slice(&s[..n]);
        return n;
    }
    let mut pos = 0;
    let v = if val < 0.0 {
        if pos < buf.len() {
            buf[pos] = b'-';
            pos += 1;
        }
        -val
    } else {
        val
    };
    pos += format_u32(v as u32, &mut buf[pos..]);
    if decimals == 0 {
        return pos;
    }
    if pos >= buf.len() {
        return pos;
    }
    buf[pos] = b'.';
    pos += 1;
    let mut frac = v - (v as u32) as f32;
    for _ in 0..decimals {
        if pos >= buf.len() {
            break;
        }
        frac *= 10.0;
        let d = frac as u32 % 10;
        buf[pos] = b'0' + d as u8;
        pos += 1;
        frac -= d as f32;
    }
    pos
}

/// Write `digits` into `out` with optional zero/space padding to `width`.
/// Handles negative sign placement for zero-pad: `-00X` → `- 00digits`.
fn pad_write(digits: &[u8], width: u8, zero_pad: bool, out: &mut [u8]) -> usize {
    let len = digits.len();
    let w = width as usize;
    if w == 0 || len >= w {
        let n = len.min(out.len());
        out[..n].copy_from_slice(&digits[..n]);
        return n;
    }
    if w > out.len() {
        return 0;
    }
    let pad = w - len;
    if zero_pad && !digits.is_empty() && digits[0] == b'-' {
        out[0] = b'-';
        for k in 0..pad {
            out[1 + k] = b'0';
        }
        out[1 + pad..w].copy_from_slice(&digits[1..len]);
    } else {
        let pad_char = if zero_pad { b'0' } else { b' ' };
        for k in 0..pad {
            out[k] = pad_char;
        }
        out[pad..w].copy_from_slice(&digits[..len]);
    }
    w
}

/// Format a string using printf-style specifiers.
/// Supported: `%[0][width]d/i/u/x/X`, `%[.N]f`, `%s`, `%%`
/// `%s` args are str_ids resolved from pool. Output is capped at 128 bytes.
pub fn sprintf(pool: &mut StringPool, fmt: &[u8], args: &[i32]) -> Option<u16> {
    const OUT_SIZE: usize = 128;
    let mut out = [0u8; OUT_SIZE];
    let mut out_pos = 0;
    let mut arg_idx = 0;
    let mut i = 0;

    while i < fmt.len() && out_pos < OUT_SIZE {
        if fmt[i] != b'%' {
            out[out_pos] = fmt[i];
            out_pos += 1;
            i += 1;
            continue;
        }
        i += 1;
        if i >= fmt.len() {
            break;
        }

        // Optional zero-pad flag (consumed separately so '0' in width works)
        let mut zero_pad = false;
        if i < fmt.len() && fmt[i] == b'0' {
            zero_pad = true;
            i += 1;
        }
        // Optional width
        let mut width: u8 = 0;
        while i < fmt.len() && fmt[i] >= b'0' && fmt[i] <= b'9' {
            width = width.saturating_mul(10).saturating_add(fmt[i] - b'0');
            i += 1;
        }
        // Optional precision for floats
        let mut precision: Option<u8> = None;
        if i < fmt.len() && fmt[i] == b'.' {
            i += 1;
            let mut n = 0u8;
            while i < fmt.len() && fmt[i] >= b'0' && fmt[i] <= b'9' {
                n = n.saturating_mul(10).saturating_add(fmt[i] - b'0');
                i += 1;
            }
            precision = Some(n.min(9));
        }

        if i >= fmt.len() {
            break;
        }
        let spec = fmt[i];
        i += 1;

        match spec {
            b'd' | b'i' => {
                if arg_idx < args.len() {
                    let mut tmp = [0u8; 16];
                    let n = format_i32(args[arg_idx], &mut tmp);
                    out_pos += pad_write(&tmp[..n], width, zero_pad, &mut out[out_pos..]);
                    arg_idx += 1;
                }
            }
            b'u' => {
                if arg_idx < args.len() {
                    let mut tmp = [0u8; 12];
                    let n = format_u32(args[arg_idx] as u32, &mut tmp);
                    out_pos += pad_write(&tmp[..n], width, zero_pad, &mut out[out_pos..]);
                    arg_idx += 1;
                }
            }
            b'x' => {
                if arg_idx < args.len() {
                    let mut tmp = [0u8; 10];
                    let n = format_hex(args[arg_idx] as u32, false, &mut tmp);
                    out_pos += pad_write(&tmp[..n], width, zero_pad, &mut out[out_pos..]);
                    arg_idx += 1;
                }
            }
            b'X' => {
                if arg_idx < args.len() {
                    let mut tmp = [0u8; 10];
                    let n = format_hex(args[arg_idx] as u32, true, &mut tmp);
                    out_pos += pad_write(&tmp[..n], width, zero_pad, &mut out[out_pos..]);
                    arg_idx += 1;
                }
            }
            b'f' => {
                if arg_idx < args.len() {
                    let v = f32::from_bits(args[arg_idx] as u32);
                    out_pos += format_f32_prec(v, precision.unwrap_or(2), &mut out[out_pos..]);
                    arg_idx += 1;
                }
            }
            b's' => {
                if arg_idx < args.len() {
                    let str_id = args[arg_idx] as u16;
                    let mut tmp = [0u8; 64];
                    let copy_n = {
                        let s = pool.get(str_id);
                        let n = s.len().min(OUT_SIZE - out_pos).min(tmp.len());
                        tmp[..n].copy_from_slice(&s[..n]);
                        n
                    };
                    out[out_pos..out_pos + copy_n].copy_from_slice(&tmp[..copy_n]);
                    out_pos += copy_n;
                    arg_idx += 1;
                }
            }
            b'%' => {
                if out_pos < OUT_SIZE {
                    out[out_pos] = b'%';
                    out_pos += 1;
                }
            }
            _ => {}
        }
    }

    pool.alloc(&out[..out_pos])
}

/// Parse a string (by ID) as f32. Returns f32 bits. Returns 0.0 bits on failure.
pub fn parse_float(pool: &StringPool, id: u16) -> u32 {
    let data = pool.get(id);
    if data.is_empty() {
        return 0.0f32.to_bits();
    }

    let mut pos = 0;
    let negative = data[0] == b'-';
    if negative || data[0] == b'+' {
        pos += 1;
    }

    // Integer part
    let mut int_part: u32 = 0;
    while pos < data.len() && data[pos] >= b'0' && data[pos] <= b'9' {
        int_part = int_part
            .wrapping_mul(10)
            .wrapping_add((data[pos] - b'0') as u32);
        pos += 1;
    }

    // Fractional part
    let mut frac: f32 = 0.0;
    if pos < data.len() && data[pos] == b'.' {
        pos += 1;
        let mut divisor: f32 = 10.0;
        while pos < data.len() && data[pos] >= b'0' && data[pos] <= b'9' {
            frac += (data[pos] - b'0') as f32 / divisor;
            divisor *= 10.0;
            pos += 1;
        }
    }

    let mut result = int_part as f32 + frac;
    if negative {
        result = -result;
    }
    result.to_bits()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configure_sets_capacity_and_limits_alloc() {
        let mut pool = StringPool::new();
        assert_eq!(pool.capacity_bytes(), DEFAULT_POOL_SIZE);
        assert_eq!(pool.capacity_strings(), DEFAULT_MAX_STRINGS);

        assert!(pool.configure(8, 2));
        assert_eq!((pool.capacity_bytes(), pool.capacity_strings()), (8, 2));
        assert!(pool.alloc(b"abcd").is_some());
        assert!(pool.alloc(b"efghi").is_none(), "exceeds 8-byte buffer");
        assert!(pool.alloc(b"efgh").is_some());
        assert!(pool.alloc(b"").is_none(), "only 2 slots");

        // 0 = back to the defaults.
        assert!(pool.configure(0, 0));
        assert_eq!(pool.capacity_bytes(), DEFAULT_POOL_SIZE);
        assert_eq!(pool.capacity_strings(), DEFAULT_MAX_STRINGS);
    }

    #[test]
    fn smart_clear_keeps_and_remaps_widget_text() {
        let mut pool = StringPool::new();
        assert!(pool.configure(64, 4));
        let mut tree = WidgetTree::new();
        let w = tree.alloc().unwrap();

        let tmp = pool.alloc(b"temp").unwrap();
        let kept = pool.alloc(b"label").unwrap();
        assert_eq!((tmp, kept), (0, 1));
        tree.ensure_ext(w).unwrap().text_id = kept;

        pool.smart_clear(&mut tree);
        let new_id = tree.text_id(w);
        assert_eq!(new_id, 0, "survivor compacted to the front");
        assert_eq!(pool.get(new_id), b"label");
        // Freed capacity is reusable up to the configured slot count.
        assert!(pool.alloc(b"a").is_some());
        assert!(pool.alloc(b"b").is_some());
        assert!(pool.alloc(b"c").is_some());
        assert!(pool.alloc(b"d").is_none());
    }

    #[test]
    fn concat_with_an_emptied_input() {
        // text_input: backspacing the last char, then strClear() in on_change,
        // leaves the input with no text (0xFFFF); concat(s, get(text)) must
        // not fail with "string pool full".
        let mut pool = StringPool::new();
        let mut tree = WidgetTree::new();
        let input = tree.alloc().unwrap();
        let t = pool.alloc(b"a").unwrap();
        tree.ensure_ext(input).unwrap().text_id = t;
        assert_eq!(pool.delete_byte(t, 0), Some(t));
        pool.smart_clear(&mut tree);
        assert_eq!(tree.text_id(input), STR_NONE);

        let s = pool.alloc(b"Name: ").unwrap();
        let out = pool.concat(s, tree.text_id(input)).expect("concat with no text");
        assert_eq!(pool.get(out), b"Name: ");
        let both = pool.concat(STR_NONE, STR_NONE).unwrap();
        assert_eq!(pool.get(both), b"");
    }

    #[test]
    fn intern_reuses_identical_literals() {
        let mut pool = StringPool::new();
        assert!(pool.configure(64, 4));
        let a = pool.intern(b"Tapped: 0").unwrap();
        let b = pool.intern(b"Reset").unwrap();
        // widget_demo on_reset: the same literals on every tap, no strClear().
        for _ in 0..20 {
            assert_eq!(pool.intern(b"Tapped: 0"), Some(a));
            assert_eq!(pool.intern(b"Reset"), Some(b));
        }
        // A computed string with the same bytes is not shared.
        let c = pool.alloc(b"Reset").unwrap();
        assert_ne!(c, b);
        assert_eq!(pool.intern(b"Reset"), Some(b));
    }

    #[test]
    fn interned_literal_is_not_freed_or_edited_in_place() {
        let mut pool = StringPool::new();
        let lit = pool.intern(b"ab").unwrap();
        pool.free(lit);
        assert_eq!(pool.get(lit), b"ab", "free ignores shared literals");

        // Typing into an input that shows the literal edits a copy.
        let edited = pool.insert_byte(lit, 2, b'c', 0).unwrap();
        assert_ne!(edited, lit);
        assert_eq!(pool.get(edited), b"abc");
        assert_eq!(pool.get(lit), b"ab", "other users keep the literal");
        // The copy is private: later edits stay in place.
        assert_eq!(pool.delete_byte(edited, 0), Some(edited));
        assert_eq!(pool.get(edited), b"bc");

        let gone = pool.delete_byte(lit, 0).unwrap();
        assert_ne!(gone, lit);
        assert_eq!(pool.get(lit), b"ab");
    }

    #[test]
    fn smart_clear_keeps_interned_flag() {
        let mut pool = StringPool::new();
        let mut tree = WidgetTree::new();
        let w = tree.alloc().unwrap();
        let _tmp = pool.alloc(b"tmp").unwrap();
        let lit = pool.intern(b"label").unwrap();
        tree.ensure_ext(w).unwrap().text_id = lit;

        pool.smart_clear(&mut tree);
        let moved = tree.text_id(w);
        assert_eq!(moved, 0);
        assert_eq!(pool.intern(b"label"), Some(moved), "still found after compaction");
        // The slot it vacated is a plain string again.
        let plain = pool.alloc(b"x").unwrap();
        pool.free(plain);
        assert_eq!(pool.get(plain), b"");
    }
}
