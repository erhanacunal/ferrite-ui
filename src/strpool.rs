/// Static string pool for VM string operations.
///
/// Buffer lives in BSS (static RAM) — zero heap allocations, no fragmentation.
/// Metadata (offsets, lengths) lives in StringPool struct inside Ctx (heap via Box<Ctx>).
/// Compacting smart_clear preserves widget text while reclaiming temporaries.
///
/// RAM cost: 2048 bytes BSS (pool buffer) + ~132 bytes in Ctx (metadata)

use crate::widget::{WidgetId, WidgetTree};

const POOL_SIZE: usize = 2048;
const MAX_STRINGS: usize = 64;
const STR_NONE: u16 = 0xFFFF;
const FMT_BUF_SIZE: usize = 32;

/// Pool buffer in BSS — static RAM, not heap. Single-threaded bare-metal.
static mut POOL_BUF: [u8; POOL_SIZE] = [0u8; POOL_SIZE];

#[inline]
fn pool_buf() -> &'static mut [u8; POOL_SIZE] {
    unsafe { &mut *core::ptr::addr_of_mut!(POOL_BUF) }
}

#[derive(Clone, Copy)]
struct StrMeta {
    offset: u16,
    len: u16,
}

impl StrMeta {
    const fn empty() -> Self {
        Self { offset: 0, len: 0 }
    }
}

pub struct StringPool {
    meta: [StrMeta; MAX_STRINGS],
    count: u8,
    next: u16,
}

impl StringPool {
    pub fn new() -> Self {
        Self {
            meta: [StrMeta::empty(); MAX_STRINGS],
            count: 0,
            next: 0,
        }
    }

    /// Allocate a string from a byte slice. Returns string ID (u16) or None if full.
    pub fn alloc(&mut self, data: &[u8]) -> Option<u16> {
        let len = data.len();
        if self.count as usize >= MAX_STRINGS || self.next as usize + len > POOL_SIZE {
            return None;
        }
        let buf = pool_buf();
        let id = self.count;
        let offset = self.next;
        buf[offset as usize..offset as usize + len].copy_from_slice(data);
        self.meta[id as usize] = StrMeta {
            offset,
            len: len as u16,
        };
        self.next += len as u16;
        self.count += 1;
        Some(id as u16)
    }

    /// Mark a string as freed (zero-length). Reclaimed on next smart_clear().
    pub fn free(&mut self, id: u16) {
        if (id as usize) < self.count as usize {
            self.meta[id as usize].len = 0;
        }
    }

    /// Get string bytes by ID.
    pub fn get(&self, id: u16) -> &[u8] {
        if id as usize >= self.count as usize {
            return &[];
        }
        let m = &self.meta[id as usize];
        let buf = pool_buf();
        &buf[m.offset as usize..m.offset as usize + m.len as usize]
    }

    /// Get string length by ID.
    pub fn len(&self, id: u16) -> u16 {
        if id as usize >= self.count as usize {
            return 0;
        }
        self.meta[id as usize].len
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
    pub fn concat(&mut self, a: u16, b: u16) -> Option<u16> {
        if a as usize >= self.count as usize || b as usize >= self.count as usize {
            return None;
        }
        let a_meta = self.meta[a as usize];
        let b_meta = self.meta[b as usize];
        let total = a_meta.len as usize + b_meta.len as usize;
        if self.count as usize >= MAX_STRINGS || self.next as usize + total > POOL_SIZE {
            return None;
        }
        let buf = pool_buf();
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
            len: total as u16,
        };
        self.next += total as u16;
        self.count += 1;
        Some(id as u16)
    }

    /// Reset the entire pool. All string IDs become invalid.
    pub fn clear(&mut self) {
        self.count = 0;
        self.next = 0;
    }

    /// Insert a byte at `pos` in string `id`. Copies to end of pool.
    /// Old space becomes a gap, reclaimed by smart_clear().
    pub fn insert_byte(&mut self, id: u16, pos: usize, byte: u8, max_len: u8) -> bool {
        if id as usize >= self.count as usize {
            return false;
        }
        let m = self.meta[id as usize];
        let old_len = m.len as usize;
        // Enforce max_length (0 = no limit)
        if max_len > 0 && old_len >= max_len as usize {
            return false;
        }
        let pos = pos.min(old_len);
        let new_len = old_len + 1;
        if self.next as usize + new_len > POOL_SIZE {
            return false;
        }
        let buf = pool_buf();
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
            len: new_len as u16,
        };
        self.next += new_len as u16;
        true
    }

    /// Delete byte at `pos` in string `id`. Copies to end of pool.
    /// Old space becomes a gap, reclaimed by smart_clear().
    pub fn delete_byte(&mut self, id: u16, pos: usize) -> bool {
        if id as usize >= self.count as usize {
            return false;
        }
        let m = self.meta[id as usize];
        let old_len = m.len as usize;
        if pos >= old_len || old_len == 0 {
            return false;
        }
        let new_len = old_len - 1;
        if new_len == 0 {
            self.meta[id as usize].len = 0;
            return true;
        }
        if self.next as usize + new_len > POOL_SIZE {
            return false;
        }
        let buf = pool_buf();
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
            len: new_len as u16,
        };
        self.next += new_len as u16;
        true
    }

    /// Smart clear: keep strings referenced by widget text_id fields,
    /// compact survivors to the front, remap widget references.
    /// Zero heap allocations — uses u32 bitmask + iterates widget array directly.
    pub fn smart_clear(&mut self, tree: &mut WidgetTree) {
        if self.count == 0 {
            return;
        }

        let wcount = tree.count();

        // Phase 1: Build keep bitmask from widget text_ids. Skip destroyed
        // widgets — their text references are about to be reclaimed.
        let mut keep: u64 = 0;
        for i in 0..wcount {
            let wid = WidgetId(i as u16);
            if !tree.is_live(wid) {
                continue;
            }
            let text_id = tree.text_id(wid);
            if text_id != STR_NONE && (text_id as usize) < MAX_STRINGS {
                keep |= 1u64 << text_id;
            }
        }

        // If nothing to keep, just clear
        if keep == 0 {
            self.clear();
            return;
        }

        // Phase 2: Compact survivors to front with new sequential IDs
        let buf = pool_buf();
        let old_count = self.count as usize;
        let mut remap = [STR_NONE; MAX_STRINGS];
        let mut new_id: u8 = 0;
        let mut write_pos: u16 = 0;

        for old_id in 0..old_count {
            if keep & (1u64 << old_id) == 0 {
                continue;
            }
            let m = self.meta[old_id];
            if m.len == 0 {
                continue;
            }
            let src = m.offset as usize;
            let len = m.len as usize;
            let dst = write_pos as usize;
            // Move bytes forward (src >= dst always since we compact left)
            if dst != src {
                for i in 0..len {
                    buf[dst + i] = buf[src + i];
                }
            }
            self.meta[new_id as usize] = StrMeta {
                offset: write_pos,
                len: m.len,
            };
            remap[old_id] = new_id as u16;
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
                let new = remap[text_id as usize];
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
    let hex = pos + 1 < data.len() && data[pos] == b'0' && (data[pos + 1] == b'x' || data[pos + 1] == b'X');
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
        result = result.wrapping_mul(if hex { 16 } else { 10 }).wrapping_add(digit);
        pos += 1;
    }

    if negative { -result } else { result }
}

// === Formatting helpers used by sprintf ===

/// Format u32 as decimal into buf. Returns bytes written.
fn format_u32(n: u32, buf: &mut [u8]) -> usize {
    if buf.is_empty() { return 0; }
    if n == 0 { buf[0] = b'0'; return 1; }
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
    if buf.is_empty() { return 0; }
    if n == 0 { buf[0] = b'0'; return 1; }
    let digits = if upper { b"0123456789ABCDEF" } else { b"0123456789abcdef" };
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
    if buf.len() < 4 { return 0; }
    if val != val {
        let s = b"NaN";
        let n = s.len().min(buf.len());
        buf[..n].copy_from_slice(&s[..n]);
        return n;
    }
    let mut pos = 0;
    let v = if val < 0.0 {
        if pos < buf.len() { buf[pos] = b'-'; pos += 1; }
        -val
    } else {
        val
    };
    pos += format_u32(v as u32, &mut buf[pos..]);
    if decimals == 0 { return pos; }
    if pos >= buf.len() { return pos; }
    buf[pos] = b'.';
    pos += 1;
    let mut frac = v - (v as u32) as f32;
    for _ in 0..decimals {
        if pos >= buf.len() { break; }
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
    if w > out.len() { return 0; }
    let pad = w - len;
    if zero_pad && !digits.is_empty() && digits[0] == b'-' {
        out[0] = b'-';
        for k in 0..pad { out[1 + k] = b'0'; }
        out[1 + pad..w].copy_from_slice(&digits[1..len]);
    } else {
        let pad_char = if zero_pad { b'0' } else { b' ' };
        for k in 0..pad { out[k] = pad_char; }
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
        if i >= fmt.len() { break; }

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

        if i >= fmt.len() { break; }
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
                if out_pos < OUT_SIZE { out[out_pos] = b'%'; out_pos += 1; }
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
        int_part = int_part.wrapping_mul(10).wrapping_add((data[pos] - b'0') as u32);
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
