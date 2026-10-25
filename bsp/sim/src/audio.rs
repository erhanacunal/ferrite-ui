//! Silent audio output for the simulator: consumes samples in real time
//! (like a DAC would) so the core's sound player, `soundPlaying()` and
//! looping behave as on a device, and logs each start/stop to stdout.
//! With `--audio-out <file.wav>` every sample written is also recorded,
//! and the recording is saved as a WAV file at each stop.

use std::time::Instant;

use ferrite_core::audio::AudioBackend;

/// Output queue: ~85 ms of 48 kHz mono, about the device's DMA ring.
const QUEUE: u64 = 4096;

pub struct SimAudio {
    rate: u32,
    channels: u8,
    /// Samples accepted since `start`.
    written: u64,
    started: Option<Instant>,
    out: Option<String>,
    recorded: Vec<i16>,
}

impl SimAudio {
    pub fn new(out: Option<String>) -> Self {
        Self { rate: 0, channels: 1, written: 0, started: None, out, recorded: Vec::new() }
    }

    /// Samples the virtual DAC has output since `start`.
    fn consumed(&self) -> u64 {
        let Some(t0) = self.started else { return self.written };
        let elapsed = t0.elapsed().as_secs_f64();
        ((elapsed * (self.rate as f64) * (self.channels as f64)) as u64).min(self.written)
    }

    fn save(&self) {
        let Some(path) = &self.out else { return };
        let data: Vec<u8> = self.recorded.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut wav = Vec::with_capacity(44 + data.len());
        let ch = self.channels as u32;
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&(ch as u16).to_le_bytes());
        wav.extend_from_slice(&self.rate.to_le_bytes());
        wav.extend_from_slice(&(self.rate * ch * 2).to_le_bytes());
        wav.extend_from_slice(&((ch * 2) as u16).to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        match std::fs::write(path, wav) {
            Ok(()) => println!("audio: saved {} samples to {path}", self.recorded.len()),
            Err(e) => eprintln!("audio: {path}: {e}"),
        }
    }
}

impl AudioBackend for SimAudio {
    fn start(&mut self, sample_rate: u32, channels: u8) -> bool {
        if !(1..=2).contains(&channels) || sample_rate == 0 {
            return false;
        }
        if self.started.is_some() {
            self.stop();
        }
        println!("audio: start {sample_rate} Hz, {channels} ch");
        self.rate = sample_rate;
        self.channels = channels;
        self.written = 0;
        self.started = Some(Instant::now());
        self.recorded.clear();
        true
    }

    fn write(&mut self, samples: &[i16]) -> usize {
        if self.started.is_none() {
            return 0;
        }
        let n = samples.len().min(self.free_space());
        if self.out.is_some() {
            self.recorded.extend_from_slice(&samples[..n]);
        }
        self.written += n as u64;
        n
    }

    fn stop(&mut self) {
        if self.started.take().is_some() {
            println!("audio: stop after {} samples", self.written);
            self.save();
        }
    }

    fn set_volume(&mut self, vol: u8) {
        println!("audio: volume {vol}");
    }

    fn free_space(&self) -> usize {
        if self.started.is_none() {
            return 0;
        }
        (QUEUE - (self.written - self.consumed()).min(QUEUE)) as usize
    }

    fn pending(&self) -> usize {
        if self.started.is_none() { 0 } else { (self.written - self.consumed()) as usize }
    }
}
