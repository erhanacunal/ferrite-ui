//! EPD framebuffer and waveform rendering.
//!
//! Ported from <https://github.com/fridolin-koch/lilygo-epd47-rs>.
//!
//! Framebuffer: 4 bpp (16 grayscale levels), 2 pixels per byte, 260 KB in PSRAM.
//! Tainted-row bitmap: 68 bytes tracking which rows have been modified.
//! `flush()` runs a 15-frame waveform update over tainted rows only.

#![cfg(feature = "epaper")]

extern crate alloc;
use alloc::boxed::Box;
use alloc::vec;

use crate::systick::delay_ms;

use super::ed047tc1;

// ---- Geometry ----

pub const EPD_WIDTH: u16 = 960;
pub const EPD_HEIGHT: u16 = 540;

const FRAMEBUFFER_SIZE: usize = (EPD_WIDTH as usize / 2) * EPD_HEIGHT as usize; // 259_200
const LINE_BYTES_4BPP: usize = EPD_WIDTH as usize / 2; // 480 bytes — 4bpp row
const BYTES_PER_LINE: usize = ed047tc1::BYTES_PER_LINE; // 240 bytes — DMA row
const TAINTED_SIZE: usize = EPD_HEIGHT as usize / 8 + 1; // 68 bytes

// ---- Waveform contrast cycles ----

const CONTRAST_BOW: [u16; 15] = [
    30, 30, 20, 20, 30, 30, 30, 40, 40, 50, 50, 50, 100, 200, 300,
];
const CONTRAST_WOB: [u16; 15] = [10, 10, 8, 8, 8, 8, 8, 10, 10, 15, 15, 20, 20, 100, 300];

// ---- Draw mode ----

#[derive(Clone, Copy, Debug)]
pub enum DrawMode {
    BlackOnWhite,
    WhiteOnBlack,
}

impl DrawMode {
    fn lut_default(self) -> u8 {
        match self {
            DrawMode::BlackOnWhite => 0x55,
            DrawMode::WhiteOnBlack => 0xAA,
        }
    }

    fn contrast_cycles(self) -> &'static [u16; 15] {
        match self {
            DrawMode::BlackOnWhite => &CONTRAST_BOW,
            DrawMode::WhiteOnBlack => &CONTRAST_WOB,
        }
    }

    fn update_lut_k(self, k: usize) -> usize {
        match self {
            DrawMode::BlackOnWhite => 15 - k, // counts down
            DrawMode::WhiteOnBlack => k,      // counts up
        }
    }
}

// ---- Area descriptor ----

#[derive(Clone, Copy)]
pub struct EpdRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub fn full_screen() -> EpdRect {
    EpdRect {
        x: 0,
        y: 0,
        width: EPD_WIDTH as i32,
        height: EPD_HEIGHT as i32,
    }
}

// ---- Display ----

pub(crate) struct EpdDisplay {
    framebuffer: Box<[u8]>,           // 4bpp, PSRAM-backed
    tainted_rows: [u8; TAINTED_SIZE], // bitmap: bit set → row modified
}

impl EpdDisplay {
    fn new() -> Self {
        Self {
            // vec! allocates on the heap (PSRAM when DRAM heap is full).
            framebuffer: vec![0xFFu8; FRAMEBUFFER_SIZE].into_boxed_slice(),
            tainted_rows: [0u8; TAINTED_SIZE],
        }
    }

    /// Performs the screen repair routine as described here
    /// https://github.com/Xinyuan-LilyGO/LilyGo-EPD47/blob/master/examples/screen_repair/screen_repair.ino
    pub fn repair(&mut self) {
        self.clear_area(full_screen());
        for _ in 0..20 {
            self.push_pixels(full_screen(), 50, false);
            delay_ms(500);
        }
        self.clear_area(full_screen());
        for _ in 0..40 {
            self.push_pixels(full_screen(), 50, true);
            delay_ms(500);
        }
        self.clear_area(full_screen());
    }

    // ---- Pixel write ----

    pub(crate) fn set_pixel(&mut self, x: u16, y: u16, gray4: u8) {
        if x >= EPD_WIDTH || y >= EPD_HEIGHT {
            return;
        }
        let idx = x as usize / 2 + y as usize * (EPD_WIDTH as usize / 2);
        let v = self.framebuffer[idx];
        self.framebuffer[idx] = if x % 2 == 1 {
            (v & 0x0F) | ((gray4 & 0x0F) << 4)
        } else {
            (v & 0xF0) | (gray4 & 0x0F)
        };
        self.taint(y);
    }

    #[inline]
    fn taint(&mut self, row: u16) {
        let idx = row as usize / 8;
        if idx < TAINTED_SIZE {
            self.tainted_rows[idx] |= 1 << (row as usize % 8);
        }
    }

    fn taint_rect(&mut self, y: u16, h: u16) {
        let y2 = (y as u32 + h as u32).min(EPD_HEIGHT as u32) as u16;
        for row in y..y2 {
            self.taint(row);
        }
    }

    #[inline]
    fn is_tainted(&self, row: u16) -> bool {
        let idx = row as usize / 8;
        let bit = row as usize % 8;
        idx < TAINTED_SIZE && (self.tainted_rows[idx] >> bit) & 1 != 0
    }

    // ---- Rect fill (RGB565 → 4-bit gray) ----

    pub(crate) fn fill_rect(&mut self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        if w == 0 || h == 0 || x >= EPD_WIDTH || y >= EPD_HEIGHT {
            return;
        }
        let gray4 = rgb565_to_gray4(color);
        let byte = (gray4 & 0x0F) | ((gray4 & 0x0F) << 4); // same nibble in both slots
        let x2 = (x as u32 + w as u32).min(EPD_WIDTH as u32) as u16;
        let y2 = (y as u32 + h as u32).min(EPD_HEIGHT as u32) as u16;
        let stride = EPD_WIDTH as usize / 2;

        for row in y..y2 {
            let base = row as usize * stride;
            // Even-aligned left: fast-fill full bytes within the row
            let lx = x as usize;
            let rx = x2 as usize;
            // Handle left partial nibble
            if lx % 2 == 1 {
                let i = base + lx / 2;
                self.framebuffer[i] = (self.framebuffer[i] & 0x0F) | ((gray4 & 0x0F) << 4);
            }
            // Middle: full bytes
            let fb_start = (lx + 1) / 2;
            let fb_end = rx / 2;
            if fb_start < fb_end {
                self.framebuffer[base + fb_start..base + fb_end].fill(byte);
            }
            // Handle right partial nibble
            if rx % 2 == 1 {
                let i = base + rx / 2;
                self.framebuffer[i] = (self.framebuffer[i] & 0xF0) | (gray4 & 0x0F);
            }
        }
        self.taint_rect(y, h);
    }

    // ---- Cursor for streaming pixel writes ----
    // (begin_pixels / write_pixel pattern for image decode)

    pub(crate) fn begin_pixels(&mut self, x: u16, y: u16, w: u16, h: u16) {
        unsafe {
            PX_X0 = x;
            PX_X = x;
            PX_Y = y;
            PX_W = w;
            PX_H = h;
            PX_REM = w as u32 * h as u32;
        }
        self.taint_rect(y, h);
    }

    pub(crate) fn write_pixel(&mut self, color: u16) {
        unsafe {
            if PX_REM == 0 {
                return;
            }
            self.set_pixel(PX_X, PX_Y, rgb565_to_gray4(color));
            PX_X += 1;
            if PX_X >= PX_X0 + PX_W {
                PX_X = PX_X0;
                PX_Y += 1;
            }
            PX_REM -= 1;
        }
    }

    // ---- Area waveform operations ----

    /// Drive all pixels in `area` with a uniform EPD drive code for one frame.
    /// `color=true` → white drive (0xAA); `color=false` → black drive (0x55).
    pub(crate) fn push_pixels(&mut self, area: EpdRect, time_us: u16, color: bool) {
        if area.width <= 0 || area.height <= 0 {
            return;
        }
        let epd = unsafe { ed047tc1::ctrl() };

        let mut row_buf = [0u8; BYTES_PER_LINE];
        let drive: u8 = if color { 0xAA } else { 0x55 };
        for i in 0..area.width as u32 {
            let pos = i + (area.x as u32 % 4);
            let mask = drive & (0b00000011 << (2 * (pos % 4)));
            let byte_idx = (area.x as u32 / 4 + pos / 4) as usize;
            if byte_idx < BYTES_PER_LINE {
                row_buf[byte_idx] |= mask;
            }
        }
        line_buffer_reorder(&mut row_buf);

        epd.skipping = 0;
        epd.frame_start();

        for i in 0..EPD_HEIGHT {
            let r = i as i32;
            if r < area.y {
                row_skip(epd, time_us);
            } else if r == area.y {
                epd.set_buffer(&row_buf);
                row_write(epd, time_us);
            } else if r >= area.y + area.height {
                row_skip(epd, time_us);
            } else {
                row_write(epd, time_us);
            }
        }
        // Extra output_row after last row — matches reference and C driver.
        row_write(epd, time_us);
        epd.frame_end();
    }

    pub(crate) fn clear_area_cycles(&mut self, area: EpdRect, cycles: u32, time_us: u16) {
        for _ in 0..cycles {
            for _ in 0..4 {
                self.push_pixels(area, time_us, false);
            }
            for _ in 0..4 {
                self.push_pixels(area, time_us, true);
            }
        }
    }

    pub(crate) fn clear_area(&mut self, area: EpdRect) {
        self.clear_area_cycles(area, 4, 50);
    }

    // ---- Full flush ----

    /// Returns true if any row has been tainted since the last flush.
    pub(crate) fn is_dirty(&self) -> bool {
        self.tainted_rows.iter().any(|&b| b != 0)
    }

    /// Run the 15-frame waveform to push tainted rows to the panel.
    pub(crate) fn flush(&mut self, mode: DrawMode) {
        //let tainted_count: u16 = (0..EPD_HEIGHT).filter(|&y| self.is_tainted(y)).count() as u16;        
        self.draw(mode);
        self.tainted_rows.fill(0);
        self.framebuffer.fill(0xFF);
    }

    fn draw(&mut self, mode: DrawMode) {
        let epd = unsafe { ed047tc1::ctrl() };

        // LUT: 65536 entries — u16 FB key → u8 2-bit drive code
        let mut lut = vec![mode.lut_default(); 65536];

        for k in 0..15usize {
            update_lut(&mut lut, k, mode);

            epd.skipping = 0;
            epd.frame_start();

            //let mut active: u16 = 0;
            for y in 0..EPD_HEIGHT {
                if !self.is_tainted(y) {
                    epd.skip();
                    continue;
                }
                //active += 1;
                let start = y as usize * LINE_BYTES_4BPP;
                let end = start + LINE_BYTES_4BPP;
                let row_buf = prepare_dma_buffer(&self.framebuffer[start..end], &lut);
                epd.set_buffer(&row_buf);
                epd.skipping = 0;
                epd.output_row(mode.contrast_cycles()[k]);
            }
            // Pipeline is one row behind: latch+drive the last active row.
            if epd.skipping == 0 {
                row_write(epd, mode.contrast_cycles()[k]);
            }
            epd.frame_end();
        }
    }
}

// ---- Global static ----

static mut DISPLAY: Option<EpdDisplay> = None;
static mut DISPLAY_INSTALLED: bool = false;

pub(crate) fn is_installed() -> bool {
    unsafe { core::ptr::read_volatile(&raw const DISPLAY_INSTALLED) }
}

pub(crate) unsafe fn disp() -> &'static mut EpdDisplay {
    (*(&raw mut DISPLAY)).as_mut().unwrap_unchecked()
}

/// Allocate framebuffer + LUT from heap (should land in PSRAM).
/// Must be called after `psram_allocator!` has been set up.
pub fn alloc_and_install() {
    unsafe {
        DISPLAY = Some(EpdDisplay::new());
        core::ptr::write_volatile(&raw mut DISPLAY_INSTALLED, true);
    }
}

// Pixel streaming cursor (static state for begin_pixels/write_pixel)
static mut PX_X: u16 = 0;
static mut PX_Y: u16 = 0;
static mut PX_X0: u16 = 0;
static mut PX_W: u16 = 0;
static mut PX_H: u16 = 0;
static mut PX_REM: u32 = 0;

// ---- Row skip helper ----

/// 3-state skip: blank row, short-time row, CKV-only pulse.
fn row_skip(epd: &mut ed047tc1::ED047TC1, time_us: u16) {
    match epd.skipping {
        0 => {
            epd.set_buffer(&[0u8; BYTES_PER_LINE]);
            epd.output_row(time_us);
        }
        1 => {
            epd.output_row(10);
        }
        _ => {
            epd.skip();
        }
    }
    epd.skipping += 1;
}

fn row_write(epd: &mut ed047tc1::ED047TC1, time_us: u16) {
    epd.skipping = 0;
    epd.output_row(time_us);
}

// ---- Algorithmic helpers ----

/// Reorder bytes to compensate for the scrambled I8080 pin mapping.
/// Swaps the two u16 halves of every u32 word in the buffer.
/// Matches `reorder_line_buffer()` in the C reference driver.
pub(crate) fn line_buffer_reorder(data: &mut [u8]) {
    for chunk in data.chunks_exact_mut(4) {
        let val = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let swapped = (val >> 16) | ((val & 0x0000_FFFF) << 16);
        chunk.copy_from_slice(&swapped.to_le_bytes());
    }
}

/// Convert a 4bpp framebuffer row (LINE_BYTES_4BPP bytes) to a 2-bit-per-pixel
/// DMA row (BYTES_PER_LINE bytes) via the waveform LUT.
fn prepare_dma_buffer(line_4bpp: &[u8], lut: &[u8]) -> [u8; BYTES_PER_LINE] {
    let mut out = [0u8; BYTES_PER_LINE];
    // Process 4 u16 FB words per 4 output bytes (16 pixels per group)
    for j in 0..(EPD_WIDTH as usize / 16) {
        let base = j * 8; // 8 bytes = 4 u16s = 16 pixels
        let v0 = u16::from_le_bytes([line_4bpp[base], line_4bpp[base + 1]]);
        let v1 = u16::from_le_bytes([line_4bpp[base + 2], line_4bpp[base + 3]]);
        let v2 = u16::from_le_bytes([line_4bpp[base + 4], line_4bpp[base + 5]]);
        let v3 = u16::from_le_bytes([line_4bpp[base + 6], line_4bpp[base + 7]]);
        out[j * 4] = lut[v0 as usize];
        out[j * 4 + 1] = lut[v1 as usize];
        out[j * 4 + 2] = lut[v2 as usize];
        out[j * 4 + 3] = lut[v3 as usize];
    }
    out
}

/// Mask out LUT transitions that have completed by frame `k`.
fn update_lut(lut: &mut [u8], k: usize, mode: DrawMode) {
    let ki = mode.update_lut_k(k);
    for l in (ki..65536).step_by(16) {
        lut[l] &= 0xFC;
    }
    for l in ((ki << 4)..65536).step_by(256) {
        for p in 0..16 {
            lut[l + p] &= 0xF3;
        }
    }
    for l in ((ki << 8)..65536).step_by(4096) {
        for p in 0..256 {
            lut[l + p] &= 0xCF;
        }
    }
    for p in (ki << 12)..((ki + 1) << 12) {
        lut[p] &= 0x3F;
    }
}

// ---- Color conversion ----

#[inline]
fn rgb565_to_gray4(color: u16) -> u8 {
    let r8 = (((color >> 11) & 0x1F) as u32 * 527) >> 6;
    let g8 = (((color >> 5) & 0x3F) as u32 * 259) >> 6;
    let b8 = ((color & 0x1F) as u32 * 527) >> 6;
    let luma = (r8 * 299 + g8 * 587 + b8 * 114) / 1000;
    (luma * 15 / 255) as u8
}
