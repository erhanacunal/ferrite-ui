/// Adafruit GFX compatible bitmap font renderer
///
/// Two sources:
///   1. Flash: combined resource (header + glyph table + bitmap) via load/load_by_id
///   2. Embedded: glyph table + bitmap in ROM via from_embedded
///
/// Flash resource layout (combined, single RES_FONT):
///   [0..2]  first: u16 LE (first character code)
///   [2..4]  last: u16 LE (last character code)
///   [4]     y_advance: u8 (line height)
///   [5]     font_id: u8 (unique ID, 0 = reserved for embedded)
///   [6..]   glyph table (7 bytes per glyph)
///   [6+N*7..] bitmap data (1-bit packed, MSB first)
///
/// GfxGlyph (7 bytes, packed):
///   bitmapOffset: u16, width: u8, height: u8,
///   xAdvance: u8, xOffset: i8, yOffset: i8

use crate::flash::Flash;
use crate::fs::{Fs, RES_FONT};
use crate::lcd::Lcd;

/// Per-glyph info — Adafruit GFX format (7 bytes)
#[derive(Clone, Copy)]
pub struct GfxGlyph {
    pub bitmap_offset: u16,
    pub width: u8,
    pub height: u8,
    pub x_advance: u8,
    pub x_offset: i8,
    pub y_offset: i8,
}

impl GfxGlyph {
    pub const fn empty() -> Self {
        GfxGlyph {
            bitmap_offset: 0,
            width: 0,
            height: 0,
            x_advance: 0,
            x_offset: 0,
            y_offset: 0,
        }
    }

    pub const fn new(
        bitmap_offset: u16,
        width: u8,
        height: u8,
        x_advance: u8,
        x_offset: i8,
        y_offset: i8,
    ) -> Self {
        GfxGlyph {
            bitmap_offset,
            width,
            height,
            x_advance,
            x_offset,
            y_offset,
        }
    }
}

/// Max glyph count (ASCII 0x20–0x7E = 95, extended up to 224)
const MAX_GLYPHS: usize = 128;

/// Glyph bitmap read buffer size
const BITMAP_BUF_SIZE: usize = 128;

/// Embedded font ID (reserved, user fonts cannot use this)
pub const EMBEDDED_FONT_ID: u8 = 0;

/// Font — glyph table in RAM, bitmap data in flash or ROM.
pub struct Font {
    pub font_id: u8,
    first: u16,
    last: u16,
    y_advance: u8,
    glyph_count: u16,
    glyphs: [GfxGlyph; MAX_GLYPHS],
    /// Absolute flash address of bitmap data
    bitmap_addr: u32,
    /// Embedded bitmap data pointer (ROM). Null means flash is used.
    embedded_bitmap: *const u8,
}

// Font only holds a const pointer — Send/Sync safe (single core, no ISR access)
unsafe impl Send for Font {}
unsafe impl Sync for Font {}

impl Font {
    /// Empty/invalid font sentinel (used for FontList initialization).
    pub const fn empty() -> Self {
        Font {
            font_id: 0xFF,
            first: 0,
            last: 0,
            y_advance: 0,
            glyph_count: 0,
            glyphs: [GfxGlyph::empty(); MAX_GLYPHS],
            bitmap_addr: 0,
            embedded_bitmap: core::ptr::null(),
        }
    }

    /// Load font from flash by resource name (combined format).
    pub fn load(fs: &Fs, flash: &Flash, name: &[u8]) -> Option<Self> {
        let entry = fs.find(flash, name)?;
        Self::load_from_entry(flash, &entry)
    }

    /// Load font from a ResourceEntry (combined format: header + glyphs + bitmap).
    pub fn load_from_entry(flash: &Flash, entry: &crate::fs::ResourceEntry) -> Option<Self> {
        if entry.size < 6 {
            return None;
        }

        let mut hdr = [0u8; 6];
        flash.read(entry.offset, &mut hdr);

        let first = u16::from_le_bytes([hdr[0], hdr[1]]);
        let last = u16::from_le_bytes([hdr[2], hdr[3]]);
        let y_advance = hdr[4];
        let font_id = hdr[5];

        if first > last {
            return None;
        }
        let glyph_count = last - first + 1;
        if glyph_count as usize > MAX_GLYPHS {
            return None;
        }

        let mut glyphs = [GfxGlyph::empty(); MAX_GLYPHS];
        let mut g_buf = [0u8; 7];

        for i in 0..glyph_count as usize {
            let offset = 6 + (i * 7) as u32;
            flash.read(entry.offset + offset, &mut g_buf);

            glyphs[i] = GfxGlyph {
                bitmap_offset: u16::from_le_bytes([g_buf[0], g_buf[1]]),
                width: g_buf[2],
                height: g_buf[3],
                x_advance: g_buf[4],
                x_offset: g_buf[5] as i8,
                y_offset: g_buf[6] as i8,
            };
        }

        let bitmap_addr = entry.offset + 6 + glyph_count as u32 * 7;

        Some(Font {
            font_id,
            first,
            last,
            y_advance,
            glyph_count,
            glyphs,
            bitmap_addr,
            embedded_bitmap: core::ptr::null(),
        })
    }

    /// Search flash for a font with the given font_id and load it.
    /// Returns None if font_id is 0 (embedded) or not found in flash.
    pub fn load_by_id(fs: &Fs, flash: &Flash, font_id: u8) -> Option<Self> {
        if font_id == EMBEDDED_FONT_ID {
            return None;
        }
        let count = fs.count_by_kind(flash, RES_FONT);
        for i in 0..count {
            if let Some(entry) = fs.find_nth_by_kind(flash, RES_FONT, i) {
                if entry.size < 6 {
                    continue;
                }
                let mut hdr = [0u8; 6];
                flash.read(entry.offset, &mut hdr);
                if hdr[5] == font_id {
                    return Self::load_from_entry(flash, &entry);
                }
            }
        }
        None
    }

    /// Create an embedded (ROM) font. Always gets font_id = 0.
    pub fn from_embedded(
        glyphs: &[GfxGlyph],
        bitmap: &'static [u8],
        first: u16,
        last: u16,
        y_advance: u8,
    ) -> Self {
        let mut font_glyphs = [GfxGlyph::empty(); MAX_GLYPHS];
        let count = glyphs.len().min(MAX_GLYPHS);
        let mut i = 0;
        while i < count {
            font_glyphs[i] = glyphs[i];
            i += 1;
        }
        Font {
            font_id: EMBEDDED_FONT_ID,
            first,
            last,
            y_advance,
            glyph_count: count as u16,
            glyphs: font_glyphs,
            bitmap_addr: 0,
            embedded_bitmap: bitmap.as_ptr(),
        }
    }

    /// Line height in pixels
    pub fn line_height(&self) -> u8 {
        self.y_advance
    }

    /// Character width (xAdvance)
    pub fn char_width(&self, ch: char) -> u8 {
        let code = ch as u16;
        if code < self.first || code > self.last {
            return 0;
        }
        let idx = (code - self.first) as usize;
        self.glyphs[idx].x_advance
    }

    /// Calculate string width in pixels.
    pub fn text_width(&self, text: &[u8]) -> u16 {
        let mut w: u16 = 0;
        for &ch in text {
            w += self.char_width(ch as char) as u16;
        }
        w
    }

    /// Draw a single character.
    pub fn draw_char(
        &self,
        lcd: &Lcd,
        flash: &Flash,
        ch: char,
        cx: i16,
        cy: i16,
        fg: u16,
        bg: Option<u16>,
    ) -> u8 {
        let code = ch as u16;
        if code < self.first || code > self.last {
            return 0;
        }

        let idx = (code - self.first) as usize;
        let glyph = &self.glyphs[idx];

        if glyph.width == 0 || glyph.height == 0 {
            return glyph.x_advance;
        }

        let draw_x = cx + glyph.x_offset as i16;
        let draw_y = cy + glyph.y_offset as i16;

        if draw_x + glyph.width as i16 <= 0
            || draw_y + glyph.height as i16 <= 0
            || draw_x >= 800
            || draw_y >= 480
        {
            return glyph.x_advance;
        }

        let total_bits = glyph.width as u32 * glyph.height as u32;
        let total_bytes = (total_bits + 7) / 8;

        if let Some(bg_color) = bg {
            self.draw_opaque(lcd, flash, total_bytes, glyph, draw_x, draw_y, fg, bg_color);
        } else {
            self.draw_transparent(lcd, flash, total_bytes, glyph, draw_x, draw_y, fg);
        }

        glyph.x_advance
    }

    /// Draw a string.
    pub fn draw_str(
        &self,
        lcd: &Lcd,
        flash: &Flash,
        text: &[u8],
        mut x: i16,
        y: i16,
        fg: u16,
        bg: Option<u16>,
    ) -> i16 {
        for &ch in text {
            let advance = self.draw_char(lcd, flash, ch as char, x, y, fg, bg);
            x += advance as i16;
        }
        x
    }

    // --- Bitmap reading (flash or ROM) ---

    fn read_bitmap(&self, flash: &Flash, glyph_offset: u16, byte_offset: u32, buf: &mut [u8]) {
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

    // --- Draw implementations ---

    fn draw_opaque(
        &self,
        lcd: &Lcd,
        flash: &Flash,
        total_bytes: u32,
        glyph: &GfxGlyph,
        draw_x: i16,
        draw_y: i16,
        fg: u16,
        bg: u16,
    ) {
        lcd.begin_pixels(draw_x as u16, draw_y as u16, glyph.width as u16, glyph.height as u16);

        let total_bits = glyph.width as u32 * glyph.height as u32;
        let mut bit_idx: u32 = 0;
        let mut bytes_read: u32 = 0;

        while bytes_read < total_bytes {
            let remaining = total_bytes - bytes_read;
            let chunk_size = if remaining < BITMAP_BUF_SIZE as u32 {
                remaining as usize
            } else {
                BITMAP_BUF_SIZE
            };

            let mut buf = [0u8; BITMAP_BUF_SIZE];
            self.read_bitmap(flash, glyph.bitmap_offset, bytes_read, &mut buf[..chunk_size]);

            for i in 0..chunk_size {
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

            bytes_read += chunk_size as u32;
        }
    }

    fn draw_transparent(
        &self,
        lcd: &Lcd,
        flash: &Flash,
        total_bytes: u32,
        glyph: &GfxGlyph,
        draw_x: i16,
        draw_y: i16,
        fg: u16,
    ) {
        let w = glyph.width as u32;
        let total_bits = w * glyph.height as u32;
        let mut bit_idx: u32 = 0;
        let mut bytes_read: u32 = 0;

        while bytes_read < total_bytes {
            let remaining = total_bytes - bytes_read;
            let chunk_size = if remaining < BITMAP_BUF_SIZE as u32 {
                remaining as usize
            } else {
                BITMAP_BUF_SIZE
            };

            let mut buf = [0u8; BITMAP_BUF_SIZE];
            self.read_bitmap(flash, glyph.bitmap_offset, bytes_read, &mut buf[..chunk_size]);

            for i in 0..chunk_size {
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

            bytes_read += chunk_size as u32;
        }
    }
}

// ============================================================
// FontList — application-wide font registry
// ============================================================

/// Maximum number of loaded fonts (embedded + flash)
const MAX_FONTS: usize = 4;

/// Application-wide font list. Holds loaded fonts and supports
/// lookup by font_id with lazy loading from flash.
pub struct FontList {
    fonts: [Font; MAX_FONTS],
    count: u8,
}

impl FontList {
    pub const fn new() -> Self {
        Self {
            fonts: [const { Font::empty() }; MAX_FONTS],
            count: 0,
        }
    }

    /// Add a font to the list. Returns false if full.
    pub fn add(&mut self, font: Font) -> bool {
        if (self.count as usize) >= MAX_FONTS {
            return false;
        }
        self.fonts[self.count as usize] = font;
        self.count += 1;
        true
    }

    /// Find a loaded font by font_id. Returns None if not loaded.
    pub fn find(&self, font_id: u8) -> Option<&Font> {
        for i in 0..self.count as usize {
            if self.fonts[i].font_id == font_id {
                return Some(&self.fonts[i]);
            }
        }
        None
    }

    /// Find a font by font_id. If not already loaded, search flash
    /// filesystem and load it. Returns true if the font is now available.
    pub fn find_or_load(&mut self, font_id: u8, fs: &Fs, flash: &Flash) -> bool {
        // Already loaded?
        if self.find(font_id).is_some() {
            return true;
        }
        // Try loading from flash
        if let Some(font) = Font::load_by_id(fs, flash, font_id) {
            return self.add(font);
        }
        false
    }

    /// Get the embedded font (font_id = 0). Panics if not added.
    pub fn embedded(&self) -> &Font {
        // Embedded font is always the first one added
        &self.fonts[0]
    }

    /// Resolve a font_id: try exact match, fall back to embedded (id=0).
    /// This implements the caller pattern: try font_id, if None use embedded.
    pub fn resolve(&self, font_id: u8) -> Option<&Font> {
        self.find(font_id).or_else(|| self.find(EMBEDDED_FONT_ID))
    }
}
