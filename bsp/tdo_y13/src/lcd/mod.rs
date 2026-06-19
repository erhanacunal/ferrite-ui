//! LCD backend for TDO Y13 — F1C100s DEBE dual-layer double-buffered framebuffer.
//!
//! The F1C100s DEBE (Display Engine Backend) scans out from a framebuffer in
//! DRAM. This backend keeps **two** framebuffers assigned to two DEBE layers:
//! layer 0 always scans from framebuffer 0, layer 1 always scans from
//! framebuffer 1. Only one layer is visible at a time — the DEBE displays the
//! *front* buffer while the CPU draws the next frame into the *back* buffer.
//! `end_frame` toggles layer visibility (disable front, enable back). DEBE
//! register writes take effect immediately, so the toggle is done *inside*
//! vertical blanking (via `lcd::wait_for_vsync`) — otherwise it tears the frame
//! mid-scanout.

use ferrite_core::lcd::LcdBackend;
use f1c100s::lcd::{self, ColorMode, Panel};
use f1c100s::panels;
use f1c100s::gpio::{self, DriveLevel, Port};
use f1c100s::soft_spi::{SoftSpi, SoftSpiConfig, SpiMode};
use f1c100s::timer::delay_ms;
use f1c100s::{cpu, mmu};
use f1c100s::ipc::{Semaphore, critical};

// 480×480 pixels, one ARGB8888 word per pixel (DEBE converts down to the 18-bit bus).
const FB_PIXELS: usize = 480 * 480;
const FB_SIZE: usize = FB_PIXELS * 4;

/// Number of framebuffers. Triple-buffering decouples the UI thread from the
/// present thread: while the present thread waits for vblank to flip frame N,
/// the UI thread rasterizes frame N+1 into a third buffer instead of stalling.
const NUM_BUFFERS: usize = 3;

/// The two framebuffers backing the double buffer (placed in .bss by the
/// linker). The DEBE scans out the *front* buffer on one layer while the CPU
/// draws into the *back* buffer on the other layer; `end_frame` toggles layer
/// visibility during vertical blanking for a tear-free flip.
///
/// In `dirty` render mode the framework never calls `begin_frame`/`end_frame`,
/// so `BACK` stays 0 and only layer 0 (buffer 0) is ever drawn or displayed —
/// effectively single-buffered, identical to the previous behaviour.
///
/// SAFETY: pixel writes come only from the UI thread (one buffer at a time, the
/// current `BACK`). The present thread only ever reads a *different* buffer
/// (`PENDING`) for the D-cache clean + DEBE flip, so the two threads never touch
/// the same framebuffer concurrently.
#[unsafe(link_section = ".bss.framebuffer")]
static mut FRAMEBUFFERS: [[u32; FB_PIXELS]; NUM_BUFFERS] = [[0u32; FB_PIXELS]; NUM_BUFFERS];

/// Row stride in **pixels** (one u32 per pixel). The framebuffer is indexed
/// through a `[u32]` view, so the stride is the pixel width — not the byte width.
const FB_STRIDE: u16 = 480;

/// Base pointer of framebuffer `index` (0..NUM_BUFFERS).
#[inline]
fn fb_ptr(index: u8) -> *mut u32 {
    // SAFETY: forms a raw pointer to one of the framebuffer statics without
    // creating a reference.
    unsafe { (&raw mut FRAMEBUFFERS[index as usize]).cast::<u32>() }
}

// ── Triple-buffer state ─────────────────────────────────────────────────────
//
// Buffer index == DEBE layer index: layer N always scans from framebuffer N,
// and exactly one layer is enabled (visible) at a time. `flip` = disable the
// old layer, enable the new one, inside vertical blanking.
//
// Roles (all guarded by `critical()` — a brief interrupt-disable — because they
// are read/written across the UI thread, the present thread, and the ISR):
//   FRONT   — buffer the DEBE currently displays (owned by the present thread)
//   BACK    — buffer the CPU draws into (owned by the UI thread between
//             begin_frame and submit_present; never touched by other threads)
//   PENDING — buffer handed to the present thread, awaiting its vblank flip
/// Buffer/layer the DEBE currently displays.
static mut FRONT: u8 = 0;
/// Buffer/layer the CPU draws into — target of every fill/pixel/blend.
static mut BACK: u8 = 0;
/// Buffer submitted to the present thread, not yet flipped.
static mut PENDING: u8 = 0;
/// Whether a submitted frame is still awaiting its flip.
static mut HAS_PENDING: bool = false;

// ── Present-thread synchronization ──────────────────────────────────────────
/// UI → present: a rasterized frame (PENDING) is ready to show.
static mut READY_SEM: Semaphore = Semaphore::new(0);
/// Single in-flight present slot. UI takes it in `submit_present`; the present
/// thread releases it after the flip. Init 1 ⇒ at most one frame in flight ⇒
/// with 3 buffers there is always a free buffer for the UI to draw into.
static mut SLOT_SEM: Semaphore = Semaphore::new(1);
/// Vblank ISR → present thread: a vertical-blank edge has begun.
static mut VSYNC_SEM: Semaphore = Semaphore::new(0);

#[inline]
fn ready_sem() -> &'static mut Semaphore {
    unsafe { &mut *core::ptr::addr_of_mut!(READY_SEM) }
}
#[inline]
fn slot_sem() -> &'static mut Semaphore {
    unsafe { &mut *core::ptr::addr_of_mut!(SLOT_SEM) }
}
#[inline]
fn vsync_sem() -> &'static mut Semaphore {
    unsafe { &mut *core::ptr::addr_of_mut!(VSYNC_SEM) }
}

/// Expand an RGB565 color (the framework's pixel format) into the ARGB8888 word
/// the DEBE framebuffer stores. Alpha is forced opaque; the 5/6-bit channels are
/// left-justified into their 8-bit slots. Shared with the panic handler.
#[inline]
pub fn rgb565_to_argb8888(color: u16) -> u32 {
    let r = ((color >> 11) & 0x1F) as u32;
    let g = ((color >> 5) & 0x3F) as u32;
    let b = (color & 0x1F) as u32;
    (0xFF << 24) | (r << 19) | (g << 10) | (b << 3)
}

/// Blend `src` (ARGB8888, opaque) over `dst` per 8-bit channel:
/// `out = (src*a + dst*(255-a) + 127) / 255`.
#[inline]
fn blend_argb8888(dst: u32, src: u32, alpha: u8) -> u32 {
    let a = alpha as u32;
    let na = 255 - a;
    let r = (((src >> 16) & 0xFF) * a + ((dst >> 16) & 0xFF) * na + 127) / 255;
    let g = (((src >> 8) & 0xFF) * a + ((dst >> 8) & 0xFF) * na + 127) / 255;
    let b = ((src & 0xFF) * a + (dst & 0xFF) * na + 127) / 255;
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

/// Hardware backend: writes to the DEBE framebuffer.
pub struct DebeLcd;

impl LcdBackend for DebeLcd {
    const WIDTH: u16 = 480;
    const HEIGHT: u16 = 480;

    // Framebuffer in DRAM → read-modify-write blending is cheap.
    // Mirrored in tools/devices/tdo_y13.json (caps.alpha / caps.image_alpha).
    const HAS_ALPHA: bool = true;

    fn begin_frame(&mut self) {
        // Buffered mode only (dirty mode never calls this). Pick a free buffer
        // to draw into — one that is neither displayed (FRONT) nor awaiting a
        // flip (PENDING). With 3 buffers and at most one in-flight present
        // (SLOT_SEM init 1), at least one buffer is always free, so this never
        // blocks. The draw primitives read `BACK` to pick their target.
        critical(|| unsafe {
            let busy_a = FRONT;
            let busy_b = if HAS_PENDING { PENDING } else { FRONT };
            let mut b: u8 = 0;
            while b == busy_a || b == busy_b {
                b += 1;
            }
            BACK = b;
        });
    }

    fn end_frame(&mut self) {
        // Synchronous present (LcdBackend contract). On tdo_y13 the platform
        // overrides `present_buffered` to hand the frame to the present thread
        // via `submit_present`, so this path is NOT taken in normal operation —
        // it is kept correct as a single-threaded fallback.
        unsafe {
            cpu::dsb();
            if mmu::dcache_status() {
                mmu::clean_dcache(fb_ptr(BACK) as u32, FB_SIZE as u32);
            }
            lcd::wait_for_vsync();
            lcd::debe_layer_disable(FRONT);
            lcd::debe_layer_enable(BACK);
            FRONT = BACK;
        }
    }

    fn back_buf(&self) -> u8 {
        unsafe { BACK }
    }

    fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        if w == 0 || h == 0 {
            return;
        }
        let x = x.min(Self::WIDTH - 1);
        let y = y.min(Self::HEIGHT - 1);
        let max_w = (Self::WIDTH - x).min(w);
        let max_h = (Self::HEIGHT - y).min(h);

        let argb = rgb565_to_argb8888(color);
        let fb = unsafe { &mut *(fb_ptr(BACK) as *mut [u32; FB_PIXELS]) };

        for row in 0..max_h {
            let base = (y + row) as usize * FB_STRIDE as usize + x as usize;
            for col in 0..max_w {
                fb[base + col as usize] = argb;
            }
        }
    }

    fn begin_pixels(&self, x: u16, y: u16, w: u16, _h: u16) {
        // Store write position + the back-buffer base in statics for write_pixel
        // to use. Capturing the base here avoids re-reading `BACK` per pixel;
        // `BACK` cannot change between begin_pixels and the pixel run.
        // Single-threaded (UI thread only): safe.
        unsafe {
            PIXEL_X = x;
            PIXEL_Y = y;
            PIXEL_START_X = x;
            PIXEL_W = w;
            PIXEL_FB = fb_ptr(BACK);
        }
    }

    fn write_pixel(&self, color: u16) {
        unsafe {
            if PIXEL_X < Self::WIDTH && PIXEL_Y < Self::HEIGHT {
                let fb = &mut *(PIXEL_FB as *mut [u32; FB_PIXELS]);
                fb[PIXEL_Y as usize * FB_STRIDE as usize + PIXEL_X as usize] =
                    rgb565_to_argb8888(color);
            }
            PIXEL_X += 1;
            if PIXEL_X >= PIXEL_START_X + PIXEL_W {
                PIXEL_X = PIXEL_START_X;
                PIXEL_Y += 1;
            }
        }
    }

    fn send_command(&self, _cmd: u16) {
        // Not used for framebuffer-based panels.
    }

    fn send_data(&self, _data: u16) {
        // Not used for framebuffer-based panels.
    }

    fn blend_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16, alpha: u8) {
        if alpha == 0 {
            return;
        }
        if alpha == 255 {
            self.fill_rect(x, y, w, h, color);
            return;
        }
        if w == 0 || h == 0 {
            return;
        }
        // Same clipping as fill_rect.
        let x = x.min(Self::WIDTH - 1);
        let y = y.min(Self::HEIGHT - 1);
        let max_w = (Self::WIDTH - x).min(w);
        let max_h = (Self::HEIGHT - y).min(h);

        let src = rgb565_to_argb8888(color);
        let fb = unsafe { &mut *(fb_ptr(BACK) as *mut [u32; FB_PIXELS]) };

        for row in 0..max_h {
            let base = (y + row) as usize * FB_STRIDE as usize + x as usize;
            for col in 0..max_w {
                let i = base + col as usize;
                fb[i] = blend_argb8888(fb[i], src, alpha);
            }
        }
    }

    fn blend_pixel(&self, x: u16, y: u16, color: u16, alpha: u8) {
        if alpha == 0 || x >= Self::WIDTH || y >= Self::HEIGHT {
            return;
        }
        let fb = unsafe { &mut *(fb_ptr(BACK) as *mut [u32; FB_PIXELS]) };
        let i = y as usize * FB_STRIDE as usize + x as usize;
        let src = rgb565_to_argb8888(color);
        fb[i] = if alpha == 255 {
            src
        } else {
            blend_argb8888(fb[i], src, alpha)
        };
    }
}

// Per-pixel write state (single-threaded: UI thread only).
static mut PIXEL_X: u16 = 0;
static mut PIXEL_Y: u16 = 0;
static mut PIXEL_START_X: u16 = 0;
static mut PIXEL_W: u16 = 0;
/// Back-buffer base captured at `begin_pixels` (avoids re-reading `BACK` per pixel).
static mut PIXEL_FB: *mut u32 = core::ptr::null_mut();

/// TL021WVC04 (ST77916) panel register init over 9-bit software SPI.
///
/// Wired identically to the `f1c100s_hello` reference for this panel:
/// SCK=PE0, SDA=PE1 (3-wire), CS=PE6. The HAL calls this from `init_all`
/// after TCON+DEBE setup and before output enable. Bit 8 of `send_9bit`
/// is the D/C line (0=command, 1=data).
fn panel_init() {
    let spi = SoftSpi::new(SoftSpiConfig {
        sck: (Port::E, 0),
        mosi: (Port::E, 1),
        miso: (Port::E, 1),
        delay_us: 100,
        mode: SpiMode::Mode0,
        cs: Some((Port::E, 6)),
    });

    let cmd = |v: u16| spi.send_9bit(v, 0);
    let data = |v: u16| spi.send_9bit(v | 0x100, 0);

    spi.init();
    cmd(0xFF); data(0x77); data(0x01); data(0x00); data(0x00); data(0x10);
    cmd(0xC0); data(0x3B); data(0x00);
    cmd(0xC1); data(0x0B); data(0x02);
    cmd(0xC2); data(0x00); data(0x02);
    cmd(0xCC); data(0x10);
    cmd(0xCD); data(0x08);
    cmd(0xB0);
    data(0x02); data(0x13); data(0x1B); data(0x0D); data(0x10);
    data(0x05); data(0x08); data(0x07); data(0x07); data(0x24);
    data(0x04); data(0x11); data(0x0E); data(0x2C); data(0x33); data(0x1D);
    cmd(0xB1);
    data(0x05); data(0x13); data(0x1B); data(0x0D); data(0x11);
    data(0x05); data(0x08); data(0x07); data(0x07); data(0x24);
    data(0x04); data(0x11); data(0x0E); data(0x2C); data(0x33); data(0x1D);
    cmd(0xFF); data(0x77); data(0x01); data(0x00); data(0x00); data(0x11);
    cmd(0xB0); data(0x5d);
    cmd(0xB1); data(0x43);
    cmd(0xB2); data(0x81);
    cmd(0xB3); data(0x80);
    cmd(0xB5); data(0x43);
    cmd(0xB7); data(0x85);
    cmd(0xB8); data(0x20);
    cmd(0xC1); data(0x78);
    cmd(0xC2); data(0x78);
    cmd(0xD0); data(0x88);
    cmd(0xE0); data(0x00); data(0x00); data(0x02);
    cmd(0xE1);
    data(0x03); data(0xA0); data(0x00); data(0x00);
    data(0x04); data(0xA0); data(0x00); data(0x00);
    data(0x00); data(0x20); data(0x20);
    cmd(0xE2);
    for _ in 0..13 { data(0x00); }
    cmd(0xE3); data(0x00); data(0x00); data(0x11); data(0x00);
    cmd(0xE4); data(0x22); data(0x00);
    cmd(0xE5);
    data(0x05); data(0xEC); data(0xA0); data(0xA0);
    data(0x07); data(0xEE); data(0xA0); data(0xA0);
    for _ in 0..8 { data(0x00); }
    cmd(0xE6); data(0x00); data(0x00); data(0x11); data(0x00);
    cmd(0xE7); data(0x22); data(0x00);
    cmd(0xE8);
    data(0x06); data(0xED); data(0xA0); data(0xA0);
    data(0x08); data(0xEF); data(0xA0); data(0xA0);
    for _ in 0..8 { data(0x00); }
    cmd(0xEB); data(0x00); data(0x00); data(0x40); data(0x40); data(0x00); data(0x00); data(0x00);
    cmd(0xED);
    data(0xFF); data(0xFF); data(0xFF); data(0xBA);
    data(0x0A); data(0xBF); data(0x45); data(0xFF);
    data(0xFF); data(0x54); data(0xFB); data(0xA0);
    data(0xAB); data(0xFF); data(0xFF); data(0xFF);
    cmd(0xEF); data(0x10); data(0x0D); data(0x04); data(0x08); data(0x3F); data(0x1F);
    cmd(0xFF); data(0x77); data(0x01); data(0x00); data(0x00); data(0x13);
    cmd(0xEF); data(0x08);
    cmd(0xFF); data(0x77); data(0x01); data(0x00); data(0x00); data(0x00);
    cmd(0x11); delay_ms(120); // sleep-out
    cmd(0x29);                // display-on
    cmd(0x36); data(0x00);    // MADCTL: scan direction
    delay_ms(50);
    cmd(0x3A); data(0x60);    // pixel format: 18-bit RGB bus
}

/// Initialize the LCD hardware: reset the panel, run its SPI register init,
/// then configure TCON + DEBE for the TL021WVC04 panel.
///
/// # Safety
/// Must be called once during boot, before any drawing. Requires the clock tree
/// and the AVS delay timer (`systick::init`) to already be configured. Must be
/// called from the same thread that owns `DebeLcd` (the UI thread).
pub unsafe fn init() {
    // Hardware reset pulse on PE5 (high → low 100 ms → high), per the reference
    // bring-up for this panel.
    gpio::set_function(Port::E, 5, gpio::function::OUTPUT);
    gpio::set_drive_level(Port::E, 5, DriveLevel::Level2);
    gpio::set_value(Port::E, 5, true);delay_ms(100);
    gpio::set_value(Port::E, 5, false);delay_ms(100);
    gpio::set_value(Port::E, 5, true);

    // The HAL invokes `panel_init` after TCON+DEBE setup and before output
    // enable. The framebuffer is ARGB8888; DEBE converts each pixel down to the
    // panel's 18-bit bus (the panel's own 0x3A=0x60 selects that bus format).
    let mut panel: Panel = panels::TL021WVC04;
    panel.panel_init = Some(panel_init);

    // Start the DEBE scanning out layer 0 (buffer 0); layers 1 and 2 are
    // pre-configured with their framebuffers but left hidden. FRONT, BACK and
    // PENDING are all 0 at boot.
    lcd::init_all(&panel, fb_ptr(0).cast::<u8>(), ColorMode::Argb8888);
    lcd::debe_layer_init(1, panel.width, panel.height, fb_ptr(1).cast::<u8>(), ColorMode::Argb8888, false);
    lcd::debe_layer_init(2, panel.width, panel.height, fb_ptr(2).cast::<u8>(), ColorMode::Argb8888, false);
}

// ── Asynchronous present (render thread) ─────────────────────────────────────

/// Hand the freshly rasterized back buffer to the present thread and return
/// immediately. Called from `TdoY13Platform::present_buffered` on the UI thread
/// after the framework has finished drawing into `BACK`.
///
/// Blocks only on `SLOT_SEM` — and only when the present thread has not yet
/// flipped the previous frame (backpressure that paces the UI to the panel
/// refresh). That wait yields the CPU via the scheduler; it does not busy-spin,
/// so the UI thread reclaims the vblank instead of polling it.
pub fn submit_present() {
    let _ = slot_sem().take(None);
    critical(|| unsafe {
        PENDING = BACK;
        HAS_PENDING = true;
    });
    let _ = ready_sem().release();
}

/// Present-thread entry point: publish submitted frames forever. Runs at higher
/// priority than the UI thread so the vblank flip happens promptly, but spends
/// almost all its time blocked on a semaphore.
pub fn present_thread_body() -> ! {
    loop {
        // Wait for a rasterized frame.
        let _ = ready_sem().take(None);
        let buf = critical(|| unsafe { PENDING });

        // Publish the pixels: drain the write buffer and write the D-cache back
        // so the DEBE (which scans DRAM directly) sees the new frame.
        cpu::dsb();
        if mmu::dcache_status() {
            mmu::clean_dcache(fb_ptr(buf) as u32, FB_SIZE as u32);
        }

        // Wait for the next vblank edge before swapping layers, so the DEBE
        // register writes land inside the blanking gap (a mid-scanout swap
        // tears).
        //
        // DIAGNOSTIC (bisection): poll the vblank status flag directly via the
        // proven `wait_for_vsync` instead of blocking on VSYNC_SEM / the vblank
        // IRQ. Paired with the vblank IRQ being left disabled in `rust_main`,
        // this removes the ISR from the system entirely. If the idle freeze
        // disappears, the IRQ/ISR path was the cause; if it persists, the cause
        // is elsewhere (scheduler / threading). This busy-waits at the present
        // thread's priority, but only while a frame is in flight (never at idle,
        // where the present thread is parked on READY_SEM), so it does not
        // affect the idle behaviour we are testing.
        lcd::wait_for_vsync();

        let old_front = critical(|| unsafe { FRONT });
        lcd::debe_layer_disable(old_front);
        lcd::debe_layer_enable(buf);
        critical(|| unsafe {
            FRONT = buf;
            HAS_PENDING = false;
        });

        // Free the present slot so the UI thread may submit the next frame.
        let _ = slot_sem().release();

    }
}

/// TCON vertical-blank interrupt handler. Installed on `TCON_INTERRUPT` and
/// woken at the start of every vblank.
///
/// Part of the (currently dormant) async-present path: presenting is done
/// synchronously on the UI thread, so this handler is not installed. Kept for
/// when the present thread is restored. MUST stay minimal and do NO blocking I/O.
pub fn vblank_isr(_vector: u32) {
    // Acknowledge first so the INTC line de-asserts (otherwise it re-fires in a
    // storm). Preserves the enable bit.
    lcd::clear_vblank_irq();

    // Only wake the present thread when it is actually waiting; otherwise the
    // counting semaphore would grow unbounded across idle frames (the panel
    // keeps blanking ~60×/s whether or not we are presenting).
    if vsync_sem().waiter_count() > 0 {
        vsync_sem().release_from_isr();
    }
}

/// Return a new `DebeLcd` instance. Must call `init()` first.
pub fn new() -> DebeLcd {
    DebeLcd
}

/// Return a raw pointer to the framebuffer the DEBE is currently displaying
/// (for use by the panic handler). Targeting the *front* buffer means the panic
/// screen is visible immediately, even if a frame was mid-draw into the back
/// buffer when the panic fired.
/// # Safety
/// Returns a raw pointer to a framebuffer static. The caller must ensure
/// no concurrent access occurs.
pub unsafe fn framebuffer_mut_ptr() -> *mut u8 {
    fb_ptr(unsafe { FRONT }).cast()
}
