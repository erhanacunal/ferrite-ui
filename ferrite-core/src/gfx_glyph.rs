/// Per-glyph metrics — Adafruit GFX compatible format (7 bytes packed).
/// Shared between font renderer and embedded font data.
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
