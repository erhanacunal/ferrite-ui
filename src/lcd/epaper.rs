//! Epaper `LcdBackend` implementation for the LilyGo T5-ePaper-S3.
//!
//! Thin glue that wraps `display::EpdDisplay` behind the `LcdBackend` trait.
//! Hardware setup is split across:
//!   - `ed047tc1`: low-level I8080 / RMT controller
//!   - `display`:  framebuffer, waveform, tainted-row tracking
//!
//! Public surface kept for `epaper_main.rs`:
//!   - `init(pins)`        — install ED047TC1 into global static
//!   - `alloc_buffers()`   — allocate PSRAM framebuffer
//!   - `clear()`           — ghost-erase + white FB reset
//!   - `EPD_WIDTH / EPD_HEIGHT`

#![cfg(feature = "epaper")]

use esp_hal::peripherals;

use super::display::{self, DrawMode, EpdRect};
use super::ed047tc1::{self, ED047TC1};
use super::LcdBackend;

// Re-export geometry for use in epaper_main.rs
pub use display::{full_screen, EPD_HEIGHT, EPD_WIDTH};

// ---- Initialisation ----

/// Pin bundle passed from `epaper_main::init_gpio`.
pub struct EpdHwPins {
    pub cfg_data: peripherals::GPIO13<'static>,
    pub cfg_clk: peripherals::GPIO12<'static>,
    pub cfg_str: peripherals::GPIO0<'static>,
    pub data0: peripherals::GPIO8<'static>,
    pub data1: peripherals::GPIO1<'static>,
    pub data2: peripherals::GPIO2<'static>,
    pub data3: peripherals::GPIO3<'static>,
    pub data4: peripherals::GPIO4<'static>,
    pub data5: peripherals::GPIO5<'static>,
    pub data6: peripherals::GPIO6<'static>,
    pub data7: peripherals::GPIO7<'static>,
    pub lcd_dc: peripherals::GPIO40<'static>,
    pub lcd_wrx: peripherals::GPIO41<'static>,
    pub dma_ch: peripherals::DMA_CH0<'static>,
    pub lcd_cam: peripherals::LCD_CAM<'static>,
    pub rmt: peripherals::RMT<'static>,
}

/// Install the ED047TC1 controller into the global static and perform
/// the power-on voltage ramp.
pub fn init(pins: EpdHwPins) {
    let epd = ED047TC1::new(
        pins.cfg_data,
        pins.cfg_clk,
        pins.cfg_str,
        pins.data0,
        pins.data1,
        pins.data2,
        pins.data3,
        pins.data4,
        pins.data5,
        pins.data6,
        pins.data7,
        pins.lcd_dc,
        pins.lcd_wrx,
        pins.dma_ch,
        pins.lcd_cam,
        pins.rmt,
    );
    ed047tc1::install(epd);
    crate::usart::dbg(b"  power_on\r\n");
    unsafe {
        ed047tc1::ctrl().power_on();
    }
    crate::usart::dbg(b"  power_on ok\r\n");
}

/// Allocate the PSRAM-backed framebuffer. Must be called after
/// `psram_allocator!` has been set up in `init_gpio`.
pub fn alloc_buffers() {
    display::alloc_and_install();
}

/// Full-screen ghost-image erase followed by a white framebuffer reset.
pub fn clear() {
    unsafe {
        display::disp().clear_area(full_screen());
    }
}

// ---- LcdBackend implementation ----

pub struct EpdLcd {
    _private: (),
}

impl EpdLcd {
    pub fn new() -> Self {
        EpdLcd { _private: () }
    }
}

impl LcdBackend for EpdLcd {
    fn begin_frame(&mut self) {}
    fn end_frame(&mut self) {}
    fn back_buf(&self) -> u8 {
        0
    }

    fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        unsafe {
            display::disp().fill_rect(x, y, w, h, color);
        }
    }

    fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16) {
        unsafe {
            display::disp().begin_pixels(x, y, w, h);
        }
    }

    fn write_pixel(&self, color: u16) {
        unsafe {
            display::disp().write_pixel(color);
        }
    }

    fn send_command(&self, _cmd: u16) {}
    fn send_data(&self, _data: u16) {}

    fn flush_dirty(&self) {
        unsafe {
            let d = display::disp();
            if d.is_dirty() {
                d.flush(DrawMode::BlackOnWhite);
            }
        }
    }
}
