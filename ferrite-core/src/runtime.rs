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
use crate::vm::{CodeAddr, FunctionKind, RenderMode, Vm, VmState};
use crate::{font, fs, heap, keyboard, protocol, render, widget};

use crate::protocol::{Protocol, RxEvent};
use crate::widget::WidgetId;

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
}

/// Per-frame input handling. Owns whatever cross-frame state the strategy needs
/// (keyboard focus, scrollbar drag, gesture accumulators).
pub trait InputHandler<P: PlatformRuntime>: Sized {
    fn new() -> Self;

    /// Poll the touch device and dispatch the resulting event: mutate the tree,
    /// enqueue VM callbacks, and render immediate press feedback.
    fn handle(&mut self, ctx: &mut Ctx<P>, vm: &mut Vm, touch: &mut TouchImpl<P::TouchB>);

    /// Per-frame housekeeping unrelated to a touch event (cursor blink).
    fn tick(&mut self, _ctx: &mut Ctx<P>) {}

    /// Draw any overlay (on-screen keyboard) on top of the rendered frame.
    /// `buffered` selects the double-buffered redraw cadence.
    fn draw_overlay(&mut self, _ctx: &Ctx<P>, _buffered: bool) {}

    /// Reset input state when a new program is flashed at runtime.
    fn reset(&mut self, _ctx: &mut Ctx<P>) {}
}

// ============================================================================
//  Entry point
// ============================================================================

/// Run the application forever. The BSP builds every hardware backend, packs
/// them into `Ctx<P>` and a `TouchImpl`, then hands them here.
pub fn run<P: PlatformRuntime>(mut ctx: Box<Ctx<P>>, mut touch: TouchImpl<P::TouchB>) -> ! {
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
        error_code = load_main(&mut ctx, &mut vm);
    }

    // --- setup() / on_program_start() / start loop() ---
    if error_code == 0 && vm.has_code() {
        error_code = start_program(&mut ctx, &mut vm);
    }

    // --- First frame / error screen ---
    if error_code != 0 && error_code != ERR_EMPTY_RUN {
        show_error(&mut ctx, error_code, None);
    } else if error_code == 0 {
        P::initial_render(&mut ctx, &vm);
    }

    // --- Main loop ---
    let mut input = <P::Input as InputHandler<P>>::new();
    let mut protocol = Protocol::new(P::FS_BASE);
    // Time the last USART byte was taken from the RX ring (for resync).
    let mut last_rx_ms = ctx.systick.millis();

    loop {
        // Resume a suspended showModal whose result is now ready.
        if error_code == 0 {
            vm.try_resume_modal(&mut ctx);
        }

        // --- VM step ---
        // Continues a suspended callback whose delay expired, or runs a burst
        // of the main program. On the GD32 the burst is 1 instruction (the loop
        // already spins at CPU speed); faster BSPs and the sim raise
        // VM_STEPS_PER_TICK so animation runs at hardware-like speed instead
        // of one instruction per presented frame.
        if error_code == 0 {
            vm.tick(&mut ctx, P::VM_STEPS_PER_TICK);
            error_code = check_vm_fault(&mut ctx, &vm);
        }

        // --- USART protocol (always serviced, even after an error, so a new
        //     program or filesystem can be flashed to recover) ---
        while let Some(byte) = ctx.usart.rx_read_byte() {
            let ev = protocol.feed(byte, &ctx.flash);
            // After feed: a flash erase it triggered is not idle time.
            last_rx_ms = ctx.systick.millis();
            if matches!(ev, RxEvent::None) {
                continue;
            }
            if P::on_extra_rx(ev, &mut ctx, &mut vm, &mut touch) {
                continue;
            }
            match ev {
                RxEvent::Ping => protocol::send_pong(&ctx.usart),
                RxEvent::Restart => {
                    ctx.eeprom.flush();
                    P::reset()
                }
                RxEvent::MemInfo => {
                    let (free, _largest) = P::heap_stats();
                    protocol::send_meminfo(&ctx.usart, free as u32);
                }
                RxEvent::StackInfo => {
                    let (used, free) = P::stack_info();
                    protocol::send_stackinfo(&ctx.usart, used, free, P::stack_peak());
                }
                RxEvent::MemDetail => {
                    let entries = mem_detail(&ctx, &vm);
                    protocol::send_memdetail(&ctx.usart, &entries);
                }
                RxEvent::SetDateTime => {
                    let dt = protocol.datetime();
                    ctx.rtc.set_time(&dt);
                    protocol::send_pong(&ctx.usart);
                }
                RxEvent::ProgramReady => {
                    error_code = reload_program(&mut ctx, &mut vm, &mut input, &mut protocol);
                    if error_code != 0 {
                        show_error(&mut ctx, error_code, None);
                    } else {
                        P::initial_render(&mut ctx, &vm);
                    }
                }
                RxEvent::ProgramTooLarge => {
                    protocol::send_error(&ctx.usart, ERR_INSUFFICIENT_MEMORY);
                }
                RxEvent::FsReady => {
                    vm.reset();
                    #[cfg(feature = "audio")]
                    ctx.audio.stop();
                    ctx.objs.clear();
                    ctx.images.clear();
                    error_code = 0;
                    protocol.alloc_sector_buf();
                    protocol::send_pong(&ctx.usart);
                }
                RxEvent::FsChunkDone => protocol::send_pong(&ctx.usart),
                RxEvent::FsWriteComplete => {
                    // Check the image before rebooting into it: on a bad
                    // upload the host gets the error and can resend.
                    let ok = fs::Fs::mount(&ctx.flash, P::FS_BASE)
                        .is_ok_and(|f| f.verify(&ctx.flash));
                    if !ok {
                        ctx.fs = None;
                        error_code = ERR_BAD_IMAGE;
                        protocol::send_error(&ctx.usart, error_code);
                        show_error(&mut ctx, error_code, None);
                        continue;
                    }
                    protocol::send_pong(&ctx.usart);
                    ctx.usart.flush();
                    ctx.eeprom.flush();
                    P::reset();
                }
                RxEvent::UserMessage => {
                    if vm.has_code() {
                        if let Some(offset) =
                            vm.find_by_kind(FunctionKind::OnUserMessage).map(|e| e.offset as CodeAddr)
                        {
                            let msg = protocol.user_message();
                            if !msg.is_empty() {
                                if let Some(arr_id) = ctx.objs.arr_from_bytes(msg) {
                                    vm.enqueue_callback(offset, &[arr_id as i32]);
                                }
                            }
                        }
                    }
                }
                _ => {} // None handled above; extras consumed by on_extra_rx
            }
        }
        // Ring drained. A message stalled mid-frame is dropped, so a stray
        // byte (RX noise, an aborted host transfer) can't wedge the parser.
        if !protocol.is_idle()
            && ctx.systick.millis().wrapping_sub(last_rx_ms) > protocol::RX_RESYNC_MS
        {
            protocol.resync();
        }

        // --- Input ---
        if error_code == 0 {
            input.handle(&mut ctx, &mut vm, &mut touch);
            input.tick(&mut ctx);
        }

        // --- Advance property animations ---
        // Time-driven (millis), independent of VM state, so tweens keep running
        // even while the program is Halted/Waiting. Runs before render so dirtied
        // props paint this frame; any onAnimEnd callbacks enqueued here are picked
        // up by the callback drain below.
        if error_code == 0 {
            let now = ctx.systick.millis();
            vm.tick_animations(&mut ctx, now);
        }

        // --- Stream the playing sound (if any) into the audio backend ---
        #[cfg(feature = "audio")]
        ctx.audio.pump(&ctx.flash);

        // --- Render (on_paint runs inside, for the widgets it drew) ---
        if error_code == 0 {
            render_phase(&mut ctx, &mut vm, &mut input);
        }

        // --- Start queued callbacks, then re-render ---
        if error_code == 0 && vm.has_pending_callbacks() && vm.drain_callbacks(&mut ctx) {
            render_phase(&mut ctx, &mut vm, &mut input);
        }

        // --- A fault in a callback or on_paint stops the program too ---
        if error_code == 0 {
            error_code = check_vm_fault(&mut ctx, &vm);
        }

        // --- Commit EEPROM writes made this pass (one erase on flash-backed
        //     boards for a whole save; no-op on write-through chips) ---
        ctx.eeprom.flush();

        // --- End-of-frame hook (host window pump / present; no-op on device) ---
        P::frame_end(&mut ctx);
    }
}

// ============================================================================
//  Startup helpers
// ============================================================================

/// Apply the image's RES_SYSCFG: resize the USART RX ring, the object pool
/// and the VM stacks, and set the animation cap. No entry → all defaults.
/// Runs once at boot (filesystem writes reboot), before the program loads.
#[inline(never)]
fn apply_syscfg<P: PlatformRuntime>(ctx: &mut Ctx<P>, vm: &mut Vm) -> u8 {
    let Some(fs) = ctx.fs.as_ref() else {
        return 0;
    };
    let cfg = match SysCfg::load(fs, &ctx.flash) {
        Ok(Some(cfg)) => cfg,
        Ok(None) => return 0,
        Err(_) => return ERR_BAD_SYSCFG,
    };
    let mut ok = ctx.usart.set_rx_capacity(cfg.uart_rx_bytes as usize);
    ok &= ctx
        .objs
        .configure(cfg.arena_bytes as usize, cfg.max_objects as usize);
    ok &= vm.configure(
        cfg.eval_stack as usize,
        cfg.call_stack as usize,
        cfg.cb_queue as usize,
        cfg.anim_slots as usize,
    );
    if ok { 0 } else { ERR_INSUFFICIENT_MEMORY }
}

#[inline(never)]
fn load_main<P: PlatformRuntime>(ctx: &mut Ctx<P>, vm: &mut Vm) -> u8 {
    let entry = match ctx
        .fs
        .as_ref()
        .and_then(|fs| fs.find_kind(&ctx.flash, b"main", fs::RES_PROGRAM))
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
        .and_then(|fs| fs.find_kind(&ctx.flash, b"main", fs::RES_PAGE))
    {
        vm.set_page(page.offset, page.size);
    }
    apply_orientation(ctx, vm);
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
        if vm.pop_result() != 0 {
            return ERR_PROGRAM_ERROR;
        }
    }
    if let Some(offset) = vm
        .find_by_kind(FunctionKind::OnProgramStart)
        .map(|e| e.offset as CodeAddr)
    {
        vm.call_now(offset, &[], ctx);
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

// ============================================================================
//  Render phase
// ============================================================================

fn render_phase<P: PlatformRuntime>(ctx: &mut Ctx<P>, vm: &mut Vm, input: &mut P::Input) {
    match vm.render_mode {
        RenderMode::Buffered => {
            if render::buffered_has_dirty(ctx) {
                ctx.lcd.begin_frame();
                let paint = render::render_buffered_content(ctx);
                // on_paint into this back buffer, before the overlay + swap.
                vm.run_paint(ctx, &paint);
                input.draw_overlay(ctx, true);
                // Rasterization is done (UI thread). Hand off the present
                // (vsync wait + buffer flip) — synchronous by default, or
                // deferred to a render thread by BSPs that override this.
                P::present_buffered(ctx);
            }
        }
        RenderMode::Dirty => {
            let mut paint = render::PaintList::new();
            render::collect_dirty_paint(ctx, &mut paint);
            render::render_dirty(ctx);
            vm.run_paint(ctx, &paint);
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
        ext.value = selected;
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
        ext.value = new_scroll;
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
    fn handle(&mut self, ctx: &mut Ctx<P>, vm: &mut Vm, touch: &mut TouchImpl<P::TouchB>) {
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
                    ctx.objs.mark_dirty(hit);
                }
            }
            TouchEventKind::Release => {
                let hit = crate::touch::hit_test(&mut ctx.objs, event.x, event.y);
                if hit.is_none() {
                    return;
                }
                ctx.objs.get_mut(hit).flags &= !widget::FLAG_PRESSED;
                ctx.objs.mark_dirty(hit);
                if !vm.has_code() {
                    return;
                }
                if ctx.objs.get(hit).flags & widget::FLAG_HAS_ON_CLICK != 0 {
                    if let Some(click_fn) = vm.modal_overlay_click_fn(hit) {
                        if let Some(off) = vm.find_func(click_fn).map(|e| e.offset as CodeAddr) {
                            vm.enqueue_callback(off, &[hit.0 as i32]);
                        }
                    } else if let Some(off) =
                        vm.find_by_kind(FunctionKind::OnClick).map(|e| e.offset as CodeAddr)
                    {
                        vm.enqueue_callback(off, &[hit.0 as i32]);
                    }
                }
                if ctx.objs.get(hit).flags & widget::FLAG_HAS_ON_TAP != 0
                    && ctx.objs.get(hit).kind != widget::KIND_INPUT
                {
                    if let Some(off) = vm.find_by_kind(FunctionKind::OnTap).map(|e| e.offset as CodeAddr) {
                        let packed = ((event.x as u32) << 16 | event.y as u32) as i32;
                        vm.enqueue_callback(off, &[hit.0 as i32, packed]);
                    }
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
    fn handle(&mut self, ctx: &mut Ctx<P>, vm: &mut Vm, touch: &mut TouchImpl<P::TouchB>) {
        let mut event = match touch.poll() {
            Some(e) => e,
            None => return,
        };
        (event.x, event.y) = ctx.lcd.touch_to_logical(event.x, event.y);
        match event.kind {
            TouchEventKind::Press => self.on_press(ctx, vm, event.x, event.y),
            TouchEventKind::Hold => self.on_hold(ctx, vm, event.x, event.y),
            TouchEventKind::Release => self.on_release(ctx, vm, event.x, event.y),
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
        self.kb.focused = WidgetId::NONE;
        self.scroll_id = WidgetId::NONE;
        self.scroll_active = false;
        ctx.cursor_visible = false;
    }
}

impl FullInput {
    fn on_press<P: PlatformRuntime>(&mut self, ctx: &mut Ctx<P>, vm: &mut Vm, x: u16, y: u16) {
        // Keyboard intercept.
        if self.kb.visible && y >= self.kb.top() {
            self.kb.handle_press(x, y);
            if vm.render_mode != RenderMode::Buffered {
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
                self.position_cursor(ctx, hit, x);
            }

            if ctx.objs.get(hit).kind == widget::KIND_SLIDER {
                let abs = ctx.objs.absolute_rect(hit);
                let new_val = touch_to_slider_value(&abs, x);
                ctx.objs.update_ext(hit, |ext| {
                    ext.value = new_val;
                });
                // The click dispatcher takes exactly one argument, the widget
                // id (a second one would be popped *as* the id); the new value
                // is already stored, and the dispatcher passes it on to
                // handlers declared as `fn h(wid, val)`.
                if vm.has_code() && ctx.objs.get(hit).flags & widget::FLAG_HAS_ON_CLICK != 0 {
                    if let Some(off) = vm.find_by_kind(FunctionKind::OnClick).map(|e| e.offset as CodeAddr)
                    {
                        vm.enqueue_callback(off, &[hit.0 as i32]);
                    }
                }
            }

            ctx.objs.mark_dirty(hit);
            if vm.render_mode != RenderMode::Buffered {
                render::render_dirty(ctx);
            }
        }

        if vm.has_code() {
            if let Some(off) = vm
                .find_by_kind(FunctionKind::OnTouchDown)
                .map(|e| e.offset as CodeAddr)
            {
                vm.enqueue_callback(off, &[x as i32, y as i32]);
            }
        }
    }

    fn on_hold<P: PlatformRuntime>(&mut self, ctx: &mut Ctx<P>, vm: &mut Vm, x: u16, y: u16) {
        if self.kb.pressed_key != keyboard::KEY_NONE {
            return;
        }

        // Scrollbar drag.
        if self.scroll_active && self.scroll_id.is_some() {
            scrollbar_set_position(ctx, self.scroll_id, y);
            if vm.render_mode != RenderMode::Buffered {
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
                    ext.value = new_val;
                });
                ctx.objs.mark_dirty(id);
                if vm.render_mode != RenderMode::Buffered {
                    render::render_dirty(ctx);
                }
                if vm.has_code() && ctx.objs.get(id).flags & widget::FLAG_HAS_ON_CLICK != 0 {
                    if let Some(off) =
                        vm.find_by_kind(FunctionKind::OnClick).map(|e| e.offset as CodeAddr)
                    {
                        // One argument — see the slider press path.
                        vm.enqueue_callback(off, &[id.0 as i32]);
                    }
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
                if vm.has_code() {
                    if let Some(off) = vm
                        .find_by_kind(FunctionKind::OnLongPress)
                        .map(|e| e.offset as CodeAddr)
                    {
                        vm.enqueue_callback(off, &[pressed_id.0 as i32, x as i32, y as i32]);
                    }
                }
                self.long_press_fired = true;
            }
        }

        if vm.has_code() {
            if let Some(off) = vm
                .find_by_kind(FunctionKind::OnTouchMove)
                .map(|e| e.offset as CodeAddr)
            {
                vm.enqueue_callback(off, &[x as i32, y as i32]);
            }
        }
    }

    fn on_release<P: PlatformRuntime>(&mut self, ctx: &mut Ctx<P>, vm: &mut Vm, x: u16, y: u16) {
        // Keyboard key release.
        if self.kb.pressed_key != keyboard::KEY_NONE {
            let prev_key = self.kb.pressed_key;
            if let Some(action) = self.kb.handle_release(x, y) {
                self.handle_key_action(ctx, vm, action);
            }
            if vm.render_mode != RenderMode::Buffered && !self.kb.dirty && self.kb.visible {
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
                ctx.objs.mark_dirty(id);
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
            self.position_cursor(ctx, clicked_id, x);
            ctx.objs.mark_dirty(clicked_id);
        }

        if vm.render_mode != RenderMode::Buffered {
            render::render_dirty(ctx);
        }

        // OnSwipe — mutually exclusive with on_click.
        if is_swipe && vm.has_code() {
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
                if let Some(off) = vm.find_by_kind(FunctionKind::OnSwipe).map(|e| e.offset as CodeAddr) {
                    let dominant = if direction <= 1 { dx } else { dy };
                    vm.enqueue_callback(off, &[direction, swipe_widget_id.0 as i32, dominant]);
                }
            }
        }

        // on_click (modal overlay func id first, else the global dispatcher).
        if clicked_id.is_some()
            && vm.has_code()
            && ctx.objs.get(clicked_id).flags & widget::FLAG_HAS_ON_CLICK != 0
        {
            if let Some(click_fn) = vm.modal_overlay_click_fn(clicked_id) {
                if let Some(off) = vm.find_func(click_fn).map(|e| e.offset as CodeAddr) {
                    vm.enqueue_callback(off, &[clicked_id.0 as i32]);
                }
            } else if let Some(off) = vm.find_by_kind(FunctionKind::OnClick).map(|e| e.offset as CodeAddr)
            {
                vm.enqueue_callback(off, &[clicked_id.0 as i32]);
            }
        }

        // on_tap (skip KIND_INPUT).
        if clicked_id.is_some()
            && vm.has_code()
            && ctx.objs.get(clicked_id).kind != widget::KIND_INPUT
            && ctx.objs.get(clicked_id).flags & widget::FLAG_HAS_ON_TAP != 0
        {
            if let Some(off) = vm.find_by_kind(FunctionKind::OnTap).map(|e| e.offset as CodeAddr) {
                let packed = ((x as u32) << 16 | y as u32) as i32;
                vm.enqueue_callback(off, &[clicked_id.0 as i32, packed]);
            }
        }

        if vm.has_code() {
            if let Some(off) = vm
                .find_by_kind(FunctionKind::OnTouchUp)
                .map(|e| e.offset as CodeAddr)
            {
                vm.enqueue_callback(off, &[]);
            }
        }
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

    fn position_cursor<P: PlatformRuntime>(&self, ctx: &mut Ctx<P>, id: WidgetId, touch_x: u16) {
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
        if text_id != 0xFFFF {
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
            ext.value = cursor_pos;
        });
    }

    fn handle_key_action<P: PlatformRuntime>(
        &mut self,
        ctx: &mut Ctx<P>,
        vm: &mut Vm,
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
                if text_id != 0xFFFF {
                    // A shared literal is edited as a private copy: store its id.
                    if let Some(new_id) = ctx.objs.str_insert_byte(text_id, cursor, ch, max_len) {
                        ctx.objs.update_ext(id, |ext| {
                            ext.text_id = new_id;
                            ext.value = (cursor + 1) as i16;
                        });
                        ctx.objs.mark_dirty(id);
                        self.fire_on_change(ctx, vm, id);
                    }
                } else {
                    let data = [ch];
                    if let Some(new_id) = ctx.objs.str_alloc(&data) {
                        ctx.objs.update_ext(id, |ext| {
                            ext.text_id = new_id;
                            ext.value = 1;
                        });
                        ctx.objs.mark_dirty(id);
                        self.fire_on_change(ctx, vm, id);
                    }
                }
                let now = ctx.systick.millis();
                self.kb.reset_blink(now);
                ctx.cursor_visible = true;
            }
            keyboard::KeyAction::Backspace => {
                let text_id = ctx.objs.text_id(id);
                let cursor = ctx.objs.value(id).max(0) as usize;
                if text_id != 0xFFFF && cursor > 0 {
                    if let Some(new_id) = ctx.objs.str_delete_byte(text_id, cursor - 1) {
                        ctx.objs.update_ext(id, |ext| {
                            ext.text_id = new_id;
                            ext.value = (cursor - 1) as i16;
                        });
                        ctx.objs.mark_dirty(id);
                        self.fire_on_change(ctx, vm, id);
                    }
                }
                let now = ctx.systick.millis();
                self.kb.reset_blink(now);
                ctx.cursor_visible = true;
            }
            keyboard::KeyAction::Enter => {
                self.fire_on_change(ctx, vm, id);
                self.dismiss_keyboard(ctx);
            }
            keyboard::KeyAction::Shift
            | keyboard::KeyAction::Symbols
            | keyboard::KeyAction::Abc => {
                // Layer change handled inside handle_release — just redraw.
            }
        }
    }

    fn fire_on_change<P: PlatformRuntime>(&self, ctx: &Ctx<P>, vm: &mut Vm, id: WidgetId) {
        if !vm.has_code() || ctx.objs.get(id).flags & widget::FLAG_HAS_ON_TAP == 0 {
            return;
        }
        if let Some(off) = vm.find_by_kind(FunctionKind::OnTap).map(|e| e.offset as CodeAddr) {
            vm.enqueue_callback(off, &[id.0 as i32, 0]);
        }
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

    fn ctx() -> Ctx<MockPlatform> {
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
        assert_eq!(row(mem_pool::OBJ_ARENA), (4 * (8 + 28 + 4), 1024, 16));
        let (live, max, heap) = row(mem_pool::OBJ_PTRS);
        assert_eq!((live, max), (6, 50));
        assert!(heap >= 1024 + 100);
        assert_eq!(row(mem_pool::WIDGETS), (4, 50, 4 * 28));
        assert_eq!(row(mem_pool::STRINGS), (1, 50, 16));
        assert_eq!(row(mem_pool::ARRAYS), (1, 50, 16));
        assert_eq!(row(mem_pool::EVAL_STACK), (0, 20, 80));
        assert_eq!((row(mem_pool::CALL_STACK).0, row(mem_pool::CALL_STACK).1), (0, 6));
        assert_eq!((row(mem_pool::CB_QUEUE).0, row(mem_pool::CB_QUEUE).1), (0, 5));
        assert_eq!((row(mem_pool::ANIMS).0, row(mem_pool::ANIMS).1), (0, 3));
        // Mock USART doesn't report its ring.
        assert_eq!(row(mem_pool::UART_RX), (0, 0, 0));
    }
}
