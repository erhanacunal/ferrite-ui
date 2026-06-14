/// Ferrite Image (FI) format — optimized image format for flash storage
///
/// Stored as RES_IMAGE in the flash filesystem.
///
/// Binary layout:
///   Header (9 bytes):
///     [0..2] magic: u16 LE = 0x4649 ("FI")
///     [2..4] width: u16 LE
///     [4..6] height: u16 LE
///     [6]    flags: u8
///            bit 0-1: mode (0=raw, 1=rle, 2=indexed_rle)
///            bit 2: has_alpha — pixel/palette units carry an alpha byte
///     [7]    colors: u8 — palette color count (indexed mode, 0=256)
///     [8]    image_id: u8 — unique ID (0 = no image, 1-254 = user images)
///
///   Indexed mode: palette — colors × 2 bytes (RGB565 LE),
///                 or colors × 3 bytes (RGB565 LE + alpha u8) with has_alpha
///   Pixel data: depends on mode
///
/// Modes:
///   Raw (0): RGB565 pixel array (width × height × 2 bytes)
///   RLE (1): PackBits RLE compressed RGB565
///   Indexed+RLE (2): Palette + PackBits RLE compressed indices
///
/// Alpha (bit 2): raw/RLE units widen to 3 bytes (RGB565 LE + alpha u8);
///   indexed palette entries widen instead, the index stream is unchanged.
///   Drawn via `LcdBackend::blend_pixel` — on devices without HAS_ALPHA this
///   degrades to a 1-bit cutout at threshold 128.
///
/// RLE encoding:
///   0x00..0x7F: literal run — next (n+1) units as-is
///   0x80..0xFF: repeat run — next unit repeated (n−126) times [2..129]
use crate::flash::{FlashBackend, FlashImpl};
extern crate alloc;
use crate::fs::{Fs, RES_IMAGE, ResourceEntry};
use alloc::vec;
use crate::lcd::{LcdBackend, LcdImpl};

const MAGIC: u16 = 0x4649; // "FI"
const HEADER_SIZE: u32 = 9;

/// Flash read buffer size (bytes)
const READ_BUF: usize = 128;

/// Max palette color count
const MAX_PALETTE: usize = 256;

// Mode constants
const MODE_RAW: u8 = 0;
const MODE_RLE: u8 = 1;
const MODE_INDEXED_RLE: u8 = 2;

/// Flags bit 2 — units carry an alpha byte.
const FLAG_ALPHA: u8 = 0x04;

/// Image metadata from flash. Pixel data is streamed during drawing.
/// RAM cost: ~22 bytes (palette stays in flash, read to stack during draw).
pub struct Image {
    pub image_id: u8,
    pub width: u16,
    pub height: u16,
    mode: u8,
    has_alpha: bool,
    palette_count: u16,
    palette_addr: u32,
    data_addr: u32,
    data_size: u32,
}

impl Image {
    /// Empty/invalid image sentinel (used for ImageList initialization).
    pub const fn empty() -> Self {
        Image {
            image_id: 0,
            width: 0,
            height: 0,
            mode: 0,
            has_alpha: false,
            palette_count: 0,
            palette_addr: 0,
            data_addr: 0,
            data_size: 0,
        }
    }

    /// Load image from flash by resource name. Only parses the header.
    pub fn load<F: FlashBackend>(fs: &Fs, flash: &FlashImpl<F>, name: &[u8]) -> Option<Self> {
        let entry = fs.find(flash, name)?;
        Self::load_from_entry(flash, &entry)
    }

    /// Load from a ResourceEntry.
    pub fn load_from_entry<F: FlashBackend>(
        flash: &FlashImpl<F>,
        entry: &ResourceEntry,
    ) -> Option<Self> {
        if entry.size < HEADER_SIZE {
            return None;
        }

        let mut hdr = [0u8; 9];
        flash.read(entry.offset, &mut hdr);

        let magic = u16::from_le_bytes([hdr[0], hdr[1]]);
        if magic != MAGIC {
            return None;
        }

        let width = u16::from_le_bytes([hdr[2], hdr[3]]);
        let height = u16::from_le_bytes([hdr[4], hdr[5]]);
        let flags = hdr[6];
        let colors_raw = hdr[7];
        let image_id = hdr[8];

        let mode = flags & 0x03;
        let has_alpha = flags & FLAG_ALPHA != 0;

        let (palette_count, palette_addr, data_addr) = if mode == MODE_INDEXED_RLE {
            let count = if colors_raw == 0 {
                256u16
            } else {
                colors_raw as u16
            };
            let pal_addr = entry.offset + HEADER_SIZE;
            let entry_size = if has_alpha { 3u32 } else { 2u32 };
            let dat_addr = pal_addr + count as u32 * entry_size;
            (count, pal_addr, dat_addr)
        } else {
            (0u16, 0u32, entry.offset + HEADER_SIZE)
        };

        let data_size = entry.size - (data_addr - entry.offset);

        Some(Image {
            image_id,
            width,
            height,
            mode,
            has_alpha,
            palette_count,
            palette_addr,
            data_addr,
            data_size,
        })
    }

    /// Search flash for an image with the given image_id and load it.
    /// Returns None if image_id is 0 or not found.
    pub fn load_by_id<F: FlashBackend>(fs: &Fs, flash: &FlashImpl<F>, image_id: u8) -> Option<Self> {
        if image_id == 0 {
            return None;
        }
        let count = fs.count_by_kind(flash, RES_IMAGE);
        for i in 0..count {
            if let Some(entry) = fs.find_nth_by_kind(flash, RES_IMAGE, i) {
                if entry.size < HEADER_SIZE {
                    continue;
                }
                // Read only byte 8 (image_id) — avoid full header parse
                let mut id_buf = [0u8; 1];
                flash.read(entry.offset + 8, &mut id_buf);
                if id_buf[0] == image_id {
                    return Self::load_from_entry(flash, &entry);
                }
            }
        }
        None
    }

    /// Draw the full image at screen coordinates (x, y).
    /// Streaming decode — image is NOT loaded into RAM.
    pub fn draw<B: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<B>,
        flash: &FlashImpl<F>,
        x: u16,
        y: u16,
    ) {
        if self.width == 0 || self.height == 0 {
            return;
        }

        if self.has_alpha {
            // Alpha images blend per positioned pixel — the streaming
            // begin_pixels/write_pixel cursor cannot read back the target.
            let mut cur = PixelCursor::new(x, y, self.width);
            match self.mode {
                MODE_RAW => self.draw_raw_alpha(lcd, flash, &mut cur),
                MODE_RLE => self.draw_rle_alpha(lcd, flash, &mut cur),
                MODE_INDEXED_RLE => self.draw_indexed_rle_alpha(lcd, flash, &mut cur),
                _ => {}
            }
            return;
        }

        lcd.begin_pixels(x, y, self.width, self.height);

        match self.mode {
            MODE_RAW => self.draw_raw(lcd, flash),
            MODE_RLE => self.draw_rle(lcd, flash),
            MODE_INDEXED_RLE => self.draw_indexed_rle(lcd, flash),
            _ => {}
        }
    }

    /// Draw a region of the image (sprite sheet, atlas usage).
    /// Only supported in raw mode — RLE has no random access. Alpha images
    /// are not supported here yet (full `draw` only).
    pub fn draw_region<B: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<B>,
        flash: &FlashImpl<F>,
        dx: u16,
        dy: u16,
        sx: u16,
        sy: u16,
        sw: u16,
        sh: u16,
    ) {
        if sw == 0 || sh == 0 || self.mode != MODE_RAW || self.has_alpha {
            return;
        }

        lcd.begin_pixels(dx, dy, sw, sh);
        let mut buf = [0u8; READ_BUF];

        for row in 0..sh {
            let src_y = sy + row;
            let row_addr = self.data_addr + (src_y as u32 * self.width as u32 + sx as u32) * 2;
            let row_bytes = sw as u32 * 2;
            let mut read = 0u32;

            while read < row_bytes {
                let remaining = row_bytes - read;
                let chunk = if remaining < READ_BUF as u32 {
                    remaining as usize
                } else {
                    READ_BUF & !1 // 2-byte aligned
                };

                flash.read(row_addr + read, &mut buf[..chunk]);

                let mut i = 0;
                while i + 1 < chunk {
                    lcd.write_pixel(u16::from_le_bytes([buf[i], buf[i + 1]]));
                    i += 2;
                }

                read += chunk as u32;
            }
        }
    }

    // --- Raw mode: RGB565 direct stream ---

    fn draw_raw<B: LcdBackend, F: FlashBackend>(&self, lcd: &LcdImpl<B>, flash: &FlashImpl<F>) {
        let total = self.width as u32 * self.height as u32 * 2;
        let mut buf = [0u8; READ_BUF];
        let mut read = 0u32;

        while read < total {
            let remaining = total - read;
            let chunk = if remaining < READ_BUF as u32 {
                remaining as usize
            } else {
                READ_BUF & !1 // 2-byte aligned
            };

            flash.read(self.data_addr + read, &mut buf[..chunk]);

            let mut i = 0;
            while i + 1 < chunk {
                lcd.write_pixel(u16::from_le_bytes([buf[i], buf[i + 1]]));
                i += 2;
            }

            read += chunk as u32;
        }
    }

    // --- RLE mode: PackBits RLE, unit = RGB565 (2 bytes) ---

    fn draw_rle<B: LcdBackend, F: FlashBackend>(&self, lcd: &LcdImpl<B>, flash: &FlashImpl<F>) {
        let mut reader = FlashReader::new(self.data_addr, self.data_size);
        let total_pixels = self.width as u32 * self.height as u32;
        let mut emitted: u32 = 0;

        while emitted < total_pixels {
            let ctrl = match reader.next(flash) {
                Some(b) => b,
                None => return,
            };

            if ctrl <= 0x7F {
                // Literal: (ctrl+1) RGB565 pixels
                let count = ctrl as u32 + 1;
                for _ in 0..count {
                    if emitted >= total_pixels {
                        return;
                    }
                    let lo = reader.next(flash).unwrap_or(0);
                    let hi = reader.next(flash).unwrap_or(0);
                    lcd.write_pixel(u16::from_le_bytes([lo, hi]));
                    emitted += 1;
                }
            } else {
                // Repeat: next pixel (ctrl - 126) times
                let count = ctrl as u32 - 126;
                let lo = reader.next(flash).unwrap_or(0);
                let hi = reader.next(flash).unwrap_or(0);
                let color = u16::from_le_bytes([lo, hi]);
                for _ in 0..count {
                    if emitted >= total_pixels {
                        return;
                    }
                    lcd.write_pixel(color);
                    emitted += 1;
                }
            }
        }
    }

    // --- Alpha variants: 3-byte units (RGB565 + alpha), positioned blend ---

    fn draw_raw_alpha<B: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<B>,
        flash: &FlashImpl<F>,
        cur: &mut PixelCursor,
    ) {
        let total_pixels = self.width as u32 * self.height as u32;
        let mut reader = FlashReader::new(self.data_addr, self.data_size);
        for _ in 0..total_pixels {
            let lo = reader.next(flash).unwrap_or(0);
            let hi = reader.next(flash).unwrap_or(0);
            let a = reader.next(flash).unwrap_or(0);
            cur.emit(lcd, u16::from_le_bytes([lo, hi]), a);
        }
    }

    fn draw_rle_alpha<B: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<B>,
        flash: &FlashImpl<F>,
        cur: &mut PixelCursor,
    ) {
        let mut reader = FlashReader::new(self.data_addr, self.data_size);
        let total_pixels = self.width as u32 * self.height as u32;
        let mut emitted: u32 = 0;

        while emitted < total_pixels {
            let ctrl = match reader.next(flash) {
                Some(b) => b,
                None => return,
            };

            if ctrl <= 0x7F {
                // Literal: (ctrl+1) units
                let count = ctrl as u32 + 1;
                for _ in 0..count {
                    if emitted >= total_pixels {
                        return;
                    }
                    let lo = reader.next(flash).unwrap_or(0);
                    let hi = reader.next(flash).unwrap_or(0);
                    let a = reader.next(flash).unwrap_or(0);
                    cur.emit(lcd, u16::from_le_bytes([lo, hi]), a);
                    emitted += 1;
                }
            } else {
                // Repeat: next unit (ctrl - 126) times
                let count = ctrl as u32 - 126;
                let lo = reader.next(flash).unwrap_or(0);
                let hi = reader.next(flash).unwrap_or(0);
                let a = reader.next(flash).unwrap_or(0);
                let color = u16::from_le_bytes([lo, hi]);
                for _ in 0..count {
                    if emitted >= total_pixels {
                        return;
                    }
                    cur.emit(lcd, color, a);
                    emitted += 1;
                }
            }
        }
    }

    fn draw_indexed_rle_alpha<B: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<B>,
        flash: &FlashImpl<F>,
        cur: &mut PixelCursor,
    ) {
        // Read 3-byte palette entries (RGB565 + alpha) to heap
        let mut colors = vec![0u16; MAX_PALETTE];
        let mut alphas = vec![0u8; MAX_PALETTE];
        let pal_count = self.palette_count as usize;

        {
            let total_pal_bytes = pal_count * 3;
            let mut pal_buf = [0u8; READ_BUF];
            let mut pal_read = 0usize;
            let mut pal_idx = 0usize;

            while pal_read < total_pal_bytes {
                let remaining = total_pal_bytes - pal_read;
                let chunk = if remaining < READ_BUF {
                    remaining
                } else {
                    READ_BUF - (READ_BUF % 3) // 3-byte aligned
                };

                flash.read(self.palette_addr + pal_read as u32, &mut pal_buf[..chunk]);

                let mut i = 0;
                while i + 3 <= chunk {
                    colors[pal_idx] = u16::from_le_bytes([pal_buf[i], pal_buf[i + 1]]);
                    alphas[pal_idx] = pal_buf[i + 2];
                    pal_idx += 1;
                    i += 3;
                }

                pal_read += chunk;
            }
        }

        // RLE decode — unit = u8 index, lookup to (color, alpha)
        let mut reader = FlashReader::new(self.data_addr, self.data_size);
        let total_pixels = self.width as u32 * self.height as u32;
        let mut emitted: u32 = 0;

        while emitted < total_pixels {
            let ctrl = match reader.next(flash) {
                Some(b) => b,
                None => return,
            };

            if ctrl <= 0x7F {
                let count = ctrl as u32 + 1;
                for _ in 0..count {
                    if emitted >= total_pixels {
                        return;
                    }
                    let idx = reader.next(flash).unwrap_or(0) as usize;
                    let (color, a) = if idx < pal_count {
                        (colors[idx], alphas[idx])
                    } else {
                        (0, 0)
                    };
                    cur.emit(lcd, color, a);
                    emitted += 1;
                }
            } else {
                let count = ctrl as u32 - 126;
                let idx = reader.next(flash).unwrap_or(0) as usize;
                let (color, a) = if idx < pal_count {
                    (colors[idx], alphas[idx])
                } else {
                    (0, 0)
                };
                for _ in 0..count {
                    if emitted >= total_pixels {
                        return;
                    }
                    cur.emit(lcd, color, a);
                    emitted += 1;
                }
            }
        }
    }

    // --- Indexed + RLE mode: palette lookup + PackBits RLE, unit = u8 index ---

    fn draw_indexed_rle<B: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<B>,
        flash: &FlashImpl<F>,
    ) {
        // Read palette to heap (avoids 512B stack allocation)
        let mut palette = vec![0u16; MAX_PALETTE];
        let pal_count = self.palette_count as usize;

        {
            let total_pal_bytes = pal_count * 2;
            let mut pal_buf = [0u8; READ_BUF];
            let mut pal_read = 0usize;
            let mut pal_idx = 0usize;

            while pal_read < total_pal_bytes {
                let remaining = total_pal_bytes - pal_read;
                let chunk = if remaining < READ_BUF {
                    remaining
                } else {
                    READ_BUF & !1
                };

                flash.read(self.palette_addr + pal_read as u32, &mut pal_buf[..chunk]);

                let mut i = 0;
                while i + 1 < chunk {
                    palette[pal_idx] = u16::from_le_bytes([pal_buf[i], pal_buf[i + 1]]);
                    pal_idx += 1;
                    i += 2;
                }

                pal_read += chunk;
            }
        }

        // RLE decode — unit = u8 index, palette lookup to RGB565
        let mut reader = FlashReader::new(self.data_addr, self.data_size);
        let total_pixels = self.width as u32 * self.height as u32;
        let mut emitted: u32 = 0;

        while emitted < total_pixels {
            let ctrl = match reader.next(flash) {
                Some(b) => b,
                None => return,
            };

            if ctrl <= 0x7F {
                let count = ctrl as u32 + 1;
                for _ in 0..count {
                    if emitted >= total_pixels {
                        return;
                    }
                    let idx = reader.next(flash).unwrap_or(0) as usize;
                    let color = if idx < pal_count { palette[idx] } else { 0 };
                    lcd.write_pixel(color);
                    emitted += 1;
                }
            } else {
                let count = ctrl as u32 - 126;
                let idx = reader.next(flash).unwrap_or(0) as usize;
                let color = if idx < pal_count { palette[idx] } else { 0 };
                for _ in 0..count {
                    if emitted >= total_pixels {
                        return;
                    }
                    lcd.write_pixel(color);
                    emitted += 1;
                }
            }
        }
    }
}

// ============================================================
// ImageList — application-wide image registry
// ============================================================

/// Maximum number of loaded images
const MAX_IMAGES: usize = 8;

/// Application-wide image list. Holds loaded image metadata and supports
/// lookup by image_id with lazy loading from flash.
pub struct ImageList {
    images: [Image; MAX_IMAGES],
    count: u8,
}

impl ImageList {
    pub const fn new() -> Self {
        Self {
            images: [const { Image::empty() }; MAX_IMAGES],
            count: 0,
        }
    }

    /// Remove all loaded images.
    pub fn clear(&mut self) {
        self.count = 0;
    }

    /// Add an image to the list. Returns false if full.
    pub fn add(&mut self, image: Image) -> bool {
        if (self.count as usize) >= MAX_IMAGES {
            return false;
        }
        self.images[self.count as usize] = image;
        self.count += 1;
        true
    }

    /// Find a loaded image by image_id. Returns None if not loaded.
    pub fn find(&self, image_id: u8) -> Option<&Image> {
        for i in 0..self.count as usize {
            if self.images[i].image_id == image_id {
                return Some(&self.images[i]);
            }
        }
        None
    }

    /// Find an image by image_id. If not already loaded, search flash
    /// filesystem and load it. Returns true if the image is now available.
    pub fn find_or_load<F: FlashBackend>(
        &mut self,
        image_id: u8,
        fs: &Fs,
        flash: &FlashImpl<F>,
    ) -> bool {
        if image_id == 0 {
            return false;
        }
        if self.find(image_id).is_some() {
            return true;
        }
        if let Some(image) = Image::load_by_id(fs, flash, image_id) {
            return self.add(image);
        }
        false
    }
}

// === Positioned pixel cursor (alpha decode) ===

/// Tracks the (x, y) raster position for alpha image decode, emitting each
/// pixel through `blend_pixel` (positioned read-modify-write) instead of the
/// streaming `write_pixel` cursor.
struct PixelCursor {
    x: u16,
    y: u16,
    start_x: u16,
    end_x: u16,
}

impl PixelCursor {
    fn new(x: u16, y: u16, w: u16) -> Self {
        Self {
            x,
            y,
            start_x: x,
            end_x: x + w,
        }
    }

    #[inline]
    fn emit<B: LcdBackend>(&mut self, lcd: &LcdImpl<B>, color: u16, alpha: u8) {
        lcd.blend_pixel(self.x, self.y, color, alpha);
        self.x += 1;
        if self.x >= self.end_x {
            self.x = self.start_x;
            self.y += 1;
        }
    }
}

// === Buffered flash reader ===

/// Chunked flash reader — sequential reads in READ_BUF-sized chunks.
/// Makes byte-by-byte RLE decoding efficient.
struct FlashReader {
    addr: u32,
    end_addr: u32,
    buf: [u8; READ_BUF],
    pos: usize,
    valid: usize,
}

impl FlashReader {
    fn new(addr: u32, size: u32) -> Self {
        FlashReader {
            addr,
            end_addr: addr + size,
            buf: [0u8; READ_BUF],
            pos: 0,
            valid: 0,
        }
    }

    /// Read next byte. Refills buffer from flash when exhausted.
    #[inline]
    fn next<F: FlashBackend>(&mut self, flash: &FlashImpl<F>) -> Option<u8> {
        if self.pos >= self.valid {
            if self.addr >= self.end_addr {
                return None;
            }
            let remaining = (self.end_addr - self.addr) as usize;
            let chunk = if remaining < READ_BUF {
                remaining
            } else {
                READ_BUF
            };
            flash.read(self.addr, &mut self.buf[..chunk]);
            self.addr += chunk as u32;
            self.pos = 0;
            self.valid = chunk;
        }

        let b = self.buf[self.pos];
        self.pos += 1;
        Some(b)
    }
}
