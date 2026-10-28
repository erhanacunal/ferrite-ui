//! Minimal JSON lookups on an HTTP body, enough for weather / time APIs
//! without a JSON parser in the VM. No allocation, no validation: a malformed
//! document just yields `None`.
//!
//! A path is dot-separated: object keys, or array indices where the value is
//! an array — `"current.temperature_2m"`, `"daily.time.0"`. Each key is looked
//! up anywhere inside the current value (first match), so a unique key can be
//! given on its own.

/// Byte index of the first `needle` in `hay`.
pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The value at `path`: string contents without the quotes (escapes are not
/// decoded), or the raw text of a number / literal / object / array.
pub fn lookup<'a>(doc: &'a [u8], path: &[u8]) -> Option<&'a [u8]> {
    let mut span = (0, doc.len());
    for seg in path.split(|&c| c == b'.') {
        if seg.is_empty() {
            return None;
        }
        let start = skip_ws(doc, span.0);
        span = match (doc.get(start), parse_index(seg)) {
            (Some(b'['), Some(idx)) => array_elem(doc, start, idx)?,
            _ => {
                let colon = after_key(&doc[..span.1], span.0, seg)?;
                value_span(doc, skip_ws(doc, colon))?
            }
        };
    }
    let v = &doc[span.0..span.1];
    if v.len() >= 2 && v[0] == b'"' && v[v.len() - 1] == b'"' {
        Some(&v[1..v.len() - 1])
    } else {
        Some(v)
    }
}

/// Parse a decimal number (`-12.345`, no exponent) multiplied by `scale`,
/// truncated toward zero.
pub fn parse_scaled(v: &[u8], scale: i32) -> Option<i32> {
    let (neg, digits) = match v.first() {
        Some(b'-') => (true, &v[1..]),
        Some(b'+') => (false, &v[1..]),
        _ => (false, v),
    };
    let mut int: i64 = 0;
    let mut frac: i64 = 0;
    let mut frac_div: i64 = 1;
    let mut seen = false;
    let mut in_frac = false;
    for &c in digits {
        match c {
            b'0'..=b'9' => {
                seen = true;
                let d = (c - b'0') as i64;
                if in_frac {
                    // Beyond 9 digits the fraction no longer matters for an i32.
                    if frac_div < 1_000_000_000 {
                        frac = frac * 10 + d;
                        frac_div *= 10;
                    }
                } else {
                    int = int.checked_mul(10)?.checked_add(d)?;
                }
            }
            b'.' if !in_frac => in_frac = true,
            _ => return None,
        }
    }
    if !seen {
        return None;
    }
    let scale = scale as i64;
    let mut r = int.checked_mul(scale)?.checked_add(frac * scale / frac_div)?;
    if neg {
        r = -r;
    }
    i32::try_from(r).ok()
}

fn skip_ws(b: &[u8], mut p: usize) -> usize {
    while p < b.len() && matches!(b[p], b' ' | b'\t' | b'\r' | b'\n') {
        p += 1;
    }
    p
}

fn parse_index(seg: &[u8]) -> Option<usize> {
    if seg.is_empty() || seg.len() > 6 || !seg.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(seg.iter().fold(0, |n, &c| n * 10 + (c - b'0') as usize))
}

/// Position just after the `:` that follows `"key"`, searching from `from`.
fn after_key(b: &[u8], from: usize, key: &[u8]) -> Option<usize> {
    let mut p = from;
    while p < b.len() {
        let i = p + find(&b[p..], key)?;
        let end = i + key.len();
        if i > 0 && b[i - 1] == b'"' && b.get(end) == Some(&b'"') {
            let colon = skip_ws(b, end + 1);
            if b.get(colon) == Some(&b':') {
                return Some(colon + 1);
            }
        }
        p = i + 1;
    }
    None
}

/// `[start, end)` of the value starting at `p`.
fn value_span(b: &[u8], p: usize) -> Option<(usize, usize)> {
    match *b.get(p)? {
        b'"' => Some((p, string_end(b, p)?)),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut i = p;
            while i < b.len() {
                match b[i] {
                    b'"' => {
                        i = string_end(b, i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some((p, i + 1));
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            None
        }
        _ => {
            let mut i = p;
            while i < b.len() && !matches!(b[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n') {
                i += 1;
            }
            (i > p).then_some((p, i))
        }
    }
}

/// Index just past the closing quote of the string starting at `p`.
fn string_end(b: &[u8], p: usize) -> Option<usize> {
    let mut i = p + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Span of element `idx` of the array starting at `p` (`[`).
fn array_elem(b: &[u8], p: usize, idx: usize) -> Option<(usize, usize)> {
    let mut pos = p + 1;
    for i in 0..=idx {
        pos = skip_ws(b, pos);
        if b.get(pos) == Some(&b']') {
            return None;
        }
        let span = value_span(b, pos)?;
        if i == idx {
            return Some(span);
        }
        pos = skip_ws(b, span.1);
        if b.get(pos) != Some(&b',') {
            return None;
        }
        pos += 1;
    }
    None
}
