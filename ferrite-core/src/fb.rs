//! Helpers for BSPs whose LCD backend draws into a CPU-addressed frame buffer
//! (n32903, tdo_y13): burst fills and fast RGB565 / ARGB8888 blends.

/// Fill `dst` with `val`.
///
/// On ARM the bulk is written 32 bytes per loop with two 4-register `STM`s.
/// The ARM926 D-cache does not allocate on write, so frame-buffer stores go
/// straight to the write buffer; multi-word bursts use the SDRAM bus far
/// better than one `STR` per word (which is what a plain loop compiles to).
#[inline]
pub fn fill_u32(dst: &mut [u32], val: u32) {
    #[cfg(target_arch = "arm")]
    {
        let blocks = dst.len() / 8;
        let mut p = dst.as_mut_ptr();
        if blocks > 0 {
            // SAFETY: writes exactly `blocks * 8` words starting at `p`, all
            // inside `dst`; `p` comes back advanced past them.
            unsafe {
                core::arch::asm!(
                    "2:",
                    "stmia r0!, {{r2, r3, r12, lr}}",
                    "stmia r0!, {{r2, r3, r12, lr}}",
                    "subs r1, r1, #1",
                    "bne 2b",
                    inout("r0") p,
                    inout("r1") blocks => _,
                    in("r2") val,
                    in("r3") val,
                    in("r12") val,
                    in("lr") val,
                    options(nostack),
                );
            }
        }
        let done = blocks * 8;
        let _ = p;
        dst[done..].fill(val);
    }
    #[cfg(not(target_arch = "arm"))]
    dst.fill(val);
}

/// Fill `dst` with the RGB565 `color`: one halfword to reach word alignment,
/// then pairs of pixels through [`fill_u32`].
#[inline]
pub fn fill_u16(dst: &mut [u16], color: u16) {
    let mut dst = dst;
    if !dst.is_empty() && (dst.as_ptr() as usize) & 2 != 0 {
        dst[0] = color;
        dst = &mut dst[1..];
    }
    let pairs = dst.len() / 2;
    if pairs > 0 {
        // SAFETY: `dst` is non-empty, so its pointer is the (now 4-byte
        // aligned) address of `dst[0]`, and it holds `pairs` whole words.
        let words = unsafe { core::slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<u32>(), pairs) };
        fill_u32(words, (color as u32) | ((color as u32) << 16));
    }
    if dst.len() & 1 != 0 {
        dst[dst.len() - 1] = color;
    }
}

/// Write cursor for the `begin_pixels` / `write_pixel` stream on a frame
/// buffer: walks a `w`-wide window row by row, reporting which pixels land on
/// the panel. Coordinates are signed (callers pass negative `i16`s cast to
/// `u16`), so a window hanging off the left/top edge clips correctly instead
/// of wrapping.
#[derive(Clone, Copy)]
pub struct StreamCursor {
    x: i32,
    y: i32,
    start_x: i32,
    end_x: i32,
}

impl StreamCursor {
    pub const fn new() -> Self {
        Self { x: 0, y: 0, start_x: 0, end_x: 0 }
    }

    /// Start a `w`-wide window at (x, y) (`u16`s reinterpreted as `i16`).
    #[inline]
    pub fn begin(&mut self, x: u16, y: u16, w: u16) {
        let x = x as i16 as i32;
        self.x = x;
        self.y = y as i16 as i32;
        self.start_x = x;
        self.end_x = x + w as i32;
    }

    /// Advance one pixel; returns its position when it is on a
    /// `width × height` panel.
    #[inline(always)]
    pub fn step(&mut self, width: usize, height: usize) -> Option<(usize, usize)> {
        let (x, y) = (self.x, self.y);
        self.x += 1;
        if self.x >= self.end_x {
            self.x = self.start_x;
            self.y += 1;
        }
        if (x as u32) < width as u32 && (y as u32) < height as u32 {
            Some((x as usize, y as usize))
        } else {
            None
        }
    }

    /// Advance `n` pixels, calling `span(x, y, len, skip)` for each on-panel
    /// run: `len` pixels at (x, y), which are pixels `skip..skip + len` of the
    /// `n` consumed.
    #[inline]
    pub fn advance(
        &mut self,
        n: usize,
        width: usize,
        height: usize,
        mut span: impl FnMut(usize, usize, usize, usize),
    ) {
        if self.end_x <= self.start_x {
            return; // zero-width window
        }
        let mut done = 0usize;
        while done < n {
            if self.y >= height as i32 {
                return; // past the bottom: nothing further is visible
            }
            let take = ((self.end_x - self.x) as usize).min(n - done);
            if self.y >= 0 {
                // Visible part of [x, x + take) on this row.
                let x0 = self.x.max(0);
                let x1 = (self.x + take as i32).min(width as i32);
                if x1 > x0 {
                    let skip = done + (x0 - self.x) as usize;
                    span(x0 as usize, self.y as usize, (x1 - x0) as usize, skip);
                }
            }
            done += take;
            self.x += take as i32;
            if self.x >= self.end_x {
                self.x = self.start_x;
                self.y += 1;
            }
        }
    }
}

impl Default for StreamCursor {
    fn default() -> Self {
        Self::new()
    }
}

/// Alpha `0..=255` → the `0..=32` weight [`blend_rgb565`] takes.
#[inline]
pub fn alpha5(alpha: u8) -> u32 {
    (alpha as u32 + 4) >> 3
}

/// Blend RGB565 `src` over `dst` with weight `a` in `0..=32` (see [`alpha5`]).
///
/// The three channels are spread into one word (`G` in the high half,
/// `R`/`B` in the low half, with headroom between them) so a single multiply
/// blends them all.
#[inline]
pub fn blend_rgb565(dst: u16, src: u16, a: u32) -> u16 {
    const MASK: u32 = 0x07E0_F81F;
    let s = ((src as u32) | ((src as u32) << 16)) & MASK;
    let d = ((dst as u32) | ((dst as u32) << 16)) & MASK;
    // d + (s - d) * a / 32 per channel, written as (s*a + d*(32-a)) / 32 so
    // every intermediate stays positive (each field has 5 bits headroom).
    let r = ((s * a + d * (32 - a)) >> 5) & MASK;
    (r | (r >> 16)) as u16
}

/// Alpha `0..=255` → the `0..=256` weight [`blend_argb8888`] takes.
#[inline]
pub fn alpha8(alpha: u8) -> u32 {
    alpha as u32 + (alpha as u32 >> 7)
}

/// Blend opaque ARGB8888 `src` over `dst` with weight `a` in `0..=256`
/// (see [`alpha8`]); two multiplies per channel pair (R+B, then G). The
/// result is opaque.
#[inline]
pub fn blend_argb8888(dst: u32, src: u32, a: u32) -> u32 {
    let na = 256 - a;
    let rb = (((src & 0x00FF_00FF) * a + (dst & 0x00FF_00FF) * na) >> 8) & 0x00FF_00FF;
    let g = (((src & 0x0000_FF00) * a + (dst & 0x0000_FF00) * na) >> 8) & 0x0000_FF00;
    0xFF00_0000 | rb | g
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference: exact per-channel `(s*a + d*(255-a)) / 255`.
    fn ref_channel(s: u32, d: u32, a: u32) -> u32 {
        (s * a + d * (255 - a) + 127) / 255
    }

    #[test]
    fn fill_handles_all_lengths_and_alignments() {
        let mut buf = [0u16; 64];
        for start in 0..4 {
            for len in 0..40 {
                buf.fill(0);
                fill_u16(&mut buf[start..start + len], 0xABCD);
                for (i, &v) in buf.iter().enumerate() {
                    let inside = i >= start && i < start + len;
                    assert_eq!(v, if inside { 0xABCD } else { 0 }, "start={start} len={len} i={i}");
                }
            }
        }
    }

    /// Reference: per-pixel `step` over a small panel.
    fn stream_ref(x: i16, y: i16, w: u16, n: usize) -> Vec<(usize, usize, usize)> {
        let mut c = StreamCursor::new();
        c.begin(x as u16, y as u16, w);
        (0..n).filter_map(|i| c.step(10, 6).map(|(px, py)| (px, py, i))).collect()
    }

    #[test]
    fn stream_advance_matches_step() {
        for x in [-12i16, -3, 0, 4, 9, 10, 30] {
            for y in [-4i16, -1, 0, 3, 5, 6] {
                for w in [0u16, 1, 3, 10, 14] {
                    for chunks in [1usize, 2, 5, 13] {
                        let n = 60;
                        let want = if w == 0 { Vec::new() } else { stream_ref(x, y, w, n) };
                        let mut got = Vec::new();
                        let mut c = StreamCursor::new();
                        c.begin(x as u16, y as u16, w);
                        let mut done = 0;
                        while done < n {
                            let k = chunks.min(n - done);
                            c.advance(k, 10, 6, |px, py, len, skip| {
                                for j in 0..len {
                                    got.push((px + j, py, done + skip + j));
                                }
                            });
                            done += k;
                        }
                        assert_eq!(got, want, "x={x} y={y} w={w} chunks={chunks}");
                    }
                }
            }
        }
    }

    #[test]
    fn rgb565_blend_endpoints_and_error() {
        let colors = [0x0000u16, 0xFFFF, 0xF800, 0x07E0, 0x001F, 0x1234, 0xBEEF];
        for &s in &colors {
            for &d in &colors {
                assert_eq!(blend_rgb565(d, s, alpha5(255)), s);
                assert_eq!(blend_rgb565(d, s, alpha5(0)), d);
                for alpha in [32u8, 64, 128, 200] {
                    let got = blend_rgb565(d, s, alpha5(alpha));
                    for (shift, mask) in [(11, 0x1F), (5, 0x3F), (0, 0x1F)] {
                        let g = ((got >> shift) & mask) as i32;
                        let want = ref_channel(
                            ((s >> shift) & mask) as u32,
                            ((d >> shift) & mask) as u32,
                            alpha as u32,
                        ) as i32;
                        // 5-bit alpha: within ~4% of the channel range.
                        assert!((g - want).abs() <= 1 + mask as i32 / 20, "s={s:#x} d={d:#x} a={alpha}");
                    }
                }
            }
        }
    }

    #[test]
    fn argb_blend_endpoints_and_error() {
        let colors = [0xFF00_0000u32, 0xFFFF_FFFF, 0xFFF8_0000, 0xFF00_FC00, 0xFF12_3456];
        for &s in &colors {
            for &d in &colors {
                assert_eq!(blend_argb8888(d, s, alpha8(255)), s);
                assert_eq!(blend_argb8888(d, s, alpha8(0)), d);
                for alpha in 1u8..255 {
                    let got = blend_argb8888(d, s, alpha8(alpha));
                    for shift in [16, 8, 0] {
                        let g = ((got >> shift) & 0xFF) as i32;
                        let want = ref_channel((s >> shift) & 0xFF, (d >> shift) & 0xFF, alpha as u32) as i32;
                        assert!((g - want).abs() <= 1, "s={s:#x} d={d:#x} a={alpha}");
                    }
                }
            }
        }
    }
}
