/// Adafruit GFX compatible bitmap font renderer — sparse (UTF-8) variant
///
/// Two sources:
///   1. Flash: combined resource via load/load_by_id
///   2. Embedded: codepoints + glyph table + bitmap in ROM via from_embedded
///
/// Flash resource layouts:
///
///   V1 (1bpp monochrome, legacy):
///     [0..1]         num_glyphs: u16 LE
///     [2]            y_advance: u8
///     [3]            font_id: u8
///     [4..4+N*2]     codepoints[] (N × u16 LE, sorted ascending)
///     [4+N*2..4+N*9] glyphs[] (N × 7 bytes)
///     [4+N*9..]      bitmap data (1-bit packed, MSB first)
///
///   V2 (2bpp anti-aliased):
///     [0..1]         magic: 0x00, 0x02
///     [2..3]         num_glyphs: u16 LE
///     [4]            y_advance: u8
///     [5]            font_id: u8
///     [6]            bpp: u8  (2 = 4-level AA)
///     [7..7+N*2]     codepoints[] (N × u16 LE, sorted ascending)
///     [7+N*2..7+N*9] glyphs[] (N × 7 bytes)
///     [7+N*9..]      bitmap data (2bpp packed, MSB first; 4 pixels per byte;
///                    each pixel = 2 bits: 0=0%, 1=33%, 2=67%, 3=100%)
///
/// GfxGlyph (7 bytes, packed):
///   bitmapOffset: u16, width: u8, height: u8,
///   xAdvance: u8, xOffset: i8, yOffset: i8
extern crate alloc;

use crate::fs::{Fs, RES_FONT};
use alloc::vec::Vec;
use crate::flash::{FlashBackend, FlashImpl};
use crate::lcd::{LcdBackend, LcdImpl, lerp_rgb565};

// Re-exported from ferrite-core — the framework's canonical glyph type.
pub use crate::gfx_glyph::GfxGlyph;

/// Glyph bitmap read buffer size
const BITMAP_BUF_SIZE: usize = 128;

/// Embedded font ID (reserved, user fonts cannot use this)
pub const EMBEDDED_FONT_ID: u8 = 0;

/// Glyph + codepoint storage — ROM reference (embedded) or heap-allocated (flash-loaded).
enum GlyphData {
    Static {
        glyphs: &'static [GfxGlyph],
        codepoints: &'static [u16],
    },
    Heap {
        glyphs: Vec<GfxGlyph>,
        codepoints: Vec<u16>,
    },
}

impl GlyphData {
    fn codepoints(&self) -> &[u16] {
        match self {
            GlyphData::Static { codepoints, .. } => codepoints,
            GlyphData::Heap { codepoints, .. } => codepoints,
        }
    }

    fn glyph(&self, index: usize) -> &GfxGlyph {
        match self {
            GlyphData::Static { glyphs, .. } => &glyphs[index],
            GlyphData::Heap { glyphs, .. } => &glyphs[index],
        }
    }
}

/// Font — sparse format, binary search on sorted codepoints[].
pub struct Font {
    pub font_id: u8,
    /// Bits per pixel: 1 = monochrome, 2 = 2bpp anti-aliased
    pub bpp: u8,
    y_advance: u8,
    glyphs: GlyphData,
    /// Absolute flash address of bitmap data
    bitmap_addr: u32,
    /// Embedded bitmap data pointer (ROM). Null means flash is used.
    embedded_bitmap: *const u8,
}

// Font only holds a const pointer — Send/Sync safe (single core, no ISR access)
unsafe impl Send for Font {}
unsafe impl Sync for Font {}

impl Font {
    fn empty() -> Self {
        Font {
            font_id: 0xFF,
            bpp: 1,
            y_advance: 0,
            glyphs: GlyphData::Heap {
                glyphs: Vec::new(),
                codepoints: Vec::new(),
            },
            bitmap_addr: 0,
            embedded_bitmap: core::ptr::null(),
        }
    }

    /// Load font from flash by resource name.
    pub fn load<F: FlashBackend>(fs: &Fs, flash: &FlashImpl<F>, name: &[u8]) -> Option<Self> {
        let entry = fs.find(flash, name)?;
        Self::load_from_entry(flash, &entry)
    }

    /// Load font from a ResourceEntry (detects V1/V2 format).
    pub fn load_from_entry<F: FlashBackend>(
        flash: &FlashImpl<F>,
        entry: &crate::fs::ResourceEntry,
    ) -> Option<Self> {
        if entry.size < 4 {
            return None;
        }

        // Read first 7 bytes (max header size); V1 only uses first 4.
        let mut hdr = [0u8; 7];
        let read_len = 7.min(entry.size as usize);
        flash.read(entry.offset, &mut hdr[..read_len]);

        // Detect V2: magic bytes [0x00, 0x02] at positions 0-1.
        // A V1 font with 512 glyphs (0x0200 LE) would collide, but this is
        // impossible on 128 KB flash (512×9B table = 4608B alone).
        let (num_glyphs, y_advance, font_id, bpp, hdr_len): (usize, u8, u8, u8, u32) =
            if hdr[0] == 0x00 && hdr[1] == 0x02 && entry.size >= 7 {
                // V2: 7-byte header
                let n = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
                (n, hdr[4], hdr[5], hdr[6], 7)
            } else {
                // V1: 4-byte header (bpp=1 legacy)
                let n = u16::from_le_bytes([hdr[0], hdr[1]]) as usize;
                (n, hdr[2], hdr[3], 1, 4)
            };

        let cp_bytes = (num_glyphs * 2) as u32;
        let gl_bytes = (num_glyphs * 7) as u32;

        if entry.size < hdr_len + cp_bytes + gl_bytes {
            return None;
        }

        let cp_base = entry.offset + hdr_len;
        let mut codepoints = Vec::with_capacity(num_glyphs);
        for i in 0..num_glyphs {
            let mut buf = [0u8; 2];
            flash.read(cp_base + (i as u32 * 2), &mut buf);
            codepoints.push(u16::from_le_bytes(buf));
        }

        let gl_base = cp_base + cp_bytes;
        let mut glyphs = Vec::with_capacity(num_glyphs);
        for i in 0..num_glyphs {
            let mut g = [0u8; 7];
            flash.read(gl_base + (i as u32 * 7), &mut g);
            glyphs.push(GfxGlyph {
                bitmap_offset: u16::from_le_bytes([g[0], g[1]]),
                width: g[2],
                height: g[3],
                x_advance: g[4],
                x_offset: g[5] as i8,
                y_offset: g[6] as i8,
            });
        }

        let bitmap_addr = gl_base + gl_bytes;

        Some(Font {
            font_id,
            bpp,
            y_advance,
            glyphs: GlyphData::Heap { glyphs, codepoints },
            bitmap_addr,
            embedded_bitmap: core::ptr::null(),
        })
    }

    /// Search flash for a font with the given font_id and load it.
    /// Returns None if font_id is 0 (embedded) or not found in flash.
    pub fn load_by_id<F: FlashBackend>(fs: &Fs, flash: &FlashImpl<F>, font_id: u8) -> Option<Self> {
        if font_id == EMBEDDED_FONT_ID {
            return None;
        }
        let count = fs.count_by_kind(flash, RES_FONT);
        for i in 0..count {
            if let Some(entry) = fs.find_nth_by_kind(flash, RES_FONT, i) {
                if entry.size < 4 {
                    continue;
                }
                // Read enough bytes to check font_id in both V1 and V2 layouts
                let mut hdr = [0u8; 7];
                let read_len = 7.min(entry.size as usize);
                flash.read(entry.offset, &mut hdr[..read_len]);
                let id = if hdr[0] == 0x00 && hdr[1] == 0x02 && entry.size >= 7 {
                    hdr[5] // V2: font_id at byte 5
                } else {
                    hdr[3] // V1: font_id at byte 3
                };
                if id == font_id {
                    return Self::load_from_entry(flash, &entry);
                }
            }
        }
        None
    }

    /// Create an embedded (ROM) font. Always gets font_id = 0.
    /// Glyphs and codepoints reference ROM directly — zero RAM cost.
    pub fn from_embedded(
        glyphs: &'static [GfxGlyph],
        codepoints: &'static [u16],
        bitmap: &'static [u8],
        y_advance: u8,
        bpp: u8,
    ) -> Self {
        Font {
            font_id: EMBEDDED_FONT_ID,
            bpp,
            y_advance,
            glyphs: GlyphData::Static { glyphs, codepoints },
            bitmap_addr: 0,
            embedded_bitmap: bitmap.as_ptr(),
        }
    }

    /// Line height in pixels
    pub fn line_height(&self) -> u8 {
        self.y_advance
    }

    /// Binary search for glyph by Unicode BMP codepoint.
    fn find_glyph(&self, cp: u16) -> Option<&GfxGlyph> {
        match self.glyphs.codepoints().binary_search(&cp) {
            Ok(idx) => Some(self.glyphs.glyph(idx)),
            Err(_) => None,
        }
    }

    /// Advance width for a BMP codepoint (0 if not in font).
    pub fn char_width_cp(&self, cp: u16) -> u8 {
        self.find_glyph(cp).map(|g| g.x_advance).unwrap_or(0)
    }

    /// Advance width for a char (0 if out of BMP or not in font).
    pub fn char_width(&self, ch: char) -> u8 {
        let cp = ch as u32;
        if cp > 0xFFFF {
            return 0;
        }
        self.char_width_cp(cp as u16)
    }

    /// Calculate string width in pixels (text is UTF-8 encoded).
    pub fn text_width(&self, text: &[u8]) -> u16 {
        let mut w: u16 = 0;
        let mut pos = 0;
        while let Some(cp) = utf8_next(text, &mut pos) {
            if cp != 0xFFFF {
                w = w.saturating_add(self.char_width_cp(cp) as u16);
            }
        }
        w
    }

    /// Draw a single Unicode character.
    pub fn draw_char<L: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<L>,
        flash: &FlashImpl<F>,
        ch: char,
        cx: i16,
        cy: i16,
        fg: u16,
        bg: Option<u16>,
    ) -> u8 {
        let cp = ch as u32;
        if cp > 0xFFFF {
            return 0;
        }
        self.draw_cp(lcd, flash, cp as u16, cx, cy, fg, bg)
    }

    /// Draw a UTF-8 encoded string. Returns the X position after the last glyph.
    pub fn draw_str<L: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<L>,
        flash: &FlashImpl<F>,
        text: &[u8],
        mut x: i16,
        y: i16,
        fg: u16,
        bg: Option<u16>,
    ) -> i16 {
        let mut pos = 0;
        while let Some(cp) = utf8_next(text, &mut pos) {
            if cp != 0xFFFF {
                x += self.draw_cp(lcd, flash, cp, x, y, fg, bg) as i16;
            }
        }
        x
    }

    // --- Internal: draw one BMP codepoint ---

    fn draw_cp<L: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<L>,
        flash: &FlashImpl<F>,
        cp: u16,
        cx: i16,
        cy: i16,
        fg: u16,
        bg: Option<u16>,
    ) -> u8 {
        let glyph = match self.find_glyph(cp) {
            Some(g) => g,
            None => return 0,
        };

        if glyph.width == 0 || glyph.height == 0 {
            return glyph.x_advance;
        }

        let draw_x = cx + glyph.x_offset as i16;
        let draw_y = cy + glyph.y_offset as i16;

        if draw_x + glyph.width as i16 <= 0
            || draw_y + glyph.height as i16 <= 0
            || draw_x >= 2048
            || draw_y >= 2048
        {
            return glyph.x_advance;
        }

        let bitmap_offset = glyph.bitmap_offset;
        let width = glyph.width;
        let height = glyph.height;
        let x_advance = glyph.x_advance;

        if self.bpp == 1 {
            // 1bpp: traditional opaque / transparent paths
            if let Some(bg_color) = bg {
                let total_bits = width as u32 * height as u32;
                let total_bytes = (total_bits + 7) / 8;
                self.draw_opaque(
                    lcd,
                    flash,
                    total_bytes,
                    bitmap_offset,
                    width,
                    height,
                    draw_x,
                    draw_y,
                    fg,
                    bg_color,
                );
            } else {
                let total_bits = width as u32 * height as u32;
                let total_bytes = (total_bits + 7) / 8;
                self.draw_transparent(
                    lcd,
                    flash,
                    total_bytes,
                    bitmap_offset,
                    width,
                    height,
                    draw_x,
                    draw_y,
                    fg,
                );
            }
        } else {
            // 2bpp: always blend fg with bg via begin_pixels stream.
            // bg=None falls back to black — callers should always pass a
            // known background for correct anti-aliased rendering.
            let bg_color = bg.unwrap_or(0);
            let total_pixels = width as u32 * height as u32;
            let total_bytes = (total_pixels + 3) / 4;
            self.draw_2bpp(
                lcd,
                flash,
                total_bytes,
                bitmap_offset,
                width,
                height,
                draw_x,
                draw_y,
                fg,
                bg_color,
            );
        }

        x_advance
    }

    // --- Bitmap reading (flash or ROM) ---

    fn read_bitmap<F: FlashBackend>(
        &self,
        flash: &FlashImpl<F>,
        glyph_offset: u16,
        byte_offset: u32,
        buf: &mut [u8],
    ) {
        let addr = glyph_offset as u32 + byte_offset;
        if !self.embedded_bitmap.is_null() {
            unsafe {
                let src = self.embedded_bitmap.add(addr as usize);
                for i in 0..buf.len() {
                    buf[i] = *src.add(i);
                }
            }
        } else {
            flash.read(self.bitmap_addr + addr, buf);
        }
    }

    // --- 1bpp draw implementations (unchanged) ---

    fn draw_opaque<L: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<L>,
        flash: &FlashImpl<F>,
        total_bytes: u32,
        bitmap_offset: u16,
        width: u8,
        height: u8,
        draw_x: i16,
        draw_y: i16,
        fg: u16,
        bg: u16,
    ) {
        lcd.begin_pixels(draw_x as u16, draw_y as u16, width as u16, height as u16);

        let total_bits = width as u32 * height as u32;
        let mut bit_idx: u32 = 0;
        let mut bytes_read: u32 = 0;

        while bytes_read < total_bytes {
            let chunk = (total_bytes - bytes_read).min(BITMAP_BUF_SIZE as u32) as usize;
            let mut buf = [0u8; BITMAP_BUF_SIZE];
            self.read_bitmap(flash, bitmap_offset, bytes_read, &mut buf[..chunk]);

            for i in 0..chunk {
                let byte = buf[i];
                for bit in (0..8).rev() {
                    if bit_idx >= total_bits {
                        return;
                    }
                    if byte & (1 << bit) != 0 {
                        lcd.write_pixel(fg);
                    } else {
                        lcd.write_pixel(bg);
                    }
                    bit_idx += 1;
                }
            }

            bytes_read += chunk as u32;
        }
    }

    fn draw_transparent<L: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<L>,
        flash: &FlashImpl<F>,
        total_bytes: u32,
        bitmap_offset: u16,
        width: u8,
        height: u8,
        draw_x: i16,
        draw_y: i16,
        fg: u16,
    ) {
        let w = width as u32;
        let total_bits = w * height as u32;
        let mut bit_idx: u32 = 0;
        let mut bytes_read: u32 = 0;

        while bytes_read < total_bytes {
            let chunk = (total_bytes - bytes_read).min(BITMAP_BUF_SIZE as u32) as usize;
            let mut buf = [0u8; BITMAP_BUF_SIZE];
            self.read_bitmap(flash, bitmap_offset, bytes_read, &mut buf[..chunk]);

            for i in 0..chunk {
                let byte = buf[i];
                for bit in (0..8).rev() {
                    if bit_idx >= total_bits {
                        return;
                    }
                    if byte & (1 << bit) != 0 {
                        let px = draw_x + (bit_idx % w) as i16;
                        let py = draw_y + (bit_idx / w) as i16;
                        lcd.fill_rect(px as u16, py as u16, 1, 1, fg);
                    }
                    bit_idx += 1;
                }
            }

            bytes_read += chunk as u32;
        }
    }

    // --- 2bpp anti-aliased draw ---

    fn draw_2bpp<L: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<L>,
        flash: &FlashImpl<F>,
        total_bytes: u32,
        bitmap_offset: u16,
        width: u8,
        height: u8,
        draw_x: i16,
        draw_y: i16,
        fg: u16,
        bg: u16,
    ) {
        let total_pixels = width as u32 * height as u32;

        lcd.begin_pixels(draw_x as u16, draw_y as u16, width as u16, height as u16);

        let mut pixel_idx: u32 = 0;
        let mut bytes_read: u32 = 0;

        while bytes_read < total_bytes {
            let chunk = (total_bytes - bytes_read).min(BITMAP_BUF_SIZE as u32) as usize;
            let mut buf = [0u8; BITMAP_BUF_SIZE];
            self.read_bitmap(flash, bitmap_offset, bytes_read, &mut buf[..chunk]);

            for i in 0..chunk {
                let byte = buf[i];
                // 4 pixels per byte, MSB first: bits [7:6]=p0, [5:4]=p1, [3:2]=p2, [1:0]=p3
                for shift in [6u8, 4, 2, 0] {
                    if pixel_idx >= total_pixels {
                        return;
                    }
                    let alpha = (byte >> shift) & 0x03;
                    let color = lerp_rgb565(bg, fg, alpha as u16, 3);
                    lcd.write_pixel(color);
                    pixel_idx += 1;
                }
            }

            bytes_read += chunk as u32;
        }
    }
}

// ============================================================
// UTF-8 decoder
// ============================================================

/// Decode one UTF-8 codepoint from `bytes` starting at `*pos`.
///
/// Advances `*pos` past the consumed bytes and returns `Some(codepoint)`.
/// Returns `Some(0xFFFF)` for invalid sequences (pos advanced past the bad byte).
/// Returns `None` when `pos >= bytes.len()` (end of input).
///
/// Covers U+0000..U+FFFF (BMP). 4-byte sequences (U+10000+) return 0xFFFF.
pub fn utf8_next(bytes: &[u8], pos: &mut usize) -> Option<u16> {
    if *pos >= bytes.len() {
        return None;
    }
    let b0 = bytes[*pos];
    *pos += 1;

    // 1-byte ASCII
    if b0 & 0x80 == 0 {
        return Some(b0 as u16);
    }

    // 2-byte: 110xxxxx 10xxxxxx
    if b0 & 0xE0 == 0xC0 {
        if *pos >= bytes.len() || bytes[*pos] & 0xC0 != 0x80 {
            return Some(0xFFFF);
        }
        let b1 = bytes[*pos];
        *pos += 1;
        return Some(((b0 as u16 & 0x1F) << 6) | (b1 as u16 & 0x3F));
    }

    // 3-byte: 1110xxxx 10xxxxxx 10xxxxxx
    if b0 & 0xF0 == 0xE0 {
        if *pos >= bytes.len() || bytes[*pos] & 0xC0 != 0x80 {
            return Some(0xFFFF);
        }
        let b1 = bytes[*pos];
        *pos += 1;
        if *pos >= bytes.len() || bytes[*pos] & 0xC0 != 0x80 {
            return Some(0xFFFF);
        }
        let b2 = bytes[*pos];
        *pos += 1;
        return Some(((b0 as u16 & 0x0F) << 12) | ((b1 as u16 & 0x3F) << 6) | (b2 as u16 & 0x3F));
    }

    // 4-byte (out of BMP): consume continuation bytes, return sentinel
    if b0 & 0xF8 == 0xF0 {
        while *pos < bytes.len() && bytes[*pos] & 0xC0 == 0x80 {
            *pos += 1;
        }
    }

    Some(0xFFFF)
}

// ============================================================
// FontList — application-wide font registry
// ============================================================

/// Application-wide font list. Heap-allocated, grows on demand.
pub struct FontList {
    fonts: Vec<Font>,
}

impl FontList {
    pub fn new() -> Self {
        Self { fonts: Vec::new() }
    }

    /// Add a font to the list.
    pub fn add(&mut self, font: Font) -> bool {
        self.fonts.push(font);
        true
    }

    /// Find a loaded font by font_id.
    pub fn find(&self, font_id: u8) -> Option<&Font> {
        self.fonts.iter().find(|f| f.font_id == font_id)
    }

    /// Find a font by font_id. If not already loaded, search flash and load it.
    pub fn find_or_load<F: FlashBackend>(
        &mut self,
        font_id: u8,
        fs: &Fs,
        flash: &FlashImpl<F>,
    ) -> bool {
        if self.find(font_id).is_some() {
            return true;
        }
        if let Some(font) = Font::load_by_id(fs, flash, font_id) {
            return self.add(font);
        }
        false
    }

    /// Get the embedded font (font_id = 0). Panics if not added.
    pub fn embedded(&self) -> &Font {
        &self.fonts[0]
    }

    /// Resolve a font_id: try exact match, fall back to embedded (id=0).
    pub fn resolve(&self, font_id: u8) -> Option<&Font> {
        self.find(font_id).or_else(|| self.find(EMBEDDED_FONT_ID))
    }
}
