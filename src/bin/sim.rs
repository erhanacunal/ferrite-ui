//! Host-side simulator binary. Loads an fsimage, mounts Fs, runs the VM
//! against a full Ctx (widget tree, fonts, images, string pool), and renders
//! through the Lcd sim backend into a minifb window.
//!
//! Usage: sim [--fsimage <path>]
//!
//! Mouse input is routed into the `Touch` HAL so the firmware's state machine,
//! hit testing, and on_click callbacks are exercised end-to-end.

use std::env;
use std::fs;
use std::process::ExitCode;

use ferrite_ui::ctx::Ctx;
use ferrite_ui::flash::Flash;
use ferrite_ui::font::{self, Font, FontList};
use ferrite_ui::fs::{self as ferrite_fs, Fs};
use ferrite_ui::image::ImageList;
use ferrite_ui::lcd::{HEIGHT, Lcd, WIDTH};
use ferrite_ui::lcd::sim::{Framebuffer, new_framebuffer};
use ferrite_ui::render;
use ferrite_ui::strpool::StringPool;
use ferrite_ui::systick;
use ferrite_ui::touch::{self, CalParams, Touch, TouchEvent, TouchEventKind};
use ferrite_ui::touch::sim::MouseState;
use ferrite_ui::types::{Rect, Size};
use ferrite_ui::vm::{FunctionKind, RenderMode, Vm, VmState};
use ferrite_ui::widget::{
    self, FLAG_CHECKED, FLAG_PRESSED, KIND_CHECKBOX, KIND_RADIO, KIND_SLIDER, WidgetId, WidgetTree,
};
use ferrite_ui::backlight::Backlight;
use ferrite_ui::embedded_font;

use minifb::{Key, MouseButton, MouseMode, Window, WindowOptions};

const COLOR_BLACK: u16 = 0x0000;

fn main() -> ExitCode {
    let fsimage_path = parse_args();

    systick::init();

    let flash = match load_flash(fsimage_path.as_deref()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("failed to load fsimage: {e}");
            return ExitCode::from(1);
        }
    };

    let fb: Framebuffer = new_framebuffer();
    let lcd = Lcd::new(fb.clone());

    let mouse = MouseState::new();
    let mut touch = Touch::new(mouse.clone());
    touch.cal = identity_cal();

    // --- Build Ctx ---
    let mut ctx = Ctx {
        lcd,
        flash,
        tree: WidgetTree::new(),
        fonts: FontList::new(),
        images: ImageList::new(),
        strpool: StringPool::new(),
        fs: None,
        backlight: Backlight::new(),
        cursor_visible: false,
    };

    ctx.fonts.add(Font::from_embedded(
        &embedded_font::GLYPHS,
        &embedded_font::BITMAP,
        embedded_font::FIRST,
        embedded_font::LAST,
        embedded_font::Y_ADVANCE,
    ));

    // --- Mount filesystem ---
    match Fs::mount(&ctx.flash) {
        Ok(f) => {
            let count_prog = f.count_by_kind(&ctx.flash, ferrite_fs::RES_PROGRAM);
            let count_font = f.count_by_kind(&ctx.flash, ferrite_fs::RES_FONT);
            let count_img = f.count_by_kind(&ctx.flash, ferrite_fs::RES_IMAGE);
            println!("fs mounted: programs={count_prog} fonts={count_font} images={count_img}");
            ctx.fs = Some(f);
        }
        Err(_) => {
            println!("fs mount failed; no program will be loaded");
        }
    }

    // --- Root widget (widget 0) ---
    let root = ctx.tree.alloc().expect("root widget alloc failed");
    {
        let w = ctx.tree.get_mut(root);
        w.size = Size { w: WIDTH, h: HEIGHT };
        w.background_color = COLOR_BLACK;
    }
    ctx.tree.root = root;

    // --- Build VM and load program from fs ---
    let mut vm = Vm::new();
    let mut program_loaded = false;

    if let Some(fs_ref) = ctx.fs.as_ref() {
        if let Some(entry) = fs_ref.find(&ctx.flash, b"main") {
            let img_len = entry.size.min(64 * 1024) as usize;
            let mut img_buf = vec![0u8; img_len];
            fs_ref.read_resource(&ctx.flash, &entry, 0, &mut img_buf);
            if vm.load_ram(&img_buf) {
                program_loaded = true;
                let mode = match vm.render_mode {
                    RenderMode::Dirty => "Dirty",
                    RenderMode::Buffered => "Buffered",
                };
                println!("loaded program 'main' ({} bytes, render_mode={})",
                    img_len, mode);
            } else {
                eprintln!("vm.load_ram('main') failed");
            }
        } else {
            println!("fs has no 'main' program");
        }
    }

    // --- Run setup() + start loop() ---
    if program_loaded {
        if let Some(entry) = vm.find_by_kind(FunctionKind::Setup) {
            let offset = entry.offset as u16;
            vm.run_callback(offset, &mut ctx);
            let rc = vm.pop_result();
            if rc != 0 {
                eprintln!("setup() returned {rc}");
            }
        }
        if let Some(entry) = vm.find_by_kind(FunctionKind::OnProgramStart) {
            let offset = entry.offset as u16;
            vm.run_callback(offset, &mut ctx);
        }
        if let Some(entry) = vm.find_by_kind(FunctionKind::Loop) {
            vm.set_pc(entry.offset as u16);
            vm.state = VmState::Running;
        }
    }

    // Do NOT call render_all here — it clears dirty flags, which would suppress
    // on_paint callbacks on the very first frame. Let the main loop's
    // render_dirty / render_buffered_content draw the initial frame from the
    // dirty flags that setup()/OnProgramStart left behind.

    run_window(fb, &mut ctx, &mut touch, &mut vm, mouse)
}

fn parse_args() -> Option<String> {
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--fsimage" {
            return args.next();
        }
    }
    None
}

fn load_flash(path: Option<&str>) -> Result<Flash, String> {
    match path {
        Some(p) => {
            let mut bytes = fs::read(p).map_err(|e| format!("{p}: {e}"))?;
            let raw_len = bytes.len();

            // Detect an fs-only image (starts with "FERR" magic at offset 0)
            // and prepend 0x1000 bytes of 0xFF so the header lands at FS_BASE.
            if bytes.len() >= 4 && &bytes[..4] == b"FERR" {
                let mut padded = vec![0xFFu8; ferrite_ui::fs::FS_BASE as usize];
                padded.extend_from_slice(&bytes);
                bytes = padded;
                println!(
                    "loaded {} bytes from {p} (fs-only image — padded with {}B reserved prefix)",
                    raw_len,
                    ferrite_ui::fs::FS_BASE
                );
            } else {
                println!("loaded {} bytes from {p}", raw_len);
            }
            Ok(Flash::from_image(bytes))
        }
        None => {
            println!("no --fsimage specified; starting with blank 32MB flash");
            Ok(Flash::new())
        }
    }
}

fn identity_cal() -> CalParams {
    CalParams {
        xy_swap: false,
        x_flip: false,
        y_flip: false,
        x_min: 0,
        x_max: WIDTH - 1,
        y_min: 0,
        y_max: HEIGHT - 1,
    }
}

fn run_window(
    fb: Framebuffer,
    ctx: &mut Ctx,
    touch_ctl: &mut Touch,
    vm: &mut Vm,
    mouse: MouseState,
) -> ExitCode {
    let mut window = match Window::new(
        "ferrite-ui sim",
        WIDTH as usize,
        HEIGHT as usize,
        WindowOptions::default(),
    ) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("window open failed: {e}");
            return ExitCode::from(1);
        }
    };
    window.set_target_fps(60);

    while window.is_open() && !window.is_key_down(Key::Escape) {
        // --- Sample mouse, push into touch HAL ---
        let pressed = window.get_mouse_down(MouseButton::Left);
        let pos = window.get_mouse_pos(MouseMode::Discard);
        let (mx, my) = match pos {
            Some((x, y)) => (
                (x.clamp(0.0, (WIDTH - 1) as f32)) as u16,
                (y.clamp(0.0, (HEIGHT - 1) as f32)) as u16,
            ),
            None => (0, 0),
        };
        mouse.update(mx, my, pressed && pos.is_some());

        // --- VM step (Running / Yielded / Waiting) ---
        match vm.state {
            VmState::Running | VmState::Yielded => {
                vm.state = VmState::Running;
                vm.step(ctx);
                while vm.is_critical() && vm.state == VmState::Running {
                    vm.step(ctx);
                }
            }
            VmState::Waiting => {
                // Non-blocking delay: check if the target tick has passed.
                if systick::millis().wrapping_sub(vm.wait_until) < 0x8000_0000 {
                    vm.state = VmState::Running;
                }
            }
            _ => {}
        }

        // --- Touch event dispatch: enqueues callbacks, does NOT drain ---
        if let Some(event) = touch_ctl.poll() {
            dispatch_touch(ctx, vm, &event);
            // Press in firmware triggers an immediate render_dirty to reflect
            // the pressed highlight without waiting for the frame render.
            if event.kind == TouchEventKind::Press && vm.render_mode == RenderMode::Dirty {
                render::render_dirty(ctx);
            }
        }

        // --- Render phase (mirrors main.rs cadence) ---
        render_phase(ctx, vm);

        // --- Drain callback queue, then re-render ---
        if vm.has_pending_callbacks() {
            vm.drain_callbacks(ctx);
            render_phase(ctx, vm);

            // Fire on_paint for widgets newly made visible by those callbacks
            // (e.g. canvas on a switched-in page whose dirty flag was already
            // cleared by an in-callback wRender()).
            let mut paint_ids: [WidgetId; 8] = [WidgetId::NONE; 8];
            let mut paint_count: usize = 0;
            {
                let dfs = ctx.tree.dfs_order().to_vec();
                for id in dfs {
                    let on_paint = ctx.tree.on_paint(id);
                    if on_paint != 0
                        && ctx.tree.is_tree_visible(id)
                        && !ctx.tree.get(id).is_dirty()
                        && paint_count < 8
                    {
                        paint_ids[paint_count] = id;
                        paint_count += 1;
                    }
                }
            }
            for i in 0..paint_count {
                let id = paint_ids[i];
                let paint_func = ctx.tree.on_paint(id);
                if paint_func > 0 {
                    if let Some(entry) = vm.find_func(paint_func) {
                        vm.enqueue_callback(entry.offset as u16, &[id.0 as i32]);
                    }
                }
            }
            if vm.has_pending_callbacks() {
                vm.drain_callbacks(ctx);
            }
        }

        let buf = fb.borrow();
        if let Err(e) = window.update_with_buffer(&buf, WIDTH as usize, HEIGHT as usize) {
            eprintln!("window update failed: {e}");
            return ExitCode::from(1);
        }
    }

    ExitCode::SUCCESS
}

fn touch_to_slider_value(abs: &Rect, touch_x: u16) -> i16 {
    let x = touch_x as i16;
    if x <= abs.x {
        0
    } else if x >= abs.x + abs.w as i16 {
        100
    } else {
        (((x - abs.x) as u32) * 100 / abs.w as u32) as i16
    }
}

fn dispatch_touch(ctx: &mut Ctx, vm: &mut Vm, event: &TouchEvent) {
    match event.kind {
        TouchEventKind::Press => {
            let hit = touch::hit_test(&mut ctx.tree, event.x, event.y);
            if hit.is_some() {
                ctx.tree.get_mut(hit).flags |= FLAG_PRESSED;

                // Slider: update value immediately from press X.
                if ctx.tree.get(hit).kind == KIND_SLIDER {
                    let abs = ctx.tree.absolute_rect(hit);
                    let new_val = touch_to_slider_value(&abs, event.x);
                    if let Some(ext) = ctx.tree.ensure_ext(hit) {
                        ext.value = new_val;
                    }
                    if vm.has_code() {
                        let on_click = ctx.tree.on_click(hit);
                        if on_click > 0 {
                            if let Some(entry) = vm.find_func(on_click) {
                                vm.enqueue_callback(
                                    entry.offset as u16,
                                    &[hit.0 as i32, new_val as i32],
                                );
                            }
                        }
                    }
                }

                ctx.tree.mark_dirty(hit);
            }
            if vm.has_code() {
                if let Some(entry) = vm.find_by_kind(FunctionKind::OnTouchDown) {
                    vm.enqueue_callback(
                        entry.offset as u16,
                        &[event.x as i32, event.y as i32],
                    );
                }
            }
        }
        TouchEventKind::Hold => {
            // Continue updating any PRESSED slider while dragging.
            let dfs = ctx.tree.dfs_order().to_vec();
            for id in dfs {
                let w = ctx.tree.get(id);
                if w.flags & FLAG_PRESSED != 0 && w.kind == KIND_SLIDER {
                    let abs = ctx.tree.absolute_rect(id);
                    let new_val = touch_to_slider_value(&abs, event.x);
                    if let Some(ext) = ctx.tree.ensure_ext(id) {
                        ext.value = new_val;
                    }
                    ctx.tree.mark_dirty(id);

                    if vm.has_code() {
                        let on_click = ctx.tree.on_click(id);
                        if on_click > 0 {
                            if let Some(entry) = vm.find_func(on_click) {
                                vm.enqueue_callback(
                                    entry.offset as u16,
                                    &[id.0 as i32, new_val as i32],
                                );
                            }
                        }
                    }
                    break;
                }
            }

            if vm.has_code() {
                if let Some(entry) = vm.find_by_kind(FunctionKind::OnTouchMove) {
                    vm.enqueue_callback(
                        entry.offset as u16,
                        &[event.x as i32, event.y as i32],
                    );
                }
            }
        }
        TouchEventKind::Release => {
            let mut clicked_id = WidgetId::NONE;
            let mut clicked_func: u16 = 0;
            let dfs = ctx.tree.dfs_order().to_vec();
            for id in dfs.iter().copied() {
                let w = ctx.tree.get_mut(id);
                if w.flags & FLAG_PRESSED != 0 {
                    w.flags &= !FLAG_PRESSED;
                    let abs = ctx.tree.absolute_rect(id);
                    if abs.contains(event.x, event.y) {
                        clicked_id = id;
                        clicked_func = ctx.tree.on_click(id);
                    }
                    ctx.tree.mark_dirty(id);
                }
            }

            // Checkbox toggle
            if clicked_id.is_some() && ctx.tree.get(clicked_id).kind == KIND_CHECKBOX {
                let w = ctx.tree.get_mut(clicked_id);
                w.flags ^= FLAG_CHECKED;
                ctx.tree.mark_dirty(clicked_id);
            }

            // Radio: uncheck siblings, check self
            if clicked_id.is_some() && ctx.tree.get(clicked_id).kind == KIND_RADIO {
                let parent = ctx.tree.get(clicked_id).parent;
                if parent.is_some() {
                    let mut sib = ctx.tree.get(parent).first_child;
                    while sib.is_some() {
                        if sib != clicked_id
                            && ctx.tree.get(sib).kind == KIND_RADIO
                            && ctx.tree.get(sib).flags & FLAG_CHECKED != 0
                        {
                            ctx.tree.get_mut(sib).flags &= !FLAG_CHECKED;
                            ctx.tree.mark_dirty(sib);
                        }
                        sib = ctx.tree.get(sib).next_sibling;
                    }
                }
                let w = ctx.tree.get_mut(clicked_id);
                w.flags |= FLAG_CHECKED;
                ctx.tree.mark_dirty(clicked_id);
            }

            if vm.has_code() && clicked_id.is_some() && clicked_func > 0 {
                if let Some(entry) = vm.find_func(clicked_func) {
                    vm.enqueue_callback(
                        entry.offset as u16,
                        &[clicked_id.0 as i32],
                    );
                }
            }

            // on_tap (widget_id, packed x|y)
            if clicked_id.is_some() && vm.has_code() {
                let tap_func = ctx.tree.on_tap(clicked_id);
                if tap_func > 0 {
                    if let Some(entry) = vm.find_func(tap_func) {
                        let packed_xy = ((event.x as u32) << 16 | event.y as u32) as i32;
                        vm.enqueue_callback(
                            entry.offset as u16,
                            &[clicked_id.0 as i32, packed_xy],
                        );
                    }
                }
            }

            if vm.has_code() {
                if let Some(entry) = vm.find_by_kind(FunctionKind::OnTouchUp) {
                    vm.enqueue_callback(entry.offset as u16, &[]);
                }
            }
        }
    }

    // Note: callbacks enqueued here are drained by the main loop's render
    // phase, not inside this function. Matches the firmware cadence.
}

fn render_phase(ctx: &mut Ctx, vm: &mut Vm) {
    // Collect dirty widgets with on_paint BEFORE render clears dirty flags.
    let mut paint_ids: [WidgetId; 8] = [WidgetId::NONE; 8];
    let mut paint_count: usize = 0;
    {
        let dfs = ctx.tree.dfs_order().to_vec();
        for id in dfs {
            let w = ctx.tree.get(id);
            let on_paint = ctx.tree.on_paint(id);
            if w.is_dirty() && on_paint != 0 && paint_count < 8 {
                paint_ids[paint_count] = id;
                paint_count += 1;
            }
        }
    }

    match vm.render_mode {
        RenderMode::Dirty => render::render_dirty(ctx),
        RenderMode::Buffered => {
            if render::buffered_has_dirty(ctx) {
                render::render_buffered(ctx);
            }
        }
    }

    if vm.has_code() {
        for i in 0..paint_count {
            let id = paint_ids[i];
            let paint_func = ctx.tree.on_paint(id);
            if paint_func > 0 {
                if let Some(entry) = vm.find_func(paint_func) {
                    vm.enqueue_callback(entry.offset as u16, &[id.0 as i32]);
                }
            }
        }
    }
}

// Silence the unused import warnings when font/widget modules expose private consts.
#[allow(dead_code)]
fn _touch_font_widget_deps() {
    let _ = font::Font::from_embedded;
    let _ = widget::FLAG_PRESSED;
}
