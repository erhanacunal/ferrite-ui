//! JPEG backend for N32903 — the SoC's JPEG engine (`n32903_rs::jpeg`).
//!
//! The engine decodes straight into the image's RGB565 buffer: the core's
//! `AlignedBuf` is cache-line aligned and a whole number of lines long, as
//! the HAL requires, and the HAL does the cache maintenance. Decoding is
//! polled and takes a few milliseconds for a full-screen image; it runs once
//! per image, when the image is loaded.

use ferrite_core::jpeg::JpegBackend;
use n32903_rs::jpeg::{self as hal, DecodeParams, OutputFormat};

/// Hardware backend: the JPEG engine.
pub struct HwJpeg;

/// Power up the engine. Needs the clock tree (it reads HCLK234).
pub fn init() -> HwJpeg {
    unsafe { hal::open() };
    HwJpeg
}

impl JpegBackend for HwJpeg {
    fn decode_rgb565(&self, src: &[u8], out: &mut [u16], stride: u16) -> bool {
        // Reject what the engine can't decode before starting it.
        if let Err(e) = hal::probe(src) {
            crate::usart::log_fmt(format_args!("jpeg: {e:?} stream\r\n"));
            return false;
        }
        // SAFETY: same allocation viewed as bytes; `u16` has no invalid
        // bit patterns, and the engine writes whole pixels.
        let out = unsafe { core::slice::from_raw_parts_mut(out.as_mut_ptr().cast::<u8>(), out.len() * 2) };
        let params = DecodeParams { format: OutputFormat::Rgb565, stride, downscale: None };
        match unsafe { hal::decode(src, out, &params) } {
            Ok(_) => true,
            Err(e) => {
                crate::usart::log_fmt(format_args!("jpeg: decode failed: {e:?}\r\n"));
                false
            }
        }
    }
}
