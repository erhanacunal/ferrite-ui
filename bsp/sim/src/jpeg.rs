//! Software JPEG decoder for the simulator (`jpeg-decoder` crate), standing
//! in for a board's hardware decoder so `"mode": "jpeg"` images show in the
//! sim. Accepts only what the N32903 engine accepts — sequential DCT, three
//! components — so a stream that works here works on the device.

use ferrite_core::jpeg::JpegBackend;
use jpeg_decoder::{CodingProcess, Decoder, PixelFormat};

pub struct SoftJpeg;

impl JpegBackend for SoftJpeg {
    fn decode_rgb565(&self, src: &[u8], out: &mut [u16], stride: u16) -> bool {
        let mut dec = Decoder::new(src);
        let pixels = match dec.decode() {
            Ok(p) => p,
            Err(e) => {
                println!("jpeg: decode failed: {e}");
                return false;
            }
        };
        let Some(info) = dec.info() else {
            return false;
        };
        if info.coding_process != CodingProcess::DctSequential || info.pixel_format != PixelFormat::RGB24 {
            println!("jpeg: unsupported stream ({:?}, {:?})", info.coding_process, info.pixel_format);
            return false;
        }
        let (w, h, stride) = (info.width as usize, info.height as usize, stride as usize);
        if stride < w || out.len() < stride * h {
            return false;
        }
        for (y, row) in pixels.chunks_exact(w * 3).take(h).enumerate() {
            for (x, rgb) in row.chunks_exact(3).enumerate() {
                let (r, g, b) = (rgb[0] as u16, rgb[1] as u16, rgb[2] as u16);
                out[y * stride + x] = ((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3);
            }
        }
        true
    }
}
