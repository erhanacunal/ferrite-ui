//! Main-loop profiler (`perf` feature): CPU cycles per `Runtime::step` phase,
//! reported over USART on request (`ferrite_cli.py perf`).
//!
//! The clock is `PlatformRuntime::cycles` (a free-running 32-bit counter at
//! `cycle_hz`). Each phase is short (well under one counter wrap), so its
//! length is a `wrapping_sub` of two reads, summed in u64. The window is the
//! time between pass starts, so it also holds what runs outside `step`
//! (other executor tasks, `yield_now`).
//!
//! A report covers everything since the previous one (or since boot) and
//! resets the counters.

/// Phase ids, in the order `step` runs them. Mirrored in
/// `tools/ferrite_cli.py` (`PERF_PHASES`) — keep in sync.
pub mod phase {
    /// VM bursts of every task (+ modal resume).
    pub const VM: u8 = 1;
    /// USART protocol (RX parsing, replies).
    pub const USART: u8 = 2;
    /// Touch polling and dispatch, keyboard tick.
    pub const INPUT: u8 = 3;
    /// Property animations.
    pub const ANIM: u8 = 4;
    /// Audio pump.
    pub const AUDIO: u8 = 5;
    /// Render pass (dirty or buffered, incl. on_paint).
    pub const RENDER: u8 = 6;
    /// Queued callbacks + the render pass after them.
    pub const CALLBACKS: u8 = 7;
    /// Program switch, task service, EEPROM flush.
    pub const MISC: u8 = 8;
    /// `PlatformRuntime::frame_end` (present / flush).
    pub const FRAME_END: u8 = 9;
    /// Time spent waiting for the display, inside the other phases
    /// (`perf_take_wait`): for vsync, or (exynos4412) for the presenter core
    /// to finish reading the shadow.
    pub const VSYNC_WAIT: u8 = 10;
    /// Time inside `draw_widget` (inside `RENDER` / `CALLBACKS`; includes
    /// the pixel writes and display waits it causes).
    pub const DRAW: u8 = 11;
    /// Time inside the LCD backend's pixel primitives, in any phase (timed by
    /// the BSP through [`super::add_sub`], after its display wait).
    pub const PIXELS: u8 = 12;
    /// Time another core spent presenting (copying frames to the display),
    /// in parallel with the step: not part of the window's sum. Reported by
    /// the BSP (exynos4412: CPU2's copies).
    pub const PRESENT: u8 = 13;
    /// Number of slots (ids 1..COUNT).
    pub const COUNT: usize = 13;
}

/// Event counts, reported as plain numbers (ids from 0x80). Mirrored in
/// `tools/ferrite_cli.py` (`PERF_COUNTS`).
pub mod count {
    /// `draw_widget` calls.
    pub const DRAWS: u8 = 0x81;
    /// Pixel primitive calls (`phase::PIXELS`).
    pub const PIXEL_CALLS: u8 = 0x82;
    /// Presents (`phase::PRESENT`).
    pub const PRESENTS: u8 = 0x83;
    /// KiB the presents copied.
    pub const PRESENT_KB: u8 = 0x84;
    /// Number of counts (ids 0x81..).
    pub const COUNT: usize = 4;
}

/// A value only the runtime's own thread touches (the runtime and the
/// renderer run on one core; the profiler is a diagnostic).
struct Local<T>(core::cell::UnsafeCell<T>);
// SAFETY: see the type's doc: never accessed from two threads.
unsafe impl<T> Sync for Local<T> {}

fn no_clock() -> u32 {
    0
}

static CLOCK: Local<fn() -> u32> = Local(core::cell::UnsafeCell::new(no_clock));
/// Number of sub-phases (`DRAW`, `PIXELS`, `PRESENT`).
pub const SUBS: usize = 3;

type Sub = ([u64; SUBS], [u32; count::COUNT]);

/// Cycles of the sub-phases (index id - `DRAW`) and the counts (index
/// id - 0x81; a sub-phase call counts in the same index).
static SUB: Local<Sub> = Local(core::cell::UnsafeCell::new(([0; SUBS], [0; count::COUNT])));

/// Sets the clock `now` reads (`PlatformRuntime::cycles`); the runtime does
/// it at startup.
pub fn set_clock(f: fn() -> u32) {
    unsafe { *CLOCK.0.get() = f };
}

/// The profiler clock, for code that has no `PlatformRuntime` bound.
#[inline]
pub fn now() -> u32 {
    unsafe { (*CLOCK.0.get())() }
}

/// Adds one call of `cycles` to sub-phase `id` (`phase::DRAW` / `PIXELS` /
/// `PRESENT`).
#[inline]
pub fn add_sub(id: u8, cycles: u32) {
    add_sub_n(id, cycles as u64, 1);
}

/// Adds `calls` calls taking `cycles` in all to sub-phase `id`.
#[inline]
pub fn add_sub_n(id: u8, cycles: u64, calls: u32) {
    let i = (id - phase::DRAW) as usize;
    let s = unsafe { &mut *SUB.0.get() };
    s.0[i] += cycles;
    s.1[i] = s.1[i].wrapping_add(calls);
}

/// Adds `n` to count `id` (`count::*`).
#[inline]
pub fn add_count(id: u8, n: u32) {
    let s = unsafe { &mut *SUB.0.get() };
    let c = &mut s.1[(id - 0x81) as usize];
    *c = c.wrapping_add(n);
}

/// Sub-phase cycles and the counts since the last call, then 0.
pub fn take_sub() -> Sub {
    unsafe { core::mem::replace(&mut *SUB.0.get(), ([0; SUBS], [0; count::COUNT])) }
}

/// Accumulated cycles since the last report.
pub struct Perf {
    /// Cycles per phase, index = id - 1.
    pub phase: [u64; phase::COUNT],
    /// Cycles between the first and the latest pass start.
    pub window: u64,
    /// Passes started in the window.
    pub passes: u32,
    /// Longest pass (step start to step end), cycles.
    pub max_pass: u32,
    /// Start of the previous pass (0 = none yet).
    last_start: u32,
    has_last: bool,
}

impl Perf {
    pub const fn new() -> Self {
        Self { phase: [0; phase::COUNT], window: 0, passes: 0, max_pass: 0, last_start: 0, has_last: false }
    }

    /// A pass starts at `now`.
    #[inline]
    pub fn pass_start(&mut self, now: u32) {
        if self.has_last {
            self.window += now.wrapping_sub(self.last_start) as u64;
        }
        self.last_start = now;
        self.has_last = true;
        self.passes += 1;
    }

    /// The pass that started at the last `pass_start` ends at `now`.
    #[inline]
    pub fn pass_end(&mut self, now: u32) {
        let d = now.wrapping_sub(self.last_start);
        if d > self.max_pass {
            self.max_pass = d;
        }
    }

    /// Adds `from..to` to phase `id`; returns `to` so calls chain.
    #[inline]
    pub fn add(&mut self, id: u8, from: u32, to: u32) -> u32 {
        self.phase[id as usize - 1] += to.wrapping_sub(from) as u64;
        to
    }

    /// Adds raw cycles to phase `id`.
    #[inline]
    pub fn add_cycles(&mut self, id: u8, cycles: u64) {
        self.phase[id as usize - 1] += cycles;
    }

    /// Clears the counters; the window restarts at the next pass.
    pub fn reset(&mut self) {
        let (last, has) = (self.last_start, self.has_last);
        *self = Self::new();
        self.last_start = last;
        self.has_last = has;
    }
}

impl Default for Perf {
    fn default() -> Self {
        Self::new()
    }
}

/// Cycles → microseconds (0 when the clock rate is unknown).
pub fn to_us(cycles: u64, hz: u32) -> u32 {
    if hz == 0 {
        return 0;
    }
    (cycles as u128 * 1_000_000 / hz as u128).min(u32::MAX as u128) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_and_phases_wrap() {
        let mut p = Perf::new();
        p.pass_start(u32::MAX - 10);
        let t = p.add(phase::VM, u32::MAX - 10, 5);
        p.add(phase::RENDER, t, 25);
        p.pass_end(25);
        p.pass_start(40);
        assert_eq!(p.phase[0], 16);
        assert_eq!(p.phase[5], 20);
        assert_eq!(p.max_pass, 36);
        assert_eq!(p.window, 51);
        assert_eq!(p.passes, 2);
        p.reset();
        assert_eq!(p.passes, 0);
        p.pass_start(50);
        assert_eq!(p.window, 10);
    }

    #[test]
    fn us_conversion() {
        assert_eq!(to_us(1_400_000_000, 1_400_000_000), 1_000_000);
        assert_eq!(to_us(5, 0), 0);
    }
}
