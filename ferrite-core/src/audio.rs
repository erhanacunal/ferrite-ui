/// Backend abstraction for audio output.
///
/// Boards with an audio DAC/codec (e.g. F1C100s, Nuvoton N3290x) implement
/// this trait; boards without audio hardware use [`NoAudio`]. A platform
/// advertises real audio support via `Platform::CAPS & CAP_AUDIO`
/// (see `platform.rs`) — keep that bit in sync with the backend choice,
/// and mirror it in `tools/devices/*.json` (`caps.audio`).
///
/// This is scaffolding for the audio milestone: the trait and null backend
/// exist so BSPs compile against a stable seam. VM builtins (`audioPlay`
/// etc.) and resource streaming land separately.
pub trait AudioBackend {
    /// Begin playback at the given sample rate / channel count.
    /// Returns `false` if unsupported or busy.
    fn start(&mut self, sample_rate: u16, channels: u8) -> bool;

    /// Queue PCM samples; returns the number accepted (0 = full or
    /// unsupported). Callers must not assume the whole slice was taken.
    fn write(&mut self, samples: &[i16]) -> usize;

    /// Stop playback and release the output path.
    fn stop(&mut self);

    /// Output volume, 0–100.
    fn set_volume(&mut self, vol: u8);

    /// Samples the backend can accept right now without dropping any.
    fn free_space(&self) -> usize {
        0
    }
}

/// Null backend for boards without audio hardware.
pub struct NoAudio;

impl AudioBackend for NoAudio {
    fn start(&mut self, _sample_rate: u16, _channels: u8) -> bool {
        false
    }
    fn write(&mut self, _samples: &[i16]) -> usize {
        0
    }
    fn stop(&mut self) {}
    fn set_volume(&mut self, _vol: u8) {}
}

/// Generic audio wrapper — backend-agnostic pass-throughs.
pub struct AudioImpl<B: AudioBackend> {
    be: B,
}

impl<B: AudioBackend> AudioImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self { be }
    }

    #[inline]
    pub fn start(&mut self, sample_rate: u16, channels: u8) -> bool {
        self.be.start(sample_rate, channels)
    }

    #[inline]
    pub fn write(&mut self, samples: &[i16]) -> usize {
        self.be.write(samples)
    }

    #[inline]
    pub fn stop(&mut self) {
        self.be.stop()
    }

    #[inline]
    pub fn set_volume(&mut self, vol: u8) {
        self.be.set_volume(vol)
    }

    #[inline]
    pub fn free_space(&self) -> usize {
        self.be.free_space()
    }
}

impl AudioImpl<NoAudio> {
    /// Convenience constructor for boards without audio hardware.
    pub fn none() -> Self {
        Self { be: NoAudio }
    }
}

#[cfg(feature = "mock")]
pub type Audio = AudioImpl<NoAudio>;
