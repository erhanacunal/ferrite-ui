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

use crate::lcd::LcdBackend;
use crate::platform::Platform;
use crate::touch::{TouchEventKind, TouchImpl};

use crate::ctx::Ctx;
use crate::strpool::StringPool;
use crate::types::{COLOR_RED, Size};
use crate::vm::{FunctionKind, RenderMode, Vm, VmState};
use crate::{font, fs, heap, keyboard, protocol, render, widget};

use crate::protocol::{Protocol, RxEvent};
use crate::widget::{WidgetExt, WidgetId};

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
/// Recovery mode — skip program load, suppress the error screen.
pub const ERR_EMPTY_RUN: u8 = 0xFE;

/// Maximum RAM-resident program image size.
pub const MAX_CODE_SIZE: usize = 4096;

// === Gesture tuning (shared with lib/touch.fl constants) ===

const LONG_PRESS_FRAMES: u16 = 60;
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
    /// On-device BSPs leave this at 1: the main loop spins as fast as the CPU
    /// (no frame pacing), so the VM still runs millions of instructions/sec.
    /// The host simulator paces the loop to the display refresh (~60 Hz) and
    /// would otherwise execute only ~60 instructions/sec — far too slow for
    /// `millis()`-based animation. It raises this so each presented frame runs
    /// a hardware-like burst of instructions. The loop still stops early on
    /// YIELD / a blocking state, so a program that yields each frame is
    /// unaffected.
    const VM_STEPS_PER_TICK: u32 = 1;

    /// Per-frame input handling strategy.
    type Input: InputHandler<Self>;

    /// Reboot the device (never returns).
    fn reset() -> !;

    /// VM syscall dispatcher, wired into `Vm::syscall_fn`.
    fn syscall(id: u8, args: &[i32], strpool: &StringPool) -> Option<i32> {
        let _ = (id, args, strpool);
        None
    }

    /// `(used, free)` stack bytes reported by the `StackInfo` USART command.
    fn stack_info() -> (u32, u32) {
        (0, 0)
    }

    /// `(free_bytes, largest_free_block)` reported by the `MemInfo` USART
    /// command. Defaults to the lib's static heap; BSPs that supply their own
    /// global allocator (feature `external_alloc`) override this to report
    /// through that allocator.
    fn heap_stats() -> (usize, usize) {
        heap::stats()
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
            Ok(f) => ctx.fs = Some(f),
            Err(_) => error_code = ERR_NO_FILESYSTEM,
        }
    }

    // --- Root widget (always widget 0) ---
    let root = ctx.tree.alloc().unwrap();
    {
        let w = ctx.tree.get_mut(root);
        w.size = Size {
            w: P::LcdB::WIDTH,
            h: P::LcdB::HEIGHT,
        };
        w.background_color = P::BG_COLOR;
    }
    ctx.tree.root = root;

    // --- VM ---
    let mut vm = Box::new(Vm::new());
    vm.syscall_fn = Some(P::syscall);
    vm.forced_render_mode = P::FORCED_RENDER_MODE;

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

    loop {
        // Resume a suspended showModal whose result is now ready.
        if error_code == 0 {
            vm.try_resume_modal(&mut ctx);
        }

        // --- VM step ---
        if error_code == 0 {
            match vm.state {
                VmState::Running | VmState::Yielded => {
                    vm.state = VmState::Running;
                    // Run a burst of instructions per loop iteration. On device
                    // this is 1 (the loop already spins at CPU speed); the sim
                    // raises it so animation runs at hardware-like speed instead
                    // of one instruction per presented frame. A critical section
                    // always runs to its YIELD regardless of the budget.
                    let mut budget = P::VM_STEPS_PER_TICK.max(1);
                    loop {
                        vm.step(&mut ctx);
                        while vm.is_critical() && vm.state == VmState::Running {
                            vm.step(&mut ctx);
                        }
                        budget -= 1;
                        if budget == 0 || vm.state != VmState::Running {
                            break;
                        }
                    }
                    if vm.state == VmState::Error {
                        error_code = ERR_PROGRAM_ERROR;
                        let pc = vm.pc();
                        show_error(&mut ctx, error_code, Some(pc));
                    }
                }
                VmState::Waiting => {
                    if ctx.systick.millis().wrapping_sub(vm.wait_until) < 0x8000_0000 {
                        vm.state = VmState::Running;
                    }
                }
                _ => {} // Halted / Error / Ready / AwaitingModal
            }
        }

        // --- USART protocol (always serviced, even after an error, so a new
        //     program or filesystem can be flashed to recover) ---
        while let Some(byte) = ctx.usart.rx_read_byte() {
            let ev = protocol.feed(byte, &ctx.flash);
            if matches!(ev, RxEvent::None) {
                continue;
            }
            if P::on_extra_rx(ev, &mut ctx, &mut vm, &mut touch) {
                continue;
            }
            match ev {
                RxEvent::Ping => protocol::send_pong(&ctx.usart),
                RxEvent::Restart => P::reset(),
                RxEvent::MemInfo => {
                    let (free, _largest) = P::heap_stats();
                    protocol::send_meminfo(&ctx.usart, free as u32);
                }
                RxEvent::StackInfo => {
                    let (used, free) = P::stack_info();
                    protocol::send_stackinfo(&ctx.usart, used, free);
                }
                RxEvent::SetDateTime => {
                    let dt = protocol.datetime();
                    ctx.rtc.set_time(&dt);
                    protocol::send_pong(&ctx.usart);
                }
                RxEvent::ProgramReady => {
                    error_code = reload_program(&mut ctx, &mut vm, &mut input, &protocol);
                    protocol.free_program();
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
                    ctx.tree.clear();
                    ctx.strpool.clear();
                    ctx.images.clear();
                    error_code = 0;
                    protocol.alloc_sector_buf();
                    protocol::send_pong(&ctx.usart);
                }
                RxEvent::FsChunkDone => protocol::send_pong(&ctx.usart),
                RxEvent::FsWriteComplete => {
                    protocol::send_pong(&ctx.usart);
                    ctx.usart.flush();
                    P::reset();
                }
                RxEvent::UserMessage => {
                    if vm.has_code() {
                        if let Some(offset) =
                            vm.find_by_kind(FunctionKind::OnUserMessage).map(|e| e.offset as u16)
                        {
                            let msg = protocol.user_message();
                            if let Some(arr_id) = vm.alloc_array_from(msg) {
                                vm.enqueue_callback(offset, &[arr_id]);
                            }
                        }
                    }
                }
                _ => {} // None handled above; extras consumed by on_extra_rx
            }
        }

        // --- Input ---
        if error_code == 0 {
            input.handle(&mut ctx, &mut vm, &mut touch);
            input.tick(&mut ctx);
        }

        // --- Render + on_paint dispatch ---
        if error_code == 0 {
            let mut paint = PaintBuf::new();
            collect_dirty_paint(&mut ctx, &mut paint);
            render_phase(&mut ctx, &vm, &mut input);
            enqueue_paint(&mut ctx, &mut vm, &paint);
        }

        // --- Drain callback queue, then re-render ---
        if error_code == 0 && vm.has_pending_callbacks() {
            vm.drain_callbacks(&mut ctx);
            render_phase(&mut ctx, &vm, &mut input);

            // Callbacks may have made on_paint widgets visible and already
            // rendered them (clearing dirty). Fire on_paint for those now.
            if vm.has_code() {
                let mut paint = PaintBuf::new();
                collect_visible_paint(&mut ctx, &mut paint);
                enqueue_paint(&mut ctx, &mut vm, &paint);
                vm.drain_callbacks(&mut ctx);
            }
        }

        // --- End-of-frame hook (host window pump / present; no-op on device) ---
        P::frame_end(&mut ctx);
    }
}

// ============================================================================
//  Startup helpers
// ============================================================================

fn load_main<P: PlatformRuntime>(ctx: &mut Ctx<P>, vm: &mut Vm) -> u8 {
    let entry = match ctx.fs.as_ref().and_then(|fs| fs.find(&ctx.flash, b"main")) {
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
    if entry.flags & fs::RES_FLAG_FLASH_EXEC != 0 {
        // The VM keeps its own (cloned) flash handle for cache refills; the
        // backend is a stateless MMIO driver so the clone shares no state.
        if !vm.load_flash(ctx.flash.clone(), entry.offset, entry.size as usize) {
            err = ERR_PROGRAM_ERROR;
        }
    } else {
        let img_len = entry.size.min(MAX_CODE_SIZE as u32) as usize;
        let mut img = alloc::vec![0u8; img_len];
        if let Some(fs) = ctx.fs.as_ref() {
            fs.read_resource(&ctx.flash, &entry, 0, &mut img);
        }
        if !vm.load_ram(&img) {
            err = ERR_PROGRAM_ERROR;
        }
    }
    reserve_tree(ctx, vm);
    err
}

/// Pre-allocate tree capacity from v4 header counts (one allocation per Vec).
fn reserve_tree<P: PlatformRuntime>(ctx: &mut Ctx<P>, vm: &Vm) {
    let wc = if vm.widget_count > 0 {
        vm.widget_count as usize
    } else {
        96
    };
    let ec = if vm.ext_count > 0 {
        vm.ext_count as usize
    } else {
        96
    };
    ctx.tree.reserve(wc, ec);
}

fn start_program<P: PlatformRuntime>(ctx: &mut Ctx<P>, vm: &mut Vm) -> u8 {
    if let Some(offset) = vm.find_by_kind(FunctionKind::Setup).map(|e| e.offset as u16) {
        vm.run_callback(offset, ctx);
        if vm.pop_result() != 0 {
            return ERR_PROGRAM_ERROR;
        }
    }
    if let Some(offset) = vm
        .find_by_kind(FunctionKind::OnProgramStart)
        .map(|e| e.offset as u16)
    {
        vm.run_callback(offset, ctx);
    }
    if let Some(offset) = vm.find_by_kind(FunctionKind::Loop).map(|e| e.offset as u16) {
        vm.set_pc(offset);
        vm.state = VmState::Running;
    }
    0
}

/// Runtime program reload (USART `ProgramReady`). Returns the new error code.
fn reload_program<P: PlatformRuntime>(
    ctx: &mut Ctx<P>,
    vm: &mut Vm,
    input: &mut P::Input,
    protocol: &Protocol,
) -> u8 {
    ctx.tree.clear();
    ctx.strpool.clear();
    vm.reset();
    input.reset(ctx);
    ctx.cursor_visible = false;

    let root = ctx.tree.alloc().unwrap();
    {
        let w = ctx.tree.get_mut(root);
        w.size = Size {
            w: P::LcdB::WIDTH,
            h: P::LcdB::HEIGHT,
        };
        w.background_color = P::BG_COLOR;
    }
    ctx.tree.root = root;

    let prog = protocol.program_code();
    if !vm.load_ram(prog) {
        vm.load_raw(prog);
    }
    reserve_tree(ctx, vm);

    if vm.has_code() {
        start_program(ctx, vm)
    } else {
        // No header/functions — run opcodes from the top (legacy compat).
        vm.state = VmState::Running;
        0
    }
}

// ============================================================================
//  Render phase
// ============================================================================

fn render_phase<P: PlatformRuntime>(ctx: &mut Ctx<P>, vm: &Vm, input: &mut P::Input) {
    match vm.render_mode {
        RenderMode::Buffered => {
            if render::buffered_has_dirty(ctx) {
                ctx.lcd.begin_frame();
                render::render_buffered_content(ctx, vm);
                input.draw_overlay(ctx, true);
                ctx.lcd.end_frame();
                ctx.lcd.flush_dirty();
            }
        }
        RenderMode::Dirty => {
            render::render_dirty(ctx, vm);
            input.draw_overlay(ctx, false);
            ctx.lcd.flush_dirty();
        }
        RenderMode::EPaper => {
            // EPD updates are driven exclusively by OP_W_RENDER inside the
            // program; the per-frame loop does nothing here.
        }
    }
}

/// Fixed-capacity scratch for the on_paint id collection (no heap per frame).
struct PaintBuf {
    ids: [i32; 64],
    len: usize,
}
impl PaintBuf {
    fn new() -> Self {
        Self {
            ids: [0; 64],
            len: 0,
        }
    }
    fn push(&mut self, id: i32) {
        if self.len < self.ids.len() {
            self.ids[self.len] = id;
            self.len += 1;
        }
    }
}

/// Dirty widgets that registered on_paint — collected *before* render clears
/// their dirty flags.
fn collect_dirty_paint<P: PlatformRuntime>(ctx: &mut Ctx<P>, out: &mut PaintBuf) {
    ctx.tree.ensure_dfs();
    for i in 0..ctx.tree.dfs_len() {
        let id = ctx.tree.dfs_at(i);
        let w = ctx.tree.get(id);
        if w.is_dirty() && w.flags & widget::FLAG_HAS_ON_PAINT != 0 {
            out.push(id.0 as i32);
        }
    }
}

/// Widgets that became visible (and were already rendered, so are no longer
/// dirty) — used after a callback drain.
fn collect_visible_paint<P: PlatformRuntime>(ctx: &mut Ctx<P>, out: &mut PaintBuf) {
    ctx.tree.ensure_dfs();
    for i in 0..ctx.tree.dfs_len() {
        let id = ctx.tree.dfs_at(i);
        if ctx.tree.get(id).flags & widget::FLAG_HAS_ON_PAINT != 0
            && ctx.tree.is_tree_visible(id)
            && !ctx.tree.get(id).is_dirty()
        {
            out.push(id.0 as i32);
        }
    }
}

fn enqueue_paint<P: PlatformRuntime>(ctx: &mut Ctx<P>, vm: &mut Vm, paint: &PaintBuf) {
    if !vm.has_code() || paint.len == 0 {
        return;
    }
    if let Some(offset) = vm.find_by_kind(FunctionKind::OnPaint).map(|e| e.offset as u16) {
        let arr_id = vm.alloc_array_from_i32s(&paint.ids[..paint.len]);
        vm.enqueue_callback(offset, &[arr_id]);
        let _ = ctx; // arrays live in the VM; ctx unused but keeps the signature uniform
    }
}

// ============================================================================
//  Error screen
// ============================================================================

pub fn show_error<P: PlatformRuntime>(ctx: &mut Ctx<P>, code: u8, vm_pc: Option<u16>) {
    let sw = P::LcdB::WIDTH;
    let sh = P::LcdB::HEIGHT;
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

    if let Some(pc) = vm_pc {
        let mut pc_buf = [0u8; 12];
        let pc_len = format_hex_u16(b"PC: 0x", pc, &mut pc_buf);
        font.draw_str(
            &ctx.lcd,
            &ctx.flash,
            &pc_buf[..pc_len],
            center(font.text_width(&pc_buf[..pc_len]) as usize),
            ty + 90,
            fg,
            Some(bg),
        );
    }

    ctx.lcd.end_frame();
    ctx.lcd.flush_dirty();

    protocol::send_error(&ctx.usart, code);
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

fn format_hex_u16(prefix: &[u8], val: u16, buf: &mut [u8; 12]) -> usize {
    let plen = prefix.len().min(6);
    buf[..plen].copy_from_slice(&prefix[..plen]);
    let hex = b"0123456789ABCDEF";
    buf[plen] = hex[((val >> 12) & 0xF) as usize];
    buf[plen + 1] = hex[((val >> 8) & 0xF) as usize];
    buf[plen + 2] = hex[((val >> 4) & 0xF) as usize];
    buf[plen + 3] = hex[(val & 0xF) as usize];
    plen + 4
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
    let parent = ctx.tree.get(id).parent;
    if parent.is_some() && ctx.tree.get(parent).kind == widget::KIND_DROPDOWN {
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
    let mut child = ctx.tree.get(dropdown).first_child;
    let max = ctx.tree.count();
    let mut guard = 0usize;
    while child.is_some() {
        if child == option {
            return idx;
        }
        child = ctx.tree.get(child).next_sibling;
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
    let mut child = ctx.tree.get(dropdown).first_child;
    let max = ctx.tree.count();
    let mut guard = 0usize;
    while child.is_some() {
        if visible {
            ctx.tree.get_mut(child).flags |= widget::FLAG_VISIBLE;
        } else {
            ctx.tree.get_mut(child).flags &= !widget::FLAG_VISIBLE;
        }
        ctx.tree.mark_dirty(child);
        child = ctx.tree.get(child).next_sibling;
        guard += 1;
        if guard > max {
            break;
        }
    }
    ctx.tree.mark_dirty(dropdown);
}

pub fn toggle_dropdown<P: Platform>(ctx: &mut Ctx<P>, dropdown: WidgetId) {
    let expanded = ctx.tree.get(dropdown).flags & widget::FLAG_CHECKED != 0;
    if expanded {
        ctx.tree.get_mut(dropdown).flags &= !widget::FLAG_CHECKED;
        set_dropdown_options_visible(ctx, dropdown, false);
    } else {
        ctx.tree.get_mut(dropdown).flags |= widget::FLAG_CHECKED;
        set_dropdown_options_visible(ctx, dropdown, true);
    }
}

pub fn select_dropdown_option<P: Platform>(
    ctx: &mut Ctx<P>,
    dropdown: WidgetId,
    option: WidgetId,
) {
    let text_id = ctx.tree.text_id(option);
    let selected = dropdown_child_index(ctx, dropdown, option);
    if let Some(ext) = ctx.tree.ensure_ext(dropdown) {
        ext.text_id = text_id;
        ext.value = selected;
    }
    ctx.tree.get_mut(dropdown).flags &= !widget::FLAG_CHECKED;
    set_dropdown_options_visible(ctx, dropdown, false);
}

pub fn scrollbar_set_position<P: Platform>(
    ctx: &mut Ctx<P>,
    scroll_id: WidgetId,
    touch_y: u16,
) {
    let cr = ctx.tree.content_rect(scroll_id);
    let ch = ctx.tree.content_height(scroll_id) as i32;
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

    if let Some(ext) = ctx.tree.ensure_ext(scroll_id) {
        ext.value = new_scroll;
    }
    ctx.tree.mark_dirty(scroll_id);
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

    fn handle(&mut self, ctx: &mut Ctx<P>, vm: &mut Vm, touch: &mut TouchImpl<P::TouchB>) {
        let event = match touch.poll() {
            Some(e) => e,
            None => return,
        };
        match event.kind {
            TouchEventKind::Press => {
                let hit = crate::touch::hit_test(&mut ctx.tree, event.x, event.y);
                if hit.is_some() {
                    ctx.tree.get_mut(hit).flags |= widget::FLAG_PRESSED;
                    ctx.tree.mark_dirty(hit);
                }
            }
            TouchEventKind::Release => {
                let hit = crate::touch::hit_test(&mut ctx.tree, event.x, event.y);
                if hit.is_none() {
                    return;
                }
                ctx.tree.get_mut(hit).flags &= !widget::FLAG_PRESSED;
                ctx.tree.mark_dirty(hit);
                if !vm.has_code() {
                    return;
                }
                if ctx.tree.get(hit).flags & widget::FLAG_HAS_ON_CLICK != 0 {
                    if let Some(click_fn) = vm.modal_overlay_click_fn(hit) {
                        if let Some(off) = vm.find_func(click_fn).map(|e| e.offset as u16) {
                            vm.enqueue_callback(off, &[hit.0 as i32]);
                        }
                    } else if let Some(off) =
                        vm.find_by_kind(FunctionKind::OnClick).map(|e| e.offset as u16)
                    {
                        vm.enqueue_callback(off, &[hit.0 as i32]);
                    }
                }
                if ctx.tree.get(hit).flags & widget::FLAG_HAS_ON_TAP != 0
                    && ctx.tree.get(hit).kind != widget::KIND_INPUT
                {
                    if let Some(off) = vm.find_by_kind(FunctionKind::OnTap).map(|e| e.offset as u16) {
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
    hold_count: u16,
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
            hold_count: 0,
            long_press_fired: false,
        }
    }

    fn handle(&mut self, ctx: &mut Ctx<P>, vm: &mut Vm, touch: &mut TouchImpl<P::TouchB>) {
        let event = match touch.poll() {
            Some(e) => e,
            None => return,
        };
        match event.kind {
            TouchEventKind::Press => self.on_press(ctx, vm, event.x, event.y),
            TouchEventKind::Hold => self.on_hold(ctx, vm, event.x, event.y),
            TouchEventKind::Release => self.on_release(ctx, vm, event.x, event.y),
        }
    }

    fn tick(&mut self, ctx: &mut Ctx<P>) {
        if self.kb.visible && self.kb.focused.is_some() {
            let now = ctx.systick.millis();
            if self.kb.update_blink(now) {
                ctx.cursor_visible = self.kb.cursor_visible;
                ctx.tree.mark_dirty(self.kb.focused);
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
        if self.kb.visible && y >= keyboard::KB_Y {
            self.kb.handle_press(x, y);
            if vm.render_mode != RenderMode::Buffered {
                self.kb
                    .draw_key_by_code(&ctx.lcd, &ctx.fonts, &ctx.flash, self.kb.pressed_key, true);
            }
            return;
        }

        let hit = crate::touch::hit_test(&mut ctx.tree, x, y);

        // Dismiss keyboard when tapping outside an input.
        if self.kb.visible && (!hit.is_some() || ctx.tree.get(hit).kind != widget::KIND_INPUT) {
            self.dismiss_keyboard(ctx);
        }

        self.scroll_id = WidgetId::NONE;
        self.scroll_active = false;
        self.gesture_start_x = x;
        self.gesture_start_y = y;
        self.hold_count = 0;
        self.long_press_fired = false;

        // Scrollbar hit-test.
        let scroll_target = if hit.is_some() {
            let w = ctx.tree.get(hit);
            if w.flags & widget::FLAG_CLIP_CHILDREN != 0 {
                hit
            } else {
                ctx.tree.scroll_parent(hit)
            }
        } else {
            WidgetId::NONE
        };
        if scroll_target.is_some() {
            let cr = ctx.tree.content_rect(scroll_target);
            let ch = ctx.tree.content_height(scroll_target);
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
            ctx.tree.get_mut(hit).flags |= widget::FLAG_PRESSED;

            if ctx.tree.get(hit).kind == widget::KIND_INPUT {
                self.focus_input(ctx, hit);
                self.position_cursor(ctx, hit, x);
            }

            if ctx.tree.get(hit).kind == widget::KIND_SLIDER {
                let abs = ctx.tree.absolute_rect(hit);
                let new_val = touch_to_slider_value(&abs, x);
                if let Some(ext) = ctx.tree.ensure_ext(hit) {
                    ext.value = new_val;
                }
                if vm.has_code() && ctx.tree.get(hit).flags & widget::FLAG_HAS_ON_CLICK != 0 {
                    if let Some(off) = vm.find_by_kind(FunctionKind::OnClick).map(|e| e.offset as u16)
                    {
                        vm.enqueue_callback(off, &[hit.0 as i32, new_val as i32]);
                    }
                }
            }

            ctx.tree.mark_dirty(hit);
            if vm.render_mode != RenderMode::Buffered {
                render::render_dirty(ctx, vm);
            }
        }

        if vm.has_code() {
            if let Some(off) = vm
                .find_by_kind(FunctionKind::OnTouchDown)
                .map(|e| e.offset as u16)
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
                render::render_dirty(ctx, vm);
            }
        }

        // Slider drag (the pressed slider).
        ctx.tree.ensure_dfs();
        for i in 0..ctx.tree.dfs_len() {
            let id = ctx.tree.dfs_at(i);
            let w = ctx.tree.get(id);
            if w.flags & widget::FLAG_PRESSED != 0 && w.kind == widget::KIND_SLIDER {
                let abs = ctx.tree.absolute_rect(id);
                let new_val = touch_to_slider_value(&abs, x);
                if let Some(ext) = ctx.tree.ensure_ext(id) {
                    ext.value = new_val;
                }
                ctx.tree.mark_dirty(id);
                if vm.render_mode != RenderMode::Buffered {
                    render::render_dirty(ctx, vm);
                }
                if vm.has_code() && ctx.tree.get(id).flags & widget::FLAG_HAS_ON_CLICK != 0 {
                    if let Some(off) =
                        vm.find_by_kind(FunctionKind::OnClick).map(|e| e.offset as u16)
                    {
                        vm.enqueue_callback(off, &[id.0 as i32, new_val as i32]);
                    }
                }
                break;
            }
        }

        // Long press.
        self.hold_count = self.hold_count.saturating_add(1);
        if self.hold_count == LONG_PRESS_FRAMES && !self.long_press_fired {
            let dx = x as i32 - self.gesture_start_x as i32;
            let dy = y as i32 - self.gesture_start_y as i32;
            if dx * dx + dy * dy <= LONG_PRESS_MAX_MOVE_SQ {
                ctx.tree.ensure_dfs();
                let mut pressed_id = WidgetId::NONE;
                for i in 0..ctx.tree.dfs_len() {
                    let id = ctx.tree.dfs_at(i);
                    if ctx.tree.get(id).flags & widget::FLAG_PRESSED != 0 {
                        pressed_id = id;
                    }
                }
                if vm.has_code() {
                    if let Some(off) = vm
                        .find_by_kind(FunctionKind::OnLongPress)
                        .map(|e| e.offset as u16)
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
                .map(|e| e.offset as u16)
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

        ctx.tree.ensure_dfs();
        for i in 0..ctx.tree.dfs_len() {
            let id = ctx.tree.dfs_at(i);
            if ctx.tree.get(id).flags & widget::FLAG_PRESSED != 0 {
                ctx.tree.get_mut(id).flags &= !widget::FLAG_PRESSED;
                if !was_scrolling && !is_swipe {
                    let abs = ctx.tree.absolute_rect(id);
                    if abs.contains(x, y) {
                        clicked_id = id;
                    }
                }
                if is_swipe {
                    swipe_widget_id = id;
                }
                ctx.tree.mark_dirty(id);
            }
        }

        // Dropdown handling.
        let dropdown = dropdown_parent(ctx, clicked_id);
        if dropdown.is_some() {
            select_dropdown_option(ctx, dropdown, clicked_id);
            clicked_id = dropdown;
        } else if clicked_id.is_some() && ctx.tree.get(clicked_id).kind == widget::KIND_DROPDOWN {
            toggle_dropdown(ctx, clicked_id);
            clicked_id = WidgetId::NONE;
        }

        // Checkbox toggle.
        if clicked_id.is_some() && ctx.tree.get(clicked_id).kind == widget::KIND_CHECKBOX {
            ctx.tree.get_mut(clicked_id).flags ^= widget::FLAG_CHECKED;
            ctx.tree.mark_dirty(clicked_id);
        }

        // Radio: uncheck siblings, check self.
        if clicked_id.is_some() && ctx.tree.get(clicked_id).kind == widget::KIND_RADIO {
            let parent = ctx.tree.get(clicked_id).parent;
            if parent.is_some() {
                let mut sib = ctx.tree.get(parent).first_child;
                while sib.is_some() {
                    if sib != clicked_id
                        && ctx.tree.get(sib).kind == widget::KIND_RADIO
                        && ctx.tree.get(sib).flags & widget::FLAG_CHECKED != 0
                    {
                        ctx.tree.get_mut(sib).flags &= !widget::FLAG_CHECKED;
                        ctx.tree.mark_dirty(sib);
                    }
                    sib = ctx.tree.get(sib).next_sibling;
                }
            }
            ctx.tree.get_mut(clicked_id).flags |= widget::FLAG_CHECKED;
            ctx.tree.mark_dirty(clicked_id);
        }

        // Input: reposition cursor on tap.
        if clicked_id.is_some() && ctx.tree.get(clicked_id).kind == widget::KIND_INPUT {
            self.position_cursor(ctx, clicked_id, x);
            ctx.tree.mark_dirty(clicked_id);
        }

        if vm.render_mode != RenderMode::Buffered {
            render::render_dirty(ctx, vm);
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
                if let Some(off) = vm.find_by_kind(FunctionKind::OnSwipe).map(|e| e.offset as u16) {
                    let dominant = if direction <= 1 { dx } else { dy };
                    vm.enqueue_callback(off, &[direction, swipe_widget_id.0 as i32, dominant]);
                }
            }
        }

        // on_click (modal overlay func id first, else the global dispatcher).
        if clicked_id.is_some()
            && vm.has_code()
            && ctx.tree.get(clicked_id).flags & widget::FLAG_HAS_ON_CLICK != 0
        {
            if let Some(click_fn) = vm.modal_overlay_click_fn(clicked_id) {
                if let Some(off) = vm.find_func(click_fn).map(|e| e.offset as u16) {
                    vm.enqueue_callback(off, &[clicked_id.0 as i32]);
                }
            } else if let Some(off) = vm.find_by_kind(FunctionKind::OnClick).map(|e| e.offset as u16)
            {
                vm.enqueue_callback(off, &[clicked_id.0 as i32]);
            }
        }

        // on_tap (skip KIND_INPUT).
        if clicked_id.is_some()
            && vm.has_code()
            && ctx.tree.get(clicked_id).kind != widget::KIND_INPUT
            && ctx.tree.get(clicked_id).flags & widget::FLAG_HAS_ON_TAP != 0
        {
            if let Some(off) = vm.find_by_kind(FunctionKind::OnTap).map(|e| e.offset as u16) {
                let packed = ((x as u32) << 16 | y as u32) as i32;
                vm.enqueue_callback(off, &[clicked_id.0 as i32, packed]);
            }
        }

        if vm.has_code() {
            if let Some(off) = vm
                .find_by_kind(FunctionKind::OnTouchUp)
                .map(|e| e.offset as u16)
            {
                vm.enqueue_callback(off, &[]);
            }
        }
    }

    // --- Input-widget / keyboard helpers ---

    fn focus_input<P: PlatformRuntime>(&mut self, ctx: &mut Ctx<P>, id: WidgetId) {
        if self.kb.focused.is_some() && self.kb.focused != id {
            ctx.tree.get_mut(self.kb.focused).flags &= !widget::FLAG_FOCUSED;
            ctx.tree.mark_dirty(self.kb.focused);
        }
        self.kb.focused = id;
        ctx.tree.get_mut(id).flags |= widget::FLAG_FOCUSED;
        ctx.tree.mark_dirty(id);
        self.kb.visible = true;
        self.kb.dirty = true;
        let now = ctx.systick.millis();
        self.kb.reset_blink(now);
        ctx.cursor_visible = true;
    }

    fn dismiss_keyboard<P: PlatformRuntime>(&mut self, ctx: &mut Ctx<P>) {
        if self.kb.focused.is_some() {
            ctx.tree.get_mut(self.kb.focused).flags &= !widget::FLAG_FOCUSED;
            ctx.tree.mark_dirty(self.kb.focused);
        }
        self.kb.focused = WidgetId::NONE;
        self.kb.visible = false;
        ctx.cursor_visible = false;

        ctx.tree.ensure_dfs();
        for i in 0..ctx.tree.dfs_len() {
            let id = ctx.tree.dfs_at(i);
            let abs = ctx.tree.absolute_rect(id);
            if abs.y + abs.h as i16 > keyboard::KB_Y as i16 {
                ctx.tree.mark_dirty(id);
            }
        }
    }

    fn position_cursor<P: PlatformRuntime>(&self, ctx: &mut Ctx<P>, id: WidgetId, touch_x: u16) {
        let ext = ctx.tree.ext(id).unwrap_or(&WidgetExt::DEFAULT);
        let text_id = ext.text_id;
        let font_id = ext.font_id;

        let font = match ctx.fonts.resolve(font_id) {
            Some(f) => f,
            None => return,
        };

        let abs = ctx.tree.absolute_rect(id);
        let b = ctx.tree.border(id);
        let p = ctx.tree.padding(id);
        let cx = abs.x + b.left as i16 + p.left as i16;

        let mut cursor_pos: i16 = 0;
        if text_id != 0xFFFF {
            let text = ctx.strpool.get(text_id);
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

        if let Some(ext) = ctx.tree.ensure_ext(id) {
            ext.value = cursor_pos;
        }
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
                let text_id = ctx.tree.text_id(id);
                let max_len = ctx.tree.max_length(id);
                let cursor = ctx.tree.value(id).max(0) as usize;
                if text_id != 0xFFFF {
                    if ctx.strpool.insert_byte(text_id, cursor, ch, max_len) {
                        if let Some(ext) = ctx.tree.ensure_ext(id) {
                            ext.value = (cursor + 1) as i16;
                        }
                        ctx.tree.mark_dirty(id);
                        self.fire_on_change(ctx, vm, id);
                    }
                } else {
                    let data = [ch];
                    if let Some(new_id) = ctx.strpool.alloc(&data) {
                        if let Some(ext) = ctx.tree.ensure_ext(id) {
                            ext.text_id = new_id;
                            ext.value = 1;
                        }
                        ctx.tree.mark_dirty(id);
                        self.fire_on_change(ctx, vm, id);
                    }
                }
                let now = ctx.systick.millis();
                self.kb.reset_blink(now);
                ctx.cursor_visible = true;
            }
            keyboard::KeyAction::Backspace => {
                let text_id = ctx.tree.text_id(id);
                let cursor = ctx.tree.value(id).max(0) as usize;
                if text_id != 0xFFFF && cursor > 0 {
                    if ctx.strpool.delete_byte(text_id, cursor - 1) {
                        if let Some(ext) = ctx.tree.ensure_ext(id) {
                            ext.value = (cursor - 1) as i16;
                        }
                        ctx.tree.mark_dirty(id);
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
        if !vm.has_code() || ctx.tree.get(id).flags & widget::FLAG_HAS_ON_TAP == 0 {
            return;
        }
        if let Some(off) = vm.find_by_kind(FunctionKind::OnTap).map(|e| e.offset as u16) {
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
        let _: fn(&mut Ctx<MockPlatform>, u8, Option<u16>) = show_error::<MockPlatform>;
    }
}
