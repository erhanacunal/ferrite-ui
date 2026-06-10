//! LCD backend for TDO Y13 — F1C100s DEBE framebuffer.
//!
//! The F1C100s DEBE (Display Engine Backend) scans out from a framebuffer
//! in DRAM. This backend writes directly to that framebuffer. The TCON
//! is configured for the TL021WVC04 480×480 panel in parallel RGB mode.

use ferrite_core::lcd::LcdBackend;
use f1c100s::lcd::{self, ColorMode, Panel};
use f1c100s::panels;
use f1c100s::gpio::{self, DriveLevel, Port};
use f1c100s::soft_spi::{SoftSpi, SoftSpiConfig, SpiMode};
use f1c100s::timer::delay_ms;

// 480×480 × 2 bytes per pixel (ARGB8888)
const FB_SIZE: usize = 480 * 480 * 4;

/// Static framebuffer in DRAM (placed in .bss by the linker).
/// SAFETY: this is the only framebuffer; accessed only from the UI thread.
#[unsafe(link_section = ".bss.framebuffer")]
#[unsafe(no_mangle)]
static mut FRAMEBUFFER: [u8; FB_SIZE] = [0u8; FB_SIZE];

/// Width in pixels of the framebuffer scanline (may be larger than visible width
/// due to stride/alignment requirements; DEBE uses visible width for display).
const FB_STRIDE: u16 = 480;

/// Hardware backend: writes to the DEBE framebuffer.
pub struct DebeLcd;

impl LcdBackend for DebeLcd {
    const WIDTH: u16 = 480;
    const HEIGHT: u16 = 480;

    fn begin_frame(&mut self) {
        // Single-buffered: no swap needed. The DEBE scans from FRAMEBUFFER.
    }

    fn end_frame(&mut self) {
        // Single-buffered: no swap needed.
    }

    fn back_buf(&self) -> u8 {
        0 // always buffer 0
    }

    fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        if w == 0 || h == 0 {
            return;
        }
        let x = x.min(Self::WIDTH - 1);
        let y = y.min(Self::HEIGHT - 1);
        let max_w = (Self::WIDTH - x).min(w);
        let max_h = (Self::HEIGHT - y).min(h);

        let fb = unsafe { &mut *((&raw mut FRAMEBUFFER) as *mut [u16; FB_SIZE / 2]) };

        for row in 0..max_h {
            let base = (y + row) as usize * FB_STRIDE as usize + x as usize;
            for col in 0..max_w {
                fb[base + col as usize] = color;
            }
        }
    }

    fn begin_pixels(&self, x: u16, y: u16, w: u16, _h: u16) {
        // Store write position in a static for write_pixel to use.
        // Single-threaded (UI thread only): safe.
        unsafe {
            PIXEL_X = x;
            PIXEL_Y = y;
            PIXEL_START_X = x;
            PIXEL_W = w;
        }
    }

    fn write_pixel(&self, color: u16) {
        unsafe {
            if PIXEL_X < Self::WIDTH && PIXEL_Y < Self::HEIGHT {
                let fb = &mut *((&raw mut FRAMEBUFFER) as *mut [u16; FB_SIZE / 2]);
                fb[PIXEL_Y as usize * FB_STRIDE as usize + PIXEL_X as usize] = color;
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
}

// Per-pixel write state (single-threaded: UI thread only).
static mut PIXEL_X: u16 = 0;
static mut PIXEL_Y: u16 = 0;
static mut PIXEL_START_X: u16 = 0;
static mut PIXEL_W: u16 = 0;

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
    // enable. Keep RGB565: the framework writes u16 pixels into the 2-byte
    // framebuffer; the panel's own 0x3A=0x60 drives its 18-bit bus.
    let mut panel: Panel = panels::TL021WVC04;
    panel.panel_init = Some(panel_init);

    let fb_ptr = (&raw mut FRAMEBUFFER).cast::<u8>();
    lcd::init_all(&panel, fb_ptr, ColorMode::Argb8888);
}

/// Return a new `DebeLcd` instance. Must call `init()` first.
pub fn new() -> DebeLcd {
    DebeLcd
}

/// Return a raw pointer to the framebuffer (for use by the panic handler).
/// # Safety
/// Returns a raw pointer to the framebuffer static. The caller must ensure
/// no concurrent access occurs.
pub unsafe fn framebuffer_mut_ptr() -> *mut u8 {
    (&raw mut FRAMEBUFFER).cast()
}
