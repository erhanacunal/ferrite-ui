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

    /// Draw the image at screen coordinates (x, y).
    /// Streaming decode — image is NOT loaded into RAM.
    ///
    /// Honors the LCD scissor: only the part of the image inside it is
    /// written, raw images read just the visible rows/columns, and RLE decode
    /// stops after the last visible row. Without a scissor the whole image is
    /// drawn.
    pub fn draw<B: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<B>,
        flash: &FlashImpl<F>,
        x: u16,
        y: u16,
    ) {
        let Some(win) = self.window(lcd, x, y) else {
            return;
        };

        if self.has_alpha {
            // Alpha images blend per positioned pixel — the streaming
            // begin_pixels/write_pixel cursor cannot read back the target.
            // `blend_pixel` applies the scissor; `limit` ends decode early.
            let limit = win.y1 * self.width as u32;
            match self.mode {
                MODE_RAW => self.draw_raw_alpha(lcd, flash, x, y, &win),
                MODE_RLE => {
                    let mut cur = PixelCursor::new(x, y, self.width);
                    self.draw_rle_alpha(lcd, flash, &mut cur, limit)
                }
                MODE_INDEXED_RLE => {
                    let mut cur = PixelCursor::new(x, y, self.width);
                    self.draw_indexed_rle_alpha(lcd, flash, &mut cur, limit)
                }
                _ => {}
            }
            return;
        }

        if self.mode == MODE_RAW {
            // Random access: stream only the visible window.
            self.draw_region(
                lcd,
                flash,
                x + win.x0 as u16,
                y + win.y0 as u16,
                win.x0 as u16,
                win.y0 as u16,
                (win.x1 - win.x0) as u16,
                (win.y1 - win.y0) as u16,
            );
            return;
        }

        lcd.begin_pixels(
            x + win.x0 as u16,
            y + win.y0 as u16,
            (win.x1 - win.x0) as u16,
            (win.y1 - win.y0) as u16,
        );
        let mut sink = Sink::new(self.width as u32, win);
        match self.mode {
            MODE_RLE => self.draw_rle(lcd, flash, &mut sink),
            MODE_INDEXED_RLE => self.draw_indexed_rle(lcd, flash, &mut sink),
            _ => {}
        }
    }

    /// Part of the image at (x, y) inside the LCD scissor, in image
    /// coordinates; `None` when nothing is visible.
    fn window<B: LcdBackend>(&self, lcd: &LcdImpl<B>, x: u16, y: u16) -> Option<Window> {
        let (w, h) = (self.width as i32, self.height as i32);
        if w == 0 || h == 0 {
            return None;
        }
        let Some(c) = lcd.scissor() else {
            return Some(Window { x0: 0, x1: w as u32, y0: 0, y1: h as u32 });
        };
        let (x, y) = (x as i32, y as i32);
        let x0 = (c.x as i32 - x).clamp(0, w);
        let x1 = (c.x as i32 + c.w as i32 - x).clamp(0, w);
        let y0 = (c.y as i32 - y).clamp(0, h);
        let y1 = (c.y as i32 + c.h as i32 - y).clamp(0, h);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some(Window { x0: x0 as u32, x1: x1 as u32, y0: y0 as u32, y1: y1 as u32 })
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
        let mut buf = [0u16; READ_BUF / 2];

        for row in 0..sh {
            let src_y = sy + row;
            let row_addr = self.data_addr + (src_y as u32 * self.width as u32 + sx as u32) * 2;
            let mut done = 0u32;
            while done < sw as u32 {
                let n = (sw as u32 - done).min(buf.len() as u32) as usize;
                read_rgb565(flash, row_addr + done * 2, &mut buf[..n]);
                lcd.write_pixels(&buf[..n]);
                done += n as u32;
            }
        }
    }

    // --- RLE mode: PackBits RLE, unit = RGB565 (2 bytes) ---

    fn draw_rle<B: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<B>,
        flash: &FlashImpl<F>,
        sink: &mut Sink,
    ) {
        let mut reader = FlashReader::new(self.data_addr, self.data_size);
        let total_pixels = sink.limit();
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
                    sink.pixel(lcd, u16::from_le_bytes([lo, hi]));
                    emitted += 1;
                }
            } else {
                // Repeat: next pixel (ctrl - 126) times
                let count = ctrl as u32 - 126;
                let lo = reader.next(flash).unwrap_or(0);
                let hi = reader.next(flash).unwrap_or(0);
                let color = u16::from_le_bytes([lo, hi]);
                let n = count.min(total_pixels - emitted);
                sink.run(lcd, color, n);
                emitted += n;
            }
        }
    }

    // --- Alpha variants: 3-byte units (RGB565 + alpha), positioned blend ---

    fn draw_raw_alpha<B: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<B>,
        flash: &FlashImpl<F>,
        x: u16,
        y: u16,
        win: &Window,
    ) {
        // Random access: read only the visible columns of each visible row.
        let w = win.x1 - win.x0;
        for row in win.y0..win.y1 {
            let addr = self.data_addr + (row * self.width as u32 + win.x0) * 3;
            let mut reader = FlashReader::new(addr, w * 3);
            let mut cur = PixelCursor::new(x + win.x0 as u16, y + row as u16, w as u16);
            for _ in 0..w {
                let lo = reader.next(flash).unwrap_or(0);
                let hi = reader.next(flash).unwrap_or(0);
                let a = reader.next(flash).unwrap_or(0);
                cur.emit(lcd, u16::from_le_bytes([lo, hi]), a);
            }
        }
    }

    fn draw_rle_alpha<B: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<B>,
        flash: &FlashImpl<F>,
        cur: &mut PixelCursor,
        limit: u32,
    ) {
        let mut reader = FlashReader::new(self.data_addr, self.data_size);
        let total_pixels = limit;
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
        limit: u32,
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
        let total_pixels = limit;
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
        sink: &mut Sink,
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
        let total_pixels = sink.limit();
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
                    sink.pixel(lcd, color);
                    emitted += 1;
                }
            } else {
                let count = ctrl as u32 - 126;
                let idx = reader.next(flash).unwrap_or(0) as usize;
                let color = if idx < pal_count { palette[idx] } else { 0 };
                let n = count.min(total_pixels - emitted);
                sink.run(lcd, color, n);
                emitted += n;
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

// === Clipped streaming (opaque decode) ===

/// Visible part of an image, in image coordinates: columns `x0..x1`, rows
/// `y0..y1`.
#[derive(Clone, Copy)]
struct Window {
    x0: u32,
    x1: u32,
    y0: u32,
    y1: u32,
}

/// Receives decoded pixels in raster order and streams only those inside the
/// window to the `begin_pixels` cursor (which was opened on the window).
struct Sink {
    width: u32,
    win: Window,
    col: u32,
    row: u32,
}

impl Sink {
    fn new(width: u32, win: Window) -> Self {
        Self { width, win, col: 0, row: 0 }
    }

    /// Pixels to decode: everything up to the end of the last visible row.
    fn limit(&self) -> u32 {
        self.win.y1 * self.width
    }

    #[inline]
    fn pixel<B: LcdBackend>(&mut self, lcd: &LcdImpl<B>, color: u16) {
        let w = &self.win;
        if self.row >= w.y0 && self.col >= w.x0 && self.col < w.x1 {
            lcd.write_pixel(color);
        }
        self.col += 1;
        if self.col == self.width {
            self.col = 0;
            self.row += 1;
        }
    }

    /// `n` pixels of `color`, split at row ends and clipped to the window.
    fn run<B: LcdBackend>(&mut self, lcd: &LcdImpl<B>, color: u16, n: u32) {
        let mut n = n;
        while n > 0 {
            let take = n.min(self.width - self.col);
            if self.row >= self.win.y0 {
                let a = self.col.max(self.win.x0);
                let b = (self.col + take).min(self.win.x1);
                if b > a {
                    lcd.write_run(color, (b - a) as usize);
                }
            }
            self.col += take;
            n -= take;
            if self.col == self.width {
                self.col = 0;
                self.row += 1;
            }
        }
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

/// Read `out.len()` little-endian RGB565 pixels from `addr` straight into
/// `out` (no separate byte buffer — keeps stack use flat on small targets).
fn read_rgb565<F: FlashBackend>(flash: &FlashImpl<F>, addr: u32, out: &mut [u16]) {
    // SAFETY: a `[u16]` is valid as twice as many bytes; every bit pattern is
    // a valid u16 once the bytes are written.
    let bytes = unsafe { core::slice::from_raw_parts_mut(out.as_mut_ptr().cast::<u8>(), out.len() * 2) };
    flash.read(addr, bytes);
    for v in out.iter_mut() {
        *v = u16::from_le(*v); // no-op on every supported (little-endian) target
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Rect;
    use core::cell::RefCell;
    use std::vec::Vec;

    const W: usize = 48;
    const H: usize = 40;
    const SENTINEL: u16 = 0xDEAD;

    /// Recording frame buffer (W×H) with a `begin_pixels` stream cursor.
    struct FbLcd {
        px: RefCell<Vec<u16>>,
        cur: RefCell<crate::fb::StreamCursor>,
    }

    impl LcdBackend for FbLcd {
        const WIDTH: u16 = W as u16;
        const HEIGHT: u16 = H as u16;
        const HAS_ALPHA: bool = true;
        fn begin_frame(&mut self) {}
        fn end_frame(&mut self) {}
        fn back_buf(&self) -> u8 {
            0
        }
        fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
            for yy in y as usize..(y as usize + h as usize).min(H) {
                for xx in x as usize..(x as usize + w as usize).min(W) {
                    self.px.borrow_mut()[yy * W + xx] = color;
                }
            }
        }
        fn begin_pixels(&self, x: u16, y: u16, w: u16, _h: u16) {
            self.cur.borrow_mut().begin(x, y, w);
        }
        fn write_pixel(&self, color: u16) {
            if let Some((x, y)) = self.cur.borrow_mut().step(W, H) {
                self.px.borrow_mut()[y * W + x] = color;
            }
        }
        fn send_command(&self, _: u16) {}
        fn send_data(&self, _: u16) {}
        // Deterministic stand-in for a blend: records color ^ alpha.
        fn blend_pixel(&self, x: u16, y: u16, color: u16, alpha: u8) {
            if (x as usize) < W && (y as usize) < H {
                self.px.borrow_mut()[y as usize * W + x as usize] = color ^ alpha as u16;
            }
        }
    }

    struct MemFlash(Vec<u8>);
    impl FlashBackend for MemFlash {
        fn read(&self, addr: u32, buf: &mut [u8]) {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = *self.0.get(addr as usize + i).unwrap_or(&0);
            }
        }
        fn write(&self, _: u32, _: &[u8]) {}
        fn erase_sector(&self, _: u32) {}
        fn read_id(&self) -> (u8, u8, u8) {
            (0, 0, 0)
        }
    }

    /// PackBits-encode `units` (each `unit` bytes) the way the converter does.
    fn rle(units: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < units.len() {
            let mut run = 1;
            while i + run < units.len() && run < 129 && units[i + run] == units[i] {
                run += 1;
            }
            if run >= 2 {
                out.push((run + 126) as u8);
                out.extend(&units[i]);
                i += run;
            } else {
                let start = i;
                while i < units.len() && i - start < 128 && (i + 1 >= units.len() || units[i + 1] != units[i]) {
                    i += 1;
                }
                if i == start {
                    i += 1;
                }
                out.push((i - start - 1) as u8);
                for u in &units[start..i] {
                    out.extend(u);
                }
            }
        }
        out
    }

    /// Build (image, flash bytes) for one mode. Pixels mix flat runs and
    /// noise so both RLE run kinds appear.
    fn make(mode: u8, alpha: bool, iw: u16, ih: u16) -> (Image, Vec<u8>) {
        let n = iw as usize * ih as usize;
        let idx: Vec<u8> = (0..n).map(|i| if (i / 7) % 3 == 0 { 5 } else { (i * 31 % 13) as u8 }).collect();
        let pal: Vec<u16> = (0..16u16).map(|i| i.wrapping_mul(0x1357) ^ 0x0A0A).collect();
        let pal_a: Vec<u8> = (0..16u8).map(|i| i.wrapping_mul(17)).collect();
        let unit = |i: usize| {
            let k = idx[i] as usize;
            let mut u = pal[k].to_le_bytes().to_vec();
            if alpha {
                u.push(pal_a[k]);
            }
            u
        };
        let mut data = Vec::new();
        let (mut pal_addr, mut pal_count) = (0, 0);
        match mode {
            MODE_RAW => (0..n).for_each(|i| data.extend(unit(i))),
            MODE_RLE => data = rle(&(0..n).map(unit).collect::<Vec<_>>()),
            _ => {
                pal_addr = 0;
                pal_count = 16;
                for k in 0..16 {
                    data.extend(pal[k].to_le_bytes());
                    if alpha {
                        data.push(pal_a[k]);
                    }
                }
                data.extend(rle(&idx.iter().map(|&b| vec![b]).collect::<Vec<_>>()));
            }
        }
        let data_addr = if mode == MODE_INDEXED_RLE { 16 * if alpha { 3 } else { 2 } } else { 0 };
        let img = Image {
            image_id: 1,
            width: iw,
            height: ih,
            mode,
            has_alpha: alpha,
            palette_count: pal_count,
            palette_addr: pal_addr,
            data_addr,
            data_size: data.len() as u32 - data_addr,
        };
        (img, data)
    }

    fn render(img: &Image, flash: &FlashImpl<MemFlash>, x: u16, y: u16, sc: Option<Rect>) -> Vec<u16> {
        let lcd = LcdImpl::with_backend(FbLcd {
            px: RefCell::new(vec![SENTINEL; W * H]),
            cur: RefCell::new(crate::fb::StreamCursor::new()),
        });
        lcd.set_scissor(sc);
        img.draw(&lcd, flash, x, y);
        lcd.backend().px.borrow().clone()
    }

    #[test]
    fn clipped_draw_matches_masked_full_draw() {
        let scissors = [
            Rect::new(0, 0, W as u16, H as u16),
            Rect::new(5, 3, 10, 7),
            Rect::new(13, 9, 1, 1),
            Rect::new(0, 12, 48, 5),
            Rect::new(20, 0, 9, 40),
            Rect::new(40, 30, 30, 30),
            Rect::new(-5, -5, 12, 12),
            Rect::new(100, 100, 5, 5),
        ];
        for &(mode, alpha) in &[
            (MODE_RAW, false),
            (MODE_RLE, false),
            (MODE_INDEXED_RLE, false),
            (MODE_RAW, true),
            (MODE_RLE, true),
            (MODE_INDEXED_RLE, true),
        ] {
            let (img, data) = make(mode, alpha, 23, 17);
            let flash = FlashImpl::with_backend(MemFlash(data));
            for &(x, y) in &[(4u16, 6u16), (0, 0), (30, 28)] {
                let full = render(&img, &flash, x, y, None);
                for sc in &scissors {
                    let got = render(&img, &flash, x, y, Some(*sc));
                    for py in 0..H {
                        for px in 0..W {
                            let p = Rect::new(px as i16, py as i16, 1, 1);
                            let want = if p.intersects(sc) { full[py * W + px] } else { SENTINEL };
                            assert_eq!(
                                got[py * W + px], want,
                                "mode={mode} alpha={alpha} at=({x},{y}) scissor=({},{},{},{}) px=({px},{py})",
                                sc.x, sc.y, sc.w, sc.h
                            );
                        }
                    }
                }
            }
        }
    }
}
