/// Protobuf binary encoding/decoding — ortak rutinler.
///
/// Wire types, property IDs, varint/zigzag codec.
/// VM, Builder ve Widget serialize/deserialize tarafından kullanılır.

// --- Wire types ---

pub const WT_VARINT: u8 = 0; // Signed/unsigned varint
pub const WT_I16: u8 = 1;    // Fixed 16-bit (2 byte LE)
pub const WT_LEN: u8 = 2;    // Length-delimited (varint len + payload)
pub const WT_NO_ARG: u8 = 5; // No argument

// --- Widget Property IDs ---

// Scalar properties (varint wire type)
pub const PROP_LOC_X: u8 = 0x01;
pub const PROP_LOC_Y: u8 = 0x02;
pub const PROP_SIZE_W: u8 = 0x03;
pub const PROP_SIZE_H: u8 = 0x04;
pub const PROP_VISIBLE: u8 = 0x05;
pub const PROP_ENABLED: u8 = 0x06;
pub const PROP_CLICKABLE: u8 = 0x07;
pub const PROP_BG_COLOR: u8 = 0x08;
pub const PROP_BORDER_COLOR: u8 = 0x09;
pub const PROP_FLAGS: u8 = 0x0A;
pub const PROP_PARENT: u8 = 0x0B;
pub const PROP_FIRST_CHILD: u8 = 0x0C;
pub const PROP_NEXT_SIBLING: u8 = 0x0D;
pub const PROP_MARGIN_T: u8 = 0x10;
pub const PROP_MARGIN_R: u8 = 0x11;
pub const PROP_MARGIN_B: u8 = 0x12;
pub const PROP_MARGIN_L: u8 = 0x13;
pub const PROP_BORDER_T: u8 = 0x14;
pub const PROP_BORDER_R: u8 = 0x15;
pub const PROP_BORDER_B: u8 = 0x16;
pub const PROP_BORDER_L: u8 = 0x17;
pub const PROP_PADDING_T: u8 = 0x18;
pub const PROP_PADDING_R: u8 = 0x19;
pub const PROP_PADDING_B: u8 = 0x1A;
pub const PROP_PADDING_L: u8 = 0x1B;

// Type-specific scalar properties
pub const PROP_KIND: u8 = 0x0E;
pub const PROP_TEXT_COLOR: u8 = 0x0F;
pub const PROP_FONT_ID: u8 = 0x1C;
pub const PROP_TEXT_ALIGN: u8 = 0x1D;
pub const PROP_PRESS_COLOR: u8 = 0x1E;
pub const PROP_IMAGE_ID: u8 = 0x1F;
pub const PROP_ON_CLICK: u8 = 0x20;
pub const PROP_ON_PAINT: u8 = 0x21;
pub const PROP_ON_TAP: u8 = 0x22;
pub const PROP_BORDER_RADIUS: u8 = 0x23;
pub const PROP_VALUE: u8 = 0x24;
pub const PROP_CHECKED: u8 = 0x25;

// Compound properties (LEN wire type — packed zigzag varints)
pub const PROP_LOCATION: u8 = 0x40;
pub const PROP_SIZE: u8 = 0x41;
pub const PROP_MARGIN: u8 = 0x42;
pub const PROP_BORDER_EDGES: u8 = 0x43;
pub const PROP_PADDING: u8 = 0x44;
pub const PROP_TEXT: u8 = 0x45; // LEN: UTF-8 text bytes

// === Varint encoding/decoding (protobuf uyumlu) ===

/// Unsigned varint decode. Dönüş: (değer, okunan byte).
pub fn decode_varint(buf: &[u8], pos: usize) -> Option<(u32, usize)> {
    let mut result: u32 = 0;
    let mut shift: u32 = 0;
    let mut i = pos;

    loop {
        if i >= buf.len() {
            return None;
        }
        let byte = buf[i];
        result |= ((byte & 0x7F) as u32) << shift;
        i += 1;
        if byte & 0x80 == 0 {
            return Some((result, i - pos));
        }
        shift += 7;
        if shift >= 35 {
            return None;
        }
    }
}

/// Signed (zigzag) varint decode. Dönüş: (değer, okunan byte).
pub fn decode_signed_varint(buf: &[u8], pos: usize) -> Option<(i32, usize)> {
    let (raw, consumed) = decode_varint(buf, pos)?;
    Some((zigzag_decode(raw), consumed))
}

/// Unsigned varint encode. Dönüş: yazılan byte.
pub fn encode_varint(buf: &mut [u8], pos: usize, mut val: u32) -> usize {
    let mut i = pos;
    loop {
        if i >= buf.len() {
            return i - pos;
        }
        if val < 0x80 {
            buf[i] = val as u8;
            return i - pos + 1;
        }
        buf[i] = (val as u8 & 0x7F) | 0x80;
        val >>= 7;
        i += 1;
    }
}

/// Signed (zigzag) varint encode. Dönüş: yazılan byte.
pub fn encode_signed_varint(buf: &mut [u8], pos: usize, val: i32) -> usize {
    encode_varint(buf, pos, zigzag_encode(val))
}

/// ZigZag decode: 0→0, 1→-1, 2→1, 3→-2, ...
pub fn zigzag_decode(n: u32) -> i32 {
    ((n >> 1) as i32) ^ -((n & 1) as i32)
}

/// ZigZag encode: 0→0, -1→1, 1→2, -2→3, ...
pub fn zigzag_encode(n: i32) -> u32 {
    ((n << 1) ^ (n >> 31)) as u32
}

// === Tag encoding/decoding ===

/// Protobuf tag yaz: (field << 3) | wire_type. Dönüş: yazılan byte.
#[inline]
pub fn write_tag(buf: &mut [u8], pos: usize, field: u8, wt: u8) -> usize {
    let tag = ((field as u32) << 3) | (wt as u32);
    encode_varint(buf, pos, tag)
}

/// Protobuf tag oku. Dönüş: (field_number, wire_type, okunan byte).
#[inline]
pub fn read_tag(buf: &[u8], pos: usize) -> Option<(u8, u8, usize)> {
    let (tag, consumed) = decode_varint(buf, pos)?;
    Some(((tag >> 3) as u8, (tag & 7) as u8, consumed))
}

// === Field-level helpers ===

/// Unsigned varint field yaz: tag + varint. Dönüş: yazılan byte.
#[inline]
pub fn write_uvarint_field(buf: &mut [u8], pos: usize, field: u8, val: u32) -> usize {
    let n = write_tag(buf, pos, field, WT_VARINT);
    n + encode_varint(buf, pos + n, val)
}

/// Signed varint field yaz: tag + zigzag varint. Dönüş: yazılan byte.
#[inline]
pub fn write_svarint_field(buf: &mut [u8], pos: usize, field: u8, val: i32) -> usize {
    let n = write_tag(buf, pos, field, WT_VARINT);
    n + encode_signed_varint(buf, pos + n, val)
}

/// Packed signed varints LEN field olarak yaz. Dönüş: yazılan byte.
pub fn write_packed_field(buf: &mut [u8], pos: usize, field: u8, values: &[i32]) -> usize {
    // Önce değerleri geçici buffer'a encode et (uzunluk hesapla)
    let mut tmp = [0u8; 20]; // max 4 değer × 5 byte
    let mut plen = 0;
    for &v in values {
        plen += encode_signed_varint(&mut tmp, plen, v);
    }

    let mut n = write_tag(buf, pos, field, WT_LEN);
    n += encode_varint(buf, pos + n, plen as u32);
    if pos + n + plen <= buf.len() {
        buf[pos + n..pos + n + plen].copy_from_slice(&tmp[..plen]);
    }
    n + plen
}

/// LEN payload'dan signed varint'leri decode et (max 4).
/// Dönüş: (değerler, sayı).
pub fn unpack_signed_varints(buf: &[u8]) -> ([i32; 4], usize) {
    let mut vals = [0i32; 4];
    let mut count = 0;
    let mut pos = 0;
    while pos < buf.len() && count < 4 {
        if let Some((v, consumed)) = decode_signed_varint(buf, pos) {
            vals[count] = v;
            count += 1;
            pos += consumed;
        } else {
            break;
        }
    }
    (vals, count)
}

/// Bilinmeyen field'ı atla. Dönüş: atlanan byte sayısı.
pub fn skip_field(buf: &[u8], pos: usize, wt: u8) -> Option<usize> {
    match wt {
        WT_VARINT => {
            let (_, consumed) = decode_varint(buf, pos)?;
            Some(consumed)
        }
        WT_I16 => {
            if pos + 2 <= buf.len() {
                Some(2)
            } else {
                None
            }
        }
        WT_LEN => {
            let (len, consumed) = decode_varint(buf, pos)?;
            let total = consumed + len as usize;
            if pos + total <= buf.len() {
                Some(total)
            } else {
                None
            }
        }
        WT_NO_ARG => Some(0),
        _ => None,
    }
}
