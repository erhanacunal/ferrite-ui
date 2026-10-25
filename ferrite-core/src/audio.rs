/// Backend abstraction for audio output.
///
/// Boards with an audio DAC/codec (e.g. F1C100s, Nuvoton N3290x) implement
/// this trait; boards without audio hardware use [`NoAudio`]. A platform
/// advertises real audio support via `Platform::CAPS & CAP_AUDIO`
/// (see `platform.rs`) — keep that bit in sync with the backend choice,
/// and mirror it in `tools/devices/*.json` (`caps.audio`).
///
/// Sounds are `RES_SOUND` resources (header + PCM16, see [`SOUND_HEADER`]).
/// [`AudioImpl::play`] opens one and [`AudioImpl::pump`] — called once per
/// main-loop pass — streams it from flash into the backend's queue.
use crate::flash::{FlashBackend, FlashImpl};
use crate::fs::{Fs, RES_SOUND};

pub trait AudioBackend {
    /// Begin playback at the given sample rate / channel count (1 or 2).
    /// Returns `false` if unsupported. Restarts a running playback.
    fn start(&mut self, sample_rate: u32, channels: u8) -> bool;

    /// Queue PCM samples (interleaved when stereo); returns the number
    /// accepted (0 = full or unsupported). Callers must not assume the
    /// whole slice was taken.
    fn write(&mut self, samples: &[i16]) -> usize;

    /// Stop playback and release the output path.
    fn stop(&mut self);

    /// Output volume, 0–100.
    fn set_volume(&mut self, vol: u8);

    /// Samples the backend can accept right now without dropping any.
    fn free_space(&self) -> usize {
        0
    }

    /// Samples accepted but not played yet; 0 once everything written has
    /// reached the output.
    fn pending(&self) -> usize {
        0
    }
}

/// Null backend for boards without audio hardware.
pub struct NoAudio;

impl AudioBackend for NoAudio {
    fn start(&mut self, _sample_rate: u32, _channels: u8) -> bool {
        false
    }
    fn write(&mut self, _samples: &[i16]) -> usize {
        0
    }
    fn stop(&mut self) {}
    fn set_volume(&mut self, _vol: u8) {}
}

/// `RES_SOUND` header: magic "SN", version u8 (= 1), channels u8 (1|2),
/// sample rate u32 LE; PCM16 LE samples (interleaved L/R) follow.
pub const SOUND_HEADER: usize = 8;
const SOUND_MAGIC: [u8; 2] = *b"SN";
const SOUND_VERSION: u8 = 1;

/// Samples moved from flash per backend write.
const PUMP_CHUNK: usize = 256;

/// The sound being streamed.
#[derive(Clone, Copy)]
struct Track {
    /// Flash address of the first sample.
    data: u32,
    /// Sample data length in bytes.
    len: u32,
    /// Next byte to queue.
    pos: u32,
    channels: u8,
    looping: bool,
}

/// Generic audio wrapper — backend pass-throughs plus the sound player.
pub struct AudioImpl<B: AudioBackend> {
    be: B,
    track: Option<Track>,
}

impl<B: AudioBackend> AudioImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self { be, track: None }
    }

    #[inline]
    pub fn start(&mut self, sample_rate: u32, channels: u8) -> bool {
        self.be.start(sample_rate, channels)
    }

    #[inline]
    pub fn write(&mut self, samples: &[i16]) -> usize {
        self.be.write(samples)
    }

    /// Stop playback (and the current sound).
    #[inline]
    pub fn stop(&mut self) {
        self.track = None;
        self.be.stop()
    }

    #[inline]
    pub fn set_volume(&mut self, vol: u8) {
        self.be.set_volume(vol.min(100))
    }

    #[inline]
    pub fn free_space(&self) -> usize {
        self.be.free_space()
    }

    /// True while a sound is playing (until its last sample is output).
    #[inline]
    pub fn playing(&self) -> bool {
        self.track.is_some()
    }

    /// Start the `RES_SOUND` resource `name`, replacing the current sound;
    /// `looping` repeats it until [`stop`](Self::stop). `false` if it isn't
    /// found, is malformed, or the backend can't play it.
    pub fn play<F: FlashBackend>(
        &mut self,
        fs: &Fs,
        flash: &FlashImpl<F>,
        name: &[u8],
        looping: bool,
    ) -> bool {
        self.stop();
        let Some(entry) = fs.find_kind(flash, name, RES_SOUND) else {
            return false;
        };
        let mut hdr = [0u8; SOUND_HEADER];
        if (entry.size as usize) < SOUND_HEADER {
            return false;
        }
        flash.read(entry.offset, &mut hdr);
        let channels = hdr[3];
        let rate = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        if hdr[..2] != SOUND_MAGIC || hdr[2] != SOUND_VERSION || !(1..=2).contains(&channels) || rate == 0 {
            return false;
        }
        if !self.be.start(rate, channels) {
            return false;
        }
        // Whole frames only.
        let frame = 2 * channels as u32;
        let len = (entry.size - SOUND_HEADER as u32) / frame * frame;
        self.track = Some(Track { data: entry.offset + SOUND_HEADER as u32, len, pos: 0, channels, looping });
        self.pump(flash);
        true
    }

    /// Feed the backend from the current sound; once it has played out,
    /// stop the backend. Call once per main-loop pass.
    pub fn pump<F: FlashBackend>(&mut self, flash: &FlashImpl<F>) {
        let Some(mut t) = self.track else { return };
        let mut bytes = [0u8; PUMP_CHUNK * 2];
        let mut samples = [0i16; PUMP_CHUNK];
        loop {
            if t.pos >= t.len {
                if t.looping && t.len > 0 {
                    t.pos = 0;
                } else {
                    break;
                }
            }
            let ch = t.channels as usize;
            let room = self.be.free_space().min(PUMP_CHUNK) / ch * ch;
            if room == 0 {
                break;
            }
            let n = room.min(((t.len - t.pos) / 2) as usize);
            flash.read(t.data + t.pos, &mut bytes[..n * 2]);
            for (s, b) in samples[..n].iter_mut().zip(bytes.chunks_exact(2)) {
                *s = i16::from_le_bytes([b[0], b[1]]);
            }
            let taken = self.be.write(&samples[..n]) / ch * ch;
            t.pos += taken as u32 * 2;
            if taken < n {
                break;
            }
        }
        if t.pos >= t.len && !t.looping && self.be.pending() == 0 {
            self.stop();
        } else {
            self.track = Some(t);
        }
    }
}

impl AudioImpl<NoAudio> {
    /// Convenience constructor for boards without audio hardware.
    pub fn none() -> Self {
        Self { be: NoAudio, track: None }
    }
}

#[cfg(feature = "mock")]
pub type Audio = AudioImpl<NoAudio>;

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use alloc::vec::Vec;

    struct MemFlash(Vec<u8>);
    impl FlashBackend for MemFlash {
        fn read(&self, addr: u32, buf: &mut [u8]) {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = *self.0.get(addr as usize + i).unwrap_or(&0xFF);
            }
        }
        fn write(&self, _: u32, _: &[u8]) {}
        fn erase_sector(&self, _: u32) {}
        fn read_id(&self) -> (u8, u8, u8) {
            (0, 0, 0)
        }
    }

    /// Records what it is given; `room` samples of queue, drained by hand.
    #[derive(Default)]
    struct Sink {
        started: Option<(u32, u8)>,
        got: Vec<i16>,
        room: usize,
        queued: usize,
        stops: u32,
    }
    impl AudioBackend for Sink {
        fn start(&mut self, rate: u32, channels: u8) -> bool {
            self.started = Some((rate, channels));
            true
        }
        fn write(&mut self, s: &[i16]) -> usize {
            let n = s.len().min(self.room - self.queued);
            self.got.extend_from_slice(&s[..n]);
            self.queued += n;
            n
        }
        fn stop(&mut self) {
            self.stops += 1;
        }
        fn set_volume(&mut self, _: u8) {}
        fn free_space(&self) -> usize {
            self.room - self.queued
        }
        fn pending(&self) -> usize {
            self.queued
        }
    }

    const BASE: u32 = 0x1000;

    /// Flash with an FS holding one sound "beep".
    fn flash_with(channels: u8, samples: &[i16]) -> FlashImpl<MemFlash> {
        let mut res = Vec::new();
        res.extend_from_slice(b"SN");
        res.push(1);
        res.push(channels);
        res.extend_from_slice(&22050u32.to_le_bytes());
        for s in samples {
            res.extend_from_slice(&s.to_le_bytes());
        }
        let mut img = alloc::vec![0xFFu8; BASE as usize];
        let data = BASE + 16 + 32;
        let mut hdr = [0u8; 16];
        hdr[0..4].copy_from_slice(b"FERR");
        hdr[4..6].copy_from_slice(&3u16.to_le_bytes());
        hdr[10..12].copy_from_slice(&1u16.to_le_bytes());
        let mut e = [0u8; 32];
        e[..4].copy_from_slice(b"beep");
        e[16] = RES_SOUND;
        e[20..24].copy_from_slice(&data.to_le_bytes());
        e[24..28].copy_from_slice(&(res.len() as u32).to_le_bytes());
        img.extend_from_slice(&hdr);
        img.extend_from_slice(&e);
        img.extend_from_slice(&res);
        FlashImpl::with_backend(MemFlash(img))
    }

    fn sink(room: usize) -> AudioImpl<Sink> {
        AudioImpl::with_backend(Sink { room, ..Default::default() })
    }

    #[test]
    fn streams_whole_sound_then_stops_when_drained() {
        let pcm: Vec<i16> = (0..700).map(|i| i as i16 - 350).collect();
        let flash = flash_with(1, &pcm);
        let fs = Fs::mount(&flash, BASE).ok().unwrap();
        let mut a = sink(300);
        assert!(a.play(&fs, &flash, b"beep", false));
        assert_eq!(a.be.started, Some((22050, 1)));
        assert_eq!(a.be.got.len(), 300);
        // Output drains, the player refills.
        while a.playing() {
            a.be.queued = 0;
            a.pump(&flash);
        }
        assert_eq!(a.be.got, pcm);
        assert_eq!(a.be.stops, 2); // play() resets, then end of sound
    }

    #[test]
    fn stereo_keeps_whole_frames_and_loops() {
        let pcm: Vec<i16> = (0..10).collect();
        let flash = flash_with(2, &pcm);
        let fs = Fs::mount(&flash, BASE).ok().unwrap();
        let mut a = sink(7); // odd room: only 3 frames fit
        assert!(a.play(&fs, &flash, b"beep", true));
        assert_eq!(a.be.got, [0, 1, 2, 3, 4, 5]);
        for _ in 0..3 {
            a.be.queued = 0;
            a.pump(&flash);
        }
        assert!(a.playing());
        assert_eq!(&a.be.got[..12], &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1]);
        a.stop();
        assert!(!a.playing());
    }

    #[test]
    fn missing_or_bad_sound_does_not_play() {
        let flash = flash_with(3, &[1, 2, 3]);
        let fs = Fs::mount(&flash, BASE).ok().unwrap();
        let mut a = sink(64);
        assert!(!a.play(&fs, &flash, b"beep", false)); // 3 channels
        assert!(!a.play(&fs, &flash, b"nope", false));
        assert!(!a.playing());
    }
}
