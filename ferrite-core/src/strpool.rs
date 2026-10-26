//! VM strings, stored as `T_STRING` objects in the object pool.
//!
//! A string object's payload is one word — byte length (bits 0-29) plus the
//! interned flag (bit 31) — followed by the bytes. A string id is the object's
//! pointer, so it stays valid until the string is freed: by `str_free`, or by
//! the sweep at `strClear()` (`str_sweep`), which frees every string no live
//! widget shows. Edits (`str_insert_byte` / `str_delete_byte`) resize the
//! object in place and keep the id.
//!
//! The formatting / parsing helpers below work on any `ObjPool`.

use crate::pool::{ObjPool, T_STRING};
use crate::widget::WidgetId;

pub const STR_NONE: u16 = 0xFFFF;
const FMT_BUF_SIZE: usize = 32;
const INTERNED: u32 = 1 << 31;
/// `str_sweep` mark bit (lengths never reach it: an object holds < 256 KB).
const KEEP: u32 = 1 << 30;
const LEN_MASK: u32 = KEEP - 1;

impl ObjPool {
    /// Allocate a string. Returns its id, or None when the pool is full.
    pub fn str_alloc(&mut self, data: &[u8]) -> Option<u16> {
        self.str_new(data, false)
    }

    fn str_new(&mut self, data: &[u8], interned: bool) -> Option<u16> {
        let p = self.alloc_obj(T_STRING, 1 + data.len().div_ceil(4))?;
        self.str_bytes_mut(p)[..data.len()].copy_from_slice(data);
        self.payload_mut(p, T_STRING)?[0] = data.len() as u32 | if interned { INTERNED } else { 0 };
        Some(p)
    }

    /// Writable byte area of string `p` (its whole payload capacity).
    fn str_bytes_mut(&mut self, p: u16) -> &mut [u8] {
        match self.payload_mut(p, T_STRING) {
            // SAFETY: the words after the length word are plain storage;
            // viewing them as bytes is always valid.
            Some(w) => unsafe {
                core::slice::from_raw_parts_mut(w[1..].as_mut_ptr() as *mut u8, (w.len() - 1) * 4)
            },
            None => &mut [],
        }
    }

    fn str_interned(&self, id: u16) -> bool {
        self.payload(id, T_STRING).is_some_and(|w| w[0] & INTERNED != 0)
    }

    /// Allocate a string literal, or return the id of an identical literal
    /// that is still live. Without this every run of a `set(text, "…")` line
    /// took a new string, and a handler without `strClear()` filled the pool.
    /// The result may be shared: `str_free` ignores it and the byte edits
    /// work on a private copy.
    pub fn str_intern(&mut self, data: &[u8]) -> Option<u16> {
        let found = self.ptrs_of(T_STRING).find(|&p| self.str_interned(p) && self.str_get(p) == data);
        found.or_else(|| self.str_new(data, true))
    }

    /// Id that can be edited in place: `id` itself, or a fresh private copy
    /// when `id` is an interned (possibly shared) literal.
    fn str_unshare(&mut self, id: u16) -> Option<u16> {
        if self.str_interned(id) {
            // concat with "nothing" = copy into a fresh, non-interned string.
            self.str_concat(id, STR_NONE)
        } else {
            Some(id)
        }
    }

    /// Free a string. Interned literals may be shared, so they are left alone.
    pub fn str_free(&mut self, id: u16) {
        if !self.str_interned(id) {
            self.free_obj(id, T_STRING);
        }
    }

    /// String bytes by id (empty when `id` names no string).
    pub fn str_get(&self, id: u16) -> &[u8] {
        match self.payload(id, T_STRING) {
            Some(w) => {
                let len = (w[0] & LEN_MASK) as usize;
                // SAFETY: see `str_bytes_mut`; len ≤ (payload words - 1) × 4.
                unsafe { core::slice::from_raw_parts(w[1..].as_ptr() as *const u8, len) }
            }
            None => &[],
        }
    }

    /// String length by id (0 when `id` names no string).
    pub fn str_len(&self, id: u16) -> u32 {
        self.str_get(id).len() as u32
    }

    /// Compare two strings lexicographically. Returns -1, 0, or 1.
    pub fn str_compare(&self, a: u16, b: u16) -> i32 {
        match self.str_get(a).cmp(self.str_get(b)) {
            core::cmp::Ordering::Less => -1,
            core::cmp::Ordering::Equal => 0,
            core::cmp::Ordering::Greater => 1,
        }
    }

    /// Concatenate two strings into a new one. An id that names no string
    /// (e.g. `0xFFFF` from `get(text)` on an emptied input) counts as empty.
    pub fn str_concat(&mut self, a: u16, b: u16) -> Option<u16> {
        let (la, lb) = (self.str_get(a).len(), self.str_get(b).len());
        let p = self.alloc_obj(T_STRING, 1 + (la + lb).div_ceil(4))?;
        // Copy through a small buffer: the sources live in the same arena.
        let mut buf = [0u8; 64];
        let mut at = 0;
        for (src, len) in [(a, la), (b, lb)] {
            let mut done = 0;
            while done < len {
                let n = (len - done).min(buf.len());
                buf[..n].copy_from_slice(&self.str_get(src)[done..done + n]);
                self.str_bytes_mut(p)[at..at + n].copy_from_slice(&buf[..n]);
                done += n;
                at += n;
            }
        }
        self.payload_mut(p, T_STRING)?[0] = (la + lb) as u32;
        Some(p)
    }

    /// Resize string `id` to `len` bytes, keeping the prefix.
    fn str_resize(&mut self, id: u16, len: usize) -> bool {
        if !self.resize_obj(id, T_STRING, 1 + len.div_ceil(4)) {
            return false;
        }
        let w = self.payload_mut(id, T_STRING).unwrap();
        w[0] = (w[0] & INTERNED) | len as u32;
        true
    }

    /// Free every string.
    pub fn str_clear(&mut self) {
        for p in 0..self.ptr_count() as u16 {
            self.free_obj(p, T_STRING);
        }
    }

    /// Insert a byte at `pos` in string `id`. Returns the id now holding the
    /// text — a new one when `id` was a shared literal, so the caller must
    /// store it — or None if nothing changed.
    pub fn str_insert_byte(&mut self, id: u16, pos: usize, byte: u8, max_len: u8) -> Option<u16> {
        self.payload(id, T_STRING)?;
        let old_len = self.str_get(id).len();
        // Enforce max_length (0 = no limit)
        if max_len > 0 && old_len >= max_len as usize {
            return None;
        }
        let id = self.str_unshare(id)?;
        if !self.str_resize(id, old_len + 1) {
            return None;
        }
        let pos = pos.min(old_len);
        let bytes = self.str_bytes_mut(id);
        bytes.copy_within(pos..old_len, pos + 1);
        bytes[pos] = byte;
        Some(id)
    }

    /// Delete the byte at `pos` in string `id`. Returns the id now holding
    /// the text (see `str_insert_byte`).
    pub fn str_delete_byte(&mut self, id: u16, pos: usize) -> Option<u16> {
        let old_len = self.str_get(id).len();
        if pos >= old_len {
            return None;
        }
        let id = self.str_unshare(id)?;
        self.str_bytes_mut(id).copy_within(pos + 1..old_len, pos);
        self.str_resize(id, old_len - 1);
        Some(id)
    }

    /// `strClear()`: free every string that no live widget shows. Widget text
    /// that became empty is dropped too (the widget then has no text).
    pub fn str_sweep(&mut self) {
        // Pass 1: mark the strings live widgets show.
        for p in 0..self.ptr_count() as u16 {
            let w = WidgetId(p);
            if !self.is_live(w) {
                continue;
            }
            let t = self.text_id(w);
            if self.str_len(t) == 0 {
                if t != STR_NONE {
                    self.set_text_id(w, STR_NONE);
                }
                continue;
            }
            if let Some(s) = self.payload_mut(t, T_STRING) {
                s[0] |= KEEP;
            }
        }
        // Pass 2: free the unmarked strings, unmark the kept ones.
        for p in 0..self.ptr_count() as u16 {
            match self.payload_mut(p, T_STRING) {
                Some(w) if w[0] & KEEP != 0 => w[0] &= !KEEP,
                Some(_) => self.free_obj(p, T_STRING),
                None => {}
            }
        }
    }

    /// Live strings.
    pub fn str_count(&self) -> usize {
        self.live_objects(T_STRING)
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
pub fn itos(pool: &mut ObjPool, val: i32) -> Option<u16> {
    let mut tmp = [0u8; FMT_BUF_SIZE];
    let len = format_i32(val, &mut tmp);
    pool.str_alloc(&tmp[..len])
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
pub fn ftos(pool: &mut ObjPool, bits: u32) -> Option<u16> {
    let val = f32::from_bits(bits);
    let mut tmp = [0u8; FMT_BUF_SIZE];
    let len = format_f32(val, &mut tmp);
    pool.str_alloc(&tmp[..len])
}

// === Parsing: string to number ===

/// Parse a string (by ID) as i32. Returns 0 on failure.
pub fn parse_int(pool: &ObjPool, id: u16) -> i32 {
    let data = pool.str_get(id);
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
pub fn sprintf(pool: &mut ObjPool, fmt: &[u8], args: &[i32]) -> Option<u16> {
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
                        let s = pool.str_get(str_id);
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

    pool.str_alloc(&out[..out_pos])
}

/// Parse a string (by ID) as f32. Returns f32 bits. Returns 0.0 bits on failure.
pub fn parse_float(pool: &ObjPool, id: u16) -> u32 {
    let data = pool.str_get(id);
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
    fn strings_keep_ids_across_edits_and_compaction() {
        let mut pool = ObjPool::new();
        let a = pool.str_alloc(b"abc").unwrap();
        let gap = pool.str_alloc(b"0123456789").unwrap();
        let b = pool.str_alloc(b"xyz").unwrap();
        pool.str_free(gap);
        assert_eq!(pool.str_insert_byte(a, 3, b'd', 0), Some(a));
        assert_eq!(pool.str_insert_byte(a, 0, b'>', 0), Some(a));
        assert_eq!(pool.str_get(a), b">abcd");
        pool.compact();
        assert_eq!(pool.str_get(a), b">abcd");
        assert_eq!(pool.str_get(b), b"xyz");
        assert_eq!(pool.str_delete_byte(a, 0), Some(a));
        assert_eq!(pool.str_get(a), b"abcd");
        assert_eq!(pool.str_insert_byte(a, 9, b'!', 4), None, "max_length");
        assert_eq!(pool.str_get(gap), b"", "freed id reads empty");
    }

    #[test]
    fn sweep_keeps_widget_text() {
        let mut pool = ObjPool::new();
        let w = pool.alloc_widget().unwrap();
        let tmp = pool.str_alloc(b"temp").unwrap();
        let kept = pool.str_alloc(b"label").unwrap();
        pool.set_text_id(w, kept);

        pool.str_sweep();
        assert_eq!(pool.text_id(w), kept, "ids never change");
        assert_eq!(pool.str_get(kept), b"label");
        assert_eq!(pool.str_get(tmp), b"");
        assert_eq!(pool.str_count(), 1);
    }

    #[test]
    fn concat_with_an_emptied_input() {
        // text_input: backspacing the last char, then strClear() in on_change,
        // leaves the input with no text (0xFFFF); concat(s, get(text)) must
        // not fail with "pool full: string".
        let mut pool = ObjPool::new();
        let input = pool.alloc_widget().unwrap();
        let t = pool.str_alloc(b"a").unwrap();
        pool.set_text_id(input, t);
        assert_eq!(pool.str_delete_byte(t, 0), Some(t));
        pool.str_sweep();
        assert_eq!(pool.text_id(input), STR_NONE);

        let s = pool.str_alloc(b"Name: ").unwrap();
        let out = pool.str_concat(s, pool.text_id(input)).expect("concat with no text");
        assert_eq!(pool.str_get(out), b"Name: ");
        let both = pool.str_concat(STR_NONE, STR_NONE).unwrap();
        assert_eq!(pool.str_get(both), b"");
    }

    #[test]
    fn intern_reuses_identical_literals() {
        let mut pool = ObjPool::new();
        let a = pool.str_intern(b"Tapped: 0").unwrap();
        let b = pool.str_intern(b"Reset").unwrap();
        // widget_demo on_reset: the same literals on every tap, no strClear().
        for _ in 0..20 {
            assert_eq!(pool.str_intern(b"Tapped: 0"), Some(a));
            assert_eq!(pool.str_intern(b"Reset"), Some(b));
        }
        // A computed string with the same bytes is not shared.
        let c = pool.str_alloc(b"Reset").unwrap();
        assert_ne!(c, b);
        assert_eq!(pool.str_intern(b"Reset"), Some(b));
    }

    #[test]
    fn interned_literal_is_not_freed_or_edited_in_place() {
        let mut pool = ObjPool::new();
        let lit = pool.str_intern(b"ab").unwrap();
        pool.str_free(lit);
        assert_eq!(pool.str_get(lit), b"ab", "free ignores shared literals");

        // Typing into an input that shows the literal edits a copy.
        let edited = pool.str_insert_byte(lit, 2, b'c', 0).unwrap();
        assert_ne!(edited, lit);
        assert_eq!(pool.str_get(edited), b"abc");
        assert_eq!(pool.str_get(lit), b"ab", "other users keep the literal");
        // The copy is private: later edits stay in place.
        assert_eq!(pool.str_delete_byte(edited, 0), Some(edited));
        assert_eq!(pool.str_get(edited), b"bc");

        let gone = pool.str_delete_byte(lit, 0).unwrap();
        assert_ne!(gone, lit);
        assert_eq!(pool.str_get(lit), b"ab");
    }

    #[test]
    fn sweep_keeps_interned_flag() {
        let mut pool = ObjPool::new();
        let w = pool.alloc_widget().unwrap();
        let _tmp = pool.str_alloc(b"tmp").unwrap();
        let lit = pool.str_intern(b"label").unwrap();
        pool.set_text_id(w, lit);

        pool.str_sweep();
        assert_eq!(pool.str_intern(b"label"), Some(lit), "still found after the sweep");
        pool.compact();
        assert_eq!(pool.str_intern(b"label"), Some(lit), "and after compaction");
    }

    #[test]
    fn long_strings_and_formatting() {
        let mut pool = ObjPool::new();
        let long = [b'x'; 300];
        let id = pool.str_alloc(&long).unwrap();
        assert_eq!(pool.str_len(id), 300);
        let both = pool.str_concat(id, id).unwrap();
        assert_eq!(pool.str_len(both), 600);
        assert!(pool.str_get(both).iter().all(|&b| b == b'x'));
        let n = itos(&mut pool, -1234).unwrap();
        assert_eq!(parse_int(&pool, n), -1234);
    }
}
