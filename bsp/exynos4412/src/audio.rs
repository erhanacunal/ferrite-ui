//! Audio backend — WM8758 codec on I2S0, the FIFO filled by `pdma0`.
//!
//! The HAL's `DmaPlayback` loops the DMA channel over a ring of two halves
//! (85 ms each) and raises one event per half. `pdma0`'s interrupt
//! (`handle_irq` in main.rs → [`dma_isr`]) refills the half that just played
//! from a frame queue, which [`I2sAudio::write`] fills from the main loop.
//! The ISR keeps the output going while the main loop is busy; a long stall
//! only drains the queue (0.68 s), then silence plays.
//!
//! The link runs at a fixed 48 kHz, 16-bit stereo: `write` resamples (linear
//! interpolation) from the sound's rate and duplicates mono to both channels.
//!
//! Bring-up (codec rail + I2C1, EPLL, I2S0, DMA, the codec's 500 ms VMID
//! charge, speaker amplifier) happens at boot ([`I2sAudio::power_on`] from
//! main.rs), so its pop is at boot and the first sound starts at once. The
//! DMA then keeps playing (silence when idle) and the outputs stay on;
//! `stop` only empties the queue. Speakers play when no jack is in (checked
//! at each `start`).
//!
//! Start latency: with the ring idle, the ISR path would play a new sound
//! only after the half being read and the next (silent) one, up to 170 ms.
//! [`prime`] instead writes it into the ring just ahead of the DMA's read
//! position, ~3 ms.

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use exynos_4412_rs::drivers::cmu::Cmu;
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::i2s::{self, DmaPlayback, DmaProgram, DmaRing, I2s};
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::pl330::PDMA0_IRQ;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::drivers::wm8758::{self, MclkDiv, Wm8758};
use exynos_4412_rs::{cpu, mmu, println};
use ferrite_core::audio::AudioBackend;

const OUT_RATE: u32 = i2s::SAMPLE_RATE_HZ;

/// Frame queue between `write` (main loop) and [`dma_isr`]: 0.68 s at 48 kHz.
const QUEUE: usize = 32 * 1024;
const MASK: usize = QUEUE - 1;
/// Frames kept free so one input frame's worth of output always fits.
const QUEUE_SLACK: usize = 8;

static mut QUEUE_BUF: [u32; QUEUE] = [0; QUEUE];
/// Free-running indices: `HEAD` is advanced only by the ISR, `TAIL` only by
/// the main loop (or with IRQs masked, in `flush`).
static HEAD: AtomicUsize = AtomicUsize::new(0);
static TAIL: AtomicUsize = AtomicUsize::new(0);
/// Ring halves still holding queued samples that have not played out.
static AUDIBLE: AtomicU8 = AtomicU8::new(0);

static mut RING: DmaRing = DmaRing::new();
static mut PROGRAM: DmaProgram = DmaProgram::new();
static mut PLAYBACK: Option<DmaPlayback> = None;

/// `setVolume` 0–100 → dB: 1 = -48 dB … 100 = 0 dB (0 mutes).
const VOL_MIN_DB: i32 = -48;
const DEFAULT_VOLUME: u8 = 80;

/// `pdma0` interrupt: refill the half that has played. IRQ context.
pub fn dma_isr() {
    // SAFETY: the main loop touches PLAYBACK only before the interrupt is
    // enabled at the GIC or with IRQs masked.
    let Some(pb) = (unsafe { (*addr_of_mut!(PLAYBACK)).as_mut() }) else {
        return;
    };
    pb.service(|half| {
        let tail = TAIL.load(Ordering::Acquire);
        let mut head = HEAD.load(Ordering::Relaxed);
        let mut any = false;
        // SAFETY: the ISR reads only the slots in [head, tail), which the
        // producer published with the Release store of TAIL.
        let buf = unsafe { &*addr_of_mut!(QUEUE_BUF) };
        for frame in half.iter_mut() {
            if head != tail {
                *frame = buf[head & MASK];
                head = head.wrapping_add(1);
                any = true;
            } else {
                *frame = 0;
            }
        }
        HEAD.store(head, Ordering::Release);
        // A half filled now plays after the one being read: two halves.
        let left = if any { 2 } else { AUDIBLE.load(Ordering::Relaxed).saturating_sub(1) };
        AUDIBLE.store(left, Ordering::Relaxed);
    });
}

/// Frames ahead of the DMA read position `prime` starts at (the I2S FIFO
/// holds 64 already).
const PRIME_LEAD: usize = 128;
const HALF: usize = i2s::DMA_HALF_FRAMES;

/// With the ring idle (only silence in it), move queued frames into it from
/// just ahead of the DMA's read position to the end of the next half, so
/// they play now rather than after two halves. `force` = prime with what is
/// queued even if it doesn't fill that span (the sound is fully queued);
/// otherwise wait until it does, so no silence is left between the primed
/// frames and the ISR's next refill.
fn prime(force: bool) {
    if AUDIBLE.load(Ordering::Relaxed) != 0 || queued() == 0 {
        return;
    }
    let masked = cpu::mask_irq();
    // SAFETY: IRQs are masked, so the ISR (the other consumer of the queue
    // and user of the playback) does not run.
    if let Some(pb) = unsafe { (*addr_of_mut!(PLAYBACK)).as_mut() }
        && let Some(pos) = pb.position()
        && AUDIBLE.load(Ordering::Relaxed) == 0
    {
        let start = pos + PRIME_LEAD;
        // From `start` to the end of the half after the one being read.
        let span = 2 * HALF - (start - pos / HALF * HALF);
        let tail = TAIL.load(Ordering::Acquire);
        let mut head = HEAD.load(Ordering::Relaxed);
        if force || tail.wrapping_sub(head) >= span {
            let ring = pb.ring();
            // SAFETY: [head, tail) is published; IRQs masked (see above).
            let buf = unsafe { &*addr_of_mut!(QUEUE_BUF) };
            for k in 0..span {
                if head == tail {
                    break;
                }
                let i = (start + k) % (2 * HALF);
                ring.0[i / HALF][i % HALF] = buf[head & MASK];
                head = head.wrapping_add(1);
            }
            HEAD.store(head, Ordering::Release);
            AUDIBLE.store(2, Ordering::Relaxed);
            mmu::clean_dcache_range(ring.0.as_ptr() as usize, size_of::<DmaRing>());
        }
    }
    cpu::restore_irq(masked);
}

fn queued() -> usize {
    TAIL.load(Ordering::Relaxed).wrapping_sub(HEAD.load(Ordering::Acquire))
}

/// Drop everything queued and silence the ring, so a stop is immediate.
fn flush() {
    let masked = cpu::mask_irq();
    TAIL.store(HEAD.load(Ordering::Relaxed), Ordering::Release);
    AUDIBLE.store(0, Ordering::Relaxed);
    // SAFETY: IRQs are masked, so the ISR does not hold the playback.
    if let Some(pb) = unsafe { (*addr_of_mut!(PLAYBACK)).as_mut() } {
        let ring = pb.ring();
        for half in ring.0.iter_mut() {
            half.fill(0);
        }
        mmu::clean_dcache_range(ring.0.as_ptr() as usize, size_of::<DmaRing>());
    }
    cpu::restore_irq(masked);
}

fn volume_db(vol: u8) -> i8 {
    (VOL_MIN_DB + (i32::from(vol.min(100)) * -VOL_MIN_DB + 50) / 100) as i8
}

pub struct I2sAudio {
    pmic: Max77686,
    codec: Option<Wm8758>,
    /// Bring-up failed once: don't retry (and block 0.5 s) on every play.
    failed: bool,
    playing: bool,
    speakers: bool,
    volume: u8,
    channels: u8,
    rate: u32,
    /// Input frames per output frame, 16.16.
    step: u32,
    /// Position of the next output frame between `prev` and the next input
    /// frame, 16.16.
    phase: u32,
    prev: (i16, i16),
}

/// The backend for `Ctx::audio`. `pmic` (I2C0, initialised) powers the
/// codec's rail on the first `start`.
pub fn new(pmic: Max77686) -> I2sAudio {
    I2sAudio {
        pmic,
        codec: None,
        failed: false,
        playing: false,
        speakers: false,
        volume: DEFAULT_VOLUME,
        channels: 2,
        rate: OUT_RATE,
        step: 1 << 16,
        phase: 0,
        prev: (0, 0),
    }
}

impl I2sAudio {
    /// Bring the output up now (boot), instead of on the first `start`.
    /// Blocks ~0.5 s. Call after `Gic::init`. Returns false if it failed
    /// (sounds then don't play).
    pub fn power_on(&mut self) -> bool {
        self.codec().is_some()
    }

    /// Codec, clocks, I2S0, DMA (playing silence) and the interrupt, then the
    /// codec's outputs. Blocks ~0.5 s (VMID charge).
    fn bring_up(&mut self) -> Option<Wm8758> {
        let mct = Mct::new();
        let codec = wm8758::power_up(self.pmic, mct)
            .map_err(|e| println!("audio: codec power-up ERROR {:?}", e))
            .ok()?;
        i2s::set_clocks(mct).map_err(|e| println!("audio: EPLL ERROR {:?}", e)).ok()?;
        i2s::take_pins();
        let i2s = I2s::new();
        i2s.configure();
        Cmu::new().open_pdma0_gate();

        // SAFETY: the only references to the ring and program, taken once
        // (bring-up runs at most once: `codec` / `failed` guard it).
        let (ring, program) = unsafe { (&mut *addr_of_mut!(RING), &mut *addr_of_mut!(PROGRAM)) };
        let mut pb = DmaPlayback::new(ring, program);
        pb.start(i2s, mct).map_err(|e| println!("audio: DMA ERROR {:?}", e)).ok()?;
        // SAFETY: the interrupt is not enabled yet.
        unsafe { *addr_of_mut!(PLAYBACK) = Some(pb) };
        let gic = Gic::new();
        gic.set_target(PDMA0_IRQ, 0);
        gic.enable(PDMA0_IRQ);

        // The DMA plays silence through the VMID charge, as the HAL's test does.
        codec
            .start_headphones(mct, MclkDiv::Div2, volume_db(self.volume))
            .map_err(|e| println!("audio: codec start ERROR {:?}", e))
            .ok()?;
        if self.volume == 0 {
            let _ = codec.set_mute(true);
        }
        // Amplifier on now (muted outputs first, see `set_speakers`), while
        // only silence plays: no pop at the first sound.
        self.set_speakers(codec, !wm8758::jack_in());
        Some(codec)
    }

    fn codec(&mut self) -> Option<Wm8758> {
        if self.codec.is_none() && !self.failed {
            self.codec = self.bring_up();
            self.failed = self.codec.is_none();
        }
        self.codec
    }

    fn set_speakers(&mut self, codec: Wm8758, on: bool) {
        if on != self.speakers {
            self.speakers = on;
            let _ = codec.set_speakers(on, volume_db(self.volume));
        }
    }

    /// Output frames one more input frame can produce.
    fn frames_per_input(&self) -> usize {
        (OUT_RATE.div_ceil(self.rate.max(1)) + 1) as usize
    }
}

impl AudioBackend for I2sAudio {
    fn start(&mut self, sample_rate: u32, channels: u8) -> bool {
        if sample_rate == 0 || !(1..=2).contains(&channels) {
            return false;
        }
        let Some(codec) = self.codec() else { return false };
        flush();
        self.rate = sample_rate;
        self.channels = channels;
        self.step = ((u64::from(sample_rate) << 16) / u64::from(OUT_RATE)) as u32;
        self.phase = 0;
        self.prev = (0, 0);
        self.playing = true;
        // `jack_in` is valid once the headphone outputs are on.
        self.set_speakers(codec, !wm8758::jack_in());
        true
    }

    fn write(&mut self, samples: &[i16]) -> usize {
        if !self.playing {
            return 0;
        }
        let ch = self.channels as usize;
        let need = self.frames_per_input();
        let mut tail = TAIL.load(Ordering::Relaxed);
        let head = HEAD.load(Ordering::Acquire);
        // SAFETY: the producer writes only slots outside [head, tail), which
        // the ISR doesn't read until TAIL is published below.
        let buf = unsafe { &mut *addr_of_mut!(QUEUE_BUF) };
        let mut used = 0;
        for input in samples.chunks_exact(ch) {
            if QUEUE - tail.wrapping_sub(head) < need + QUEUE_SLACK {
                break;
            }
            let cur = (input[0], input[ch - 1]);
            while self.phase < 1 << 16 {
                // (b - a) * phase needs 33 bits.
                let lerp = |a: i16, b: i16| {
                    (i64::from(a) + ((i64::from(b) - i64::from(a)) * i64::from(self.phase) >> 16)) as i16
                };
                buf[tail & MASK] = i2s::frame(lerp(self.prev.0, cur.0), lerp(self.prev.1, cur.1));
                tail = tail.wrapping_add(1);
                self.phase += self.step;
            }
            self.phase -= 1 << 16;
            self.prev = cur;
            used += ch;
        }
        TAIL.store(tail, Ordering::Release);
        prime(false);
        used
    }

    fn stop(&mut self) {
        if !self.playing {
            return;
        }
        self.playing = false;
        flush();
    }

    fn set_volume(&mut self, vol: u8) {
        self.volume = vol.min(100);
        let Some(codec) = self.codec else { return };
        let db = volume_db(self.volume);
        let _ = codec.set_mute(self.volume == 0);
        let _ = codec.set_headphone_db(db);
        if self.speakers {
            let _ = codec.set_speaker_db(db);
        }
    }

    fn free_space(&self) -> usize {
        if !self.playing {
            return 0;
        }
        let free = QUEUE.saturating_sub(queued() + QUEUE_SLACK + self.frames_per_input());
        let frames = (free as u64 * u64::from(self.rate) / u64::from(OUT_RATE)) as usize;
        frames * self.channels as usize
    }

    fn pending(&self) -> usize {
        // `pump` asks only once the whole sound is queued: a sound too short
        // to fill the prime span starts now too.
        if self.playing {
            prime(true);
        }
        queued() + usize::from(AUDIBLE.load(Ordering::Relaxed) > 0)
    }
}
