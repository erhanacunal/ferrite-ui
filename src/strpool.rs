/// Static string pool for VM string operations.
///
/// Global static (no allocator) — same pattern as USART ring buffer.
/// Single-threaded bare-metal: safe to access via unsafe global.
///
/// Pool: 2048 byte buffer, 16 string slots, append-only.
/// Strings are immutable once created. `clear()` resets the entire pool.
///
/// RAM cost: 2048 + 16×4 + 4 = ~2.1 KB

const POOL_SIZE: usize = 2048;
const MAX_STRINGS: usize = 16;

/// Temporary formatting buffer (shared, not re-entrant — fine for single-threaded VM)
const FMT_BUF_SIZE: usize = 32;

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
    buf: [u8; POOL_SIZE],
    meta: [StrMeta; MAX_STRINGS],
    count: u8,
    next: u16,
}

static mut POOL: StringPool = StringPool::new();

impl StringPool {
    pub const fn new() -> Self {
        Self {
            buf: [0; POOL_SIZE],
            meta: [StrMeta::empty(); MAX_STRINGS],
            count: 0,
            next: 0,
        }
    }

    /// Allocate a string from a byte slice. Returns string ID or None if full.
    pub fn alloc(&mut self, data: &[u8]) -> Option<u8> {
        let len = data.len();
        if self.count as usize >= MAX_STRINGS || self.next as usize + len > POOL_SIZE {
            return None;
        }
        let id = self.count;
        let offset = self.next;
        self.buf[offset as usize..offset as usize + len].copy_from_slice(data);
        self.meta[id as usize] = StrMeta {
            offset,
            len: len as u16,
        };
        self.next += len as u16;
        self.count += 1;
        Some(id)
    }

    /// Get string bytes by ID.
    pub fn get(&self, id: u8) -> &[u8] {
        if id >= self.count {
            return &[];
        }
        let m = &self.meta[id as usize];
        &self.buf[m.offset as usize..m.offset as usize + m.len as usize]
    }

    /// Get string length.
    pub fn len(&self, id: u8) -> u16 {
        if id >= self.count {
            return 0;
        }
        self.meta[id as usize].len
    }

    /// Concatenate two strings. Returns new string ID or None if full.
    pub fn concat(&mut self, a: u8, b: u8) -> Option<u8> {
        if a >= self.count || b >= self.count {
            return None;
        }
        let a_meta = self.meta[a as usize];
        let b_meta = self.meta[b as usize];
        let total = a_meta.len as usize + b_meta.len as usize;
        if self.count as usize >= MAX_STRINGS || self.next as usize + total > POOL_SIZE {
            return None;
        }
        let id = self.count;
        let offset = self.next;
        // Copy a
        let a_start = a_meta.offset as usize;
        let a_len = a_meta.len as usize;
        for i in 0..a_len {
            self.buf[offset as usize + i] = self.buf[a_start + i];
        }
        // Copy b
        let b_start = b_meta.offset as usize;
        let b_len = b_meta.len as usize;
        for i in 0..b_len {
            self.buf[offset as usize + a_len + i] = self.buf[b_start + i];
        }
        self.meta[id as usize] = StrMeta {
            offset,
            len: total as u16,
        };
        self.next += total as u16;
        self.count += 1;
        Some(id)
    }

    /// Reset the entire pool. All string IDs become invalid.
    pub fn clear(&mut self) {
        self.count = 0;
        self.next = 0;
    }
}

// === Global access ===

/// Get mutable reference to the global string pool.
/// Safety: single-threaded bare-metal, no interrupts access this.
#[inline]
pub fn pool() -> &'static mut StringPool {
    unsafe { &mut *(&raw mut POOL) }
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

/// Convert i32 to string in the global pool. Returns string ID or None.
pub fn itos(val: i32) -> Option<u8> {
    let mut tmp = [0u8; FMT_BUF_SIZE];
    let len = format_i32(val, &mut tmp);
    pool().alloc(&tmp[..len])
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

/// Convert f32 to string in the global pool. Returns string ID or None.
pub fn ftos(bits: u32) -> Option<u8> {
    let val = f32::from_bits(bits);
    let mut tmp = [0u8; FMT_BUF_SIZE];
    let len = format_f32(val, &mut tmp);
    pool().alloc(&tmp[..len])
}

// === Parsing: string to number ===

/// Parse a string (by ID) as i32. Returns 0 on failure.
pub fn parse_int(id: u8) -> i32 {
    let data = pool().get(id);
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

/// Parse a string (by ID) as f32. Returns f32 bits. Returns 0.0 bits on failure.
pub fn parse_float(id: u8) -> u32 {
    let data = pool().get(id);
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
