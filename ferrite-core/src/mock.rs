//! Host-only mock platform — exists purely so the device-agnostic framework can be
//! monomorphized and type-checked on the host (`cargo check --features mock`) without
//! an embedded toolchain. Backends are no-ops; they must never be run, only compiled.
//!
//! Receivers mirror the real backends exactly (`&self` everywhere) so the mock cannot
//! hide a trait-bound mismatch that would only surface on real hardware.

use crate::backlight::BacklightBackend;
use crate::flash::FlashBackend;
use crate::lcd::LcdBackend;
use crate::platform::Platform;
use crate::rtc::{DateTime, RtcBackend};
use crate::sdcard::{CardType, SdCardBackend, SdError};
use crate::systick::SystickBackend;
use crate::touch::{CalParams, TouchBackend};
use crate::usart::UsartBackend;

pub struct MockLcd;
impl LcdBackend for MockLcd {
    const WIDTH: u16 = 800;
    const HEIGHT: u16 = 480;
    fn begin_frame(&mut self) {}
    fn end_frame(&mut self) {}
    fn back_buf(&self) -> u8 {
        0
    }
    fn fill_rect(&self, _x: u16, _y: u16, _w: u16, _h: u16, _color: u16) {}
    fn begin_pixels(&self, _x: u16, _y: u16, _w: u16, _h: u16) {}
    fn write_pixel(&self, _color: u16) {}
    fn send_command(&self, _cmd: u16) {}
    fn send_data(&self, _data: u16) {}
}

#[derive(Clone)]
pub struct MockFlash;
impl FlashBackend for MockFlash {
    fn read(&self, _addr: u32, buf: &mut [u8]) {
        buf.fill(0);
    }
    fn write(&self, _addr: u32, _data: &[u8]) {}
    fn erase_sector(&self, _addr: u32) {}
    fn read_id(&self) -> (u8, u8, u8) {
        (0xEF, 0x40, 0x19)
    }
}

pub struct MockTouch;
impl TouchBackend for MockTouch {
    fn is_active(&self) -> bool {
        false
    }
    fn read_screen(&self, _cal: &CalParams) -> Option<(u16, u16)> {
        None
    }
}

pub struct MockBacklight;
impl BacklightBackend for MockBacklight {
    fn set_brightness(&self, _percent: u8) {}
    fn brightness(&self) -> u8 {
        0
    }
}

pub struct MockRtc;
impl RtcBackend for MockRtc {
    fn read_time(&self) -> DateTime {
        DateTime::zero()
    }
    fn set_time(&self, _dt: &DateTime) {}
    fn is_valid(&self) -> bool {
        false
    }
    fn clear_vl_flag(&self) {}
}

pub struct MockSdCard;
impl SdCardBackend for MockSdCard {
    fn card_type(&self) -> CardType {
        CardType::SdHc
    }
    fn read_block(&self, _block: u32, _buf: &mut [u8; 512]) -> Result<(), SdError> {
        Err(SdError::InitFailed)
    }
    fn release_bus(&self) {}
    fn acquire_bus(&self) {}
}

pub struct MockUsart;
impl UsartBackend for MockUsart {
    fn write_byte(&self, _byte: u8) {}
    fn flush(&self) {}
}

pub struct MockSystick;
impl SystickBackend for MockSystick {
    fn init() {}
    fn delay_ms(_ms: u32) {}
    fn millis() -> u32 {
        0
    }
}

// Convenience constructors so the (not-yet-generic) framework's concrete-alias call
// sites — e.g. `Flash::new()`, `Rtc::init()` — resolve under `--features mock`.
// Defined here because the wrappers are foreign to the framework crate.
impl crate::flash::FlashImpl<MockFlash> {
    pub fn new() -> Self {
        Self::with_backend(MockFlash)
    }
    pub fn init() -> Self {
        Self::with_backend(MockFlash)
    }
}
impl crate::rtc::RtcImpl<MockRtc> {
    pub fn init() -> Self {
        Self::with_backend(MockRtc)
    }
}

/// Marker platform binding every associated type to a mock backend.
pub struct MockPlatform;
impl Platform for MockPlatform {
    type LcdB = MockLcd;
    type FlashB = MockFlash;
    type TouchB = MockTouch;
    type BacklightB = MockBacklight;
    type RtcB = MockRtc;
    type SdCardB = MockSdCard;
    type UsartB = MockUsart;
    type SystickB = MockSystick;
}
