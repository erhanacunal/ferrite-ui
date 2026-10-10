//! Flash backend for N32903 — NAND region shadowed in SDRAM.
//!
//! ferrite-core expects NOR-style flash (byte reads, page program, 4 KB sector
//! erase); the board has NAND (2 KB pages, 128 KB blocks, ECC, bad blocks). So
//! a NAND region is mirrored in an SDRAM *shadow*:
//!
//! - boot: every logical block of the region is read into the shadow
//! - `read`: memcpy from the shadow
//! - `write` / `erase_sector`: change the shadow with NOR semantics (program
//!   only clears bits, erase sets 0xFF) and mark the 128 KB block dirty
//! - [`flush_if_idle`] (every main-loop iteration): once no write has come for
//!   [`IDLE_FLUSH_MS`], each dirty block is erased and reprogrammed.
//!   [`flush`] does it unconditionally (before a reset)
//!
//! Address 0 of the "flash" is the start of the shadow, so the framework's
//! default layout applies: config sector at 0x0000, resource FS from 0x1000.
//!
//! The region is [`REGION_FIRST_BLOCK`].. (+[`REGION_BLOCKS`]) on the NAND,
//! far above the loader (blocks 0-3) and the application (block 4 on). Bad
//! blocks are skipped: logical block *n* is the *n*-th good block of the
//! region. A block that fails to erase or program is marked bad, the map is
//! rebuilt and everything from that block on is written again.

use alloc::alloc::{Layout, alloc};
use ferrite_core::flash::FlashBackend;
use n32903_rs::nand::{self, NandError};

use crate::{systick, usart};

/// First NAND block of the resource region (16 MB into a 128 MB chip).
const REGION_FIRST_BLOCK: u32 = 128;
/// Physical blocks reserved for the region, spares for bad blocks included.
const REGION_BLOCKS: u32 = 32;
/// Logical blocks mirrored in SDRAM (= shadow size / block size).
const LOGICAL_BLOCKS: usize = 16;

/// Delay after the last write before dirty blocks go to NAND. Longer than
/// the gap between two 4 KB sectors of a `writefs` upload at 115200 baud.
const IDLE_FLUSH_MS: u32 = 1500;

const SECTOR_SIZE: u32 = 4096;

struct Shadow {
    data: *mut u8,
    /// Shadow size in bytes (logical blocks × block size)
    size: u32,
    block_size: u32,
    page_size: u32,
    pages_per_block: u32,
    /// Physical NAND block of each logical block
    map: [u32; LOGICAL_BLOCKS],
    /// Logical blocks in use (fewer than LOGICAL_BLOCKS if the region ran out
    /// of good blocks)
    blocks: usize,
    /// Bit n = logical block n differs from NAND
    dirty: u32,
    last_write_ms: u32,
}

/// SAFETY: only the main loop touches this (no interrupt handler does).
static mut SHADOW: Option<Shadow> = None;

fn shadow() -> Option<&'static mut Shadow> {
    unsafe { (&mut *(&raw mut SHADOW)).as_mut() }
}

/// Hardware backend: the SDRAM shadow of the NAND region.
#[derive(Clone)]
pub struct NandFlash;

impl FlashBackend for NandFlash {
    fn read(&self, addr: u32, buf: &mut [u8]) {
        let Some(s) = shadow() else {
            buf.fill(0xFF);
            return;
        };
        let start = addr.min(s.size) as usize;
        let n = buf.len().min(s.size as usize - start);
        unsafe { core::ptr::copy_nonoverlapping(s.data.add(start), buf.as_mut_ptr(), n) };
        buf[n..].fill(0xFF);
    }

    fn write(&self, addr: u32, data: &[u8]) {
        let Some(s) = shadow() else { return };
        if addr >= s.size {
            return;
        }
        let n = data.len().min((s.size - addr) as usize);
        for (i, &b) in data[..n].iter().enumerate() {
            // NOR program: bits can only go from 1 to 0
            unsafe { *s.data.add(addr as usize + i) &= b };
        }
        s.touch(addr, n as u32);
    }

    fn erase_sector(&self, addr: u32) {
        let Some(s) = shadow() else { return };
        let base = addr & !(SECTOR_SIZE - 1);
        if base >= s.size {
            return;
        }
        let n = SECTOR_SIZE.min(s.size - base);
        unsafe { core::ptr::write_bytes(s.data.add(base as usize), 0xFF, n as usize) };
        s.touch(base, n);
    }

    fn read_id(&self) -> (u8, u8, u8) {
        let id = nand::info().id;
        (id[0], id[1], id[2])
    }
}

impl Shadow {
    /// Mark the blocks covering `[addr, addr + len)` dirty.
    fn touch(&mut self, addr: u32, len: u32) {
        if len == 0 {
            return;
        }
        let first = addr / self.block_size;
        let last = (addr + len - 1) / self.block_size;
        for b in first..=last {
            self.dirty |= 1 << b;
        }
        self.last_write_ms = systick::millis();
    }

    fn block_ptr(&self, logical: usize) -> *mut u8 {
        unsafe { self.data.add(logical * self.block_size as usize) }
    }

    /// Assign good physical blocks to logical blocks, starting the search
    /// at logical block `from`.
    fn build_map(&mut self, from: usize) {
        let mut phys = if from == 0 {
            REGION_FIRST_BLOCK
        } else {
            self.map[from - 1] + 1
        };
        let end = REGION_FIRST_BLOCK + REGION_BLOCKS;
        self.blocks = from;
        while self.blocks < LOGICAL_BLOCKS && phys < end {
            if !unsafe { nand::is_bad_block(phys) } {
                self.map[self.blocks] = phys;
                self.blocks += 1;
            }
            phys += 1;
        }
        self.size = self.blocks as u32 * self.block_size;
    }

    /// Read logical block `logical` from NAND into the shadow.
    fn load_block(&mut self, logical: usize) {
        let ptr = self.block_ptr(logical);
        for page in 0..self.pages_per_block {
            let buf = unsafe { ptr.add((page * self.page_size) as usize) };
            if let Err(e) = unsafe { nand::read_page(self.map[logical], page, buf) } {
                // Unreadable (e.g. foreign data with a different ECC layout):
                // treat as erased so the region can be reformatted.
                unsafe { core::ptr::write_bytes(buf, 0xFF, self.page_size as usize) };
                log_error("read", self.map[logical], e);
            }
        }
    }

    /// Erase and program logical block `logical` from the shadow.
    fn store_block(&self, logical: usize) -> Result<(), NandError> {
        let block = self.map[logical];
        unsafe { nand::erase_block(block)? };
        let ptr = self.block_ptr(logical);
        for page in 0..self.pages_per_block {
            let buf = unsafe { ptr.add((page * self.page_size) as usize) };
            let data = unsafe { core::slice::from_raw_parts(buf, self.page_size as usize) };
            // All-0xFF pages stay erased (reads back as Erased → 0xFF anyway)
            if data.iter().any(|&b| b != 0xFF) {
                unsafe { nand::write_page(block, page, buf)? };
            }
        }
        Ok(())
    }

    /// Write every dirty block to NAND.
    fn flush(&mut self) {
        let mut logical = 0;
        let mut retries = 0;
        while logical < self.blocks {
            if self.dirty & (1 << logical) == 0 {
                logical += 1;
                continue;
            }
            match self.store_block(logical) {
                Ok(()) => {
                    self.dirty &= !(1 << logical);
                    logical += 1;
                }
                Err(e) => {
                    // Retire the block; this and every later logical block
                    // moves up one good block and must be written again.
                    let bad = self.map[logical];
                    log_error("write", bad, e);
                    unsafe { nand::mark_bad_block(bad) };
                    let old_blocks = self.blocks;
                    self.build_map(logical);
                    for b in logical..old_blocks {
                        self.dirty |= 1 << b;
                    }
                    retries += 1;
                    if retries > REGION_BLOCKS || self.blocks < old_blocks {
                        usart::log("flash: NAND region out of good blocks\r\n");
                        self.dirty = 0;
                        return;
                    }
                }
            }
        }
    }
}

fn log_error(op: &str, block: u32, e: NandError) {
    let mut buf = [0u8; 64];
    let mut w = Fmt { buf: &mut buf, len: 0 };
    let _ = core::fmt::write(&mut w, format_args!("flash: NAND {op} block {block}: {e:?}\r\n"));
    let len = w.len;
    usart::log_bytes(&buf[..len]);
}

/// Tiny `fmt::Write` into a stack buffer (truncates).
struct Fmt<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl core::fmt::Write for Fmt<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// Initialise the NAND, map the region's good blocks and load the shadow.
///
/// # Safety
/// Call once at boot, before any flash access, after `systick::init`.
pub unsafe fn init() {
    let info = match unsafe { nand::init() } {
        Ok(info) => info,
        Err(e) => {
            log_error("init", 0, e);
            return;
        }
    };
    let block_size = info.block_size();
    let layout = Layout::from_size_align(LOGICAL_BLOCKS * block_size as usize, nand::BUF_ALIGN)
        .unwrap();
    let data = unsafe { alloc(layout) };
    if data.is_null() {
        usart::log("flash: no memory for the NAND shadow\r\n");
        return;
    }

    let mut s = Shadow {
        data,
        size: 0,
        block_size,
        page_size: info.page_size,
        pages_per_block: info.page_per_block,
        map: [0; LOGICAL_BLOCKS],
        blocks: 0,
        dirty: 0,
        last_write_ms: 0,
    };
    s.build_map(0);
    for logical in 0..s.blocks {
        s.load_block(logical);
    }

    let mut buf = [0u8; 80];
    let mut w = Fmt { buf: &mut buf, len: 0 };
    let _ = core::fmt::write(
        &mut w,
        format_args!(
            "flash: NAND {:02X} {:02X}, shadow {} KB from block {}\r\n",
            info.id[0],
            info.id[1],
            s.size / 1024,
            REGION_FIRST_BLOCK
        ),
    );
    let len = w.len;
    usart::log_bytes(&buf[..len]);

    unsafe { SHADOW = Some(s) };
}

/// Write dirty blocks to NAND once writes have paused for [`IDLE_FLUSH_MS`].
pub fn flush_if_idle() {
    if let Some(s) = shadow() {
        if s.dirty != 0 && systick::millis().wrapping_sub(s.last_write_ms) >= IDLE_FLUSH_MS {
            s.flush();
        }
    }
}

/// Write dirty blocks to NAND now (before a reset).
pub fn flush() {
    if let Some(s) = shadow() {
        if s.dirty != 0 {
            s.flush();
        }
    }
}

/// Return a new `NandFlash` instance. Must call `init()` first.
pub fn new() -> NandFlash {
    NandFlash
}
