//! Audio backend for N32903 — the SPU (`n32903_rs::spu`) driving the
//! on-chip DAC (headphone/speaker output).
//!
//! The SPU plays a small DMA ring buffer in stereo PCM16 and interrupts
//! each time it finishes one half; the interrupt refills that half from a
//! sample queue that `write` (the core's sound player, once per main-loop
//! pass) fills. Mono sounds are duplicated to both channels. When the
//! queue runs dry the half is filled with silence; two silent halves in a
//! row mean everything queued has been output (`pending` = 0).
//!
//! The SPU is opened and the DAC powered up (a ~0.75 s pop-suppression
//! ramp) on the first `start`, not at boot. `stop` stops the engine; the
//! DAC stays powered, which avoids a pop per sound.
//!
//! The DAC drives the speaker through an 8002-type amplifier (LM4871
//! class) whose SHUTDOWN pin is wired to GPB1: high = shutdown, low =
//! amplifier on (the opposite of the vendor demo boards' `SPK_EN`). It is
//! switched on
//! while a sound plays (after the ring is silent, so it doesn't pop) and
//! off at `stop`, so the speaker doesn't hiss between sounds.
//!
//! The queue is single-producer (main loop) / single-consumer (ISR) on one
//! in-order core: every queue access is volatile, so the compiler keeps
//! samples before the counter update (armv5te has no fence instructions).

use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};

use ferrite_core::audio::AudioBackend;
use n32903_rs::aic;
use n32903_rs::clock::{self, ClockNode};
use n32903_rs::gpio::{self, GpioDirection, GpioPort};
use n32903_rs::registers::IRQ_SPU;
use n32903_rs::spu;

/// DMA ring size in bytes: two halves of 1024 stereo frames (~21 ms at
/// 48 kHz) — the output latency.
const DMA_SIZE: usize = 8 * 1024;
const HALF_FRAMES: usize = DMA_SIZE / 2 / 4;

/// Sample queue length (interleaved i16): ~0.7 s of 48 kHz mono, the
/// longest main-loop stall that doesn't drop out.
const QUEUE: usize = 32 * 1024;

/// Speaker amplifier shutdown: GPB1, and the level that turns it on
/// (the 8002's SHUTDOWN input is active high).
const AMP_PORT: GpioPort = GpioPort::PortB;
const AMP_PIN: u8 = 1;
const AMP_ON_HIGH: bool = false;

/// Switch the speaker amplifier on or off (configures GPB1 as an output).
fn amp(on: bool) {
    let mask = 1u16 << AMP_PIN;
    let level = if on == AMP_ON_HIGH { mask } else { 0 };
    unsafe {
        gpio::gpio_configure_pin(AMP_PORT, AMP_PIN);
        gpio::gpio_set_output(AMP_PORT, mask, level);
        gpio::gpio_set_direction(AMP_PORT, mask, GpioDirection::Output);
    }
}

/// Channel volume (vendor `spuOpen` default; 0x3F is suggested for a
/// speaker).
const CH_VOLUME: u8 = 0x4F;

#[repr(C, align(256))]
struct DmaBuf([u8; DMA_SIZE]);

static mut DMA: DmaBuf = DmaBuf([0; DMA_SIZE]);
static mut QUEUE_BUF: [i16; QUEUE] = [0; QUEUE];
/// Free-running sample counters: written by `write`, consumed by the ISR.
static mut HEAD: u32 = 0;
static mut TAIL: u32 = 0;
/// Source channels (1 or 2) of the current sound.
static mut CHANNELS: u32 = 1;
/// Halves filled with silence since the queue last had samples.
static mut QUIET: u32 = 0;

/// Refill one played half of the DMA ring (called by the SPU interrupt).
unsafe fn refill(half: *mut u8) {
    unsafe {
        let out = half as *mut i16;
        let head = read_volatile(addr_of!(HEAD));
        let mut tail = TAIL;
        let ch = CHANNELS;
        let q = addr_of!(QUEUE_BUF) as *const i16;
        let mut had = false;
        for f in 0..HALF_FRAMES {
            let (l, r) = if head.wrapping_sub(tail) >= ch {
                had = true;
                let l = read_volatile(q.add(tail as usize % QUEUE));
                let r = if ch == 2 { read_volatile(q.add((tail + 1) as usize % QUEUE)) } else { l };
                tail = tail.wrapping_add(ch);
                (l, r)
            } else {
                (0, 0)
            };
            // Stereo PCM16: left in the low half-word of each frame.
            write_volatile(out.add(2 * f), l);
            write_volatile(out.add(2 * f + 1), r);
        }
        write_volatile(addr_of_mut!(TAIL), tail);
        QUIET = if had { 0 } else { QUIET.saturating_add(1) };
    }
}

/// SPU-backed audio output.
pub struct SpuAudio {
    /// SPU opened and DAC powered (first `start`).
    open: bool,
    rate: u32,
    volume: u8,
    running: bool,
}

/// The backend, with the speaker amplifier off until the first sound.
pub fn new() -> SpuAudio {
    amp(false);
    SpuAudio { open: false, rate: 0, volume: 100, running: false }
}

impl SpuAudio {
    fn queued() -> u32 {
        unsafe { read_volatile(addr_of!(HEAD)).wrapping_sub(read_volatile(addr_of!(TAIL))) }
    }

    fn apply_volume(&self) {
        let v = (self.volume as u32 * 0x3F / 100) as u8;
        unsafe { spu::set_volume(v, v) };
    }
}

impl AudioBackend for SpuAudio {
    fn start(&mut self, sample_rate: u32, channels: u8) -> bool {
        if !(1..=2).contains(&channels) || sample_rate == 0 {
            return false;
        }
        // Show what has been drawn so far: the first start blocks for the
        // DAC ramp, and the frame buffer is only flushed at frame end.
        crate::lcd::flush_cache();
        unsafe {
            aic::aic_disable_interrupt(IRQ_SPU);
            spu::stop_play();
            let upll_hz = clock::get_clock(ClockNode::UPll) * 1000;
            let real = if !self.open {
                let buf = addr_of_mut!(DMA) as *mut u8;
                match spu::open(buf, DMA_SIZE as u32, upll_hz, sample_rate, CH_VOLUME) {
                    Ok(real) => {
                        self.apply_volume();
                        spu::dac_on(1);
                        self.open = true;
                        real
                    }
                    Err(e) => {
                        crate::usart::log_fmt(format_args!("audio: open failed: {e:?}\r\n"));
                        return false;
                    }
                }
            } else if sample_rate != self.rate {
                spu::set_sample_rate(upll_hz, sample_rate)
            } else {
                0
            };
            self.rate = sample_rate;
            if real != 0 {
                crate::usart::log_fmt(format_args!(
                    "audio: {sample_rate} Hz (DAC {real} Hz, UPLL {upll_hz} Hz)\r\n"
                ));
            }

            // Empty queue, silent ring.
            write_volatile(addr_of_mut!(HEAD), 0);
            write_volatile(addr_of_mut!(TAIL), 0);
            CHANNELS = channels as u32;
            QUIET = 0;
            let dma = (addr_of_mut!(DMA) as u32 | spu::UNCACHED) as *mut u8;
            core::ptr::write_bytes(dma, 0, DMA_SIZE);

            spu::start_play(refill);
            aic::aic_enable_interrupt(IRQ_SPU);
        }
        amp(true);
        self.running = true;
        true
    }

    fn write(&mut self, samples: &[i16]) -> usize {
        if !self.running {
            return 0;
        }
        let n = samples.len().min(self.free_space());
        unsafe {
            let q = addr_of_mut!(QUEUE_BUF) as *mut i16;
            let head = read_volatile(addr_of!(HEAD));
            for (i, &s) in samples[..n].iter().enumerate() {
                write_volatile(q.add(head.wrapping_add(i as u32) as usize % QUEUE), s);
            }
            write_volatile(addr_of_mut!(HEAD), head.wrapping_add(n as u32));
        }
        n
    }

    fn stop(&mut self) {
        if !self.running {
            return;
        }
        crate::lcd::flush_cache();
        unsafe {
            aic::aic_disable_interrupt(IRQ_SPU);
            spu::stop_play();
            write_volatile(addr_of_mut!(TAIL), read_volatile(addr_of!(HEAD)));
        }
        amp(false);
        self.running = false;
    }

    fn set_volume(&mut self, vol: u8) {
        self.volume = vol.min(100);
        if self.open {
            self.apply_volume();
        }
    }

    fn free_space(&self) -> usize {
        if self.running { QUEUE - Self::queued() as usize } else { 0 }
    }

    fn pending(&self) -> usize {
        if !self.running {
            return 0;
        }
        let queued = Self::queued() as usize;
        // The last samples sit in the ring until two silent halves follow.
        if queued == 0 && unsafe { read_volatile(addr_of!(QUIET)) } >= 2 { 0 } else { queued.max(1) }
    }
}
