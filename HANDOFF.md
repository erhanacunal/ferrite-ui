# Draw-list renderer — design handoff (2026-06-19)

Design sketch for moving rendering behind an immutable **draw list** (à la Dear
ImGui's draw list), so the rasterization can later run on a **separate thread**
— specifically on a multi-core target (the dual-core ESP32-S3 epaper BSP), while
the single-core F1C100s keeps drawing synchronously at zero cost.

## Why

A render thread that executes the drawing primitives needs the widget data to be
**stable** while it draws. Today `render.rs` (`draw_widget`/`draw_widget_clipped`)
reads the **live** `WidgetTree` + `StringPool` and calls `ctx.lcd.*` directly. The
VM thread mutates that same state (`W_ALLOC` grows the `Vec`s and reallocates,
`set(text,…)` appends to the `StringPool`, `clear()` wipes it). Drawing and
mutating the tree concurrently is a data race / use-after-free.

The draw list breaks the dependency: the VM thread walks the tree **once** and
records a flat, self-contained command buffer; the consumer replays that buffer
and **never touches the tree**. That is the only thing that makes off-thread
rendering safe — the threading itself is the easy part.

> Single-core reality: on the F1C100s this buys **no** throughput (one core,
> rasterization is serial). The value is (a) the clean decoupling and (b) true
> parallelism on the **multi-core** epaper board. On F1C100s you keep the
> synchronous sink and the draw list never exists.

## Core idea: one tree-walk, two sinks

Insert a trait between the tree-walk and the pixels. The tree-walk targets the
trait instead of `LcdBackend` directly.

```rust
/// Receives primitive draw commands. The render.rs tree-walk targets this.
pub trait DrawSink {
    fn clip(&mut self, r: Rect);                                   // set scissor
    fn fill_rect(&mut self, r: Rect, color: u16);
    fn blend_rect(&mut self, r: Rect, color: u16, alpha: u8);
    fn gradient(&mut self, r: Rect, c0: u16, c1: u16, dir: u8);
    fn image(&mut self, r: Rect, image_id: u16);
    fn text(&mut self, x: i16, y: i16, color: u16, font_id: u8, s: &[u8]);
    fn circle(&mut self, cx: i16, cy: i16, rad: u16, fill: u16, stroke: u16, sw: u8);
    fn line(&mut self, a: Offset, b: Offset, color: u16, w: u8);
    fn polygon(&mut self, pts: &[Offset], fill: u16, stroke: u16);
    // … ellipse, rounded_rect — one method per primitive render.rs already uses
}
```

Two implementations:

```rust
// 1. Direct — exactly today's behavior, zero overhead. F1C100s uses this.
impl<B: LcdBackend> DrawSink for Direct<'_, B> {
    fn fill_rect(&mut self, r: Rect, c: u16) { self.lcd.fill_rect(r.x, r.y, r.w, r.h, c); }
    // text(): the current font draw_str path; etc.
}

// 2. Record — serialize into a flat, self-contained buffer.
impl DrawSink for DrawList { /* push a DrawCmd, copy bytes into the arena */ }
```

`draw_widget` becomes `fn draw_widget<S: DrawSink>(tree, vm, id, abs, sink: &mut S)`.
That is the whole structural change — the tree-walk, clip math, occlusion,
gradients all stay identical; they just target `S` instead of `ctx.lcd`.

## The draw list (the ImGui draw-list equivalent)

```rust
pub enum DrawCmd {
    Clip(Rect),
    FillRect  { r: Rect, color: u16 },
    BlendRect { r: Rect, color: u16, alpha: u8 },
    Gradient  { r: Rect, c0: u16, c1: u16, dir: u8 },
    Image     { r: Rect, image_id: u16 },
    Text      { x: i16, y: i16, color: u16, font_id: u8, off: u32, len: u16 }, // off/len → bytes
    Circle    { cx: i16, cy: i16, rad: u16, fill: u16, stroke: u16, sw: u8 },
    Line      { a: Offset, b: Offset, color: u16, w: u8 },
    Polygon   { off: u32, n: u16, fill: u16, stroke: u16 },                    // points → bytes
}

pub struct DrawList {
    cmds:  Vec<DrawCmd>,   // ~12–16 B each
    bytes: Vec<u8>,        // copied strings + polygon points — NO StringPool refs
}
```

**The one rule that makes threading safe:** a `DrawCmd` may contain **no
references into the widget tree or `StringPool`**. Text bytes and polygon points
are **copied** into `bytes` at record time. After recording, the list is fully
self-contained and immutable — the consumer needs only read-only resources.

## Record → replay split

```rust
// UI/VM thread: walk the live tree once, emit ops. All tree/clip/occlusion
// logic lives here. (This is render_all / render_dirty with sink = &mut list.)
fn record_frame(ctx, vm) -> DrawList { /* … draw_widget(.., &mut list) … */ }

// Consumer: a dumb, immutable executor. Touches ONLY read-only resources.
fn replay<B: LcdBackend>(list: &DrawList, lcd: &B, fonts: &FontList, flash: &Flash) {
    let mut clip = screen_rect();
    for cmd in &list.cmds {
        match *cmd {
            DrawCmd::Clip(r)               => clip = r,
            DrawCmd::FillRect { r, color } => fill_clipped(lcd, r, color, &clip),
            DrawCmd::Text { x, y, color, font_id, off, len } =>
                draw_text(lcd, fonts, flash, font_id, x, y, color,
                          &list.bytes[off as usize..][..len as usize], &clip),
            // …
        }
    }
}
```

`replay` needs: `LcdBackend` (framebuffer), `FontList` (RAM glyph headers),
`Flash` (glyph/image bitmaps), `ImageList`. **All read-only and immutable after
boot** → safe to share with another thread. It explicitly never touches
`WidgetTree`, `StringPool`, or `Vm`. That isolation is what lets it run on core 2.

## Where it slots into `render_phase`

```rust
RenderMode::Buffered => if render::buffered_has_dirty(ctx) {
    // single-core (F1C100s): sink straight to the LCD — no list, no cost
    #[cfg(not(feature = "threaded_render"))] {
        ctx.lcd.begin_frame();
        render::record_frame_into(&mut Direct(&ctx.lcd), ctx, vm); // == today
        P::present_buffered(ctx);
    }
    // multi-core (ESP32-S3): record here, replay on the render thread
    #[cfg(feature = "threaded_render")] {
        let list = render::record_frame(ctx, vm);   // UI / core-0
        P::submit_draw_list(list);                   // hand to core-1
    }
}
```

On single core you keep `sink = &lcd` and the draw list never exists — zero
overhead, identical to today. The same `record_frame` code drives both.

## Threaded handoff (target: dual-core epaper)

- **Double-buffer the lists** (ping-pong): UI thread records list B while the
  render thread replays list A. Sync with the existing `Semaphore` (READY/DONE)
  — same pattern as the dormant present thread, but the payload is the whole
  frame, not just the flip.
- **Single framebuffer owner:** the render thread does `begin_frame → replay →
  end_frame`; the UI thread never touches the LCD.
- **Arena reuse:** make `DrawList` a bump arena (`cmds`/`bytes` are `clear()`-ed,
  never freed) → no per-frame heap churn (important for `no_std` determinism).
  Two arenas, alternated.
- On ESP32-S3 pin the render thread to core 1, UI/VM to core 0 → real parallel
  rasterization.

## Migration (each step independently shippable + testable)

1. **Introduce `DrawSink`, route current rendering through `Direct`.** Pure
   refactor, no behavior change — verify pixel-identical output on sim +
   F1C100s. This is the bulk of the work and it is mechanical.
2. **Add `DrawList` as a second sink + `replay`.** Debug path: record then
   replay inline; assert it matches `Direct`. Still single-threaded.
3. **Add the render thread on the epaper BSP** behind `threaded_render`.
   F1C100s stays on `Direct`.

## Cost / care

- List size: ~25 widgets → ~50–150 cmds × ~14 B + a small byte arena ≈ 2–4 KB
  per frame; double-buffered ≈ 8 KB. Fine on both targets.
- Step 1 risk is low (no-op refactor). The real care: **every** primitive
  currently called on `ctx.lcd` inside `render.rs` must go through `DrawSink`.
  Grep `render.rs` for `.lcd.` — clip fills, gradient, alpha-blend, shapes, font
  `draw_str`, image blit, and the scissor-based shape path near the bottom of
  `draw_widget_clipped` (shapes draw raw geometry confined via the LCD scissor;
  those become a `Clip` op + primitive in the list).

## Status

Not started — design only. The async-**present** scaffolding (present thread,
`READY/SLOT/VSYNC` semaphores, `vblank_isr`, triple framebuffer) already exists
dormant in `bsp/tdo_y13/src/lcd/mod.rs` + `main.rs`; it offloads only the vsync
wait + buffer flip, **not** the drawing. This draw-list design is the path to
offloading the actual rasterization, and supersedes that approach for the
multi-core case.
