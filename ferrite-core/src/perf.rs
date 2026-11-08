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
    /// Time reading the system timer (`systick::millis`), in any phase
    /// (timed by the BSP; exynos4412: the MCT register read).
    pub const TIMER: u8 = 14;
    /// Time in G2D drawing operations (exynos4412 `g2d_render`: fills,
    /// colour blends, sprite blits; including the wait for a present copy in
    /// flight), in any phase. Also counted in `PIXELS`.
    pub const G2D: u8 = 15;
    /// Time drawing new sprites (exynos4412 renderer: a widget drawn into
    /// its sprite, plus the cache clean), inside `DRAW`.
    pub const SPRITE: u8 = 16;
    /// Number of slots (ids 1..COUNT).
    pub const COUNT: usize = 16;
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
    /// Timer reads (`phase::TIMER`).
    pub const TIMER_READS: u8 = 0x85;
    /// G2D drawing operations (`phase::G2D`).
    pub const G2D_OPS: u8 = 0x86;
    /// Kilopixels the G2D drawing operations covered.
    pub const G2D_KPX: u8 = 0x87;
    /// Sprites drawn (`phase::SPRITE`).
    pub const SPRITES_MADE: u8 = 0x88;
    /// Sprite blits (one per clip rect of a sprite-drawn widget).
    pub const SPRITE_BLITS: u8 = 0x89;
    /// Sprite-eligible widgets drawn by the CPU because their appearance was
    /// new (a sprite is made on its second drawing).
    pub const SPRITE_FIRST: u8 = 0x8A;
    /// Kilopixels the CPU blended on the screen (read-modify-write: text,
    /// anti-aliased edges, translucent fills, alpha images).
    pub const CPU_BLEND_KPX: u8 = 0x8B;
    /// Subtrees `render_dirty` moved on screen instead of redrawing them
    /// (pass 0, `LcdBackend::move_rects`).
    pub const SHIFTS: u8 = 0x8C;
    /// Kilopixels those moves covered.
    pub const SHIFT_KPX: u8 = 0x8D;
    /// G2D picture copies (exynos4412 `draw_picture`, one per clip rect).
    pub const PICTURE_COPIES: u8 = 0x8E;
    /// G2D copies of a pixel move (two per moved rect: in and out of the
    /// scratch surface).
    pub const MOVE_OPS: u8 = 0x8F;
    /// Number of counts (ids 0x81..).
    pub const COUNT: usize = 15;
}

/// Ids of the longest pass's breakdown: `MAX_PASS + phase id`, microseconds
/// spent in that phase during the pass `max_pass` measured. Mirrored in
/// `tools/ferrite_cli.py` (`PERF_MAX_BASE`).
pub const MAX_PASS: u8 = 0x40;

/// Phases reported for the longest pass (`MAX_PASS + id`): the step phases,
/// display wait, and the sub-phases that run on this core and say where a
/// long pass went. (PRESENT runs on another core; TIMER is left out to keep
/// the report within 255 bytes: it is 251.)
pub const MAX_PASS_PHASES: [u8; 14] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 15, 16];

/// Step phases (ids 1..=`phase::VSYNC_WAIT`) tracked per pass.
const PASS_PHASES: usize = phase::VSYNC_WAIT as usize;

/// A value only the runtime's own thread touches (the runtime and the
/// renderer run on one core; the profiler is a diagnostic).
struct Local<T>(core::cell::UnsafeCell<T>);
// SAFETY: see the type's doc: never accessed from two threads.
unsafe impl<T> Sync for Local<T> {}

fn no_clock() -> u32 {
    0
}

static CLOCK: Local<fn() -> u32> = Local(core::cell::UnsafeCell::new(no_clock));
/// Number of sub-phases (`DRAW` ..= `SPRITE`).
pub const SUBS: usize = 6;

type Sub = ([u64; SUBS], [u32; count::COUNT]);

/// Cycles of the sub-phases (index id - `DRAW`) and the counts (index
/// id - 0x81; a sub-phase call counts in its count, `sub_count`).
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
/// `PRESENT` / `TIMER`).
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
    let c = &mut s.1[sub_count(i)];
    *c = c.wrapping_add(calls);
}

/// Count index (id - 0x81) of sub-phase index `i` (id - `DRAW`): each
/// sub-phase call counts one in its count.
const SUB_COUNT: [usize; SUBS] = [
    (count::DRAWS - 0x81) as usize,
    (count::PIXEL_CALLS - 0x81) as usize,
    (count::PRESENTS - 0x81) as usize,
    (count::TIMER_READS - 0x81) as usize,
    (count::G2D_OPS - 0x81) as usize,
    (count::SPRITES_MADE - 0x81) as usize,
];

#[inline]
const fn sub_count(i: usize) -> usize {
    SUB_COUNT[i]
}

/// Adds `n` to count `id` (`count::*`).
#[inline]
pub fn add_count(id: u8, n: u32) {
    let s = unsafe { &mut *SUB.0.get() };
    let c = &mut s.1[(id - 0x81) as usize];
    *c = c.wrapping_add(n);
}

/// Sub-phase cycles accumulated so far (not reset).
fn sub_totals() -> [u64; SUBS] {
    unsafe { (*SUB.0.get()).0 }
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
    /// The running pass sent the report (`reset` inside it): its length is
    /// mostly that send (~14 ms for ~165 bytes at 115200), so it is not a
    /// `max_pass` candidate.
    skip_pass: bool,
    /// The running pass's cycles per step phase (index id - 1).
    cur: [u32; PASS_PHASES],
    /// Sub-phase totals at the running pass's start.
    sub_at_start: [u64; SUBS],
    /// The longest pass's cycles: step phases (index id - 1), then the
    /// sub-phases (index `PASS_PHASES` + id - `DRAW`).
    pub max_phases: [u32; PASS_PHASES + SUBS],
}

impl Perf {
    pub const fn new() -> Self {
        Self {
            phase: [0; phase::COUNT],
            window: 0,
            passes: 0,
            max_pass: 0,
            last_start: 0,
            has_last: false,
            skip_pass: false,
            cur: [0; PASS_PHASES],
            sub_at_start: [0; SUBS],
            max_phases: [0; PASS_PHASES + SUBS],
        }
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
        self.cur = [0; PASS_PHASES];
        self.sub_at_start = sub_totals();
    }

    /// The pass that started at the last `pass_start` ends at `now`.
    #[inline]
    pub fn pass_end(&mut self, now: u32) {
        if self.skip_pass {
            self.skip_pass = false;
            return;
        }
        let d = now.wrapping_sub(self.last_start);
        if d > self.max_pass {
            self.max_pass = d;
            self.max_phases[..PASS_PHASES].copy_from_slice(&self.cur);
            let subs = sub_totals();
            for i in 0..SUBS {
                self.max_phases[PASS_PHASES + i] = subs[i].saturating_sub(self.sub_at_start[i]).min(u32::MAX as u64) as u32;
            }
        }
    }

    /// Adds `from..to` to phase `id`; returns `to` so calls chain.
    #[inline]
    pub fn add(&mut self, id: u8, from: u32, to: u32) -> u32 {
        let d = to.wrapping_sub(from);
        self.phase[id as usize - 1] += d as u64;
        if let Some(c) = self.cur.get_mut(id as usize - 1) {
            *c = c.wrapping_add(d);
        }
        to
    }

    /// Adds raw cycles to phase `id`.
    #[inline]
    pub fn add_cycles(&mut self, id: u8, cycles: u64) {
        self.phase[id as usize - 1] += cycles;
        if let Some(c) = self.cur.get_mut(id as usize - 1) {
            *c = c.wrapping_add(cycles as u32);
        }
    }

    /// Cycles phase `id` took in the longest pass (`MAX_PASS_PHASES`).
    pub fn max_phase(&self, id: u8) -> u32 {
        let i = if id <= phase::VSYNC_WAIT { id - 1 } else { id - phase::DRAW + phase::VSYNC_WAIT } as usize;
        self.max_phases.get(i).copied().unwrap_or(0)
    }

    /// Clears the counters; the window restarts at the next pass. Called
    /// inside a pass (the one answering the query), which then does not
    /// count as the new window's longest.
    pub fn reset(&mut self) {
        let (last, has) = (self.last_start, self.has_last);
        *self = Self::new();
        self.last_start = last;
        self.has_last = has;
        self.skip_pass = has;
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
    fn reporting_pass_is_not_the_longest() {
        let mut p = Perf::new();
        p.pass_start(0);
        p.pass_end(10);
        p.pass_start(20);
        // The report is sent mid-pass, then the counters reset.
        p.reset();
        p.pass_end(30_000);
        assert_eq!(p.max_pass, 0);
        p.pass_start(30_010);
        p.pass_end(30_050);
        assert_eq!(p.max_pass, 40);
    }

    /// The longest pass keeps its own per-phase times, sub-phases included.
    #[test]
    fn longest_pass_breakdown() {
        let mut p = Perf::new();
        p.pass_start(0);
        p.add(phase::VM, 0, 5);
        p.add(phase::RENDER, 5, 15);
        p.pass_end(15);
        p.pass_start(20);
        let t = p.add(phase::VM, 20, 22);
        add_sub(phase::DRAW, 70);
        p.add(phase::RENDER, t, 102);
        p.add_cycles(phase::VSYNC_WAIT, 3);
        p.pass_end(102);
        p.pass_start(110);
        p.add(phase::VM, 110, 140);
        p.pass_end(140);
        assert_eq!(p.max_pass, 82);
        assert_eq!((p.max_phase(phase::VM), p.max_phase(phase::RENDER)), (2, 80));
        assert_eq!((p.max_phase(phase::VSYNC_WAIT), p.max_phase(phase::DRAW)), (3, 70));
        assert_eq!(p.max_phase(phase::G2D), 0);
    }

    #[test]
    fn us_conversion() {
        assert_eq!(to_us(1_400_000_000, 1_400_000_000), 1_000_000);
        assert_eq!(to_us(5, 0), 0);
    }
}
