/// Backend abstraction for JPEG decoding.
///
/// Boards with a JPEG decoder (e.g. the Nuvoton N3290x JPEG engine)
/// implement this trait; boards without one use [`NoJpeg`]. A platform
/// advertises it via `Platform::CAPS & CAP_JPEG` (see `platform.rs`) —
/// keep that bit in sync with the backend choice, and mirror it in
/// `tools/devices/*.json` (`caps.jpeg`), which makes the build tools accept
/// `"mode": "jpeg"` images for the board.
///
/// JPEG images are FI resources with mode 3 (see `image.rs`). They are
/// decoded once, when the image is loaded, into an RGB565 buffer on the
/// heap; drawing copies from that buffer. Only boards with plenty of RAM
/// should enable this.
extern crate alloc;

use alloc::alloc::{Layout, alloc, dealloc};
use core::ptr::NonNull;

pub trait JpegBackend {
    /// `false` only for [`NoJpeg`]: lets callers skip reading the stream.
    const AVAILABLE: bool = true;

    /// Decode the baseline JPEG `src` into `out` as RGB565, `stride` pixels
    /// per row. `out` holds at least `stride` × (height rounded up to 16)
    /// pixels, `stride` is at least the width rounded up to 16. Both
    /// buffers start on a [`BUF_ALIGN`] boundary and their byte lengths are
    /// multiples of it. Returns `false` on any error.
    fn decode_rgb565(&self, src: &[u8], out: &mut [u16], stride: u16) -> bool;
}

/// Null backend for boards without a JPEG decoder.
pub struct NoJpeg;

impl JpegBackend for NoJpeg {
    const AVAILABLE: bool = false;

    fn decode_rgb565(&self, _src: &[u8], _out: &mut [u16], _stride: u16) -> bool {
        false
    }
}

/// Generic JPEG wrapper — backend-agnostic pass-throughs.
pub struct JpegImpl<B: JpegBackend> {
    be: B,
}

impl<B: JpegBackend> JpegImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self { be }
    }

    #[allow(dead_code)]
    pub(crate) fn backend(&self) -> &B {
        &self.be
    }

    #[inline]
    pub fn available(&self) -> bool {
        B::AVAILABLE
    }

    #[inline]
    pub fn decode_rgb565(&self, src: &[u8], out: &mut [u16], stride: u16) -> bool {
        self.be.decode_rgb565(src, out, stride)
    }
}

impl JpegImpl<NoJpeg> {
    /// Convenience constructor for boards without a JPEG decoder.
    pub fn none() -> Self {
        Self { be: NoJpeg }
    }
}

#[cfg(feature = "mock")]
pub type Jpeg = JpegImpl<NoJpeg>;

/// Alignment and length granularity of [`AlignedBuf`] — a cache line on
/// the boards whose decoders write memory by DMA.
pub const BUF_ALIGN: usize = 32;

/// Zeroed heap buffer that starts on a [`BUF_ALIGN`] boundary and whose
/// length is rounded up to a multiple of it. Allocation failure returns
/// `None` instead of aborting, so a too-large image is skipped.
pub struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
}

impl AlignedBuf {
    pub fn new(len: usize) -> Option<Self> {
        let len = len.max(1).checked_next_multiple_of(BUF_ALIGN)?;
        let layout = Layout::from_size_align(len, BUF_ALIGN).ok()?;
        // SAFETY: `layout` has a non-zero size.
        let ptr = NonNull::new(unsafe { alloc(layout) })?;
        // SAFETY: freshly allocated, `len` bytes.
        unsafe { ptr.as_ptr().write_bytes(0, len) };
        Some(Self { ptr, len })
    }

    pub fn bytes(&self) -> &[u8] {
        // SAFETY: owned allocation of `len` initialized bytes.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as `bytes`, borrowed mutably.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    pub fn u16s(&self) -> &[u16] {
        // SAFETY: BUF_ALIGN-aligned, even length.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr().cast(), self.len / 2) }
    }

    pub fn u16s_mut(&mut self) -> &mut [u16] {
        // SAFETY: as `u16s`, borrowed mutably.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr().cast(), self.len / 2) }
    }

    /// Heap bytes held.
    pub fn len(&self) -> usize {
        self.len
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this exact layout.
        unsafe { dealloc(self.ptr.as_ptr(), Layout::from_size_align_unchecked(self.len, BUF_ALIGN)) }
    }
}
