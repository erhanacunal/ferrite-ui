//! Device-agnostic application runtime.
//!
//! `run::<P>()` owns the ~85% of the firmware/epaper/sim main loops that is
//! identical across devices: build nothing (the BSP hands in a fully-built
//! `Ctx<P>`), run the startup sequence, then loop forever stepping the VM,
//! servicing the USART protocol, dispatching input, rendering, and draining
//! callbacks.
//!
//! Per-device variance is expressed through two traits rather than `#[cfg]`:
//!
//! * [`PlatformRuntime`] — constants + hooks a BSP must supply (reset, syscall
//!   table, boot sequence, forced render mode, …).
//! * [`InputHandler`] — the per-frame touch/keyboard handling. [`FullInput`]
//!   gives the rich Nextion experience (keyboard, scrollbars, gestures);
//!   [`BasicInput`] gives the minimal epaper experience (press → on_click).
//!   Monomorphization dead-strips the unused one, so epaper never links the
//!   keyboard module.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::lcd::LcdBackend;
use crate::platform::Platform;
use crate::touch::{TouchEventKind, TouchImpl};

use crate::ctx::Ctx;
use crate::pool::{ObjPool, T_ARRAY, T_FREE, T_STRING, T_WIDGET};
use crate::syscfg::SysCfg;
use crate::types::{COLOR_RED, Size};
use crate::vm::{CodeAddr, FunctionKind, ProgramRef, RenderMode, Vm, VmState};
use crate::{font, fs, heap, keyboard, protocol, render, widget};

use crate::protocol::{Protocol, RxEvent};
use crate::widget::{WValue, WidgetId};

// === Error codes ===

#[allow(dead_code)]
pub const ERR_PAGE_NOT_FOUND: u8 = 1;
pub const ERR_PROGRAM_NOT_FOUND: u8 = 2;
#[allow(dead_code)]
pub const ERR_IMAGE_NOT_FOUND: u8 = 3;
#[allow(dead_code)]
pub const ERR_FONT_NOT_FOUND: u8 = 4;
pub const ERR_NO_FILESYSTEM: u8 = 5;
pub const ERR_PROGRAM_ERROR: u8 = 6;
pub const ERR_INSUFFICIENT_MEMORY: u8 = 7;
/// The image's RES_SYSCFG entry is malformed (bad kind/size/magic/version).
pub const ERR_BAD_SYSCFG: u8 = 8;
/// The flash image doesn't match its header checksum (`Fs::verify`).
pub const ERR_BAD_IMAGE: u8 = 9;
/// Recovery mode — skip program load, suppress the error screen.
pub const ERR_EMPTY_RUN: u8 = 0xFE;

/// Default maximum RAM-resident program image size (see
/// `PlatformRuntime::MAX_CODE_SIZE`).
pub const MAX_CODE_SIZE: usize = 4096;

// === Gesture tuning (shared with lib/touch.fl constants) ===

/// Hold time before a long press. Time-based, not frame-based: the main loop
/// is unpaced, so a frame count would fire within milliseconds on a board
/// whose loop does little work per pass (and then suppress every swipe).
const LONG_PRESS_MS: u32 = 500;
const LONG_PRESS_MAX_MOVE_SQ: i32 = 20 * 20;
const SWIPE_MIN_DIST_SQ: i32 = 40 * 40;
const SWIPE_AXIS_RATIO: i32 = 2;

// ============================================================================
//  Platform hooks
// ============================================================================

/// Everything a BSP must provide to drive [`run`] beyond the raw hardware
/// backends already grouped by [`Platform`].
pub trait PlatformRuntime: Platform + Sized + 'static {
    /// Background color for the root widget and the error screen (RGB565).
    const BG_COLOR: u16;
    /// Foreground (text/border) color for the error screen (RGB565).
    const FG_COLOR: u16;
    /// Render mode forced regardless of the image header (epaper = EPaper).
    const FORCED_RENDER_MODE: Option<RenderMode> = None;

    /// Maximum VM instructions to execute per main-loop iteration.
    ///
    /// The default of 1 suits a main loop whose other per-pass work is cheap
    /// (it spins as fast as the CPU, so the VM still runs millions of
    /// instructions/sec). Raise it when a pass carries fixed costs — e.g. a
    /// touch controller read on every pass while pressed — or the VM starves
    /// and `loop()` frames stretch while the screen is touched.
    /// The host simulator paces the loop to the display refresh (~60 Hz) and
    /// would otherwise execute only ~60 instructions/sec — far too slow for
    /// `millis()`-based animation. It raises this so each presented frame runs
    /// a hardware-like burst of instructions. The loop still stops early on
    /// YIELD / a blocking state, so a program that yields each frame is
    /// unaffected.
    const VM_STEPS_PER_TICK: u32 = 1;

    /// Largest program image loaded into RAM (`exec_mode: "ram"`). A larger
    /// image executes from flash instead. The default suits the 20 KB GD32;
    /// SDRAM boards raise it (64 KB max — the VM program counter is `u16`).
    const MAX_CODE_SIZE: usize = MAX_CODE_SIZE;

    /// Per-frame input handling strategy.
    type Input: InputHandler<Self>;

    /// Reboot the device (never returns).
    fn reset() -> !;

    /// VM syscall dispatcher, wired into `Vm::syscall_fn`.
    fn syscall(id: u8, args: &[i32], strpool: &mut ObjPool) -> Option<i32> {
        let _ = (id, args, strpool);
        None
    }

    /// `(used, free)` stack bytes reported by the `StackInfo` USART command.
    fn stack_info() -> (u32, u32) {
        (0, 0)
    }

    /// Deepest stack usage since boot in bytes (high-water mark), reported by
    /// the `StackInfo` command alongside `stack_info`. 0 = not measured.
    fn stack_peak() -> u32 {
        0
    }

    /// `(free_bytes, largest_free_block)` reported by the `MemInfo` USART
    /// command. Defaults to the lib's static heap; BSPs that supply their own
    /// global allocator (feature `external_alloc`) override this to report
    /// through that allocator.
    fn heap_stats() -> (usize, usize) {
        heap::stats()
    }

    /// Total heap size in bytes for the `MemDetail` report (0 = unknown).
    /// Defaults to the lib's static heap arena; BSPs with their own global
    /// allocator may override it.
    fn heap_total() -> usize {
        heap::size()
    }

    /// Pre-filesystem boot hook: backlight, screen clear, recovery mode, SD-card
    /// flashing, flash-address probing. Returns a nonzero error code to abort
    /// program load (`ERR_EMPTY_RUN` enters recovery without an error screen).
    fn boot(ctx: &mut Ctx<Self>, touch: &mut TouchImpl<Self::TouchB>) -> u8 {
        let _ = (ctx, touch);
        0
    }

    /// Initial draw after a clean program load (epaper does a full render + EPD
    /// flush; raster panels rely on the first dirty frame instead).
    fn initial_render(ctx: &mut Ctx<Self>, vm: &Vm) {
        let _ = (ctx, vm);
    }

    /// Handle a USART event the shared loop does not (e.g. Nextion
    /// `TouchCalibrate`). Return `true` if consumed.
    fn on_extra_rx(
        ev: RxEvent,
        ctx: &mut Ctx<Self>,
        vm: &mut Vm,
        touch: &mut TouchImpl<Self::TouchB>,
    ) -> bool {
        let _ = (ev, ctx, vm, touch);
        false
    }

    /// Called at the very end of every main-loop iteration. Host simulators pump
    /// their window / event loop and present the framebuffer here (and sample
    /// input for the next frame); on-device BSPs leave it a no-op.
    fn frame_end(ctx: &mut Ctx<Self>) {
        let _ = ctx;
    }

    /// Present a buffered frame after the framework has rasterized it into the
    /// back buffer. Default: finish synchronously on the calling thread —
    /// `end_frame` does the vsync wait + buffer flip, then `flush_dirty`.
    ///
    /// A BSP with a dedicated render thread overrides this to hand the freshly
    /// drawn back buffer off and return immediately, so the UI thread does not
    /// block on the panel's vertical blanking. Only the *present* is deferred:
    /// rasterization stays on the calling thread, so the widget tree remains
    /// single-threaded and needs no lock.
    fn present_buffered(ctx: &mut Ctx<Self>) {
        ctx.lcd.end_frame();
        ctx.lcd.flush_dirty();
    }

    /// Free-running 32-bit cycle counter for the `perf` profiler (0 = none).
    fn cycles() -> u32 {
        0
    }

    /// Rate of `cycles` in Hz (0 = unknown: the report shows no times).
    fn cycle_hz() -> u32 {
        0
    }

    /// Cycles spent waiting for vsync since the last call, then reset to 0
    /// (the profiler's `VSYNC_WAIT` row, part of `frame_end`).
    fn perf_take_wait() -> u64 {
        0
    }

    /// Dirty mode: whether a render now would reach the screen any sooner.
    /// A BSP whose present runs in the background answers false while what
    /// was drawn since the last present is still waiting for it: drawing
    /// again then only moves pixels that are never shown. The dirty flags
    /// stay set, and the next due render draws the latest state once
    /// (property animations tick every pass, each changing a widget a
    /// little: the exynos pull-down shade was moved ~7 times per present).
    fn render_due() -> bool {
        true
    }
}

/// Per-frame input handling. Owns whatever cross-frame state the strategy needs
/// (keyboard focus, scrollbar drag, gesture accumulators).
pub trait InputHandler<P: PlatformRuntime>: Sized {
    fn new() -> Self;

    /// Poll the touch device and dispatch the resulting event: mutate the tree,
    /// enqueue VM callbacks (a widget's to the task that handles it, the
    /// global ones to the foreground task), and render immediate press
    /// feedback.
    fn handle(&mut self, ctx: &mut Ctx<P>, tasks: &mut Tasks, touch: &mut TouchImpl<P::TouchB>);

    /// Per-frame housekeeping unrelated to a touch event (cursor blink).
    fn tick(&mut self, _ctx: &mut Ctx<P>) {}

    /// Nothing for `tick` to do on its own (no cursor blinking): the passes may
    /// stop until an event (`Runtime::idle_ms`).
    fn idle(&self) -> bool {
        true
    }

    /// Draw any overlay (on-screen keyboard) on top of the rendered frame.
    /// `buffered` selects the double-buffered redraw cadence.
    fn draw_overlay(&mut self, _ctx: &Ctx<P>, _buffered: bool) {}

    /// Reset input state when a new program is flashed at runtime.
    fn reset(&mut self, _ctx: &mut Ctx<P>) {}

    /// Widgets were destroyed behind the handler's back (a task ended): drop
    /// any focus / drag state that names a widget that is gone.
    fn forget_dead(&mut self, _ctx: &mut Ctx<P>) {}
}

// ============================================================================
//  Tasks — the program VMs input and render dispatch to
// ============================================================================

/// The program VMs: `main`, plus with `multi_program` up to
/// `task::MAX_TASKS - 1` children (slot = task id). Input handlers send a
/// widget's events to the task that handles it and the global events to the
/// foreground task; without `multi_program` both are `main`.
pub struct Tasks {
    main: Box<Vm>,
    #[cfg(feature = "multi_program")]
    kids: Vec<Option<Box<Vm>>>,
    /// Foreground task (copied from `Ctx::tasks` before input handling).
    #[cfg(feature = "multi_program")]
    fg: u8,
    /// A runtime-owned widget (the task-fault dialog's button) was clicked.
    #[cfg(feature = "multi_program")]
    sys_click: bool,
}

impl Tasks {
    fn new(main: Box<Vm>) -> Self {
        Self {
            main,
            #[cfg(feature = "multi_program")]
            kids: (1..crate::task::MAX_TASKS).map(|_| None).collect(),
            #[cfg(feature = "multi_program")]
            fg: crate::task::TASK_MAIN,
            #[cfg(feature = "multi_program")]
            sys_click: false,
        }
    }

    /// The `main` program's VM.
    pub fn main(&mut self) -> &mut Vm {
        &mut self.main
    }

    pub fn main_ref(&self) -> &Vm {
        &self.main
    }

    /// The main program's render mode holds for every task.
    pub fn buffered(&self) -> bool {
        self.main.render_mode == RenderMode::Buffered
    }

    /// VM of running task `id`.
    pub fn get(&mut self, id: u8) -> Option<&mut Vm> {
        if id == 0 {
            return Some(&mut self.main);
        }
        #[cfg(feature = "multi_program")]
        {
            self.kids.get_mut(id as usize - 1).and_then(|k| k.as_deref_mut())
        }
        #[cfg(not(feature = "multi_program"))]
        None
    }

    /// The task that gets global input events.
    fn fg(&mut self) -> &mut Vm {
        #[cfg(feature = "multi_program")]
        if self.fg != 0 && self.get(self.fg).is_some() {
            return self.get(self.fg).unwrap();
        }
        &mut self.main
    }

    /// The task that handles widget `id`'s events (None: a runtime widget).
    fn for_widget(&mut self, objs: &ObjPool, id: WidgetId) -> Option<&mut Vm> {
        #[cfg(feature = "multi_program")]
        {
            self.get(objs.handler_of(id))
        }
        #[cfg(not(feature = "multi_program"))]
        {
            let _ = (objs, id);
            Some(&mut self.main)
        }
    }

    /// Queue a global callback (touch down/move/up, swipe, long press) on
    /// the foreground task.
    pub fn global_event(&mut self, kind: FunctionKind, args: &[i32]) {
        send_kind(self.fg(), kind, args);
    }

    /// Queue `kind` on the task that handles widget `id`.
    pub fn widget_event(&mut self, objs: &ObjPool, id: WidgetId, kind: FunctionKind, args: &[i32]) {
        if let Some(vm) = self.for_widget(objs, id) {
            send_kind(vm, kind, args);
        }
    }

    /// on_click of widget `id` (it has `FLAG_HAS_ON_CLICK`): a modal
    /// overlay's click function, else the handling task's click dispatcher.
    pub fn widget_click(&mut self, objs: &ObjPool, id: WidgetId) {
        #[cfg(feature = "multi_program")]
        if objs.handler_of(id) == crate::task::TASK_SYSTEM {
            self.sys_click = true;
            return;
        }
        let Some(vm) = self.for_widget(objs, id) else {
            return;
        };
        if !vm.has_code() {
            return;
        }
        if let Some(click_fn) = vm.modal_overlay_click_fn(id) {
            if let Some(off) = vm.find_func(click_fn).map(|e| e.offset as CodeAddr) {
                vm.enqueue_callback(off, &[id.0 as i32]);
            }
        } else {
            send_kind(vm, FunctionKind::OnClick, &[id.0 as i32]);
        }
    }

    /// Drop every child task's VM (program switch / reload).
    #[cfg(feature = "multi_program")]
    fn drop_kids(&mut self) {
        for k in self.kids.iter_mut() {
            *k = None;
        }
        self.fg = crate::task::TASK_MAIN;
        self.sys_click = false;
    }
}

/// Queue the `kind` handler of `vm` (if it has one). True if queued.
fn send_kind(vm: &mut Vm, kind: FunctionKind, args: &[i32]) -> bool {
    if !vm.has_code() {
        return false;
    }
    match vm.find_by_kind(kind).map(|e| e.offset as CodeAddr) {
        Some(off) => {
            vm.enqueue_callback(off, args);
            true
        }
        None => false,
    }
}

/// Run `f` on every task's VM, `main` first, with the pool stamping new
/// objects with that task.
fn each_task<P: PlatformRuntime>(ctx: &mut Ctx<P>, tasks: &mut Tasks, mut f: impl FnMut(&mut Ctx<P>, &mut Vm)) {
    #[cfg(feature = "multi_program")]
    {
        ctx.objs.cur_task = crate::task::TASK_MAIN;
    }
    f(ctx, &mut tasks.main);
    #[cfg(feature = "multi_program")]
    {
        for (i, slot) in tasks.kids.iter_mut().enumerate() {
            if let Some(vm) = slot.as_deref_mut() {
                ctx.objs.cur_task = i as u8 + 1;
                f(ctx, vm);
            }
        }
        ctx.objs.cur_task = crate::task::TASK_MAIN;
    }
}

// ============================================================================
//  Entry point
// ============================================================================

/// Run the application forever. The BSP builds every hardware backend, packs
/// them into `Ctx<P>` and a `TouchImpl`, then hands them here.
pub fn run<P: PlatformRuntime>(ctx: Box<Ctx<P>>, touch: TouchImpl<P::TouchB>) -> ! {
    let mut rt = Runtime::<P>::new(ctx, touch);
    loop {
        rt.step();
    }
}

/// The application after its startup sequence, run one main-loop pass at a
/// time by [`step`](Self::step). [`run`] is `new` + `step` forever; a BSP with
/// its own scheduler (an async executor) calls `step` from a task instead and
/// does other work between passes.
pub struct Runtime<P: PlatformRuntime> {
    ctx: Box<Ctx<P>>,
    touch: TouchImpl<P::TouchB>,
    tasks: Tasks,
    input: P::Input,
    protocol: Protocol,
    error_code: u8,
    /// Time the last USART byte was taken from the RX ring (for resync).
    last_rx_ms: u32,
    /// The open task-fault dialog (`multi_program`).
    #[cfg(feature = "multi_program")]
    fault_dialog: WidgetId,
    /// Main-loop profile (`perf` feature).
    #[cfg(feature = "perf")]
    perf: crate::perf::Perf,
}

impl<P: PlatformRuntime> Runtime<P> {
    /// Runs the startup sequence: ROM font, `P::boot`, mount, syscfg, root
    /// widget, `main` + `setup()`, first frame or error screen.
    pub fn new(mut ctx: Box<Ctx<P>>, mut touch: TouchImpl<P::TouchB>) -> Self {
        #[cfg(feature = "perf")]
        crate::perf::set_clock(P::cycles);
        // --- Embedded ROM font (always font 0) ---
        ctx.fonts.add(font::Font::from_embedded(
            &crate::embedded_font::GLYPHS,
            &crate::embedded_font::CODEPOINTS,
            &crate::embedded_font::BITMAP,
            crate::embedded_font::Y_ADVANCE,
            crate::embedded_font::BPP,
        ));

        // --- Platform boot: backlight / recovery / SD flashing / FS preamble ---
        let mut error_code = P::boot(&mut ctx, &mut touch);

        // --- Mount flash filesystem ---
        if error_code == 0 {
            match fs::Fs::mount(&ctx.flash, P::FS_BASE) {
                Ok(f) if P::VERIFY_FS_ON_BOOT && !f.verify(&ctx.flash) => error_code = ERR_BAD_IMAGE,
                Ok(f) => ctx.fs = Some(f),
                Err(_) => error_code = ERR_NO_FILESYSTEM,
            }
        }

        // --- VM ---
        let mut vm = Box::new(Vm::new());
        vm.syscall_fn = Some(P::syscall);
        vm.forced_render_mode = P::FORCED_RENDER_MODE;

        // --- Size the runtime pools from the image's RES_SYSCFG (if any) ---
        if error_code == 0 {
            error_code = apply_syscfg(&mut ctx, &mut vm);
        }

        // --- Root widget (always widget 0; after the pool is sized, which drops
        // every object) ---
        alloc_root(&mut ctx);

        // --- Load main program ---
        if error_code == 0 {
            error_code = load_program(&mut ctx, &mut vm, ProgramRef::new(b"main", 0));
        }

        // --- setup() / on_program_start() / start loop() ---
        if error_code == 0 && vm.has_code() {
            error_code = start_program(&mut ctx, &mut vm);
        }

        // --- First frame / error screen ---
        if error_code != 0 && error_code != ERR_EMPTY_RUN {
            show_error(&mut ctx, error_code, None);
        } else if error_code == 0 && vm.state != VmState::Exit {
            P::initial_render(&mut ctx, &vm);
        }

        let input = <P::Input as InputHandler<P>>::new();
        let last_rx_ms = ctx.systick.millis();
        Self {
            ctx,
            touch,
            tasks: Tasks::new(vm),
            input,
            protocol: Protocol::new(P::FS_BASE),
            error_code,
            last_rx_ms,
            #[cfg(feature = "multi_program")]
            fault_dialog: WidgetId::NONE,
            #[cfg(feature = "perf")]
            perf: crate::perf::Perf::new(),
        }
    }

    /// The hardware context, between passes.
    pub fn ctx(&mut self) -> &mut Ctx<P> {
        &mut self.ctx
    }

    /// The programs want the CPU again at once: a VM task runs or yielded (its next burst is
    /// due), callbacks wait, the main program exited (the next one loads), or a USART message is
    /// half received. Otherwise the next pass only polls (input, `delay` timers, animations,
    /// presents) and a BSP may sleep a little between passes instead of spinning.
    pub fn busy(&self) -> bool {
        let wants = |vm: &Vm| {
            matches!(vm.state, VmState::Running | VmState::Yielded | VmState::Exit) || vm.has_pending_callbacks()
        };
        #[cfg(feature = "multi_program")]
        if self.tasks.kids.iter().flatten().any(|vm| wants(vm)) {
            return true;
        }
        (self.error_code == 0 && wants(&self.tasks.main)) || !self.protocol.is_idle()
    }

    /// How long the runtime can go without a pass, for a BSP that sleeps
    /// between passes: `Some(0)` = keep passing (code to run, an animation, a
    /// touch down, something to draw, a sound streaming, the keyboard's
    /// cursor), `Some(ms)` = until the earliest `delay` ends, None = until an
    /// event (a touch, a message): nothing time-based is pending. The BSP
    /// still passes for its own work (presents, polled keys, messages from
    /// other cores).
    pub fn idle_ms(&self) -> Option<u32> {
        if self.busy() || !self.touch.is_idle() || !self.input.idle() || self.ctx.objs.maybe_dirty {
            return Some(0);
        }
        #[cfg(feature = "layers")]
        if self.ctx.objs.layers.placed {
            return Some(0);
        }
        #[cfg(feature = "audio")]
        if self.ctx.audio.playing() {
            return Some(0);
        }
        if self.error_code != 0 {
            return None;
        }
        let now = self.ctx.systick.millis();
        let mut left: Option<u32> = None;
        #[cfg(feature = "multi_program")]
        let kids = self.tasks.kids.iter().filter_map(|k| k.as_deref());
        #[cfg(not(feature = "multi_program"))]
        let kids = core::iter::empty::<&Vm>();
        for vm in core::iter::once(&*self.tasks.main).chain(kids) {
            if vm.animating() {
                return Some(0);
            }
            if let Some(ms) = vm.wait_left(now) {
                left = Some(left.map_or(ms, |l| l.min(ms)));
            }
        }
        left
    }

    /// One main-loop pass: VM, USART protocol, input, animations, audio,
    /// render, callbacks, `P::frame_end`.
    pub fn step(&mut self) {
        #[cfg(feature = "perf")]
        let mut t = {
            let now = P::cycles();
            self.perf.pass_start(now);
            now
        };
        // Ends the phase that started at `t` (`perf` feature; else nothing).
        macro_rules! lap {
            ($id:ident) => {
                #[cfg(feature = "perf")]
                {
                    t = self.perf.add(crate::perf::phase::$id, t, P::cycles());
                }
            };
        }

        // `runProgram` ended the main program: load the next one (every
        // child task ends with it).
        if self.error_code == 0 && self.tasks.main.state == VmState::Exit {
            self.drop_children();
            self.error_code = switch_program(&mut self.ctx, &mut self.tasks.main, &mut self.input);
        }
        lap!(MISC);

        // --- VM step, each task in turn ---
        // Resumes a suspended showModal whose result is now ready, then
        // continues a suspended callback whose delay expired, or runs a burst
        // of the program. On the GD32 the burst is 1 instruction (the loop
        // already spins at CPU speed); faster BSPs and the sim raise
        // VM_STEPS_PER_TICK so animation runs at hardware-like speed instead
        // of one instruction per presented frame.
        if self.error_code == 0 {
            each_task(&mut self.ctx, &mut self.tasks, |ctx, vm| {
                vm.try_resume_modal(ctx);
                vm.tick(ctx, P::VM_STEPS_PER_TICK);
            });
            self.error_code = check_vm_fault(&mut self.ctx, &self.tasks.main);
        }
        lap!(VM);

        self.service_usart();
        lap!(USART);

        // --- Input ---
        if self.error_code == 0 {
            #[cfg(feature = "multi_program")]
            {
                self.tasks.fg = self.ctx.tasks.fg;
            }
            self.input.handle(&mut self.ctx, &mut self.tasks, &mut self.touch);
            self.input.tick(&mut self.ctx);
            #[cfg(feature = "multi_program")]
            self.close_fault_dialog_on_click();
        }
        lap!(INPUT);

        // --- Advance property animations ---
        // Time-driven (millis), independent of VM state, so tweens keep running
        // even while the program is Halted/Waiting. Runs before render so dirtied
        // props paint this frame; any onAnimEnd callbacks enqueued here are picked
        // up by the callback drain below.
        if self.error_code == 0 {
            let now = self.ctx.systick.millis();
            each_task(&mut self.ctx, &mut self.tasks, |ctx, vm| vm.tick_animations(ctx, now));
        }
        lap!(ANIM);

        // --- Stream the playing sound (if any) into the audio backend ---
        #[cfg(feature = "audio")]
        self.ctx.audio.pump(&self.ctx.flash);
        lap!(AUDIO);

        // --- Render (on_paint runs inside, for the widgets it drew) ---
        if self.error_code == 0 {
            render_phase(&mut self.ctx, &mut self.tasks, &mut self.input);
        }
        lap!(RENDER);

        // --- Start queued callbacks, then re-render ---
        if self.error_code == 0 {
            let mut ran = false;
            each_task(&mut self.ctx, &mut self.tasks, |ctx, vm| {
                if vm.has_pending_callbacks() {
                    ran |= vm.drain_callbacks(ctx);
                }
            });
            if ran {
                render_phase(&mut self.ctx, &mut self.tasks, &mut self.input);
            }
        }

        // --- A fault in a callback or on_paint stops the program too ---
        if self.error_code == 0 {
            self.error_code = check_vm_fault(&mut self.ctx, &self.tasks.main);
        }
        lap!(CALLBACKS);

        // --- Child tasks: end the ones that exited or faulted, act on
        //     spawn / kill requests, deliver messages ---
        #[cfg(feature = "multi_program")]
        if self.error_code == 0 {
            self.service_tasks();
        }

        // --- Commit EEPROM writes made this pass (one erase on flash-backed
        //     boards for a whole save; no-op on write-through chips) ---
        self.ctx.eeprom.flush();
        lap!(MISC);

        // --- End-of-frame hook (host window pump / present; no-op on device) ---
        P::frame_end(&mut self.ctx);
        lap!(FRAME_END);
        #[cfg(feature = "perf")]
        {
            self.perf.add_cycles(crate::perf::phase::VSYNC_WAIT, P::perf_take_wait());
            self.perf.pass_end(t);
        }
    }

    /// Answers a `Perf` query: the profile since the last one, then resets it.
    #[cfg(feature = "perf")]
    fn send_perf(&mut self) {
        use crate::perf::{phase, to_us};
        let hz = P::cycle_hz();
        let mut instructions = 0u64;
        each_task(&mut self.ctx, &mut self.tasks, |_, vm| {
            instructions += vm.instructions;
            vm.instructions = 0;
        });
        use crate::perf::count;
        let (sub, counts) = crate::perf::take_sub();
        for (i, &c) in sub.iter().enumerate() {
            self.perf.add_cycles(phase::DRAW + i as u8, c);
        }
        let p = &self.perf;
        const MAXN: usize = crate::perf::MAX_PASS_PHASES.len();
        let mut rows = [(0u8, 0u32); phase::COUNT + count::COUNT + MAXN];
        for (i, row) in rows.iter_mut().take(phase::COUNT).enumerate() {
            *row = (i as u8 + 1, to_us(p.phase[i], hz));
        }
        for (i, &n) in counts.iter().enumerate() {
            rows[phase::COUNT + i] = (count::DRAWS + i as u8, n);
        }
        // Where the longest pass went.
        for (i, &id) in crate::perf::MAX_PASS_PHASES.iter().enumerate() {
            rows[phase::COUNT + count::COUNT + i] = (crate::perf::MAX_PASS + id, to_us(p.max_phase(id) as u64, hz));
        }
        let header = protocol::PerfHeader {
            cycle_hz: hz,
            window_us: to_us(p.window, hz),
            passes: p.passes,
            max_pass_us: to_us(p.max_pass as u64, hz),
            vm_instructions: instructions,
        };
        protocol::send_perf(&self.ctx.usart, &header, &rows);
        self.perf.reset();
    }

    /// USART protocol: always serviced, even after an error, so a new program
    /// or filesystem can be flashed to recover.
    fn service_usart(&mut self) {
        while let Some(byte) = self.ctx.usart.rx_read_byte() {
            let ev = self.protocol.feed(byte, &self.ctx.flash);
            // After feed: a flash erase it triggered is not idle time.
            self.last_rx_ms = self.ctx.systick.millis();
            if matches!(ev, RxEvent::None) {
                continue;
            }
            if P::on_extra_rx(ev, &mut self.ctx, &mut self.tasks.main, &mut self.touch) {
                continue;
            }
            self.handle_event(ev);
        }
        // Ring drained. A message stalled mid-frame is dropped, so a stray
        // byte (RX noise, an aborted host transfer) can't wedge the parser.
        if !self.protocol.is_idle()
            && self.ctx.systick.millis().wrapping_sub(self.last_rx_ms) > protocol::RX_RESYNC_MS
        {
            self.protocol.resync();
        }
        // A transfer the host gave up on: let other commands through again.
        if self.protocol.in_transfer()
            && self.ctx.systick.millis().wrapping_sub(self.last_rx_ms) > protocol::TRANSFER_IDLE_MS
        {
            self.protocol.end_transfer();
        }
    }

    /// ACK of a WriteFs chunk: FsAck in sequence-ACK mode, else a pong.
    fn ack_chunk(&self) {
        if self.protocol.fs_seq_ack() {
            let (next_seq, last_crc) = self.protocol.fs_ack_state();
            protocol::send_fs_ack(&self.ctx.usart, next_seq, last_crc);
        } else {
            protocol::send_pong(&self.ctx.usart);
        }
    }

    /// Acts on one finished USART message.
    fn handle_event(&mut self, ev: RxEvent) {
        match ev {
            RxEvent::Ping => protocol::send_pong(&self.ctx.usart),
            RxEvent::Restart => {
                self.ctx.eeprom.flush();
                P::reset()
            }
            RxEvent::MemInfo => {
                let (free, _largest) = P::heap_stats();
                protocol::send_meminfo(&self.ctx.usart, free as u32);
            }
            RxEvent::StackInfo => {
                let (used, free) = P::stack_info();
                protocol::send_stackinfo(&self.ctx.usart, used, free, P::stack_peak());
            }
            RxEvent::MemDetail => {
                let entries = mem_detail(&self.ctx, &self.tasks.main);
                protocol::send_memdetail(&self.ctx.usart, &entries);
            }
            #[cfg(feature = "perf")]
            RxEvent::Perf => self.send_perf(),
            RxEvent::SetDateTime => {
                let dt = self.protocol.datetime();
                self.ctx.rtc.set_time(&dt);
                protocol::send_pong(&self.ctx.usart);
            }
            RxEvent::ProgramReady => {
                self.drop_children();
                self.error_code =
                    reload_program(&mut self.ctx, &mut self.tasks.main, &mut self.input, &mut self.protocol);
                if self.error_code != 0 {
                    show_error(&mut self.ctx, self.error_code, None);
                } else {
                    P::initial_render(&mut self.ctx, &self.tasks.main);
                }
            }
            RxEvent::ProgramTooLarge => {
                protocol::send_error(&self.ctx.usart, ERR_INSUFFICIENT_MEMORY);
            }
            RxEvent::FsReady => {
                self.drop_children();
                self.tasks.main.reset();
                #[cfg(feature = "audio")]
                self.ctx.audio.stop();
                self.ctx.objs.clear();
                self.ctx.images.clear();
                self.error_code = 0;
                self.protocol.alloc_sector_buf();
                if self.protocol.fs_crc_mode() {
                    protocol::send_fs_caps(&self.ctx.usart, self.protocol.fs_caps());
                } else {
                    protocol::send_pong(&self.ctx.usart);
                }
            }
            RxEvent::FsChunkDone => self.ack_chunk(),
            // Dropped, nothing written: the host resends that header/chunk.
            // Inside a sequence-ACK transfer the FsAck says so (and how far
            // the device is); a bad header before one gets error 10.
            RxEvent::FsTransferBad => {
                if self.protocol.fs_seq_ack() && self.protocol.in_transfer() {
                    self.ack_chunk();
                } else {
                    protocol::send_error(&self.ctx.usart, protocol::ERR_TRANSFER_CRC)
                }
            }
            RxEvent::FsWriteComplete => {
                // Check the image before rebooting into it: on a bad
                // upload the host gets the error and can resend.
                let ok = fs::Fs::mount(&self.ctx.flash, P::FS_BASE)
                    .is_ok_and(|f| f.verify(&self.ctx.flash));
                if !ok {
                    self.ctx.fs = None;
                    self.error_code = ERR_BAD_IMAGE;
                    protocol::send_error(&self.ctx.usart, self.error_code);
                    show_error(&mut self.ctx, self.error_code, None);
                    return;
                }
                self.ack_chunk();
                self.ctx.usart.flush();
                self.ctx.eeprom.flush();
                P::reset();
            }
            RxEvent::UserMessage => {
                if self.tasks.main.has_code() {
                    if let Some(offset) = self
                        .tasks
                        .main
                        .find_by_kind(FunctionKind::OnUserMessage)
                        .map(|e| e.offset as CodeAddr)
                    {
                        let msg = self.protocol.user_message();
                        if !msg.is_empty() {
                            if let Some(arr_id) = self.ctx.objs.arr_from_bytes(msg) {
                                self.tasks.main.enqueue_callback(offset, &[arr_id as i32]);
                            }
                        }
                    }
                }
            }
            _ => {} // None handled above; extras consumed by on_extra_rx
        }
    }
}

// ============================================================================
//  Startup helpers
// ============================================================================

/// Apply the image's RES_SYSCFG: resize the USART RX ring, the object pool
/// and the VM stacks, and set the animation cap. Fields at 0 (or no entry)
/// → the board's `Platform::DEFAULT_*`, then the built-in defaults.
/// Runs once at boot (filesystem writes reboot), before the program loads.
#[inline(never)]
fn apply_syscfg<P: PlatformRuntime>(ctx: &mut Ctx<P>, vm: &mut Vm) -> u8 {
    let Some(fs) = ctx.fs.as_ref() else {
        return 0;
    };
    let cfg = match SysCfg::load(fs, &ctx.flash) {
        Ok(Some(cfg)) => Some(cfg),
        Ok(None) => None,
        Err(_) => return ERR_BAD_SYSCFG,
    };
    // A syscfg field of 0 (or no syscfg) falls back to the board default,
    // and a board default of 0 to the built-in one.
    let pick = |v: usize, board: usize| if v != 0 { v } else { board };
    let board_defaults = P::DEFAULT_ARENA_BYTES
        | P::DEFAULT_MAX_OBJECTS
        | P::DEFAULT_EVAL_STACK
        | P::DEFAULT_CALL_STACK
        | P::DEFAULT_CB_QUEUE
        != 0;
    let Some(cfg) = cfg.or_else(|| board_defaults.then(SysCfg::default)) else {
        return 0;
    };
    let mut ok = ctx.usart.set_rx_capacity(cfg.uart_rx_bytes as usize);
    ok &= ctx.objs.configure(
        pick(cfg.arena_bytes as usize, P::DEFAULT_ARENA_BYTES),
        pick(cfg.max_objects as usize, P::DEFAULT_MAX_OBJECTS),
    );
    ok &= vm.configure(
        pick(cfg.eval_stack as usize, P::DEFAULT_EVAL_STACK),
        pick(cfg.call_stack as usize, P::DEFAULT_CALL_STACK),
        pick(cfg.cb_queue as usize, P::DEFAULT_CB_QUEUE),
        cfg.anim_slots as usize,
    );
    if ok { 0 } else { ERR_INSUFFICIENT_MEMORY }
}

/// Load the RES_PROGRAM `program` names (and its widget page) into the reset
/// VM: `main` at boot, others through `runProgram`.
#[inline(never)]
fn load_program<P: PlatformRuntime>(ctx: &mut Ctx<P>, vm: &mut Vm, program: ProgramRef) -> u8 {
    let err = load_program_code(ctx, vm, program);
    apply_orientation(ctx, vm);
    err
}

/// Load program `program` (and its widget page) into the reset VM, without
/// touching the screen orientation (a child task keeps main's).
fn load_program_code<P: PlatformRuntime>(ctx: &mut Ctx<P>, vm: &mut Vm, program: ProgramRef) -> u8 {
    vm.program = program;
    let name = program.name();
    let entry = match ctx
        .fs
        .as_ref()
        .and_then(|fs| fs.find_kind(&ctx.flash, name, fs::RES_PROGRAM))
    {
        Some(e) => e,
        None => {
            return if ctx.fs.is_some() {
                ERR_PROGRAM_NOT_FOUND
            } else {
                ERR_NO_FILESYSTEM
            };
        }
    };

    let mut err = 0;
    // A RAM image over the platform limit runs from flash rather than being
    // truncated (which would cut off the functions at the end of the image).
    let size = entry.size as usize;
    let mut img = Vec::new();
    // Too big for the RAM limit, or the heap can't hold it: also run from flash.
    let flash_exec = entry.flags & fs::RES_FLAG_FLASH_EXEC != 0
        || size > P::MAX_CODE_SIZE
        || img.try_reserve_exact(size).is_err();
    if flash_exec {
        // The VM keeps its own (cloned) flash handle for cache refills; the
        // backend is a stateless MMIO driver so the clone shares no state.
        if !vm.load_flash(ctx.flash.clone(), entry.offset, entry.size as usize) {
            err = ERR_PROGRAM_ERROR;
        }
    } else {
        img.resize(size, 0);
        if let Some(fs) = ctx.fs.as_ref() {
            fs.read_resource(&ctx.flash, &entry, 0, &mut img);
        }
        if vm.load_ram_owned(img).is_err() {
            err = ERR_PROGRAM_ERROR;
        }
    }
    // Static widget setup the compiler moved out of the bytecode (optional).
    if let Some(page) = ctx
        .fs
        .as_ref()
        .and_then(|fs| fs.find_kind(&ctx.flash, name, fs::RES_PAGE))
    {
        vm.set_page(page.offset, page.size);
    }
    err
}

/// Per-pool memory usage for the `MemDetail` USART command.
fn mem_detail<P: PlatformRuntime>(ctx: &Ctx<P>, vm: &Vm) -> [protocol::MemEntry; 12] {
    use protocol::{MemEntry, mem_pool};
    let e = |id: u8, used: usize, capacity: usize, extra: usize| MemEntry {
        id,
        used: used as u32,
        capacity: capacity as u32,
        extra: extra as u32,
    };
    let (free, largest) = P::heap_stats();
    let total = P::heap_total();
    let (stack_used, stack_free) = P::stack_info();
    let (rx_waiting, rx_size) = ctx.usart.rx_stats();
    let objs = &ctx.objs;
    let max_objs = objs.capacity_objects();
    let [eval, call, cbq, anims] = vm.mem_stats::<P>();
    [
        e(mem_pool::HEAP, total.saturating_sub(free), total, largest),
        e(mem_pool::CPU_STACK, stack_used as usize, (stack_used + stack_free) as usize, P::stack_peak() as usize),
        e(mem_pool::OBJ_ARENA, objs.used_bytes(), objs.capacity_bytes(), objs.garbage_bytes()),
        e(mem_pool::OBJ_PTRS, objs.live_objects(T_FREE), max_objs, objs.heap_bytes()),
        e(mem_pool::UART_RX, rx_waiting, rx_size, rx_size),
        e(mem_pool::WIDGETS, objs.live_objects(T_WIDGET), max_objs, objs.bytes_of(T_WIDGET)),
        e(mem_pool::STRINGS, objs.live_objects(T_STRING), max_objs, objs.bytes_of(T_STRING)),
        e(mem_pool::ARRAYS, objs.live_objects(T_ARRAY), max_objs, objs.bytes_of(T_ARRAY)),
        e(mem_pool::EVAL_STACK, eval.0, eval.1, eval.2),
        e(mem_pool::CALL_STACK, call.0, call.1, call.2),
        e(mem_pool::CB_QUEUE, cbq.0, cbq.1, cbq.2),
        e(mem_pool::ANIMS, anims.0, anims.1, anims.2),
    ]
}

/// Rotate the screen as the loaded image asks (header portrait flag) and size
/// the root widget to the resulting logical screen.
fn apply_orientation<P: PlatformRuntime>(ctx: &mut Ctx<P>, vm: &Vm) {
    ctx.lcd.set_portrait(vm.portrait);
    let size = Size {
        w: ctx.lcd.width(),
        h: ctx.lcd.height(),
    };
    let root = ctx.objs.root;
    ctx.objs.get_mut(root).size = size;
}

#[inline(never)]
fn start_program<P: PlatformRuntime>(ctx: &mut Ctx<P>, vm: &mut Vm) -> u8 {
    if let Some(offset) = vm.find_by_kind(FunctionKind::Setup).map(|e| e.offset as CodeAddr) {
        vm.call_now(offset, &[], ctx);
        // setup() called runProgram: the main loop switches.
        if vm.state == VmState::Exit {
            return 0;
        }
        if vm.pop_result() != 0 {
            return ERR_PROGRAM_ERROR;
        }
    }
    if let Some(offset) = vm
        .find_by_kind(FunctionKind::OnProgramStart)
        .map(|e| e.offset as CodeAddr)
    {
        vm.call_now(offset, &[], ctx);
        if vm.state == VmState::Exit {
            return 0;
        }
    }
    if let Some(offset) = vm.find_by_kind(FunctionKind::Loop).map(|e| e.offset as CodeAddr) {
        vm.start_main(offset);
    }
    0
}

/// Allocate the full-screen root widget into the (empty) object pool.
fn alloc_root<P: PlatformRuntime>(ctx: &mut Ctx<P>) {
    let Some(root) = ctx.objs.alloc_widget() else {
        return;
    };
    let w = ctx.objs.get_mut(root);
    w.size = Size {
        w: P::LcdB::WIDTH,
        h: P::LcdB::HEIGHT,
    };
    w.background_color = P::BG_COLOR;
    ctx.objs.root = root;
}

/// `runProgram(name, arg)` ended the running program: drop all of its state
/// (widgets, strings, arrays, images, sound, input focus) and start
/// `vm.next_program` from scratch, as at boot. Returns the new error code.
#[inline(never)]
fn switch_program<P: PlatformRuntime>(ctx: &mut Ctx<P>, vm: &mut Vm, input: &mut P::Input) -> u8 {
    let Some(next) = vm.next_program.take() else {
        return 0;
    };
    ctx.objs.clear();
    ctx.images.clear();
    vm.reset();
    #[cfg(feature = "audio")]
    ctx.audio.stop();
    input.reset(ctx);
    ctx.cursor_visible = false;
    alloc_root(ctx);

    let mut err = load_program(ctx, vm, next);
    if err == 0 && vm.has_code() {
        err = start_program(ctx, vm);
    }
    if err != 0 {
        show_error(ctx, err, None);
    } else if vm.state != VmState::Exit {
        P::initial_render(ctx, vm);
    }
    err
}

/// Runtime program reload (USART `ProgramReady`). Returns the new error code.
#[inline(never)]
fn reload_program<P: PlatformRuntime>(
    ctx: &mut Ctx<P>,
    vm: &mut Vm,
    input: &mut P::Input,
    protocol: &mut Protocol,
) -> u8 {
    ctx.objs.clear();
    vm.reset();
    #[cfg(feature = "audio")]
    ctx.audio.stop();
    input.reset(ctx);
    ctx.cursor_visible = false;

    alloc_root(ctx);

    if let Err(prog) = vm.load_ram_owned(protocol.take_program()) {
        vm.load_raw(prog);
    }
    apply_orientation(ctx, vm);

    if vm.has_code() {
        start_program(ctx, vm)
    } else {
        // No header/functions — run opcodes from the top (legacy compat).
        vm.state = VmState::Running;
        0
    }
}

/// Show the Code 6 screen if the VM has faulted. Returns the new error code.
fn check_vm_fault<P: PlatformRuntime>(ctx: &mut Ctx<P>, vm: &Vm) -> u8 {
    if vm.state != VmState::Error {
        return 0;
    }
    show_error(ctx, ERR_PROGRAM_ERROR, Some((vm.fault(), vm.fault_pc())));
    ERR_PROGRAM_ERROR
}

/// Run the on_paint dispatchers for the widgets in `paint`: each task paints
/// the widgets it handles.
fn run_paint<P: PlatformRuntime>(ctx: &mut Ctx<P>, tasks: &mut Tasks, paint: &render::PaintList) {
    #[cfg(not(feature = "multi_program"))]
    tasks.main.run_paint(ctx, paint);
    #[cfg(feature = "multi_program")]
    each_task(ctx, tasks, |ctx, vm| {
        let me = ctx.objs.cur_task;
        let mut mine = render::PaintList::new();
        for &id in paint.ids() {
            if ctx.objs.handler_of(id) == me {
                mine.push(id);
            }
        }
        vm.run_paint(ctx, &mine);
    });
}

// ============================================================================
//  Child tasks (`multi_program`)
// ============================================================================

impl<P: PlatformRuntime> Runtime<P> {
    /// End every child task (main program switch, reload, filesystem write).
    fn drop_children(&mut self) {
        #[cfg(feature = "multi_program")]
        {
            self.tasks.drop_kids();
            self.ctx.tasks.reset();
            self.fault_dialog = WidgetId::NONE;
        }
    }
}

#[cfg(feature = "multi_program")]
impl<P: PlatformRuntime> Runtime<P> {
    /// Once per pass, after the bursts and callbacks: end the children that
    /// exited or faulted, act on kill / spawn requests, deliver messages.
    fn service_tasks(&mut self) {
        use crate::task::{MAX_TASKS, SlotState, mask_slots};
        for id in 1..MAX_TASKS as u8 {
            let Some(vm) = self.tasks.get(id) else {
                continue;
            };
            match vm.state {
                VmState::Error => {
                    let (fault, pc) = (vm.fault(), vm.fault_pc());
                    self.end_task(id, fault as i32, Some(pc));
                }
                VmState::Exit => match vm.next_program.take() {
                    Some(next) => self.restart_task(id, next),
                    None => self.end_task(id, 0, None),
                },
                _ => {}
            }
        }
        let kills = core::mem::take(&mut self.ctx.tasks.kills);
        for id in mask_slots(kills) {
            if self.ctx.tasks.alive(id as i32) {
                self.end_task(id, 0, None);
            }
        }
        for id in 1..MAX_TASKS as u8 {
            if self.ctx.tasks.slots[id as usize].state == SlotState::Starting {
                self.start_task(id);
            }
        }
        while let Some(m) = self.ctx.tasks.mail.pop_front() {
            if let Some(vm) = self.tasks.get(m.to) {
                send_kind(vm, FunctionKind::OnTaskMessage, &[m.from as i32, m.a, m.b]);
            }
        }
    }

    /// Start the program a `spawnProgram` reserved slot `id` for: a VM sized
    /// like main's, its code + widget page, `setup()` / `on_program_start()`,
    /// then its `loop()` runs from the next pass.
    fn start_task(&mut self, id: u8) {
        use crate::task::{SlotState, TASK_MAIN};
        let program = self.ctx.tasks.slots[id as usize].program;
        let Some(vm) = self.tasks.main.sibling() else {
            return self.end_task(id, -1, None);
        };
        let mut vm = Box::new(vm);
        self.ctx.tasks.slots[id as usize].state = SlotState::Running;
        self.ctx.objs.cur_task = id;
        let mut err = load_program_code(&mut self.ctx, &mut vm, program);
        if err == 0 && vm.has_code() {
            err = start_program(&mut self.ctx, &mut vm);
        }
        self.ctx.objs.cur_task = TASK_MAIN;
        let fault = (vm.state == VmState::Error).then(|| (vm.fault(), vm.fault_pc()));
        self.tasks.kids[id as usize - 1] = Some(vm);
        if let Some((fault, pc)) = fault {
            self.end_task(id, fault as i32, Some(pc));
        } else if err != 0 {
            self.end_task(id, -1, None);
        }
    }

    /// Free what task `id` holds: its VM (callbacks, animations, dialogs),
    /// its objects, the input focus on its widgets; repaint the screen.
    fn clear_task(&mut self, id: u8) {
        self.tasks.kids[id as usize - 1] = None;
        self.ctx.objs.free_task(id);
        let root = self.ctx.objs.root;
        self.ctx.objs.mark_dirty(root);
        self.input.forget_dead(&mut self.ctx);
    }

    /// End task `id`: `code` 0 = exited or killed, > 0 = VM fault (at
    /// `fault_pc`), -1 = could not start. Its parent (main when gone) gets
    /// `on_task_exit(id, code)`; a fault nobody handles opens the fault dialog.
    fn end_task(&mut self, id: u8, code: i32, fault_pc: Option<CodeAddr>) {
        use crate::task::TASK_MAIN;
        self.clear_task(id);
        let info = self.ctx.tasks.release(id);
        let parent = if self.ctx.tasks.alive(info.parent as i32) { info.parent } else { TASK_MAIN };
        let handled = self
            .tasks
            .get(parent)
            .is_some_and(|vm| send_kind(vm, FunctionKind::OnTaskExit, &[id as i32, code]));
        if let Some(pc) = fault_pc {
            protocol::send_vm_error(&self.ctx.usart, ERR_PROGRAM_ERROR, code as u8, pc as u32);
            if !handled {
                self.show_fault_dialog(info.program.name(), code as u8, pc);
            }
        }
    }

    /// `runProgram` in a child: the same slot (id, parent, foreground) starts
    /// `next` from scratch later in this pass.
    fn restart_task(&mut self, id: u8, next: ProgramRef) {
        self.clear_task(id);
        let slot = &mut self.ctx.tasks.slots[id as usize];
        slot.program = next;
        slot.state = crate::task::SlotState::Starting;
        self.ctx.tasks.mail.retain(|m| m.to != id);
    }

    /// "Program <name> stopped" over everything, with the fault and its PC
    /// and an OK button. Runtime-owned widgets (`TASK_SYSTEM`): no task frees
    /// them, and their click comes back through `Tasks::sys_click`.
    fn show_fault_dialog(&mut self, name: &[u8], fault: u8, pc: CodeAddr) {
        use crate::task::{TASK_MAIN, TASK_SYSTEM};
        self.close_fault_dialog();
        self.ctx.objs.cur_task = TASK_SYSTEM;
        let built = build_fault_dialog(&mut self.ctx, name, fault, pc);
        self.ctx.objs.cur_task = TASK_MAIN;
        match built {
            Some(d) => self.fault_dialog = d,
            // Out of objects: drop whatever got built.
            None => self.ctx.objs.free_task(TASK_SYSTEM),
        }
    }

    fn close_fault_dialog(&mut self) {
        let d = self.fault_dialog;
        self.fault_dialog = WidgetId::NONE;
        if self.ctx.objs.is_live(d) && self.ctx.objs.owner_of(d.0) == crate::task::TASK_SYSTEM {
            self.ctx.objs.free_task(crate::task::TASK_SYSTEM);
            let root = self.ctx.objs.root;
            self.ctx.objs.mark_dirty(root);
        }
    }

    /// The fault dialog's OK button was clicked.
    fn close_fault_dialog_on_click(&mut self) {
        if core::mem::take(&mut self.tasks.sys_click) {
            self.close_fault_dialog();
        }
    }
}

/// A line of text assembled from parts (cut at 64 bytes).
#[cfg(feature = "multi_program")]
struct Line {
    buf: [u8; 64],
    len: usize,
}

#[cfg(feature = "multi_program")]
impl Line {
    fn new() -> Self {
        Self { buf: [0; 64], len: 0 }
    }
    fn put(&mut self, s: &[u8]) -> &mut Self {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s[..n]);
        self.len += n;
        self
    }
    fn dec(&mut self, v: u32) -> &mut Self {
        let mut d = [0u8; 10];
        let (mut v, mut n) = (v, 0);
        loop {
            d[9 - n] = b'0' + (v % 10) as u8;
            n += 1;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        self.put(&d[10 - n..])
    }
    /// Upper-case hex, 4 digits (8 past 0xFFFF).
    fn hex(&mut self, v: u32) -> &mut Self {
        let digits = if v > 0xFFFF { 8 } else { 4 };
        for i in (0..digits).rev() {
            self.put(&[b"0123456789ABCDEF"[((v >> (4 * i)) & 0xF) as usize]]);
        }
        self
    }
    fn text(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// A runtime widget under `parent`, with centred `text` (if any).
#[cfg(feature = "multi_program")]
#[allow(clippy::too_many_arguments)]
fn sys_widget<P: PlatformRuntime>(
    ctx: &mut Ctx<P>,
    parent: WidgetId,
    kind: u8,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    text: &[u8],
) -> Option<WidgetId> {
    let id = ctx.objs.alloc_widget()?;
    ctx.objs.add_child(parent, id);
    let wd = ctx.objs.get_mut(id);
    wd.kind = kind;
    wd.location = crate::types::Offset { x, y };
    wd.size = Size { w, h };
    if !text.is_empty() {
        let s = ctx.objs.str_alloc(text)?;
        ctx.objs.update_ext(id, |e| {
            e.text_id = s;
            e.text_color = P::FG_COLOR;
            e.text_align = widget::ALIGN_CENTER;
        });
    }
    Some(id)
}

/// The task-fault dialog (see `Runtime::show_fault_dialog`).
#[cfg(feature = "multi_program")]
fn build_fault_dialog<P: PlatformRuntime>(
    ctx: &mut Ctx<P>,
    name: &[u8],
    fault: u8,
    pc: CodeAddr,
) -> Option<WidgetId> {
    use widget::{FLAG_CLICKABLE, FLAG_HAS_ON_CLICK, KIND_BASE, KIND_BUTTON, KIND_LABEL};
    let root = ctx.objs.root;
    let screen = ctx.objs.get(root).size;
    let (bw, bh) = (480u16.min(screen.w), 170u16.min(screen.h));
    let (x, y) = ((screen.w - bw) as i16 / 2, (screen.h - bh) as i16 / 2);
    let dialog = sys_widget(ctx, root, KIND_BASE, x, y, bw, bh, b"")?;
    let w = ctx.objs.get_mut(dialog);
    w.background_color = P::BG_COLOR;
    w.border_color = COLOR_RED;
    ctx.objs.update_ext(dialog, |e| e.border = crate::types::Edges::uniform(2));
    let mut lines = [Line::new(), Line::new(), Line::new()];
    lines[0].put(b"Program '").put(name).put(b"' stopped");
    lines[1].put(b"VM ").dec(fault as u32).put(b": ").put(crate::vm::fault::name(fault));
    lines[2].put(b"PC 0x").hex(pc as u32);
    for (i, l) in lines.iter().enumerate() {
        sys_widget(ctx, dialog, KIND_LABEL, 8, 12 + 28 * i as i16, bw - 20, 24, l.text())?;
    }
    let ok = sys_widget(ctx, dialog, KIND_BUTTON, (bw as i16 - 120) / 2, bh as i16 - 56, 120, 40, b"OK")?;
    let w = ctx.objs.get_mut(ok);
    w.flags |= FLAG_CLICKABLE | FLAG_HAS_ON_CLICK;
    w.background_color = 0x4208;
    ctx.objs.update_ext(ok, |e| e.press_color = COLOR_RED);
    Some(dialog)
}

// ============================================================================
//  Render phase
// ============================================================================

fn render_phase<P: PlatformRuntime>(ctx: &mut Ctx<P>, tasks: &mut Tasks, input: &mut P::Input) {
    match tasks.main.render_mode {
        RenderMode::Buffered => {
            if render::buffered_has_dirty(ctx) {
                ctx.lcd.begin_frame();
                let paint = render::render_buffered_content(ctx);
                // on_paint into this back buffer, before the overlay + swap.
                run_paint(ctx, tasks, &paint);
                input.draw_overlay(ctx, true);
                // Rasterization is done (UI thread). Hand off the present
                // (vsync wait + buffer flip) — synchronous by default, or
                // deferred to a render thread by BSPs that override this.
                P::present_buffered(ctx);
            }
        }
        RenderMode::Dirty => {
            if !P::render_due() {
                return;
            }
            let mut paint = render::PaintList::new();
            render::collect_dirty_paint(ctx, &mut paint);
            render::render_dirty(ctx);
            run_paint(ctx, tasks, &paint);
            input.draw_overlay(ctx, false);
            ctx.lcd.flush_dirty();
        }
        RenderMode::EPaper => {
            // EPD updates are driven exclusively by OP_W_RENDER inside the
            // program; the per-frame loop does nothing here.
        }
    }
}

// ============================================================================
//  Error screen
// ============================================================================

/// `vm_fault` = `(fault code, faulting PC)` for VM runtime errors.
#[inline(never)]
pub fn show_error<P: PlatformRuntime>(
    ctx: &mut Ctx<P>,
    code: u8,
    vm_fault: Option<(u8, CodeAddr)>,
) {
    let sw = ctx.lcd.width();
    let sh = ctx.lcd.height();
    let bg = P::BG_COLOR;
    let fg = P::FG_COLOR;

    ctx.lcd.begin_frame();
    ctx.lcd.fill_rect(0, 0, sw, sh, bg);

    let bw: u16 = 460;
    let bh: u16 = 210;
    let bx: u16 = (sw - bw) / 2;
    let by: u16 = (sh - bh) / 2;

    ctx.lcd.draw_rect(bx, by, bw, bh, COLOR_RED);
    ctx.lcd.fill_rect(bx + 2, by + 2, bw - 4, bh - 4, bg);

    let font = ctx.fonts.embedded();
    let center = |w: usize| bx as i16 + (bw as i16 - w as i16) / 2;
    let ty = by as i16 + 34;

    let title = b"ERROR";
    font.draw_str(
        &ctx.lcd,
        &ctx.flash,
        title,
        center(font.text_width(title) as usize),
        ty,
        COLOR_RED,
        Some(bg),
    );

    let mut code_line = [0u8; 16];
    let code_len = format_error_code(code, &mut code_line);
    font.draw_str(
        &ctx.lcd,
        &ctx.flash,
        &code_line[..code_len],
        center(font.text_width(&code_line[..code_len]) as usize),
        ty + 30,
        fg,
        Some(bg),
    );

    let desc = error_description(code);
    font.draw_str(
        &ctx.lcd,
        &ctx.flash,
        desc,
        center(font.text_width(desc) as usize),
        ty + 60,
        fg,
        Some(bg),
    );

    if let Some((fault, pc)) = vm_fault {
        // "VM 9: invalid array"
        let mut fault_line = [0u8; 40];
        let mut n = 0;
        for &b in b"VM " {
            fault_line[n] = b;
            n += 1;
        }
        if fault >= 100 {
            fault_line[n] = b'0' + fault / 100;
            n += 1;
        }
        if fault >= 10 {
            fault_line[n] = b'0' + (fault / 10) % 10;
            n += 1;
        }
        fault_line[n] = b'0' + fault % 10;
        n += 1;
        fault_line[n] = b':';
        fault_line[n + 1] = b' ';
        n += 2;
        let name = crate::vm::fault::name(fault);
        let take = name.len().min(fault_line.len() - n);
        fault_line[n..n + take].copy_from_slice(&name[..take]);
        n += take;
        font.draw_str(
            &ctx.lcd,
            &ctx.flash,
            &fault_line[..n],
            center(font.text_width(&fault_line[..n]) as usize),
            ty + 90,
            fg,
            Some(bg),
        );

        let mut pc_buf = [0u8; 14];
        let pc_len = format_hex_pc(b"PC: 0x", pc as u32, &mut pc_buf);
        font.draw_str(
            &ctx.lcd,
            &ctx.flash,
            &pc_buf[..pc_len],
            center(font.text_width(&pc_buf[..pc_len]) as usize),
            ty + 120,
            fg,
            Some(bg),
        );
    }

    ctx.lcd.end_frame();
    ctx.lcd.flush_dirty();

    match vm_fault {
        Some((fault, pc)) => protocol::send_vm_error(&ctx.usart, code, fault, pc as u32),
        None => protocol::send_error(&ctx.usart, code),
    }
}

fn error_description(code: u8) -> &'static [u8] {
    match code {
        1 => b"page_main not found",
        2 => b"main program not found",
        3 => b"image not found",
        4 => b"font not found",
        5 => b"no file system in flash",
        6 => b"program execution error",
        7 => b"insufficient memory",
        8 => b"invalid system config",
        9 => b"flash image corrupted",
        _ => b"error",
    }
}

fn format_error_code(code: u8, buf: &mut [u8; 16]) -> usize {
    buf[..6].copy_from_slice(b"Code: ");
    if code >= 10 {
        buf[6] = b'0' + code / 10;
        buf[7] = b'0' + code % 10;
        8
    } else {
        buf[6] = b'0' + code;
        7
    }
}

/// Hex PC: 4 digits, or 8 once it no longer fits 16 bits (wide images).
fn format_hex_pc(prefix: &[u8], val: u32, buf: &mut [u8; 14]) -> usize {
    let plen = prefix.len().min(6);
    buf[..plen].copy_from_slice(&prefix[..plen]);
    let hex = b"0123456789ABCDEF";
    let digits = if val > 0xFFFF { 8 } else { 4 };
    for i in 0..digits {
        let shift = (digits - 1 - i) * 4;
        buf[plen + i] = hex[((val >> shift) & 0xF) as usize];
    }
    plen + digits
}

// ============================================================================
//  Shared widget helpers (used by InputHandler implementations)
// ============================================================================

/// Convert a touch X within a slider's rect to a 0..100 value.
pub fn touch_to_slider_value(abs: &crate::types::Rect, touch_x: u16) -> i16 {
    let x = touch_x as i16;
    if x <= abs.x {
        0
    } else if x >= abs.x + abs.w as i16 {
        100
    } else {
        (((x - abs.x) as u32) * 100 / abs.w as u32) as i16
    }
}

pub fn dropdown_parent<P: Platform>(ctx: &Ctx<P>, id: WidgetId) -> WidgetId {
    if id.is_none() {
        return WidgetId::NONE;
    }
    let parent = ctx.objs.get(id).parent;
    if parent.is_some() && ctx.objs.get(parent).kind == widget::KIND_DROPDOWN {
        parent
    } else {
        WidgetId::NONE
    }
}

pub fn dropdown_child_index<P: Platform>(
    ctx: &Ctx<P>,
    dropdown: WidgetId,
    option: WidgetId,
) -> i16 {
    let mut idx: i16 = 0;
    let mut child = ctx.objs.get(dropdown).first_child;
    let max = ctx.objs.count();
    let mut guard = 0usize;
    while child.is_some() {
        if child == option {
            return idx;
        }
        child = ctx.objs.get(child).next_sibling;
        idx += 1;
        guard += 1;
        if guard > max {
            break;
        }
    }
    0
}

pub fn set_dropdown_options_visible<P: Platform>(
    ctx: &mut Ctx<P>,
    dropdown: WidgetId,
    visible: bool,
) {
    let mut child = ctx.objs.get(dropdown).first_child;
    let max = ctx.objs.count();
    let mut guard = 0usize;
    while child.is_some() {
        if visible {
            ctx.objs.get_mut(child).flags |= widget::FLAG_VISIBLE;
        } else {
            ctx.objs.get_mut(child).flags &= !widget::FLAG_VISIBLE;
        }
        ctx.objs.mark_dirty(child);
        child = ctx.objs.get(child).next_sibling;
        guard += 1;
        if guard > max {
            break;
        }
    }
    ctx.objs.mark_dirty(dropdown);
}

/// Whether pressing or releasing `id` changes what it draws, so it must be
/// redrawn: it has a `press_color` (it and the children sharing its
/// background take it), or its kind reacts to the touch itself (slider,
/// input, checkbox, radio, dropdown…), or its `on_paint` might. A plain box,
/// label, button, image or shape without a press colour looks the same:
/// redrawing it is wasted, and costly for a big one (a full-screen
/// clickable scrim, or a layer's root, whose whole surface is redrawn and
/// copied).
fn press_shows(objs: &ObjPool, id: WidgetId) -> bool {
    let w = objs.get(id);
    let plain = matches!(
        w.kind,
        widget::KIND_BASE
            | widget::KIND_LABEL
            | widget::KIND_BUTTON
            | widget::KIND_IMAGE
            | widget::KIND_RECTANGLE
            | widget::KIND_CIRCLE
            | widget::KIND_POLYGON
            | widget::KIND_LINE
            | widget::KIND_ELLIPSE
    );
    !plain || w.flags & widget::FLAG_HAS_ON_PAINT != 0 || objs.press_color(id) != 0
}

pub fn toggle_dropdown<P: Platform>(ctx: &mut Ctx<P>, dropdown: WidgetId) {
    let expanded = ctx.objs.get(dropdown).flags & widget::FLAG_CHECKED != 0;
    if expanded {
        ctx.objs.get_mut(dropdown).flags &= !widget::FLAG_CHECKED;
        set_dropdown_options_visible(ctx, dropdown, false);
    } else {
        ctx.objs.get_mut(dropdown).flags |= widget::FLAG_CHECKED;
        set_dropdown_options_visible(ctx, dropdown, true);
    }
}

pub fn select_dropdown_option<P: Platform>(
    ctx: &mut Ctx<P>,
    dropdown: WidgetId,
    option: WidgetId,
) {
    let text_id = ctx.objs.text_id(option);
    let selected = dropdown_child_index(ctx, dropdown, option);
    ctx.objs.update_ext(dropdown, |ext| {
        ext.text_id = text_id;
        ext.value = selected as WValue;
    });
    ctx.objs.get_mut(dropdown).flags &= !widget::FLAG_CHECKED;
    set_dropdown_options_visible(ctx, dropdown, false);
}

pub fn scrollbar_set_position<P: Platform>(
    ctx: &mut Ctx<P>,
    scroll_id: WidgetId,
    touch_y: u16,
) {
    let cr = ctx.objs.content_rect(scroll_id);
    let ch = ctx.objs.content_height(scroll_id) as i32;
    let vh = cr.h as i32;
    if ch <= vh {
        return;
    }
    let max_scroll = ch - vh;

    let ty = touch_y as i32;
    let track_top = cr.y as i32;
    let track_h = cr.h as i32;
    if track_h <= 0 {
        return;
    }
    let ratio = ((ty - track_top).clamp(0, track_h) * 1000) / track_h;
    let new_scroll = ((ratio * max_scroll) / 1000).clamp(0, max_scroll) as i16;

    ctx.objs.update_ext(scroll_id, |ext| {
        ext.value = new_scroll as WValue;
    });
    ctx.objs.mark_dirty(scroll_id);
}

// ============================================================================
//  BasicInput — minimal press → on_click (epaper buttons)
// ============================================================================

/// Stateless input handler: a press marks the hit widget pressed; a release on
/// a widget fires its `on_click` / `on_tap`. No keyboard, scrollbars, gestures.
pub struct BasicInput;

impl<P: PlatformRuntime> InputHandler<P> for BasicInput {
    fn new() -> Self {
        BasicInput
    }

    #[inline(never)]
    fn handle(&mut self, ctx: &mut Ctx<P>, tasks: &mut Tasks, touch: &mut TouchImpl<P::TouchB>) {
        let mut event = match touch.poll() {
            Some(e) => e,
            None => return,
        };
        (event.x, event.y) = ctx.lcd.touch_to_logical(event.x, event.y);
        match event.kind {
            TouchEventKind::Press => {
                let hit = crate::touch::hit_test(&mut ctx.objs, event.x, event.y);
                if hit.is_some() {
                    ctx.objs.get_mut(hit).flags |= widget::FLAG_PRESSED;
                    if press_shows(&ctx.objs, hit) {
                        ctx.objs.mark_dirty(hit);
                    }
                }
            }
            TouchEventKind::Release => {
                let hit = crate::touch::hit_test(&mut ctx.objs, event.x, event.y);
                if hit.is_none() {
                    return;
                }
                ctx.objs.get_mut(hit).flags &= !widget::FLAG_PRESSED;
                if press_shows(&ctx.objs, hit) {
                    ctx.objs.mark_dirty(hit);
                }
                if ctx.objs.get(hit).flags & widget::FLAG_HAS_ON_CLICK != 0 {
                    tasks.widget_click(&ctx.objs, hit);
                }
                if ctx.objs.get(hit).flags & widget::FLAG_HAS_ON_TAP != 0
                    && ctx.objs.get(hit).kind != widget::KIND_INPUT
                {
                    let packed = ((event.x as u32) << 16 | event.y as u32) as i32;
                    tasks.widget_event(&ctx.objs, hit, FunctionKind::OnTap, &[hit.0 as i32, packed]);
                }
            }
            TouchEventKind::Hold => {}
        }
    }
}

// ============================================================================
//  FullInput — keyboard + scrollbars + sliders + gestures (Nextion)
// ============================================================================

pub struct FullInput {
    kb: keyboard::Keyboard,
    scroll_id: WidgetId,
    scroll_active: bool,
    gesture_start_x: u16,
    gesture_start_y: u16,
    /// `millis()` at the press.
    press_ms: u32,
    /// The long-press check ran for this contact (it runs once).
    long_press_checked: bool,
    long_press_fired: bool,
}

impl<P: PlatformRuntime> InputHandler<P> for FullInput {
    fn new() -> Self {
        Self {
            kb: keyboard::Keyboard::new(),
            scroll_id: WidgetId::NONE,
            scroll_active: false,
            gesture_start_x: 0,
            gesture_start_y: 0,
            press_ms: 0,
            long_press_checked: false,
            long_press_fired: false,
        }
    }

    #[inline(never)]
    fn handle(&mut self, ctx: &mut Ctx<P>, tasks: &mut Tasks, touch: &mut TouchImpl<P::TouchB>) {
        let mut event = match touch.poll() {
            Some(e) => e,
            None => return,
        };
        (event.x, event.y) = ctx.lcd.touch_to_logical(event.x, event.y);
        match event.kind {
            TouchEventKind::Press => self.on_press(ctx, tasks, event.x, event.y),
            TouchEventKind::Hold => self.on_hold(ctx, tasks, event.x, event.y),
            TouchEventKind::Release => self.on_release(ctx, tasks, event.x, event.y),
        }
    }

    #[inline(never)]
    fn tick(&mut self, ctx: &mut Ctx<P>) {
        if self.kb.visible && self.kb.focused.is_some() {
            let now = ctx.systick.millis();
            if self.kb.update_blink(now) {
                ctx.cursor_visible = self.kb.cursor_visible;
                ctx.objs.mark_dirty(self.kb.focused);
            }
        }
    }

    fn draw_overlay(&mut self, ctx: &Ctx<P>, buffered: bool) {
        if buffered {
            if self.kb.visible {
                self.kb.draw(&ctx.lcd, &ctx.fonts, &ctx.flash);
            }
        } else if self.kb.visible && self.kb.dirty {
            self.kb.draw(&ctx.lcd, &ctx.fonts, &ctx.flash);
            self.kb.dirty = false;
        }
    }

    fn reset(&mut self, ctx: &mut Ctx<P>) {
        self.kb.visible = false;
        ctx.objs.overlay = None;
        self.kb.focused = WidgetId::NONE;
        self.scroll_id = WidgetId::NONE;
        self.scroll_active = false;
        ctx.cursor_visible = false;
    }

    fn idle(&self) -> bool {
        !self.kb.visible
    }

    fn forget_dead(&mut self, ctx: &mut Ctx<P>) {
        if self.kb.focused.is_some() && !ctx.objs.is_live(self.kb.focused) {
            self.dismiss_keyboard(ctx);
        }
        if !ctx.objs.is_live(self.scroll_id) {
            self.scroll_id = WidgetId::NONE;
            self.scroll_active = false;
        }
    }
}

impl FullInput {
    fn on_press<P: PlatformRuntime>(&mut self, ctx: &mut Ctx<P>, tasks: &mut Tasks, x: u16, y: u16) {
        // Keyboard intercept.
        if self.kb.visible && y >= self.kb.top() {
            self.kb.handle_press(x, y);
            if !tasks.buffered() {
                self.kb
                    .draw_key_by_code(&ctx.lcd, &ctx.fonts, &ctx.flash, self.kb.pressed_key, true);
            }
            return;
        }

        let hit = crate::touch::hit_test(&mut ctx.objs, x, y);

        // Dismiss keyboard when tapping outside an input.
        if self.kb.visible && (!hit.is_some() || ctx.objs.get(hit).kind != widget::KIND_INPUT) {
            self.dismiss_keyboard(ctx);
        }

        self.scroll_id = WidgetId::NONE;
        self.scroll_active = false;
        self.gesture_start_x = x;
        self.gesture_start_y = y;
        self.press_ms = ctx.systick.millis();
        self.long_press_checked = false;
        self.long_press_fired = false;

        // Scrollbar hit-test.
        let scroll_target = if hit.is_some() {
            let w = ctx.objs.get(hit);
            if w.flags & widget::FLAG_CLIP_CHILDREN != 0 {
                hit
            } else {
                ctx.objs.scroll_parent(hit)
            }
        } else {
            WidgetId::NONE
        };
        if scroll_target.is_some() {
            let cr = ctx.objs.content_rect(scroll_target);
            let ch = ctx.objs.content_height(scroll_target);
            if ch > cr.h
                && (x as i16) >= cr.x + cr.w as i16 - 10
                && (x as i16) < cr.right()
                && (y as i16) >= cr.y
                && (y as i16) < cr.bottom()
            {
                self.scroll_id = scroll_target;
                self.scroll_active = true;
                scrollbar_set_position(ctx, scroll_target, y);
            }
        }

        if hit.is_some() {
            ctx.objs.get_mut(hit).flags |= widget::FLAG_PRESSED;

            if ctx.objs.get(hit).kind == widget::KIND_INPUT {
                self.focus_input(ctx, hit);
                self.position_cursor(ctx, hit, x, y);
            }

            if ctx.objs.get(hit).kind == widget::KIND_SLIDER {
                let abs = ctx.objs.absolute_rect(hit);
                let new_val = touch_to_slider_value(&abs, x);
                ctx.objs.update_ext(hit, |ext| {
                    ext.value = new_val as WValue;
                });
                // The click dispatcher takes exactly one argument, the widget
                // id (a second one would be popped *as* the id); the new value
                // is already stored, and the dispatcher passes it on to
                // handlers declared as `fn h(wid, val)`.
                if ctx.objs.get(hit).flags & widget::FLAG_HAS_ON_CLICK != 0 {
                    tasks.widget_event(&ctx.objs, hit, FunctionKind::OnClick, &[hit.0 as i32]);
                }
            }

            if press_shows(&ctx.objs, hit) {
                ctx.objs.mark_dirty(hit);
                if !tasks.buffered() {
                    render::render_dirty(ctx);
                }
            }
        }

        tasks.global_event(FunctionKind::OnTouchDown, &[x as i32, y as i32]);
    }

    fn on_hold<P: PlatformRuntime>(&mut self, ctx: &mut Ctx<P>, tasks: &mut Tasks, x: u16, y: u16) {
        if self.kb.pressed_key != keyboard::KEY_NONE {
            return;
        }

        // Scrollbar drag.
        if self.scroll_active && self.scroll_id.is_some() {
            scrollbar_set_position(ctx, self.scroll_id, y);
            if !tasks.buffered() {
                render::render_dirty(ctx);
            }
        }

        // Slider drag (the pressed slider).
        ctx.objs.ensure_dfs();
        for i in 0..ctx.objs.dfs_len() {
            let id = ctx.objs.dfs_at(i);
            let w = ctx.objs.get(id);
            if w.flags & widget::FLAG_PRESSED != 0 && w.kind == widget::KIND_SLIDER {
                let abs = ctx.objs.absolute_rect(id);
                let new_val = touch_to_slider_value(&abs, x);
                ctx.objs.update_ext(id, |ext| {
                    ext.value = new_val as WValue;
                });
                ctx.objs.mark_dirty(id);
                if !tasks.buffered() {
                    render::render_dirty(ctx);
                }
                if ctx.objs.get(id).flags & widget::FLAG_HAS_ON_CLICK != 0 {
                    // One argument — see the slider press path.
                    tasks.widget_event(&ctx.objs, id, FunctionKind::OnClick, &[id.0 as i32]);
                }
                break;
            }
        }

        // Long press.
        if !self.long_press_checked
            && ctx.systick.millis().wrapping_sub(self.press_ms) >= LONG_PRESS_MS
        {
            self.long_press_checked = true;
            let dx = x as i32 - self.gesture_start_x as i32;
            let dy = y as i32 - self.gesture_start_y as i32;
            if dx * dx + dy * dy <= LONG_PRESS_MAX_MOVE_SQ {
                ctx.objs.ensure_dfs();
                let mut pressed_id = WidgetId::NONE;
                for i in 0..ctx.objs.dfs_len() {
                    let id = ctx.objs.dfs_at(i);
                    if ctx.objs.get(id).flags & widget::FLAG_PRESSED != 0 {
                        pressed_id = id;
                    }
                }
                tasks.global_event(FunctionKind::OnLongPress, &[pressed_id.0 as i32, x as i32, y as i32]);
                self.long_press_fired = true;
            }
        }

        tasks.global_event(FunctionKind::OnTouchMove, &[x as i32, y as i32]);
    }

    fn on_release<P: PlatformRuntime>(&mut self, ctx: &mut Ctx<P>, tasks: &mut Tasks, x: u16, y: u16) {
        // Keyboard key release.
        if self.kb.pressed_key != keyboard::KEY_NONE {
            let prev_key = self.kb.pressed_key;
            if let Some(action) = self.kb.handle_release(x, y) {
                self.handle_key_action(ctx, tasks, action);
            }
            if !tasks.buffered() && !self.kb.dirty && self.kb.visible {
                self.kb
                    .draw_key_by_code(&ctx.lcd, &ctx.fonts, &ctx.flash, prev_key, false);
            }
            return;
        }

        let was_scrolling = self.scroll_active;
        self.scroll_id = WidgetId::NONE;
        self.scroll_active = false;

        let dx = x as i32 - self.gesture_start_x as i32;
        let dy = y as i32 - self.gesture_start_y as i32;
        let is_swipe =
            !self.long_press_fired && !was_scrolling && dx * dx + dy * dy > SWIPE_MIN_DIST_SQ;

        let mut clicked_id = WidgetId::NONE;
        let mut swipe_widget_id = WidgetId::NONE;

        ctx.objs.ensure_dfs();
        for i in 0..ctx.objs.dfs_len() {
            let id = ctx.objs.dfs_at(i);
            if ctx.objs.get(id).flags & widget::FLAG_PRESSED != 0 {
                ctx.objs.get_mut(id).flags &= !widget::FLAG_PRESSED;
                if !was_scrolling && !is_swipe {
                    let abs = ctx.objs.absolute_rect(id);
                    if abs.contains(x, y) {
                        clicked_id = id;
                    }
                }
                if is_swipe {
                    swipe_widget_id = id;
                }
                if press_shows(&ctx.objs, id) {
                    ctx.objs.mark_dirty(id);
                }
            }
        }

        // Dropdown handling.
        let dropdown = dropdown_parent(ctx, clicked_id);
        if dropdown.is_some() {
            select_dropdown_option(ctx, dropdown, clicked_id);
            clicked_id = dropdown;
        } else if clicked_id.is_some() && ctx.objs.get(clicked_id).kind == widget::KIND_DROPDOWN {
            toggle_dropdown(ctx, clicked_id);
            clicked_id = WidgetId::NONE;
        }

        // Checkbox toggle.
        if clicked_id.is_some() && ctx.objs.get(clicked_id).kind == widget::KIND_CHECKBOX {
            ctx.objs.get_mut(clicked_id).flags ^= widget::FLAG_CHECKED;
            ctx.objs.mark_dirty(clicked_id);
        }

        // Radio / button group: select this one, deselect the rest of its
        // group (radio group_id 0 = sibling radios).
        if clicked_id.is_some()
            && matches!(ctx.objs.get(clicked_id).kind, widget::KIND_RADIO | widget::KIND_BUTTON_GROUP)
        {
            ctx.objs.select_in_group(clicked_id);
        }

        // Input: reposition cursor on tap.
        if clicked_id.is_some() && ctx.objs.get(clicked_id).kind == widget::KIND_INPUT {
            self.position_cursor(ctx, clicked_id, x, y);
            ctx.objs.mark_dirty(clicked_id);
        }

        if !tasks.buffered() {
            render::render_dirty(ctx);
        }

        // OnSwipe — mutually exclusive with on_click.
        if is_swipe {
            let abs_dx = dx.abs();
            let abs_dy = dy.abs();
            let direction: i32 = if abs_dx >= SWIPE_AXIS_RATIO * abs_dy {
                if dx < 0 { 0 } else { 1 }
            } else if abs_dy >= SWIPE_AXIS_RATIO * abs_dx {
                if dy < 0 { 2 } else { 3 }
            } else {
                -1
            };
            if direction >= 0 {
                let dominant = if direction <= 1 { dx } else { dy };
                tasks.global_event(FunctionKind::OnSwipe, &[direction, swipe_widget_id.0 as i32, dominant]);
            }
        }

        // on_click (modal overlay func id first, else the click dispatcher).
        if clicked_id.is_some() && ctx.objs.get(clicked_id).flags & widget::FLAG_HAS_ON_CLICK != 0 {
            tasks.widget_click(&ctx.objs, clicked_id);
        }

        // on_tap (skip KIND_INPUT).
        if clicked_id.is_some()
            && ctx.objs.get(clicked_id).kind != widget::KIND_INPUT
            && ctx.objs.get(clicked_id).flags & widget::FLAG_HAS_ON_TAP != 0
        {
            let packed = ((x as u32) << 16 | y as u32) as i32;
            tasks.widget_event(&ctx.objs, clicked_id, FunctionKind::OnTap, &[clicked_id.0 as i32, packed]);
        }

        tasks.global_event(FunctionKind::OnTouchUp, &[]);
    }

    // --- Input-widget / keyboard helpers ---

    fn focus_input<P: PlatformRuntime>(&mut self, ctx: &mut Ctx<P>, id: WidgetId) {
        if self.kb.focused.is_some() && self.kb.focused != id {
            ctx.objs.get_mut(self.kb.focused).flags &= !widget::FLAG_FOCUSED;
            ctx.objs.mark_dirty(self.kb.focused);
        }
        self.kb.focused = id;
        ctx.objs.get_mut(id).flags |= widget::FLAG_FOCUSED;
        ctx.objs.mark_dirty(id);
        self.kb.set_screen(ctx.lcd.width(), ctx.lcd.height());
        self.kb.visible = true;
        let top = self.kb.top();
        ctx.objs.overlay = Some(crate::types::Rect::new(0, top as i16, ctx.lcd.width(), ctx.lcd.height().saturating_sub(top)));
        self.kb.dirty = true;
        let now = ctx.systick.millis();
        self.kb.reset_blink(now);
        ctx.cursor_visible = true;
    }

    fn dismiss_keyboard<P: PlatformRuntime>(&mut self, ctx: &mut Ctx<P>) {
        if self.kb.focused.is_some() {
            ctx.objs.get_mut(self.kb.focused).flags &= !widget::FLAG_FOCUSED;
            ctx.objs.mark_dirty(self.kb.focused);
        }
        self.kb.focused = WidgetId::NONE;
        self.kb.visible = false;
        ctx.objs.overlay = None;
        ctx.cursor_visible = false;

        // The widgets under the keyboard redraw in full, not incrementally.
        ctx.objs.screen_touched();
        ctx.objs.ensure_dfs();
        for i in 0..ctx.objs.dfs_len() {
            let id = ctx.objs.dfs_at(i);
            let abs = ctx.objs.absolute_rect(id);
            if abs.y + abs.h as i16 > self.kb.top() as i16 {
                ctx.objs.mark_dirty(id);
            }
        }
    }

    fn position_cursor<P: PlatformRuntime>(
        &self,
        ctx: &mut Ctx<P>,
        id: WidgetId,
        touch_x: u16,
        touch_y: u16,
    ) {
        if ctx.objs.get(id).flags & widget::FLAG_MULTI_LINE != 0 {
            self.position_cursor_lines(ctx, id, touch_x, touch_y);
            return;
        }
        let ext = &ctx.objs.ext(id);
        let text_id = ext.text_id;
        let font_id = ext.font_id;

        let font = match ctx.fonts.resolve(font_id) {
            Some(f) => f,
            None => return,
        };

        let abs = ctx.objs.absolute_rect(id);
        let b = ctx.objs.border(id);
        let p = ctx.objs.padding(id);
        let cx = abs.x + b.left as i16 + p.left as i16;

        let mut cursor_pos: i16 = 0;
        if text_id != crate::pool::PTR_NONE {
            let text = ctx.objs.str_get(text_id);
            let tx = touch_x as i16;
            let mut acc = cx;
            let mut byte_pos = 0;
            while byte_pos < text.len() {
                let char_start = byte_pos;
                if let Some(cp) = font::utf8_next(text, &mut byte_pos) {
                    if cp == 0xFFFF {
                        continue;
                    }
                    let cw = font.char_width_cp(cp) as i16;
                    if tx < acc + cw / 2 {
                        cursor_pos = char_start as i16;
                        break;
                    }
                    acc += cw;
                    cursor_pos = byte_pos as i16;
                } else {
                    break;
                }
            }
        }

        ctx.objs.update_ext(id, |ext| {
            ext.value = cursor_pos as WValue;
        });
    }

    /// `position_cursor` for a multi-line input: the wrapped line under
    /// `touch_y` (as drawn, scrolled to the current cursor), then the
    /// character under `touch_x` in it.
    fn position_cursor_lines<P: PlatformRuntime>(
        &self,
        ctx: &mut Ctx<P>,
        id: WidgetId,
        touch_x: u16,
        touch_y: u16,
    ) {
        let ext = ctx.objs.ext(id);
        let font = match ctx.fonts.resolve(ext.font_id) {
            Some(f) => f,
            None => return,
        };
        let abs = ctx.objs.absolute_rect(id);
        let (b, p) = (ext.border, ext.padding);
        let cx = abs.x + b.left as i16 + p.left as i16;
        let cy = abs.y + b.top as i16 + p.top as i16;
        let cw = abs
            .w
            .saturating_sub(b.left as u16 + b.right as u16 + p.left as u16 + p.right as u16);
        let ch = abs
            .h
            .saturating_sub(b.top as u16 + b.bottom as u16 + p.top as u16 + p.bottom as u16);
        let pos = {
            let text: &[u8] = if ext.text_id != crate::pool::PTR_NONE {
                ctx.objs.str_get(ext.text_id)
            } else {
                &[]
            };
            let cur = (ext.value.max(0) as usize).min(text.len());
            let first = render::input_first_line(font, text, cw, ch, cur);
            let lh = (font.line_height() as i16).max(1);
            let want = first + ((touch_y as i16 - cy).max(0) / lh) as usize;
            let (mut ls, mut le) = (0, 0);
            let (mut li, mut i) = (0usize, 0usize);
            while i <= text.len() {
                let (end, next) = font.wrap_line(text, i, cw);
                ls = i;
                le = end;
                if li == want {
                    break;
                }
                li += 1;
                i = next;
            }
            let tx = touch_x as i16;
            let mut acc = cx;
            let mut pos = ls;
            let mut bp = ls;
            while bp < le {
                let at = bp;
                match font::utf8_next(text, &mut bp) {
                    Some(0xFFFF) => continue,
                    Some(cp) => {
                        let w = font.char_width_cp(cp) as i16;
                        if tx < acc + w / 2 {
                            pos = at;
                            break;
                        }
                        acc += w;
                        pos = bp;
                    }
                    None => break,
                }
            }
            pos
        };
        ctx.objs.update_ext(id, |e| e.value = pos as WValue);
    }

    fn handle_key_action<P: PlatformRuntime>(
        &mut self,
        ctx: &mut Ctx<P>,
        tasks: &mut Tasks,
        action: keyboard::KeyAction,
    ) {
        let id = self.kb.focused;
        if id.is_none() {
            return;
        }
        match action {
            keyboard::KeyAction::Char(ch) => {
                let text_id = ctx.objs.text_id(id);
                let max_len = ctx.objs.max_length(id);
                let cursor = ctx.objs.value(id).max(0) as usize;
                if text_id != crate::pool::PTR_NONE {
                    // A shared literal is edited as a private copy: store its id.
                    if let Some(new_id) = ctx.objs.str_insert_byte(text_id, cursor, ch, max_len) {
                        ctx.objs.adopt(new_id, id.0);
                        ctx.objs.update_ext(id, |ext| {
                            ext.text_id = new_id;
                            ext.value = (cursor + 1) as WValue;
                        });
                        ctx.objs.mark_dirty(id);
                        self.fire_on_change(ctx, tasks, id);
                    }
                } else {
                    let data = [ch];
                    if let Some(new_id) = ctx.objs.str_alloc(&data) {
                        ctx.objs.adopt(new_id, id.0);
                        ctx.objs.update_ext(id, |ext| {
                            ext.text_id = new_id;
                            ext.value = 1;
                        });
                        ctx.objs.mark_dirty(id);
                        self.fire_on_change(ctx, tasks, id);
                    }
                }
                let now = ctx.systick.millis();
                self.kb.reset_blink(now);
                ctx.cursor_visible = true;
            }
            keyboard::KeyAction::Backspace => {
                let text_id = ctx.objs.text_id(id);
                let cursor = ctx.objs.value(id).max(0) as usize;
                if text_id != crate::pool::PTR_NONE && cursor > 0 {
                    if let Some(new_id) = ctx.objs.str_delete_byte(text_id, cursor - 1) {
                        ctx.objs.adopt(new_id, id.0);
                        ctx.objs.update_ext(id, |ext| {
                            ext.text_id = new_id;
                            ext.value = (cursor - 1) as WValue;
                        });
                        ctx.objs.mark_dirty(id);
                        self.fire_on_change(ctx, tasks, id);
                    }
                }
                let now = ctx.systick.millis();
                self.kb.reset_blink(now);
                ctx.cursor_visible = true;
            }
            keyboard::KeyAction::Enter => {
                // A multi-line input takes the newline; tapping outside
                // it closes the keyboard.
                if ctx.objs.get(id).flags & widget::FLAG_MULTI_LINE != 0 {
                    self.handle_key_action(ctx, tasks, keyboard::KeyAction::Char(b'\n'));
                    return;
                }
                self.fire_on_change(ctx, tasks, id);
                self.dismiss_keyboard(ctx);
            }
            keyboard::KeyAction::Shift
            | keyboard::KeyAction::Symbols
            | keyboard::KeyAction::Abc => {
                // Layer change handled inside handle_release — just redraw.
            }
        }
    }

    fn fire_on_change<P: PlatformRuntime>(&self, ctx: &Ctx<P>, tasks: &mut Tasks, id: WidgetId) {
        if ctx.objs.get(id).flags & widget::FLAG_HAS_ON_TAP == 0 {
            return;
        }
        tasks.widget_event(&ctx.objs, id, FunctionKind::OnTap, &[id.0 as i32, 0]);
    }
}

// ============================================================================
//  Mock runtime — monomorphization guard target for `--features mock`.
// ============================================================================

#[cfg(feature = "mock")]
mod mock_runtime {
    use super::*;
    use crate::mock::MockPlatform;

    impl PlatformRuntime for MockPlatform {
        const BG_COLOR: u16 = 0x0000;
        const FG_COLOR: u16 = 0xFFFF;
        type Input = FullInput;

        fn reset() -> ! {
            loop {}
        }
    }

    #[allow(dead_code)]
    pub fn _guard() {
        let _: fn(Box<Ctx<MockPlatform>>, TouchImpl<crate::mock::MockTouch>) -> ! =
            run::<MockPlatform>;
        let _: fn(&mut Ctx<MockPlatform>, u8, Option<(u8, CodeAddr)>) = show_error::<MockPlatform>;
    }
}

#[cfg(all(test, feature = "mock"))]
mod mem_detail_tests {
    use super::*;
    #[cfg(feature = "audio")]
    use crate::audio::AudioImpl;
    use crate::backlight::BacklightImpl;
    use crate::flash::FlashImpl;
    use crate::font::FontList;
    use crate::gpio::GpioImpl;
    use crate::image::ImageList;
    use crate::lcd::LcdImpl;
    use crate::mock::{MockBacklight, MockFlash, MockGpio, MockLcd, MockPlatform, MockRtc, MockUsart};
    use crate::protocol::mem_pool;
    use crate::rtc::RtcImpl;
    use crate::systick::SystickImpl;
    use crate::usart::UsartImpl;

    pub(super) fn ctx() -> Ctx<MockPlatform> {
        Ctx {
            lcd: LcdImpl::with_backend(MockLcd),
            flash: FlashImpl::with_backend(MockFlash),
            fonts: FontList::new(),
            images: ImageList::new(),
            objs: ObjPool::new(),
            fs: None,
            backlight: BacklightImpl::with_backend(MockBacklight),
            rtc: RtcImpl::with_backend(MockRtc),
            usart: UsartImpl::with_backend(MockUsart),
            systick: SystickImpl::init(),
            #[cfg(feature = "audio")]
            audio: AudioImpl::none(),
            #[cfg(feature = "jpeg")]
            jpeg: crate::jpeg::JpegImpl::none(),
            gpio: GpioImpl::with_backend(MockGpio::default()),
            eeprom: crate::eeprom::EepromImpl::with_backend(crate::mock::MockEeprom::default()),
            cursor_visible: false,
            #[cfg(feature = "multi_program")]
            tasks: crate::task::TaskTable::new(),
        }
    }

    #[test]
    fn mem_detail_reports_pool_usage_and_capacity() {
        let mut ctx = ctx();
        let mut vm = Vm::new();
        assert!(ctx.objs.configure(1024, 50));
        assert!(vm.configure(20, 6, 5, 3));
        let a = ctx.objs.str_alloc(b"hello").unwrap(); // 1 + 1 + 2 words
        ctx.objs.str_alloc(b"world!").unwrap(); // 1 + 1 + 2 words
        for _ in 0..4 {
            ctx.objs.alloc_widget().unwrap(); // 1 + 6 words each
        }
        ctx.objs.arr_alloc(3).unwrap(); // 1 + 3 words
        ctx.objs.str_free(a);

        let rows = mem_detail(&ctx, &vm);
        let row = |id: u8| {
            let e = rows.iter().find(|e| e.id == id).expect("pool row");
            (e.used, e.capacity, e.extra)
        };
        assert_eq!(rows.len(), 12);
        // Block sizes in words: strings and the array header + 3, widgets header + header struct.
        let (h, w) = (crate::pool::HDR as u32, (core::mem::size_of::<crate::widget::Widget>() / 4) as u32);
        assert_eq!(row(mem_pool::OBJ_ARENA), (4 * (3 * (h + 3) + 4 * (h + w)), 1024, 4 * (h + 3)));
        let (live, max, heap) = row(mem_pool::OBJ_PTRS);
        assert_eq!((live, max), (6, 50));
        assert!(heap >= 1024 + 100);
        assert_eq!(row(mem_pool::WIDGETS), (4, 50, 4 * 4 * (h + w)));
        assert_eq!(row(mem_pool::STRINGS), (1, 50, 4 * (h + 3)));
        assert_eq!(row(mem_pool::ARRAYS), (1, 50, 4 * (h + 3)));
        assert_eq!(row(mem_pool::EVAL_STACK), (0, 20, 80));
        assert_eq!((row(mem_pool::CALL_STACK).0, row(mem_pool::CALL_STACK).1), (0, 6));
        assert_eq!((row(mem_pool::CB_QUEUE).0, row(mem_pool::CB_QUEUE).1), (0, 5));
        assert_eq!((row(mem_pool::ANIMS).0, row(mem_pool::ANIMS).1), (0, 3));
        // Mock USART doesn't report its ring.
        assert_eq!(row(mem_pool::UART_RX), (0, 0, 0));
    }
}

#[cfg(all(test, feature = "mock", feature = "multi_program"))]
mod multi_tests {
    //! Several programs on one tree, through `Runtime::new` + `step` on a
    //! mock filesystem with hand-assembled programs.
    use super::*;
    use crate::mock::MockPlatform;
    use crate::proto::{PROP_ON_CLICK, PROP_SIZE_H, PROP_SIZE_W};
    use crate::task::{SlotState, TASK_SYSTEM};
    use crate::touch::TouchImpl;

    // Opcodes (vm.rs).
    const HALT: u8 = 0x00;
    const POP: u8 = 0x01;
    const DUP: u8 = 0x02;
    const RET: u8 = 0x13;
    const YIELD: u8 = 0x14;
    const W_ALLOC: u8 = 0x1A;
    const PUSH_0: u8 = 0x20;
    const PUSH_1: u8 = 0x21;
    const PUSH_I8: u8 = 0x30;
    const LOAD: u8 = 0x33;
    const STORE: u8 = 0x34;
    const JMP: u8 = 0x35;
    const W_SET: u8 = 0x3A;
    const W_TARGET_S: u8 = 0x42;
    const W_PARENT_S: u8 = 0x43;
    const STR_LIT: u8 = 0x88;
    const RUN_PROGRAM: u8 = 0xBD;
    const PROGRAM_ARG: u8 = 0xBE;
    const SPAWN: u8 = 0xD4;
    const TASK_ID: u8 = 0xD6;
    const SEND: u8 = 0xD8;
    const BAD: u8 = 0xFF;

    const SETUP: u8 = FunctionKind::Setup as u8;
    const LOOP: u8 = FunctionKind::Loop as u8;
    const ON_CLICK: u8 = FunctionKind::OnClick as u8;
    const ON_TOUCH_DOWN: u8 = FunctionKind::OnTouchDown as u8;
    const ON_MSG: u8 = FunctionKind::OnTaskMessage as u8;
    const ON_EXIT: u8 = FunctionKind::OnTaskExit as u8;

    /// `loop() { yield; }`: the first function, so its jump target is 0.
    const IDLE: &[u8] = &[YIELD, JMP, 0, 0];

    fn lit(s: &[u8]) -> Vec<u8> {
        let mut v = alloc::vec![STR_LIT, s.len() as u8];
        v.extend_from_slice(s);
        v
    }

    /// Alloc a widget under the root, keep its id in global 0, target it.
    const NEW_WIDGET: &[u8] = &[W_ALLOC, DUP, STORE, 0, W_TARGET_S, PUSH_0, W_PARENT_S];
    /// Target the widget in global 0.
    const MY_WIDGET: &[u8] = &[LOAD, 0, W_TARGET_S];

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    /// v4 image with 8 globals; functions laid out in order.
    fn prog(funcs: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut img = alloc::vec![4, funcs.len() as u8, 0, 8, 0, 0, 0, 0, 0];
        let mut off = 0u32;
        for (i, (kind, code)) in funcs.iter().enumerate() {
            img.extend_from_slice(&(i as u16).to_le_bytes());
            img.extend_from_slice(&[*kind, 0]);
            img.extend_from_slice(&off.to_le_bytes());
            img.extend_from_slice(&(code.len() as u32).to_le_bytes());
            off += code.len() as u32;
        }
        for (_, code) in funcs {
            img.extend_from_slice(code);
        }
        img
    }

    /// Filesystem at the default base with these programs; returns a
    /// runtime booted into `main`.
    fn boot(progs: &[(&[u8], Vec<u8>)]) -> Runtime<MockPlatform> {
        const BASE: usize = crate::fs::DEFAULT_FS_BASE as usize;
        const DATA: usize = BASE + 0x1000;
        let mut img = alloc::vec![0u8; DATA];
        img[BASE..BASE + 4].copy_from_slice(b"FERR");
        img[BASE + 4..BASE + 6].copy_from_slice(&3u16.to_le_bytes());
        img[BASE + 10..BASE + 12].copy_from_slice(&(progs.len() as u16).to_le_bytes());
        for (i, (name, code)) in progs.iter().enumerate() {
            let e = BASE + 16 + 32 * i;
            let at = img.len() as u32;
            img[e..e + name.len()].copy_from_slice(name);
            img[e + 16] = crate::fs::RES_PROGRAM;
            img[e + 20..e + 24].copy_from_slice(&at.to_le_bytes());
            img[e + 24..e + 28].copy_from_slice(&(code.len() as u32).to_le_bytes());
            img.extend_from_slice(code);
        }
        crate::mock::set_flash_image(img);
        let ctx = alloc::boxed::Box::new(super::mem_detail_tests::ctx());
        let rt = Runtime::<MockPlatform>::new(ctx, TouchImpl::with_backend(crate::mock::MockTouch));
        assert_eq!(rt.error_code, 0);
        rt
    }

    fn widgets_of(rt: &Runtime<MockPlatform>, task: u8) -> Vec<WidgetId> {
        let objs = &rt.ctx.objs;
        (0..objs.ptr_count() as crate::pool::ObjPtr)
            .map(WidgetId)
            .filter(|&w| objs.is_live(w) && objs.owner_of(w.0) == task && w != objs.root)
            .collect()
    }

    fn size(rt: &Runtime<MockPlatform>, w: WidgetId) -> (u16, u16) {
        let s = rt.ctx.objs.get(w).size;
        (s.w, s.h)
    }

    /// main: a widget sized (spawnProgram(kid, 5), 99), idle loop, and with
    /// `exit_handler` an `on_task_exit(task, code)` that sizes it (task, code).
    fn main_spawning(kid: &[u8], exit_handler: bool) -> Vec<u8> {
        let setup = cat(&[
            NEW_WIDGET,
            &lit(kid),
            &[PUSH_I8, 5, SPAWN, DUP, STORE, 1, W_SET, PROP_SIZE_W],
            &[PUSH_I8, 99, W_SET, PROP_SIZE_H, PUSH_0, RET],
        ]);
        let on_exit = cat(&[MY_WIDGET, &[W_SET, PROP_SIZE_H, W_SET, PROP_SIZE_W, RET]]);
        let mut funcs = alloc::vec![(LOOP, IDLE.to_vec()), (SETUP, setup)];
        if exit_handler {
            funcs.push((ON_EXIT, on_exit));
        }
        prog(&funcs)
    }

    /// kid: a widget sized (programArg(), taskId()), idle loop, plus `extra`.
    fn kid(extra: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let setup = cat(&[
            NEW_WIDGET,
            &[PROGRAM_ARG, W_SET, PROP_SIZE_W, TASK_ID, W_SET, PROP_SIZE_H, PUSH_0, RET],
        ]);
        let mut funcs = alloc::vec![(LOOP, IDLE.to_vec()), (SETUP, setup)];
        funcs.extend_from_slice(extra);
        prog(&funcs)
    }

    #[test]
    fn spawned_task_runs_with_its_own_objects() {
        let mut rt = boot(&[(b"main", main_spawning(b"kid", false)), (b"kid", kid(&[]))]);
        assert_eq!(rt.ctx.tasks.slots[1].state, SlotState::Starting);
        rt.step();
        assert_eq!(rt.ctx.tasks.slots[1].state, SlotState::Running);
        let main_w = widgets_of(&rt, 0);
        let kid_w = widgets_of(&rt, 1);
        assert_eq!((main_w.len(), kid_w.len()), (1, 1));
        assert_eq!(size(&rt, main_w[0]), (1, 99), "spawnProgram returned task 1");
        assert_eq!(size(&rt, kid_w[0]), (5, 1), "programArg 5, taskId 1");
        assert!(rt.ctx.objs.get(kid_w[0]).parent == rt.ctx.objs.root, "on the shared tree");
        for _ in 0..5 {
            rt.step();
        }
        assert!(rt.tasks.get(1).is_some_and(|vm| vm.state != VmState::Error));
    }

    #[test]
    fn spawn_of_a_missing_program_returns_minus_one() {
        let mut rt = boot(&[(b"main", main_spawning(b"nope", false))]);
        rt.step();
        let w = widgets_of(&rt, 0)[0];
        assert_eq!(size(&rt, w).0, u16::MAX, "-1");
        assert!(!rt.ctx.tasks.alive(1));
    }

    #[test]
    fn kill_frees_the_task_and_tells_its_parent() {
        let mut rt = boot(&[(b"main", main_spawning(b"kid", true)), (b"kid", kid(&[]))]);
        rt.step();
        let kid_w = widgets_of(&rt, 1)[0];
        assert!(rt.ctx.tasks.request_kill(1));
        rt.step();
        assert!(!rt.ctx.objs.is_live(kid_w));
        assert_eq!(rt.ctx.objs.task_objects(1), [0; 4]);
        assert!(!rt.ctx.tasks.alive(1) && rt.tasks.get(1).is_none());
        rt.step(); // on_task_exit runs in main
        assert_eq!(size(&rt, widgets_of(&rt, 0)[0]), (1, 0), "on_task_exit(1, 0)");
        assert!(rt.fault_dialog.is_none());
    }

    #[test]
    fn a_faulting_child_ends_alone_and_opens_the_dialog() {
        let bad = prog(&[(LOOP, alloc::vec![BAD]), (SETUP, alloc::vec![PUSH_0, RET])]);
        let mut rt = boot(&[(b"main", main_spawning(b"bad", false)), (b"bad", bad)]);
        rt.step(); // starts
        rt.step(); // its loop faults; the pass ends it
        assert_eq!(rt.error_code, 0, "main keeps running");
        assert!(!rt.ctx.tasks.alive(1));
        assert!(rt.ctx.objs.is_live(rt.fault_dialog));
        // OK closes it.
        let ok = widgets_of(&rt, TASK_SYSTEM)
            .into_iter()
            .find(|&w| rt.ctx.objs.get(w).flags & widget::FLAG_HAS_ON_CLICK != 0)
            .unwrap();
        rt.tasks.widget_click(&rt.ctx.objs, ok);
        rt.close_fault_dialog_on_click();
        assert!(widgets_of(&rt, TASK_SYSTEM).is_empty());
        rt.step();
        assert_eq!(rt.error_code, 0);
    }

    #[test]
    fn a_parent_with_on_task_exit_gets_the_fault_instead() {
        let bad = prog(&[(LOOP, alloc::vec![BAD]), (SETUP, alloc::vec![PUSH_0, RET])]);
        let mut rt = boot(&[(b"main", main_spawning(b"bad", true)), (b"bad", bad)]);
        for _ in 0..3 {
            rt.step();
        }
        assert!(rt.fault_dialog.is_none());
        let fault = crate::vm::fault::BAD_OPCODE as u16;
        assert_eq!(size(&rt, widgets_of(&rt, 0)[0]), (1, fault));
    }

    #[test]
    fn messages_reach_on_task_message() {
        // main: spawn kid, sendMessage(kid, 7, 9).
        let setup = cat(&[&lit(b"kid"), &[PUSH_0, SPAWN, PUSH_I8, 7, PUSH_I8, 9, SEND, POP, PUSH_0, RET]]);
        let main = prog(&[(LOOP, IDLE.to_vec()), (SETUP, setup)]);
        // kid: on_task_message(from, a, b) sizes its widget (a, b), keeps from in g1.
        let on_msg = cat(&[MY_WIDGET, &[W_SET, PROP_SIZE_H, W_SET, PROP_SIZE_W, STORE, 1, RET]]);
        let mut rt = boot(&[(b"main", main), (b"kid", kid(&[(ON_MSG, on_msg)]))]);
        rt.step(); // start + deliver
        rt.step(); // handler runs
        assert_eq!(size(&rt, widgets_of(&rt, 1)[0]), (7, 9));
    }

    #[test]
    fn widget_events_go_to_the_handler_and_global_ones_to_the_foreground() {
        // kid: its widget gets on_click; click: size_h = 77; touch down: size_w = x.
        let setup = cat(&[NEW_WIDGET, &[PUSH_1, W_SET, PROP_ON_CLICK, PUSH_0, RET]]);
        let on_click = cat(&[MY_WIDGET, &[POP, PUSH_I8, 77, W_SET, PROP_SIZE_H, RET]]);
        let on_down = cat(&[MY_WIDGET, &[POP, W_SET, PROP_SIZE_W, RET]]);
        let kid = prog(&[
            (LOOP, IDLE.to_vec()),
            (SETUP, setup),
            (ON_CLICK, on_click),
            (ON_TOUCH_DOWN, on_down),
        ]);
        let mut rt = boot(&[(b"main", main_spawning(b"kid", false)), (b"kid", kid)]);
        rt.step();
        let w = widgets_of(&rt, 1)[0];
        assert_eq!(rt.ctx.objs.handler_of(w), 1);
        rt.tasks.widget_click(&rt.ctx.objs, w);
        rt.step();
        assert_eq!(size(&rt, w).1, 77, "the kid's click dispatcher ran");

        // Global events: main is the foreground (it has no handler: nothing).
        rt.tasks.global_event(FunctionKind::OnTouchDown, &[40, 50]);
        rt.step();
        assert_eq!(size(&rt, w).0, 0);
        assert!(rt.ctx.tasks.set_fg(1));
        rt.step(); // fg synced
        rt.tasks.global_event(FunctionKind::OnTouchDown, &[40, 50]);
        rt.step();
        assert_eq!(size(&rt, w).0, 40);
        // The foreground goes back to main when the kid ends.
        rt.ctx.tasks.request_kill(1);
        rt.step();
        assert_eq!(rt.ctx.tasks.fg, 0);
    }

    #[test]
    fn run_program_in_a_child_restarts_its_slot() {
        // other: setup runs runProgram("kid", 3).
        let other = prog(&[
            (LOOP, IDLE.to_vec()),
            (SETUP, cat(&[NEW_WIDGET, &lit(b"kid"), &[PUSH_I8, 3, RUN_PROGRAM, HALT]])),
        ]);
        let mut rt = boot(&[
            (b"main", main_spawning(b"other", false)),
            (b"other", other),
            (b"kid", kid(&[])),
        ]);
        rt.step(); // other starts, setup switches
        rt.step(); // the slot restarts as kid
        assert_eq!(rt.ctx.tasks.slots[1].program.name(), b"kid");
        let kid_w = widgets_of(&rt, 1);
        assert_eq!(kid_w.len(), 1, "the widget of other is gone");
        assert_eq!(size(&rt, kid_w[0]), (3, 1));
    }

    #[test]
    fn main_run_program_ends_every_child() {
        // main: spawn kid in setup; its loop runs runProgram("solo").
        let loop_ = cat(&[&[YIELD], &lit(b"solo"), &[PUSH_0, RUN_PROGRAM, JMP, 0, 0]]);
        let setup = cat(&[&lit(b"kid"), &[PUSH_0, SPAWN, POP, PUSH_0, RET]]);
        let main = prog(&[(LOOP, loop_), (SETUP, setup)]);
        let solo = prog(&[(LOOP, IDLE.to_vec()), (SETUP, alloc::vec![PUSH_0, RET])]);
        let mut rt = boot(&[(b"main", main), (b"kid", kid(&[])), (b"solo", solo)]);
        rt.step();
        assert!(rt.ctx.tasks.alive(1));
        // The mock runs one instruction per pass.
        for _ in 0..10 {
            rt.step();
        }
        assert_eq!(rt.tasks.main.program.name(), b"solo");
        assert!(!rt.ctx.tasks.alive(1) && rt.tasks.get(1).is_none());
        assert!(widgets_of(&rt, 1).is_empty());
    }
}
