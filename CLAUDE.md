# CLAUDE.md — ferrite-ui

Bare-metal HMI framework written in Rust for the Nextion NX8048K070 display.

## Hardware

- **CPU:** GD32F103RBT6 (Cortex-M3, 108MHz, 128KB Flash, 20KB RAM)
- **Display:** NX8048K070 (800x480)
- **FPGA:** Display controller — refreshes LCD independently from CPU
  - 2MB frame buffer on FPGA, no frame buffer on CPU
  - Double buffer: swap via CMD 4/5, no tearing
- **Touch:** XPT2046 (SPI)
- **Flash:** W25Q256JVFQ 32MB (SPI) — UI bytecode, fonts, images stored here
- **RTC:** AT8563T (I2C, PC0/PC1)
- **EEPROM:** 24C08 1 KB (same I2C bus as the RTC, 0x54–0x57)

## FPGA Protocol

- `GPIOB[15:0]` = 16-bit data bus
- `PA15`: 1=data, 0=command (LCD_CMD_DATA)
- `PA12`: clock (BC=falling edge, BOP=rising edge)
- `spin(3)` delay required — 108MHz CPU outruns the FPGA

| CMD  | Function             | Data          |
|------|----------------------|---------------|
| 0x02 | Y start (y1)        | uint16 pixel  |
| 0x03 | X start (x1)        | uint16 pixel  |
| 0x06 | Y end (y2)          | uint16 pixel  |
| 0x07 | X end (x2)          | uint16 pixel  |
| 0x0D | Write scan order     | bit 0: 0=row-major (x first), 1=column-major (y first) |
| 0x0F | Begin pixel write    | subsequent data = pixel color |
| 0x04 | Front buffer swap    | lcd4Value = lcd5Value |
| 0x05 | Select back buffer   | 0 or 1        |

### Double Buffer Flow

```
begin_frame() → CMD5 (back buffer toggle)
  → set_address() + pixel data (write to back buffer)
end_frame()   → CMD4 (front ← back, FPGA swap)
```

`lcd4Value` = what FPGA displays (front), `lcd5Value` = what is written (back).
`begin_frame` is not called when `lcd4 == lcd5` — buffer is already fresh.

### CMD 0x0D — Write Scan Order

- Bit 0 = 1: pixel data fills the window column by column (y increments first, then the next x column). The window registers (0x02/0x03/0x06/0x07) mean the same thing in both modes.
- Solid fills look identical in both modes, but streamed content (glyphs, images) comes out transposed inside its window — a transpose is a rotation *plus a mirror*, so portrait mode does not use it (see Portrait Mode).
- Firmware never sets it; leave it 0 (row-major), or text drawing garbles.
- Bits 1–15 showed no effect on writes (text and solid fills); scan-out effects not checked.
- Found with `examples/nextion/fpga/fpga_probe.fl` (bit sweep), `fpga_map_probe.fl`, `fpga_stream_probe.fl`.

## Architecture Decisions

### Rust no_std
- `#![no_std]` + `#![no_main]`
- Rust 2024 edition (`edition = "2024"` — stable with Rust 1.85)
- `cortex-m-rt` crate — startup, interrupt table, `#[entry]` macro
- Custom panic handler — displays error on LCD + sends via USART (no heap, ROM font only)
- No newlib, no hidden runtime, RAM fully owned by user
- Target: `thumbv7m-none-eabi`

### Widget System
- **HTML-like nesting** — widgets inside widgets, tree structure
- **Stored in the object pool** (`ctx.objs`, see Object Pool) — a widget is one `T_WIDGET` object, `WidgetId` = its pointer
- **Tree structure:** left-child right-sibling (parent + first_child + next_sibling)
- **WidgetId:** `ObjPtr` object pointer (u16, u32 with `wide_obj`), `ObjPtr::MAX` = NONE sentinel
- **Variable-size object: 24 B header (32 B with `wide_obj`: u32 links, `WValue` value i32) + optional 8/16 B sections**
  - **Header (`Widget`):** tree links, kind, section mask, flags, location, size, background_color, border_color, value
  - **Sections** (stored in bit order, added only when a field gets a non-default value): `SEC_BOX` border/padding, `SEC_TEXT` text id/color/font/align/max_length, `SEC_STYLE` border_radius/press/gradient/alpha, `SEC_EXTRA` image_id/group_id/flex fields, `SEC_PREV` (16 B) last-drawn rects, `SEC_DRAWN` (12 B, renderer-private, gauges only) what an incrementally redrawn widget last drew. Adding one grows the object (it may move; the id doesn't change).
  - `WidgetExt` is a by-value view of all optional fields: `objs.ext(id)` gathers, `objs.update_ext(id, |e| …)` / `put_ext` scatter. Single fields: `objs.border(id)`, `objs.press_color(id)`, etc.
  - Stale/destroyed ids: `get` returns a static destroyed placeholder, `get_mut` a scratch one (writes are lost)
- **Box model (CSS border-box style):**
  - `border` → border line (included in size)
  - `padding` → inner spacing (included in size)
  - `location` → relative offset from parent content area
  - `size` → border box dimensions
- **Flags:** `VISIBLE` (0x01), `ENABLED` (0x02), `CLICKABLE` (0x04), `DIRTY` (0x08), `PRESSED` (0x10), `CHECKED` (0x20)
- **Widget types:** `KIND_BASE` (0, container), `KIND_LABEL` (1, text), `KIND_BUTTON` (2, clickable container), `KIND_PROGRESS` (3, progress bar), `KIND_SLIDER` (4, slider), `KIND_CHECKBOX` (5, checkbox), `KIND_RADIO` (6, radio button), `KIND_INPUT` (7, text input), `KIND_GAUGE` (8, gauge), `KIND_DROPDOWN` (9, dropdown), `KIND_GRAPH` (10, spline graph), `KIND_IMAGE` (11, image)
- **Shape widgets:** `KIND_RECTANGLE` (12), `KIND_CIRCLE` (13), `KIND_POLYGON` (14), `KIND_LINE` (15), `KIND_ELLIPSE` (16) — fill = `bg_color`, stroke = `border_color`, stroke/line thickness = border width. Polygon points come from `text` as `"x,y x,y …"` (relative to the widget's top-left); Line draws the widget-box diagonal; Circle/Ellipse fill the widget box (ellipse uses the full w×h). Kind numbers are duplicated in `widget.rs` and `tools/ferrite_designer.py` — keep in sync. `lib/shapes.fl` provides named `KIND_*` constants.
- **Button Group:** `KIND_BUTTON_GROUP` (17) — exclusive toggle button. Widgets of this kind with the same `group_id` (`PROP_GROUP_ID` 0x32, `WidgetExt.group_id`, tree-wide) form one group; only one is CHECKED. A tap or `set(checked, 1)` → `ObjPool::select_in_group`. Re-tapping the selected one keeps it selected. Selected = drawn with `press_color` (`render::shows_press`). Set `group_id` before `checked`. Kind number is in `widget.rs`, `tools/ferrite_designer.py`, `lib/core.fl`.
- **Radio groups:** `KIND_RADIO` uses the same `group_id` via `select_in_group`, with one difference: `group_id` 0 (the default) = sibling radios with group 0 (the original behaviour), ≠ 0 = tree-wide like Button Group. `set(checked, 1)` enforces the group only for ≠ 0. Radio and Button Group groups are separate even with equal ids. The designer mirrors this in `DesignerModel.enforce_exclusive_group`.
- **Margin removed:** prop ids 0x10–0x13 / 0x42 are reserved.
- **Tree links readable:** `get(parent/first_child/next_sibling)` (`PROP_PARENT`/`FIRST_CHILD`/`NEXT_SIBLING` 0x0B–0x0D) return a widget id or -1. Read-only: the compiler rejects `set` and dot assignment (`_READ_ONLY_PROPS` in `ferrite_lang.py`).
- **Flex layout (`lib/flex.fl`):** layout lives in the library, not the firmware. `PROP_FLEX` 0x33 (container: bits 0-1 direction 1=row/2=column, 2-4 justify start/center/end/space-between/stretch, 5-6 cross align start/center/end/stretch), `PROP_FLEX_GAP` 0x34, `PROP_FLEX_GROW` 0x35 (child) are only stored in `WidgetExt`. `flexLayout(box)` walks the children and sets their loc/size, then recurses into flex children. Growing children ignore their own main size (CSS `flex: 1`), so re-running is idempotent. Bit layout is duplicated in `lib/flex.fl` and docs — keep in sync. Example: `examples/nextion/flex_demo`.
- **Label:** text_color, font_id, text_align (LEFT/CENTER/RIGHT), text (a string object id)
- **Button:** press_color (background when pressed), accepts child widgets
- **Color:** RGB565 (`background_color`, `border_color`, `text_color`, `press_color`)
- **Painter's algorithm** — z-order: DFS pre-order (lower index = behind)
- **Clip region** (inspired by ReactOS Region API)
  - Static rect pool: `MAX_CLIP_RECTS = 32`
  - Each `subtract` operation produces max 4 new rects (top/bottom/left/right strips)
  - Pool full fallback: draw entire dirty rect (overdraw, no tearing)
- **Dirty redraw flow:**
  1. `mark_dirty(id)` → marks widget + entire subtree as dirty
  2. `render_dirty()` → computes DFS order. Skipped without a scan while `ObjPool::maybe_dirty` is down: raised by `mark_dirty`/`alloc_widget` (the only ways to set a dirty bit), lowered by a render scan that finds nothing dirty (`render_dirty`, `buffered_has_dirty`; `collect_dirty_paint` also checks it)
  3. For each dirty widget, collects occluders (later in DFS, not descendants)
     - **Geometry table (`render_cache` feature, `geo.rs`; exynos4412 + sim):** each render pass (`render_dirty`, `render_buffered_content`, `render_all`) brackets itself with `geo_begin`/`geo_end`; in between, `abs_at`/`vis_at`/`scroll_parent_at`/`sub_end_at` (by DFS index) read a table built in one pre-order pass instead of walking the parent chain (the occluder search is O(n²) lookups: 192 dirty boxes 436 → 67 µs on the host). Without the feature (GD32) or if the table can't be allocated they compute live. Only valid while nothing changes geometry/visibility/tree shape — the passes change only flags and recorded rects. Test: `geo::tests::table_matches_live_walks`.
  4. Subtracts occluder rects from ClipRegion
  5. Widget is drawn over remaining visible rects
  6. Child widgets are drawn recursively with the same occluder list
- **Hidden / moved widgets (dirty mode):** pass 1 restores what a newly hidden widget covered (`restore_behind`: ancestor bg + earlier widgets clipped to its rect, then marks the visible widgets in front that the restore overwrote dirty so pass 2 redraws them). A location/size change (`PROP_LOC_*`/`PROP_SIZE_*`, compound `location`/`size`) calls `ObjPool::will_move` first: a drawn widget remembers its on-screen rect as SEC_PREV A (once per frame: an animation's x/y/w/h writes keep the first), pass 1b restores it and clears it. Only moved widgets get the section. Before this, dirty mode left trails behind moving widgets and a hidden popup left a hole (only the nearest ancestor's bg was painted).
- **Moved-subtree restore reuse (dirty mode, readback backends):** `render_dirty` records its pass 1/1b restores (`Restores`, 16 entries). A dirty subtree root that moved had what is behind its old box put back in pass 1b, and outside that box it never drew, so its see-through descendants inside its box get `behind_is_fresh(.., fresh = true)` and don't each redraw everything behind again — unless a later restore for a widget past the root touches its old or new box (that one drew the subtree back in). Relies on labels/graphs being clipped to their box. Nor if a widget before it was drawn in this pass over its box (`painted_before`): pass 2 subtracts every later widget, see-through ones too, from an earlier one's clip, so the later one must restore there (two clock hands moving in one tick left a gap in the minute hand; test `overlapping_see_through_widgets_moving_together`). Empty groups (`paints_nothing`: a label without text/bg/border/image, not the root, inside the root's box) are skipped. Tablet page slide 8.9 → 1.8 Mpx drawn per frame, shade/drawer slides no longer re-restore; pixel-identical results. Test: `restores_fresh_behind_only_until_the_subtree_is_redrawn`. Still full price: a translucent widget changing in place (a full-screen alpha fade recomposites the whole screen per frame; the tablet's scrim therefore switches instead of fading).
- **Moved-subtree pixel shift (dirty mode, `LcdBackend::CAN_MOVE`: exynos4412, sim):** `render_dirty` pass 0 (`shift_moved`) moves the on-screen pixels of a dirty subtree root that only moved (`move_rects(rects, dx, dy)`: every source read before any destination is written) instead of restoring its old box and redrawing it. Conditions:
  - the screen is settled: `draw_epoch == settled_epoch`, the DFS is valid;
  - same size, a previous rect, the parent is not dirty, not inside a scroll container;
  - nothing in the subtree changed: `mark_dirty` notes changed ids (`ObjPool::changed`, 8 entries, overflow = changed), `mark_moved` (`PROP_LOC_X/Y`, `location`, and their animation ticks) does not;
  - opaque (`opaque_radius`: alpha 255, a bg, no `on_paint`, not freeform);
  - `plan_moves` gets exact regions: the source/destination are clipped to the screen, without the 4 corner squares and every later visible widget's old/new box (at most `SHIFT_MAX` 32 rects).
  
  Boxes under `LcdBackend::MOVE_MIN_PX` (exynos 128×128; each moved rect costs two G2D copies of ~5 µs setup) are redrawn instead: the Monitor stress test's small tiles were slower moved. Then only `behind` (old box − destinations) is restored, the shifted rects are kept out of pass 2's clip (`shift_keep`), and another restore that touches them cancels that. GD32: `CAN_MOVE` false, so the code is left out (`#[inline(never)]`, heap Vecs). Sim: `FERRITE_SIM_NO_MOVE=1` turns it off for A/B runs (drawer/shade open/close: pixel-identical). Tests: `render::shift_tests`. Scroll containers are not shifted yet.
- **Word wrap:** `Font::wrap_line(text, start, max_w)` → `(end, next)` (breaks at `'\n'`, the last space that fits, or inside a too-long word). Multi-line labels wrap (centred block, top-aligned when taller than the box, only whole lines drawn). Multi-line inputs (`multi_line` on a `KIND_INPUT`) wrap top-aligned, scroll to keep the cursor line visible (`render::input_first_line`, shared with the tap-to-cursor code `position_cursor_lines`), and ENTER inserts `'\n'` (tap outside closes the keyboard). Test: `font::tests::wrap_line_*`.
- **Clip to the box:** label text, graphs, value fills (progress/slider), check indicators, input and dropdown contents draw through the scissor = each clip rect ∩ the widget's box (the last four used to ignore the clip: drawn over widgets in front, and a shifted subtree's fill ends were blended twice). `render_all` clips to the screen only, so glyphs taller than the box (and a spline's overshoot) used to be drawn past it on the first frame, and no later redraw or restore (both clipped to the box) erased them. Graphs used to ignore the clip region entirely and drew over widgets in front (the tablet's pull-down shade).
- **Transparent kinds:** labels and images without a bg count as `shows_behind` (readback backends restore behind them first): an alpha icon swapped with `set(image_id)` used to keep the old icon's pixels.
- **Incremental gauge redraw (dirty mode):** a gauge that is itself the dirty root (no ancestor repainted over it) and whose pixels still show its last full drawing redraws only the boxes its value change touches (`GaugeGeom::change_boxes`: both needles + hub, the arc span in between): the same `draw_widget` call with the clip `restrict`ed to those boxes, so the result is pixel-identical to a full redraw (tests: `gauge_update_matches_full_draw_*`). "Still shows" = `SEC_DRAWN` holds the same appearance key (`gauge_key`: rect, colours, border, flags, backdrop…) and the current `ObjPool::draw_epoch`. The epoch is bumped by anything that may draw over widgets outside the renderer: VM drawing builtins (`fillRect`, `line`, `drawText`, `beginFrame`, `fpgaCmd`…), the pass-1 erase, `redraw_behind`, keyboard dismiss, every buffered frame; partial redraws (`redraw_behind_region`) forget the widget's record. Gauges draw through the scissor (they used to ignore the clip region) and keep ticks inside their box.
- **Board renderer (`renderer.rs`, `Platform::Renderer`):** `render::draw_widget` asks `P::Renderer::draw(ctx, id, abs, clip)` first; `false` = the built-in drawing (`render::draw_default`, public for overrides that add to it). Hooks with defaults: `draw`, `shows_behind` (`Option<bool>`, None = built-in rule), `partial_redraw` (gauge incremental redraw). Static dispatch, zero-sized; every board uses `DefaultRenderer` (GD32 firmware byte-identical) except exynos4412 (`bsp/exynos4412/src/renderer.rs`). An override must stay inside `clip` ∩ `abs`, paint opaquely where `covers_opaquely` says, and draw the same whatever the clip (contract in `renderer.rs`). Test: `renderer::tests`. The tree, DFS, clipping and restores are not part of it.
- **Whole-area compose (`WidgetRenderer::COMPOSES` / `compose`):** a renderer that sets `COMPOSES` gets each dirty render first (`render::compose_dirty`, right after `geo_begin`): the areas it changes (every dirty widget's box where it is visible or was rendered, its SEC_PREV A rect), rounded out to `COMPOSE_GRID` 16, merged until disjoint, at most `COMPOSE_AREAS` 8. It repaints them from scratch (every visible widget touching them, DFS order; geometry via the now-public `ObjPool::abs_at`/`vis_at`/`scroll_parent_at`/`sub_end_at`) or returns false (nothing drawn → the passes run). Taken: no passes; core clears SEC_PREV A, sets/clears FLAG_RENDERED, bumps `draw_epoch` (gauge records stale), clears dirty, settles. `ObjPool::overlay` = the on-screen keyboard's rect (set by `FullInput`), so a composer can leave areas under it alone. Test: `renderer::tests::composing_renderer_gets_grid_aligned_merged_areas`.
- **Three render functions:**
  - `render_all()` — full screen, DFS pre-order, no clipping (initial draw)
  - `render_dirty()` — iterative (not recursive), uses DFS cache, clipped (partial update)
  - `render_buffered()` — double-buffered full redraw: begin_frame + render_all + end_frame (flicker-free, only fires when dirty)
  - **DFS cache:** `ObjPool.dfs_cache` — not recomputed unless tree changes (alloc/add_child/clear invalidate it)
- **Render mode** (per-program, stored in image header v3):
  - `dirty` (default): partial update, direct front buffer writes
  - `buffered`: double-buffered full redraw every dirty frame, no tearing
  - Set via `"render_mode": "dirty"|"buffered"` in project.json
  - In buffered mode: OP_BEGIN_FRAME/OP_END_FRAME are no-ops (framework handles buffer management)

### Portrait Mode
- `"portrait": true` in project.json → image header flags bit 3 → `runtime::apply_orientation` (after load) sets `LcdImpl::set_portrait` and sizes the root widget to the logical screen. Off by default; only one rotation exists.
- Logical screen = `HEIGHT`×`WIDTH` (480×800 on Nextion), mounted with the panel's **right edge at the top**: logical `(x, y)` → panel `(WIDTH-1-y, x)`. `lcd.width()`/`height()` give the logical size; use them, not `LcdBackend::WIDTH/HEIGHT`, for screen bounds.
- Done entirely in `LcdImpl` (all backends, sim included): rects/pixels are remapped + clipped to the logical screen; a `begin_pixels` stream opens one 1 px wide panel column window per logical row (`RotStream`), ~10 extra bus writes per streamed row. CMD 0x0D is not used — column-major writes give a transpose (mirror), not a rotation.
- Touch: `lcd.touch_to_logical` in both `InputHandler`s. Raw `fpgaCmd`/`fpgaData` and boot-time screens (recovery, SD boot) stay in panel coordinates.
- On-screen keyboard: the layout tables are an 800 px design, scaled to the screen width (`Keyboard::key_span`) and docked at the bottom (`Keyboard::top`); `focus_input` calls `set_screen(lcd.width(), lcd.height())`.
- Designer: "Portrait (rotate 90°)" checkbox in the Resources panel → `DesignerModel.set_portrait` swaps the canvas to the device screen rotated; saved as `"portrait"` in the `.fui`, exported as `"portrait": true` + the rotated `"screen"` in project.json. `ferrite_build.py` checks a portrait project's screen against the rotated device screen.
- `lib/modal.fl` sizes the backdrop from the root widget (`target(0); get(size_w/size_h)`) and centres the 400×200 box, so `showAlert`/`showConfirmation` work in both orientations. App `.fl` code should do the same instead of hard-coding 800×480.

### Render
- **Anti-aliasing (`lcd/smooth.rs`, adapted from TFT_eSPI smooth graphics, FreeBSD notice in the file + THIRD_PARTY_NOTICES.md):** circles/ellipses/rings/arcs (half-pixel ring rasterizer, `sqrt_fraction` coverage), rounded rects (fill, gradient fill + border ring), lines/polylines/paths (wedge-line distance coverage, round caps, tapered `draw_wedge`), polygon fills (exact-area signed-accumulation scanline, even-odd, one screen-width f32 row buffer on the heap). Interiors stay `fill_rect` spans. Selected inside the public `LcdImpl` primitives (`lcd.smooth()`).
  - **Readback backends** (`LcdBackend::HAS_ALPHA`: n32903, tdo_y13, sim): everything above; edge pixels → `blend_pixel`.
  - **Write-only backends** (`LcdBackend::AA_UNDERLAY`: nextion): integer subset only — circles, rings, arcs, rounded rects (not gradient corners), lines/paths. Edge pixels are mixed against `Underlay` colours (`outer` / ring-hole `inner` / arc-end `sweep`) and written opaque. The renderer sets the underlay per widget in `draw_widget` (backdrop = nearest ancestor that paints a bg; contents use the widget's own bg) and clears it after; with no underlay (VM `on_paint` drawing) these backends draw aliased. Shapes over images/gradients get a faint halo of the assumed colour. Ellipses, polygon fills and filled-polygon outlines stay aliased. ~+8 KB GD32 flash. Sim: `--features no-readback` renders like this.
  - Don't stack 1 px primitives for thickness (overlapping soft edges double-blend / overwrite): use `fill_arc`, `stroke_rounded_rect`, `draw_line_thick`, `draw_wedge`, `draw_oval(x, y, w, h, t)` (box-inscribed). Multi-segment lines go through `draw_polyline` or `lcd.pen()` (streaming, for graphs): each pixel shared by nearby segments is drawn once, by the segment covering it most.
  - Rounded (`border_radius`) and freeform shape widgets count as `shows_behind` on readback backends, so dirty redraws restore what is behind first — otherwise edge pixels blend over their previous output and accumulate. (Underlay writes are opaque, so write-only backends don't need this.)
  - Polygon fills (`smooth_polygon`) skip rows outside the scissor and columns right of it, and evaluate no coverage left of it (still accumulated); `fabsf % 2` only for |sum| ≥ 2 (a libm call). Per row only the accumulator entries an edge touched are visited and cleared (`accumulate_edge` reports the range); between them the coverage is reused, past them it is a run (`polygon_matches_full_width_reference` checks it against evaluating every column). A thin diagonal clock hand used to cost its whole bounding box per row. A restore repainting a clock hand through small clip rects rasterized the whole hand each time (exynos Clock app: one ~40 ms pass a second). Test: `scissored_polygon_matches_the_full_draw`. Strokes (`smooth_stroke`: thick lines, wedges, polylines/outlines) likewise only visit the scissor's rows and columns (`scissored_strokes_match_the_full_draw`). The tablet Clock app's hands are polygon widgets sized to their own bounding box and only touched when their rounded points change (a face-sized box made every tick restore the whole face).
  - Rounded rects partly off-screen (`fill_rounded_rect_at`, `fill_gradient_rounded_rect_at`, signed coords) keep their shape on `HAS_ALPHA` backends: they used to be cut to the screen first and rounded at the screen edge (a pixel shift would carry those corners along). Write-only backends still clip first.
- **Row blends (`LcdBackend::blend_span` / `blend_row`):** a row of one colour with per-pixel coverage (2 bpp text: `font::draw_2bpp_blend` → `blend_coverage`, one call per glyph row, pieces of 64 px) and a row with a colour per pixel (alpha images: `image::PixelCursor` buffers runs of 32 and must be `flush`ed when a decode stops mid-row). `LcdImpl` takes signed coordinates and clips once to the scissor + logical screen (portrait: per pixel through `blend_pixel`). The default loops over `blend_pixel`; exynos4412 (one `fence`/`timed!`/`mark` per row instead of per pixel) and the sim override it. Tablet page slide: ~37–41 K → ~11–12.5 K primitive calls per frame, pixel-identical. Test: `lcd::tests::blend_span_and_row_match_per_pixel_blends`. AA shape edges still blend per pixel (short runs).
- `fill_rect` → `set_address` + burst pixel write (hardware speed on FPGA)
- No frame buffer in CPU RAM — writes directly to FPGA
- **Dirty mode:** partial update — only dirty widgets are redrawn (front buffer)
- **Buffered mode:** full redraw to back buffer, FPGA atomic swap (no tearing)

### Runtime (`runtime.rs`)
- `run::<P>(ctx, touch)` = `Runtime::<P>::new(ctx, touch)` (startup: ROM font, `P::boot`, mount, syscfg, root, `main`, first frame) then `step()` forever. One `step` = one main-loop pass (VM burst, USART protocol, input, animations, audio pump, render, callbacks, `P::frame_end`).
- **Frame pacing (`PlatformRuntime::render_due`, dirty mode):** the render phase is skipped while the BSP says a render would not reach the screen sooner (default: always due). exynos4412 (`lcd::render_due`): not due while something drawn since the last present waits for a present still in flight (COPY/FLIP), so one render per present overlaps the copy and the next waits; `frame_end`'s `flush` still runs every pass and moves FLIP → IDLE. Animations tick every pass: the pull-down shade used to be re-rendered (and shifted by 1–2 px, through the scratch surface) ~7 times per present.
- A BSP with its own scheduler calls `Runtime::new` + `step` from a task instead (exynos4412: an Embassy task on CPU0). The VM and render stay in the same pass on the same core.

### Bytecode Interpreter (VM)
- **Protobuf tag encoding:** `tag = (opcode << 3) | wire_type`
- **Wire types:** 0=varint, 1=i16 fixed (2B LE), 2=LEN (varint len + payload), 5=no-arg
- **ZigZag varint:** signed integer encoding (protobuf compatible)
- **37 opcodes:** stack ops (PUSH/POP/DUP/SWAP), arithmetic (ADD/SUB/MUL/DIV/MOD/NEG), comparison (EQ/NE/LT/LE/GT/GE), logic (AND/OR/NOT), control (JMP/JZ/JNZ/CALL/RET/YIELD/HALT), widget (W_TARGET/W_SET/W_GET/W_DIRTY/W_RENDER/W_ALLOC/W_PARENT), flash (F_READ/F_WRITE)
- **Opcode 0–15:** 1-byte tag (frequent), **16+:** 2-byte tag (rare)
- **W_ALLTAR opcode (0x1C):** combined alloc + store + target (saves 5 bytes per widget)
- **Vm struct:** eval stack (32-deep), vars (sparse Vec, max 256), call stack (16-deep). Arrays are `T_ARRAY` objects in `ctx.objs` (array id = pointer); pool full → fault 17 (`ARR_ALLOC`)
- **Property R/W:** scalar (W_SET wt=0, single value from stack) and compound (W_SET wt=2, LEN payload with multiple zigzag varints)
- **Builder:** builds bytecode in RAM, forward jump patching, writes to `&mut [u8]` buffer
- **Execution:** `vm.run(&code[..len], &mut tree, &mut lcd, &flash)` — F_READ/F_WRITE operate via flash
- **Callbacks (interrupt-style frames):** `enqueue_callback` queues an event; `drain_callbacks` enters it on the VM's own stacks like `OP_CALL`, saving only registers in a `CbFrame` (pc, sp, call depth, target, state, wait timer, frame). Args go on the eval stack above `sp_floor`, and the callback cannot pop below it. `OP_RET` at the entry call depth (or `OP_HALT`) leaves the callback and restores the interrupted code, discarding the callback's leftover stack values. There are no heap copies.
  - Callbacks preempt the main program anywhere outside a critical section.
  - `delay`/`yield`/`showModal` inside a callback suspend it; `Vm::tick` continues it on a later iteration. Later callbacks queue behind a suspended one (FIFO), except while it awaits a modal, so the dialog's button callbacks can run on top of it.
  - `call_now` runs setup / on_program_start / on_paint synchronously: `delay` busy-waits and `yield` is a no-op there.
  - Nesting is capped by `CB_NEST_MAX` (= `MODAL_STACK_MAX + 2`).
  - A callback shares the eval/call stack depth with the code it interrupted. Raise `eval_stack`/`call_stack` in `"memory"` if handlers run deep.
  - A fault inside a callback or on_paint shows the Code 6 screen.
- **`fast_vm` feature (exynos4412):** dense `[i32; 512]` variable table (`VAR_SLOTS`, a slot is always < 383) instead of the sparse `Vec<VmVar>` search, RAM code read inline (`read_u8` → `read_u8_slow` for flash code / out of range), `push`/`pop`/`pop2_raw`/`read_u16` inlined, and `tick` runs its burst with the instruction body (`exec_one`) inlined, so `step`'s big prologue is paid per burst, not per instruction. Same semantics; off = the GD32 code is unchanged (its `inline(never)` choices save flash).
- **Superinstructions (`superops` feature, `CAP_SUPEROPS` 1<<4, `caps.superops`; exynos4412 + sim):** opcodes 0x45–0x4D fuse compare-and-branch (`JCMP`, `JCMP_VI` var vs i16, `JCMP_VV`), binary op with an immediate / variable operand (`BIN_I`, `BIN_I32`, `BIN_V`), `ADD_VI` (`x++`, `x += k`), `ARR_LOAD_V`, `STORE_OP` (`x = x op e`, e side-effect free). Sub-op byte = the fused opcode (`vm::int_binop`/`int_cmp`). `ferrite_build` passes the device's `caps.superops` to `build_image(superops=)` (`CodeGen._gen_cond_jump`, `_gen_super_binop`, `_add_imm_target`, `_gen_store_op`); with it, `while`/`for` are also inverted (test at the bottom). Integer constant folding (`CodeGen._const_int`/`_fold`) runs for every device. Bench mode 0 loop: 17 → 6 instructions per iteration. `fast_vm` fetches an instruction's operand bytes with one bounds check (`Vm::fetch::<N>`, also `read_u16`/`read_i32`): +6% on the device. Host A/B: `cargo test --release -p ferrite-core --features host,mock,fast_vm,wide_code,superops --target x86_64-pc-windows-msvc vm_dispatch_bench -- --ignored --nocapture` (an opcode fetch fast path without the state check measured 10% slower there, so it isn't used). Tests: `vm::tests::superops_match_plain_sequences` (each fused op vs its plain sequence); end to end, a program built with and without superops gave identical results in the sim. Opcodes in `vm.rs` and `ferrite_cc.py` (`Op`, `OP_NAMES`, `superop_size`, disassembler) and `docs/bytecode.md` — keep in sync. DIV/MOD now wrap `i32::MIN / -1` instead of panicking.
- **Fault codes:** every `VmState::Error` goes through `Vm::fail(fault::X)` — `vm.fault()` (u8, `vm::fault::*`) + `vm.fault_pc()` (start of the faulting instruction; first fault wins). Shown on the Code 6 error screen as `VM n: <name>` + PC, and sent over USART as a 4-byte error payload `[code, fault, pc_lo, pc_hi]` (`ferrite_cli.py` `VM_FAULT_NAMES` — keep in sync)
- **Control flow:** if/while/for — via JZ/JNZ/JMP combinations
- **Program switching:** an image holds any number of `RES_PROGRAM`s (`"programs"` in project.json); `main` starts at boot. `runProgram(name [, arg])` (`OP_RUN_PROGRAM` 0xBD → 0 if missing) sets `vm.next_program` and `VmState::Exit`: nothing of the program runs after it (callbacks, modals, the interrupted main code: `can_take_callback` is false, every run loop stops on a non-`Running` state; `start_program` returns early if setup/on_program_start exited). The main loop then calls `runtime::switch_program`: objs + images cleared, VM reset, sound stopped, input reset, new root, `load_program` (program + its same-named `RES_PAGE`), `start_program`. `programArg()` (0xBE) = the `arg` (`vm.program`, 0 at boot), `programName(i)` (0xBF, `Fs::nth_name_by_kind`) lists programs, `-1` = own name. Only the int arg, EEPROM and files survive a switch; syscfg is per image. Example: `examples/exynos4412/launcher`.
- **Tasks (`multi_program` feature, `CAP_MULTI_PROGRAM` 1<<3, `caps.multi_program`; exynos4412 only):** several programs run at once on one widget tree.
  - `Ctx::tasks` (`task::TaskTable`: `MAX_TASKS` 16 slots Free/Starting/Running with parent + `ProgramRef`, `fg`, `kills` mask, `mail` queue `MAIL_MAX` 64). Task 0 = main, `TASK_SYSTEM` 0xFF = runtime-owned objects (fault dialog). The runtime's `Tasks` holds main's `Vm` + one `Vm::sibling()` per child; each gets a burst per pass.
  - Ownership: `ObjPool.owner`/`handler` tables stamped from `objs.cur_task` (alloc; on_click/on_tap/on_paint set → `take_handler`). Widget callbacks run in the handler task, global input events (`on_touch_*`, swipe, long press) in the foreground task. Any task may write any widget. `str_sweep`/interning are scoped to the running task (`mine`); keyboard input strings are `adopt`ed by the widget's owner.
  - Opcodes 0xD4–0xDA: `spawnProgram(name [, arg])` → id / -1, `killProgram(id)` (self → Exit, no next program), `taskId`, `taskAlive`, `sendMessage(id, a [, b])` → `on_task_message(from, a, b)` (FunctionKind 15), `setForeground`, `foregroundTask`. Other builds: bad opcode; `ferrite_build` rejects them without `caps.multi_program` (`TASK_BUILTINS`).
  - `runtime::service_tasks` (per pass): faults, exits, child `runProgram` (= `restart_task`, same slot), kills, starts, mail. `end_task` → `ObjPool::free_task` (destroys the task's widget subtrees; strings/graph arrays still shown by survivors go to their owners), then the parent's (or main's, if the parent is gone) `on_task_exit(task, code)` (FunctionKind 16; 0 end/kill, >0 fault, -1 start failed). A fault with no handler → `send_vm_error` + the system-owned "Program stopped" dialog. Grandchildren survive their parent. Main's `runProgram`, ProgramReady and FsReady drop every child (`drop_children`). A fault in main is still Code 6. Render mode comes from main.
  - Tests: `runtime::multi_tests`, `task::tests` (`--features host,mock,wide_obj,multi_program`). Example: `examples/exynos4412/multitask`.
- **Widget page (RES_PAGE):** `"widget_page": true` in project.json (top level = default, per-program overrides) → `ferrite_lang.build_image(widget_page=True)` moves static widget setup (runs of `W_ALLTAR`/target/parent/constant `set`/constant stores in `setup()` and functions only `setup()` reaches) into a `RES_PAGE` resource named like the program. Each run becomes `OP_W_LOAD` (0x44) + u16 block offset; `Vm::load_page_block` reads the block from flash and replays the same ops (`PG_*` in vm.rs = `PAGE_*` in ferrite_lang.py, keep in sync) through the shared `w_alltar`/`w_parent`/`w_set`/`w_set_len`, so ids and tree order are unchanged. `runtime::load_main` attaches it via `Fs::find_kind(b"main", RES_PAGE)` → `vm.set_page`. Bad/missing page → fault 15. Format in `docs/bytecode.md`.
- **Image header v3:** version(u8=3) + func_count(u16) + global_count(u16) + flags(u16) + func_table
  - flags bit 0: render_mode (0=dirty, 1=buffered)
  - flags bit 2: wide_addr — JMP/JZ/JNZ/CALL targets are u32 instead of u16. The compiler sets it automatically only when bytecode exceeds 64 KB (smaller images are byte-identical to before).
  - flags bit 3: portrait — `"portrait": true` in project.json (see Portrait mode)
  - Backward compatible: v1/v2 images parsed with 5-byte header, v3 with 7-byte header
- **Code addresses (`vm::CodeAddr`):** `u16` by default; `u32` with the ferrite-core `wide_code` feature (enabled by tdo_y13, n32903, sim). Widens pc, call frames, callback queue, snapshots. A narrow build (nextion, epaper) rejects wide images and >64 KB code at load → Code 6. VM error USART payload carries a u32 PC (6 bytes) once PC > 0xFFFF.

### Object Pool (`pool.rs`)
- **One allocator for widgets, strings and VM arrays:** `ObjPool` = word arena (`Box<[u32]>`) + pointer table (`Box<[Off]>`, `Off` = u16 word offset, u32 with the `pool_large` feature). An object id (`ObjPtr`, u16) indexes the table; the table gives the block's arena offset, so ids stay valid while blocks move.
- **Block:** u32 header = owning pointer (bits 0–13) | type (14–15: `T_FREE`/`T_WIDGET`/`T_STRING`/`T_ARRAY`) | payload words (16–31), then the payload. Max 0x3FFF objects, 0xFFFF payload words each.
- **`wide_obj` feature (implies `pool_large`; exynos4412 only):** `ObjPtr`/`WidgetId` = u32, live blocks get two header words (type | words, then ptr; `HDR` = 2, holes stay one word), `MAX_OBJECTS` 0x7FFF_FFFE, `SecText.text_id` u32. Code that sizes blocks must use `HDR` / `size_of::<Widget>()`, not 1 / 24. The VM/bytecode still carry ids as i32 (`WidgetId::from_vm`/`to_vm`).
- **Allocation:** bump at `top`. Freeing a block marks it `T_FREE` (garbage) or lowers `top` if it is the last one. When a request doesn't fit, `compact()` slides live blocks down and rewrites their table entries via the header pointer. `resize_obj` grows in place if the block is last, else moves it.
- **Pointers:** freed pointers are reused FIFO (free list threaded through the table, top-bit tag), so a stale id isn't handed out again right away. Typed access (`payload(p, ty)`) returns None for a free or wrong-type pointer.
- **Lifetimes are manual:** `destroy` frees a widget subtree, `strFree`/`strClear` (sweep: frees every string no live widget shows), `arrFree`. No tracing GC.
- **Strings:** payload word 0 = length (bits 0–29) | `KEEP` sweep mark (bit 30) | interned (bit 31), then the bytes. Interned literals are shared: `str_free` ignores them, byte edits copy first.
- **Sizing:** syscfg `arena_bytes` / `max_objects` (0 = default: 8 KB / 320, `pool_large` 2 MB / 8192). `pool_large` is enabled by n32903, tdo_y13 and exynos4412 (16 MB / 256 K objects, `wide_obj`).

### External Flash (W25Q256, 32MB)
- **Pin assignment:** PA4=CS, PA5=CLK, PA6=MISO, PA7=MOSI (bit-bang SPI)
- **4-byte address mode:** activated with 0xB7 at init (full 32MB access)
- **API:** `read(addr, buf)`, `write(addr, data)`, `erase_sector(addr)`, `read_id()`
- `write()` automatically splits across page boundaries (256B page program)
- `erase_sector()` and `page_program()` busy-wait until complete

### Flash Filesystem (Fs)
- **Simple TOC structure** — access resources by name
- **Layout:**
  - `0x000000 - 0x000FFF`: Reserved (4KB = 1 sector, erase guard)
  - `0x001000 - 0x00100F`: Header (16B: magic "FERR" + version + screen W/H + resource count + checksum)
  - `0x001010 - 0x001FFF`: Resource Table (max 127 entries × 32B)
  - `0x002000+`: Resource data (packed)
- **Entry format (32B):** name[16] + kind(1) + pad(3) + offset(4) + size(4) + reserved(4)
- **Resource types:** Font=0, Image=1, Program=2, Page=3, File=4, SysCfg=5, Sound=6, Mesh=7
- **API:** `mount()`, `find(name)`, `read_resource()`, `count_by_kind()`, `find_nth_by_kind()`, `verify()`
- **Checksum (FS v3):** CRC-32 (IEEE = `zlib.crc32`, nibble table) of header bytes 0..12 + table + data up to the furthest resource end — the whole image except the checksum field. `mount` still accepts v2 (table byte sum); older firmware rejects v3 images (BadVersion), so update firmware before uploading images built by the new tools.
  - Checked after every `writefs` (before the pong; a bad image → error reply 9 + Code 9 screen, no reboot, so the host can resend) and after SD `PROGRAM.BIN` flashing.
  - At boot only where `Platform::VERIFY_FS_ON_BOOT`: n32903, sim (RAM-shadowed, a few ms), tdo_y13 (50 MHz SPI NOR, ~0.2 s per MB). Off on Nextion: the bit-banged SPI reads ~125 KB/s, ~8 s per MB.
  - `ferrite_build.py` writes it (`fs_checksum`), `--disasm` and `ferrite_analyze.py` check it.
- **RAM cost:** 12 bytes (header cache only — table stays in flash)

### System Config (RES_SYSCFG)
- **Purpose:** Per-image sizing of the runtime pools, applied once at boot right after `Fs::mount` (FS writes reboot, so pools never resize under a running program). No heap-size field — the heap is linker-sized (`__sheap`..`_heap_end`, `_stack_size` in `memory.x`), and every configurable pool is allocated from it.
- **Embedding:** `project.json` → `"memory": {"arena_bytes", "max_objects", "uart_rx_bytes", "eval_stack", "call_stack", "cb_queue", "anim_slots"}` → 32-byte entry named `"sys"`. Omitted key / 0 = firmware default; no `"memory"` block = no entry = all defaults.
- **Record (LE):** magic `"SC"` u16, version u8 (=2; v1 records are rejected → Code 8), rsvd u8, arena_bytes u32, max_objects low u16, uart_rx_bytes u16, max_objects high u16 (bytes 12–13, 0 on old images), 2 B reserved, eval_stack u8, call_stack u8, cb_queue u8, anim_slots u8, 12 B reserved. Layout duplicated in `syscfg.rs` and `tools/ferrite_build.py` (`SYSCFG_FIELDS`) — keep in sync.
- **Apply (`runtime::apply_syscfg`):** `usart.set_rx_capacity` (`UsartBackend` hook; nextion: heap ring, power of two 16 B–32 KB; other BSPs ignore it), `objs.configure(arena_bytes, max_objects)`, `vm.configure` (eval/call ≤ 255, queue ≥ 2; snapshots save only the live stack prefix).
- **Designer:** Resources panel → "Memory (system config)" group, one spin box per key (0 shown as "default"). Stored as `"memory"` in the `.fui`; exported to project.json only for non-zero keys. Field list comes from `ferrite_build.SYSCFG_FIELDS` (labels: `MEMORY_LABELS` in `ferrite_designer.py`).
- **Errors:** malformed entry → Code 8 (invalid system config); a pool the heap can't hold → Code 7 (insufficient memory).
- **Board defaults:** `Platform::DEFAULT_ARENA_BYTES` / `DEFAULT_MAX_OBJECTS` / `DEFAULT_EVAL_STACK` / `DEFAULT_CALL_STACK` / `DEFAULT_CB_QUEUE` (0 = built-in) fill syscfg fields left at 0, and apply even without a `"sys"` entry. exynos4412: 16 MB arena, 256 K objects (`wide_obj`), eval/call 255, queue 64.

### Memory Report (MemDetail)
- `ferrite_cli.py -p COMx memdetail` → per-pool table: heap (used/total, largest free block), CPU stack (+ peak), object arena (used/size, reclaimable garbage), object pointers (live/max, heap bytes of arena + table + DFS cache), UART RX ring (waiting/size), widgets / strings / arrays (live/max objects, bytes in the arena), eval/call stack, callback queue, animations (each with the heap bytes it holds). epaper's heap row is esp-alloc over all regions (internal + PSRAM), with no largest-block figure.
- Protocol: host field 11 varint (`0x58`) → device field 6 payload (`0x32`): version u8 (=1) + count u8 + count × [id u8, used u32, capacity u32, extra u32] LE. Pool ids `protocol::mem_pool` = `MEM_POOLS` in `ferrite_cli.py` (keep in sync). capacity 0 = board doesn't report that pool (CLI prints n/a).
- Gathered in `runtime::mem_detail` from `heap_stats`/`heap_total`/`stack_info` (PlatformRuntime), `UsartBackend::rx_stats` (nextion only), `ObjPool::used_bytes/garbage_bytes/live_objects/bytes_of/heap_bytes`, `Vm::mem_stats`. VM stack depths are the current values (≈0 between iterations), not high-water marks. `meminfo` is unchanged.

### Main-Loop Profiler (`perf` feature)
- `ferrite_cli.py -p COMx perf [--interval 2] [--count N|0]` → time per `Runtime::step` phase (vm, usart, input, animations, audio, render, callbacks, tasks/misc, frame_end + its vsync wait), passes/s, max pass, VM instructions/s, time outside `step`. Each query reports the window since the previous one and resets it.
- ferrite-core feature `perf` (`perf.rs`): `PlatformRuntime::cycles` (free-running u32) / `cycle_hz` / `perf_take_wait` (default 0 = no clock), `lap!` timestamps in `step`, `Vm::instructions` counter in `Vm::step`. Without the feature nothing is compiled and the query gets no reply.
- Sub-phases (`perf::add_sub`, not part of the step sum): `DRAW` (11, time in `draw_widget`) and `PIXELS` (12, time in the BSP's pixel primitives after their fence; exynos4412 `lcd.rs` `timed!`), plus counts 0x81 draw_widget calls / 0x82 pixel calls; `G2D` (15, time in exynos `g2d_render` drawing ops incl. the wait for a present copy; also inside `PIXELS`) and `SPRITE` (16, drawing new sprites, inside `DRAW`) with counts 0x86 G2D ops / 0x87 G2D kpx / 0x88 sprites made / 0x89 sprite blits / 0x8A sprite first sightings (drawn by the CPU) / 0x8B CPU-blended kpx on the screen (BSP `perf::add_px`) → CLI "per present:" line; `PRESENT` (13, another core's present copies, parallel to the step) with counts 0x83 presents / 0x84 KiB → CLI "present copy rate". exynos4412: CPU2 enables its own PMCCNTR; mode 1 = 59 presents/s (panel 60 Hz), copy 0.64 GB/s even with the NEON+`pld` loop (`copy_row`: the uncached writes are the limit). A `pldw` next-row prefetch in `fill_rect` made fills 11% slower (DRAM shared with CPU2's copy), so it is not used. The CLI derives "render walk" (render + callbacks − draws) and "draw setup" (draws − pixels − wait).
- Protocol: host field 12 varint (`0x60`) → device field 9 payload (`0x4A`): version u8 (=1) + count u8 + cycle_hz u32 + window_us u32 + passes u32 + max_pass_us u32 + vm_instructions u64 + count × [phase id u8, us u32] LE. Phase ids `perf::phase` = `PERF_PHASES` in `ferrite_cli.py` (keep in sync). Counts 0x8C subtrees shifted / 0x8D shifted kpx (core pass 0), 0x8E G2D picture copies / 0x8F G2D move copies (exynos) → CLI second "per present:" line. Longest pass breakdown: ids `perf::MAX_PASS` 0x40 + phase id (`MAX_PASS_PHASES`: steps 1–9, display wait, draws, pixels, G2D, sprite drawing; `Perf::cur` per pass + sub-phase totals at the pass start) → CLI "longest pass (N us): …" (`PERF_MAX_BASE`). The reply must stay ≤ 255 bytes (CLI `_perf_length_ok`; now 251).
- exynos4412 (BSP feature `perf`, default on): CPU0 PMU cycle counter (PMCCNTR), rate measured against the MCT at boot (printed: `CPU0 clock N MHz`); CPU0's waits for the presenter (`lcd::fence`) → `VSYNC_WAIT` (CLI: "display wait"). Benchmark: `examples/exynos4412/bench` (VM / widgets / fillRect / idle modes, tap Next).
- exynos4412 LCD (`bsp/exynos4412/src/lcd.rs`): CPU0 draws into a cached `SHADOW`; two scan buffers A/B are written through the non-cacheable alias (`mmu::DRAM_UNCACHED`, no cache clean: 17 → 5 ms per full-screen copy). CPU2 is the presenter (`lcd::start_presenter`, no executor, `wfe`/`sev`): `flush` hands the dirty box over (`STATE` IDLE → COPY) and returns; CPU2 copies `prev ∪ req` into the back buffer, sets FLIP (CPU0 may draw again), `Fimd::show` + polls `scan_address` (switch at vsync, no tearing), then IDLE. Primitives call `fence(y0, y1)`: during COPY, CPU2 publishes its top-to-bottom row progress (`COPY_LO`/`COPY_HI`, Release after each row), so CPU0 waits only while the rows it writes are not copied yet (mode 1 bench: display wait 1.8 → 0.8 ms per pass). Busy presenter at `flush` → the box keeps growing for a later flush. Without CPU2, `flush` presents on CPU0. `flush_wait` for boot/recovery screens.
  - **G2D present (BSP feature `g2d_present`, default on):** replaces the CPU2 presenter (CPU2 is not started). `flush` (IDLE) does `mmu::clean_l1_all` + `L2c::clean_all` (the G2D reads DRAM; PL310 r3p2, so no erratum 727915) and starts the G2D on the first 32-row band (`BAND`); the G2D interrupt (`soc::G2D_IRQ`, targeted to CPU0, `lcd::g2d_isr` in `handle_irq`) raises `COPY_LO`, starts the next band, after the last `Fimd::show` → FLIP. A later `flush` sees `scan_address` switched (or `FLIP_MS` 100 ms passed) → IDLE. `wait_rows` takes band ends itself (`G2d::ack`, IRQs masked) and gives the G2D up after `G2D_POLLS` → CPU copy through the alias for good (`G2D_ON` false). Mode 1 bench: 204 (NEON on CPU2) → 212 passes/s, display wait ~500 → 0.7 µs per pass, copy 1.17 GB/s; the cost left is the cache clean in `frame_end` (~1 ms per present: the PL310 writing the shadow's dirty lines back). Not tried yet: start the L2 clean by way without waiting for it, and start the first band once it is done.
- **exynos4412 G2D drawing (BSP feature `g2d_render`, default on; not yet verified on the board, 2026-11-07):** the shadow is drawn through the non-cacheable alias (`lcd::SHADOW_PX`; the present skips the ~1 ms L1/L2 clean, a `dsb` instead), so the G2D can draw on it: fills ≥ 64 K px (`g2d_fill`) and translucent fills ≥ 2048 px (`g2d_blend_color`, a 1×1 premultiplied pixel scaled) go to the G2D. `ExynosRenderer` draws large (≥ 64×64) see-through widgets (base/button/rectangle/shapes/images that `shows_behind`) once into a premultiplied ARGB8888 sprite (`lcd::begin_sprite`/`end_sprite`: the same primitives retargeted, blends composite "over" transparent; cleaned for the G2D) and then G2D-blends it per clip rect (`lcd::blit_sprite`). Sprites are keyed by appearance (FNV of what the drawing reads, position excluded; `Image::data_addr` for images), LRU 48 MB / 96 entries. G2D ops are synchronous with IRQs masked, after any present copy (`wait_rows(0, H)`); translucent fills blend unscaled from a screen-sized scratch ARGB surface the G2D fills (kept while the colour stays). Drawing ops wait with IRQs enabled (audio refills; masked waits made the click sound loop), only `G2D_IRQ` disabled at the GIC meanwhile. A drawing op over 50 ms (`G2D_OP_MS`), a refused present band or a present timeout → `g2d_give_up` (G2D drawing off for good, shadow back to cached: CPU copies from the uncached shadow ran at 0.01–0.14 GB/s) + `g2d_shutdown` (GIC off + `G2d::power_on`); console line `ferrite: G2D <op> timed out` (the console drains by interrupt: nothing comes out while CPU0 is stuck). A G2D interrupt outside a present copy re-inits it (`g2d_step`). Sprites only on an appearance's second drawing in a later frame (`Cache::seen` keeps `lcd::frames()`: an expanding launch rect made a new sprite per frame, and a clock hand at a new angle, repainted by a restore in the same frame, got a sprite it never reused), rows padded to 16 px, 64-byte aligned. Board history: run 1 (1×1-scaled colour blend, masked waits) stalled presents and hung on the Monitor app. Not bit-identical to the CPU (≤ 3/255 per channel). Cost: CPU blends (text, AA edges) read DRAM uncached. Off: `--no-default-features --features perf,g2d_present,wifi`.
- **exynos4412 pictures and moves (`g2d_render`):** `ExynosRenderer::draw_picture`: a base/image widget without border/radius/alpha whose raw opaque RGB565 image (`Image::is_raw_opaque`) is exactly its size → `lcd::copy_rgb565` (G2D copy 565 → XRGB straight from the flash shadow: `flash::phys`, cleaned once per `flash::generation`), per clip rect. `g2d_565_matches` checks once that the G2D expands 565 like `rgb565_to_xrgb`, else the CPU draws. `ferrite_build` pads so every image's pixel data is 8-byte aligned in flash (`IMAGE_HEADER` 9). `FimdLcd::move_rects` (`g2d_move`): first straight shadow → shadow copies in bands no taller than |dy| (no wider than |dx|), furthest along first (`ferrite_core::lcd::move_bands`, test `move_bands_match_a_buffered_move`; at most `MOVE_BANDS_MAX` 64 bands, half the traffic), else through the scratch surface (invalidates its colour), else `cpu_move` through a kept heap buffer. A G2D failure after a destination was written (`Moved::Partly`) returns false without trying another way (sources may be gone; the renderer redraws). Neither verified on the board yet (2026-11-07).
- exynos4412 build: `bsp/exynos4412/.cargo/config.toml` sets `opt-level = 3` (overrides the workspace's `"s"` for builds from that directory only) and `-Ctarget-cpu=cortex-a9 -Ctarget-feature=+neon` (rustc warns `neon` is unstable; without it no NEON code is generated). d16–d31 are enabled by `startup.s` and saved on IRQ entry.

- **Sim clock:** `FERRITE_SIM_CLOCK=HH:MM[:SS]` starts the sim's RTC at that time of day (reproducing a clock-face state).
- **Sim CPU profiler (`bsp/sim/src/prof.rs`, Windows x64):** `--prof SECS[,DELAY_MS] [--prof-lines]` samples the main thread every ~1 ms (SuspendThread + RtlVirtualUnwind, dbghelp names; no admin needed), prints self/inclusive/line tops and exits. Build with `CARGO_PROFILE_RELEASE_DEBUG=true CARGO_PROFILE_RELEASE_OPT_LEVEL=3`. Most samples are the 60 fps wait; compare render entries.
- **Tablet findings (2026-11-07):** page slides are pixel-bound on re-blending the wallpaper's big translucent blob circles in restores → exynos `blend_rect` is NEON (`blend_fill`, bit-identical to `fb::blend_argb8888`): slide pixels 7–8 → 4–6 ms/present. Rounded rects skip rows outside the scissor (restore behind a small widget repainted a whole card's AA rows): monitor stress test 30 → 55 fps, draws 10.3 → 3.0 ms/present. `perf` sub-phase 14 `timer reads` (count 0x85): an MCT read is 20 ns; `systick::millis` reads only the low word (`Mct::count_low`, extended against an atomic, `resync` at boot/after standby). The `vm` phase's µs per pass are spin passes between presents, not frame cost. Open: a ~30 ms (up to 83 ms) max pass in every window drops frames — next = per-phase breakdown of the longest pass. Part of it was the profiler itself: the pass that answers a `perf` query is now left out of the max (`Perf::skip_pass`).
  - `rtcRead()` cost 16.6 ms on exynos (the MAX77686 RTC waits 16 ms for its read-buffer update) and the tablet calls it in every `loop()` (every 30–50 ms): a third of idle time and a dropped frame each call (board perf.log 2026-11-08: the longest pass was always `vm 16.6 ms`, 33 ms in the Clock app). `bsp/exynos4412/src/rtc.rs` now reads the chip once, runs the time on `systick::millis`, and re-reads every 10 min (`RESYNC_MS`) and after `set_time`.
  - Wallpaper: a pre-composited 1280×800 RGB565 picture per theme (`wall1`/`wall2`, `make_assets.py` `make_wallpaper`: gradient + rounded blobs, ordered dither that keeps exact 565 values) on the root widget instead of three translucent blob widgets; a restore behind is a G2D copy. Drawer and shade are opaque (shiftable).

### GPU Compositor (exynos4412 Mali-400, BSP feature `gpu_compose`, compiled in by default but switched off at boot (`compose::ENABLED`; `POST /api/compose?on=1` turns it on); `bsp/exynos4412/src/compose.rs`, `gpu.rs`)
- `ExynosRenderer::compose` → `compose::compose`: the Mali repaints each dirty render's areas whole, straight into the uncached LCD shadow (the GPU's target; `lcd::gpu_target`/`gpu_begin` (waits for a present copy)/`gpu_drew` (marks the present box)). Painter's order, premultiplied "over" blending; no occluders/clips/restores. Areas → full-width frames of ≤ `gpu::MAX_TILES` 512 tiles (1280 px → 96-line strips); a frame starts at the last widget whose box opaquely covers it (`covers_opaquely`).
- Pieces (`Prim`): base/button/button-group/rectangle/image boxes from `ui_round` (border strips + inside, or nine pieces with corners anti-aliased from the rounded-box distance; gradients as per-vertex colours; alpha premultiplied); raw opaque RGB565 pictures sampled in the flash shadow (`ui_flat`; a non-64-byte-aligned start is read as a wider texture from the boundary before it); model3d widgets from their buffer; everything else (labels/text, gauges, sliders, shapes, compressed/alpha images) drawn by the CPU into a premultiplied sprite (`renderer::Sprite::draw`), cached by appearance (`renderer::appearance`, now covering labels' text/font/colour/align; `appearance_complete`) on the second sighting, else drawn for this render only. Batches = runs of one program + texture → one per-frame mesh + draw (`MAX_DRAWS` 256).
- Declines (passes draw): no GPU/shaders/self-test, portrait, an area meeting `ObjPool::overlay`, a visible scroll container or `on_paint` widget in the areas, a sprite widget larger than the panel, no memory, a GPU error (printed once).
- `gpu.rs`: one shared `Gpu` (`gpu::get`, lazy; `start_offscreen` on the shadow + `Gpu::map` of the whole heap), the UI programs, and a self-test frame into a 16×16 buffer at start: ARGB / RGB565 red-blue swap (picks `TexelFormat` swaps), premultiplied blend, exact texel coordinates across a 1280-texel line (mediump fragment shader), RGB565 strides that are not a multiple of 64 bytes (`any_565_stride`). Failure → compositor off, console `ferrite: GPU compositor off: …`.
- Shaders: `bsp/exynos4412/shaders/ui_flat.{vert,frag}` (texture × colour), `ui_round.{vert,frag}` (rounded-box coverage: fill inside the border's inner edge, border between). Compiled by the HAL's `tools/shaderc` (Linux, the user runs it) into `src/compose/shaders.rs`. `ui_round` uses no `length`/`sqrt` (no complex-unit op): past a corner circle's centre the distance is `(len²/r² − 1)·r/2` with per-piece constants in the `arc` attribute (`compose::arc`: 1/r, r/2, (r/ri)², ri/2). The self-test checks two corners (r 6 and 100, with borders) against exact coverage.
- **Forward pixel kill:** shaderc's render state (compiled without blending) has `aux0` bit 0x1000 (pixel kill) on; `Draw::blend(Premultiplied)` must clear it, as Mesa does when blending. Left on, fragments under translucent draws went missing in scattered 2×2 quads, mostly in the lower rows of each 16×16 tile (black where the clear showed, icons/cards showing the wallpaper, dithered pictures showing a neighbour texel); small self-test frames didn't show it (board diffs 2026-11-08).
- Diagnostics (HTTP, `net/http.rs`): `GET /api/screen` = the shadow as a 32-bit BMP (CPU0 copies it between passes, `lcd::grab`), `POST /api/compose?on=0|1` switches the compositor at run time (`compose::ENABLED`) and repaints everything (`compose::service` in `frame_end`): same-firmware A/B screenshots and perf.
- HAL additions (`gpu/runtime.rs`): `Gpu::map` (+ MMU zap), `Gpu::set_target`, `Gpu::shaders` + `Frame::mesh` (per-frame meshes in the frame arena; stale use → `Error::StaleFrame`), `Frame::texture_at` (texels where they are, any stride/`TexelFormat`, descriptor per frame), `Draw::blend(Blend::Premultiplied)` (render state word 2 = 0x0C8B_C2D2 | colour mask, from `lima_calculate_alpha_blend`), `MAX_DRAWS` 64 → 256, `MAP_TABLES` 160 (640 MB); `mali::texture` / `TexelFormat`, `Mali::zap_mmu`; `heap::region`.
- perf: GPU frames count under sub-phase 15 (renamed "G2D/GPU drawing"; the reply has no room for another entry).
- Speed (board, 2026-11-08, page slide at 160 MHz): ~37 ms GPU per present (5.4 frames of 1280×96), 6.5 of 6.8 ms per frame in the PPs (GP 0.15 ms, resets/L2/waits 0.04 ms, CPU build 0.16 ms): shader-bound, `ui_round` (~a dozen instructions) ran over whole rounded boxes. Now only the 4 corner squares use `ui_round`; straight pieces (border bands + fill, pixel-aligned edges, same output) and solid boxes are `ui_flat` with one white texel (`solid_prim`). `gpu::raise_clock` sets BUCK4 (`vdd_g3d`) to 1.025 V and the GPU to 400 MHz (MPLL ÷ 2; Linux OPPs allow 440 MHz at 1.025 V) at GPU start; refused → 160 MHz. `/api/status` `compositor.time` sums frames/draws/pieces/kpx/GP/PP/frame/compose µs (`compose::TIMES`).
- Board results (2026-11-08, 400 MHz, after the pixel-kill fix and the flat/round split): pixel-identical to the passes except AA edges; page slide render 8.7–8.9 ms per present vs 9.1–9.5 ms for the passes; Monitor stress test 4.0 vs 2.05 ms (per GPU frame: PP 0.93, GP 0.09, other hardware 0.03, CPU build 0.17 ms, so many small areas lose). Hence off at boot. Next direction: retained layers on FIMD hardware windows (see FIMD Layers).

### Retained Layers / FIMD windows (core + sim done 2026-11-08; exynos backend 2026-11-09, not yet run on the board)
- Goal: SurfaceFlinger-style retained layers. A widget subtree marked as a layer draws into its own buffer, and the FIMD's hardware windows composite it at scan-out, so a layer that only moves or fades costs register writes, not redraws (page slides, app launch/back, scrim fades). Window 0 = the base (today's shadow + present path), window 1 = the app layer (later window 2 for the shade/drawer). No scaler: zooms still need drawing.
- HAL (`drivers/display.rs`): per-window register arrays (`vidosd` A–D at 0x40 + 16·win, `buf_start`/`buf_end` 0xA0/0xD0 + 8·win, `buf_size` 0x100 + 4·win, `wkeycon` 0x140, `vidw_alpha` 0x21C, `blendeq` 0x244, `blendcon` 0x260, `buf_start_shadow` 0x40A0 + 8·win). `Fimd::set_overlay(win 1–4, Option<&Overlay>)`: premultiplied ARGB8888 (`Overlay` address/stride/width/height, signed x/y cut to the screen through the start address and `BUF_SIZE` offset, 8-bit plane alpha via `ALPHA_MUL` + `BLENDEQ` A = ONE or ALPHA0, B = 1 − αA; the vendor kernel's 0xc1), 4-word bursts under 128 px wide, written under the window's `SHADOWCON` protect bit → applies at the next vsync. `overlay_scan_address(win)` tells when.
- Board test (`bsp/exynos4412/src/overlay_test.rs`, `wifi` feature): `POST /api/overlay?mode=off|still|move|fade|full[&x=&y=&alpha=]` puts a 256×256 test picture (frame, transparent quarter, 50% green, opaque disc, alpha ramp) or a full-screen 50% scrim on window 1; `frame_end` places it (IRQs masked: the G2D present ISR writes `SHADOWCON` too). `/api/status` `"overlay"` = mode + scanned address. Verified on the board 2026-11-08 (blend, crop, move, fade, full-screen scrim).
- **Core (ferrite-core feature `layers`, `layer.rs`; sim on, GD32 off):** `PROP_LAYER` 0x36 (`set(layer, 1..4)`, 0 = none) / `PROP_LAYER_ALPHA` 0x37 (plane alpha) on a subtree root → `ObjPool.layers` (`Layers`: `MAX_LAYERS` 4 `Slot`s root/alpha/active/size/`at`/`full`/`restore`, `scope`, `origin`). Each `render_dirty`/`render_all`: `layers_sync` decides which are active (`layer_eligible`: backend `LcdBackend::LAYERS` > n, not portrait, not in a scroll container, no `on_paint` in the subtree, keyboard overlay not over it, no later visible base widget over the root's box, a later overlapping layer has a higher number), allocates surfaces (`lcd.layer_alloc`, root size), hands dropped ones back to the tree (`mark_dirty(root)`). Then the base passes run with `Scope::Base` (active subtrees excluded through `vis_at`; activation restores `slot.restore` = what the screen last drew of it), then `layer_pass` per active slot (`Scope::Layer`, `origin` = the root's abs position, subtracted in `abs_at`/`absolute_rect`, `lcd.layer_target` retargets every primitive and makes `width()`/`height()` the surface's; full redraw on allocation, after a move with pending prev rects, after changes while hidden), then `place_layers` (`lcd.layer_place(n, x, y, alpha, visible)`). Moves (`PROP_LOC_X/Y`, `location`), `visible` and `layer_alpha` of an active root only set `layers.placed` (no dirty, no `will_move`). Buffered mode: `layers_off`. Fallback to the tree ignores `layer_alpha` (no group opacity there). `LcdBackend` hooks: `LAYERS`, `layer_alloc/free/target/clear/place`.
- **`SoftLayers`** (`layer.rs`): premultiplied ARGB software surfaces + `composite` (plane alpha "over"); the sim's `DoubleBuffer` forwards primitives to it while targeted (`move_rects` declines) and `with_shown` composites for the window. Tests `layer::tests` (tree vs layers pixel comparison within 2/255, draw-call counts: a moving/fading layer draws nothing). Found on the way: `shift_moved` carried a child that moved inside a moving panel (`shift_tests::child_moving_inside_a_moving_panel`).
- Example: `examples/exynos4412/layers` (drawer layer 1 sliding yoyo, card layer 2 fading, 160 translucent tiles, Layers on/off toggle, passes/frames line). Sim: identical look both ways except the card's fade.
- **exynos backend (BSP feature `layers`, default on; `bsp/exynos4412/src/lcd_layers.rs`):** layer n = FIMD window n + 1. One cached heap block per layer: a draw shadow + two scan buffers (premultiplied ARGB8888, the layer's size). `layer_target` makes the primitives draw like a sprite (`Target.sprite`, premultiplied `over`, no G2D, no fence) and mark the layer's own dirty box; the board renderer declines while a layer is targeted (`lcd::layer_targeted`: its G2D paths draw on the screen's shadow). `flush` at IDLE: `layers::prepare` copies each shown layer's `prev ∪ dirty` into its back scan buffer (CPU + clean per row) and returns the window changes (`Commit`: new buffer, place, alpha, shown/hidden); they go out with window 0's switch (`lcd::show`: HAL `Fimd::hold_updates(1 | mask)` → `show` + `set_overlay`s → `release_updates`, so both apply at the same vsync; G2D ISR, CPU0 present or CPU2 presenter) or at once if window 0 has nothing (`commit_now`, counts as a frame). A layer with new pixels waits until the FIMD scans its last flip (`overlay_scan_address`, or `FLIP_MS`), and window 0 waits with it. Only the on-screen part goes to the HAL (a window is ≤ 2048 px wide; the tablet pager is 2560). Freed blocks are kept until their hide has applied (`DEAD_MS`). `render_due`/`flush_wait` count pending layer work; `grab` (`/api/screen`) composites the shown windows; `overlay_test` turns itself off while layers are in use (`lcd::layers_in_use`). HAL: `show`/`set_overlay` leave a held protect bit alone.
- Tablet: the pager (2560×560) is layer 1 (`set(layer, 1)` in `main.fl`): a page slide moves the window. Sim A/B: before/after a slide identical within 1/255. The sim logs `sim: layer N WxH allocated` / `freed`.
- Next: run on the board (layers demo + tablet), `ferrite_cli perf` A/B of page slides; then a G2D layer copy if the CPU copy shows up.

### 3D Models (exynos4412 Mali-400, `bsp/exynos4412/src/model3d.rs`)
- `KIND_MODEL3D` (18; `widget.rs`, `ferrite_designer.py`, `lib/core.fl`): a box the board renderer draws; elsewhere (sim, other boards) only its background. What it shows is set by syscalls 0x60–0x64 (`lib/model3d.fl`: `model3dShape` cube/torus/sphere, `model3dTurn` yaw/pitch/roll in 0.1°, `model3dColor` RGB565, `model3dZoom` %, `model3dStat` frames / GP / PP / total µs / error / triangles); setters mark the widget dirty. The sim answers them (setters 1, stats 0). Ids duplicated in `model3d.rs`, `lib/model3d.fl`, `bsp/sim/src/platform.rs`.
- HAL `gpu::Gpu::start_offscreen(mct, Target)` (vendor change): the GPU without the FIMD, drawing into the caller's buffer (`Target` address/stride/size, 64-byte aligned, whole tiles); its MMU maps only the pool and the target. `Gpu::write_texture` rewrites a texture in place (frames are synchronous).
- Uses the shared GPU (`gpu.rs`); each model widget renders into its own ARGB8888 heap buffer (`discard_dcache_range`d, never CPU-touched; `Gpu::set_target` before its frame) covering the box rounded up to 16, ≤ `gpu::MAX_TILES` 512 tiles (else bg only, error 3); the projection is squeezed into the box's part of the frame. `ExynosRenderer::draw` → `model3d::draw` → G2D `blit_sprite` per clip rect; the compositor samples the buffer (`model3d::texture`). A frame is run only when (model, size, bg) changed. Shader: `lit` from `gpu_cube_test` (per-vertex light, colour from a 4×4 texture), copied to `model3d/shaders.rs`; new shaders need `tools/shaderc` (Linux + Mesa lima under drm-shim).
- **Meshes (`RES_MESH` 7):** `"meshes": [{"name", "source": "x.obj", "normals": "auto|smooth|flat"}]` → `ferrite_build.convert_mesh` (v/vn/f, polygons fanned, negative indices; centred on the bounding box, scaled to radius 1; auto = the file's normals, else smooth area-weighted): header `"MS"`, version 1, flags, triangle count u32, then 3 vertices a triangle of position + normal as i16 (/32767). `model3dMesh(w, "name")` (syscall 0x65, string id arg); read into GPU memory on first draw (`load_mesh`), kept (16 names, failures remembered → error 6). HAL GPU pool raised for it: `LASTING_BYTES` 8 MB (~260 K vertices), `FRAME_BYTES` 4 MB (~170 K vertices a frame), PLBU heap 1 MB, MMU `IdentityMap<8>`; exynos image + .bss now ~28 of the 32 MB below the heap (`memory.x`).
- Example: `examples/exynos4412/model3d` (cube, trefoil knot from `knot.obj` (`make_knot.py`), torus; GPU stats line). Verified on the board 2026-11-08: correct shapes, light from the upper left (NDC +y = top row of the target), recolour works; 59 fps with three 352×352 models, longest pass ~5.2 ms (3 GPU frames, ~1.2 ms GPU wait each, + ~1 ms of G2D copies).

### System Info Syscalls (`sysinfo.rs`, `lib/sysinfo.fl`)
- `ferrite_core::sysinfo::syscall(id, args, objs, info)`, called by a BSP's `syscall` for ids it doesn't handle itself: 0x40 loop passes, 0x41 presented frames, 0x42/0x43 heap used/total KB, 0x44/0x45 object arena used/total KB, 0x46 object count (arg 0 all, 1 widgets, 2 strings, 3 arrays), 0x47 CPU MHz (0 = unknown). Counters are free-running; programs take differences. exynos4412: `platform::PASSES` (per `frame_end`), `lcd::frames()` (per present), MHz from `perf::hz()`. Sim: frames = passes, heap 0. Wrappers `sysPasses()`… in `lib/sysinfo.fl` (ids duplicated, keep in sync).

### Tablet Demo (`examples/exynos4412/tablet`)
- Android-like launcher (`main`) + 8 apps (clock, calc, paint, monitor, gallery, music, notes, settings), one program each; shared look/state in `ui.fl` (state crosses `runProgram` in the int arg). `make_assets.py` generates icons (from the Visual Studio Image Library SVGs, `--icons` / `FERRITE_ICONS`, default `E:\ico\images`), fonts, pictures and sounds into `assets/` — git-ignored (icon license), regenerate before building.
- Sim: `cargo build -p bsp-sim --features tablet` (1280×800, `ANIM_SLOTS` 64 like exynos). `--swipe X0,Y0,X1,Y1,MS[,DUR]` scripts a drag (with `--tap`).
- `.fl` pitfalls found here: a generated on_click dispatcher calls **every** handler registered for a widget (ui.fl's back button uses `ui_back_custom` for apps that override it); arrays can't be passed to functions; a global may not share a name with any function's param/local (`color`, `radius` in ui.fl).
- Images: `MAX_IMAGES` 64 with `pool_large`/`host` (8 otherwise); `Image::draw` takes signed coords and clips to the scissor/screen (a picture sliding in from off-screen).

### Compiler: untyped float locals
- An unannotated `var` (type `any`) initialized from a float expression is now typed float (`CodeGen._stored_type`), locals and globals. Before, `var c = itof(n); var d = sin(a); c + d` compiled to the **int** add (any + any → int path, and `BIN_V` with superops). Checked: every other example compiles to identical bytecode.

### User Files (RES_FILE)
- **Purpose:** Arbitrary user data accessible from ferrite programs — config, level data, raw tables.
- **Embedding:** `project.json` → `"files": [{"name": "cfg", "source": "data/cfg.bin"}]`
- **VM state:** 2 open-file slots in Vm (~24B). Only 2 files can be open at once.
- **VM builtins:**
  - `fileOpen(name)` → handle `1` or `2`, or `0xFF` on error. **Caller MUST check.**
  - `fileRead(handle)` → byte `0..255`, or `-1` on EOF.
  - `fileSize(handle)` → total size in bytes.
  - `fileSeek(handle, pos)` — seek to byte position (clamped to `[0, size]`; negative → 0).
  - `fileClose(handle)` — release slot.
- **Error handling:** passing `0xFF` (or any unopened handle) to `fileRead`/`fileSize`/`fileSeek`/`fileClose` → VM enters `Error` state. Programs must guard calls.
- **Typical loop:**
  ```
  var h = fileOpen("cfg");
  if (h != 0xFF) {
      var b;
      while ((b = fileRead(h)) >= 0) {
          // process b
      }
      fileClose(h);
  }
  ```

### User GPIO Header
- **Backend:** `gpio::GpioBackend` (`PIN_COUNT`, `set_mode`, `write`, `read`) → `Platform::GpioB` → `ctx.gpio` (`GpioImpl`, range-checks pin/mode). Boards without a header use `NoGpio` (0 pins).
- **Pins 0..7, same numbers on every board:** GD32 Nextion PC2–PC9 (`bsp/nextion/src/user_gpio.rs`), N32903 GPD3, GPD2, GPB2–GPB6, GPA0 (`bsp/n32903/src/gpio`). Sim: 8 virtual pins that log to stdout. epaper: 1 input-only pin, 0 = BUTTON_1 (GPIO21, `bsp/epaper/src/user_gpio.rs`; pull-up on from boot, `pinMode` switches the IO_MUX pulls, output → fault 14); it has no touch backend (`touch::NoTouch`). tdo_y13: `NoGpio`.
- **VM builtins:** `pinMode(pin, mode)` (`OP_PIN_MODE` 0xB0), `pinWrite(pin, value)` (0xB1, non-zero = high), `pinRead(pin)` (0xB2 → 0/1). Modes: 0 input, 1 output push-pull, 2 input pull-up, 3 input pull-down (GD32 only). `lib/gpio.fl` has the `GPIO_*` constants and pin table.
- Pins keep their power-on state until `pinMode`. Writing a non-output pin is ignored (on GD32 the ODR would flip an input's pull). Unknown pin / unsupported mode → VM fault 14 (`GPIO`). No pin-change callback: poll from `loop()`.

### User EEPROM
- **Backend:** `eeprom::EepromBackend` (`SIZE`, `read`, `write`, `flush`) → `Platform::EepromB` → `ctx.eeprom` (`EepromImpl`: range checks, and it skips 16-byte chunks that already hold the data, to save write cycles and wear). Boards without one use `NoEeprom` (size 0).
- **GD32 Nextion:** 24C08 (1 KB) on the RTC's I2C bus (SCL = PC0, SDA = PC1). The bit-bang moved to `bsp/nextion/src/i2c.rs`, which is shared with `rtc/hw.rs`. The A2 pin is at VCC, so the 4 × 256 B blocks answer at 0x54–0x57 (A0/A1 at GND, unused: on the 24C08 those bits select the block). The RTC is at 0x51. `eeprom/hw.rs` splits reads at block boundaries and writes into 16-byte pages, with bounded ACK polling after each page (`POLL_MAX`, ~30 ms, so a missing chip faults instead of hanging). No ISR uses the bus.
- **N32903:** 1 KB in the last 4 KB sector (0x7F000) of the BY25Q40 SPI NOR flash (512 KB). SPI0: GPD12 = CLK, GPD13 = CS (SS0), GPD14 = DI (flash SO), GPD15 = DO (flash SI). Drivers in the HAL: `n32903_rs::spi` + `n32903_rs::spiflash`, ported from the vendor BSP (`D:\repos\N32905_NonOS_BSP-master\SPI\Src\spi.c`, `spiflash.c`). The waits are bounded, there is an added 4 KB sector erase (0x20; the vendor's `spiFlashEraseSector` is really a 64 KB 0xD8 erase), and page splitting. `bsp/n32903/src/eeprom` keeps a RAM copy: `write` only updates it, and `flush` commits. If only bits were cleared, it programs just the changed pages; otherwise it does one sector erase plus programming. Every commit is read back and compared. A dead chip or a failed commit → every access faults 16.
- **`flush`:** the runtime calls `ctx.eeprom.flush()` once per main-loop pass and before the USART resets (Restart, FsWriteComplete). All writes in one callback (e.g. the relay board's save) cost one erase. Write-through backends (24C08, sim) keep the default no-op.
- **Sim:** 1 KB `SimEeprom`, and `--eeprom <file>` loads it at start and saves it after each write. tdo_y13 / epaper: `NoEeprom`.
- **VM builtins:** `eepromSize()` (0xB3), `eepromRead(addr)` (0xB4), `eepromWrite(addr, value)` (0xB5, low byte), `eepromReadArr(addr, arr, count)` (0xB6), `eepromWriteArr(addr, arr, count)` (0xB7). Bad range, no EEPROM or a bus error → VM fault 16 (`EEPROM`). Bad array → 9, `count` > array length → 13. Opcodes are duplicated in `vm.rs` and `tools/ferrite_cc.py` (`Op`, `Builtin`, `OP_NAMES`, `_NO_ARG_OPS`), fault names in `ferrite_cli.py` `VM_FAULT_NAMES`. Keep them in sync.
- **Examples:** `examples/nextion/eeprom_demo` (boot counter + reset; `examples/n32903/eeprom_demo` builds the same source for the N32903). `examples/nextion/relay_board` saves its buttons on OK and loads them at startup (format in `main.fl`: header + 16-byte slots, checksummed, header written last).
- `strByte(s, i)` (0xB8) returns byte `i` of a string, or -1 past the end. It is the counterpart of `arrToStr`, used to save names byte by byte.

### Device Features (CAPS)
- **Pattern for optional hardware** (audio, JPEG, later video): a ferrite-core cargo feature gating a backend trait (null impl `NoAudio`/`NoJpeg` for tests), a `Platform::*B` slot, an `*Impl<B>` field in `Ctx`, the VM opcodes / runtime calls that use it, and a `Platform::CAPS` bit (`CAP_AUDIO`, `CAP_VIDEO` reserved, `CAP_JPEG`). Several boards can share one feature through their own backends.
- **ferrite-core features** — a BSP enables only what its board has; with a feature off the code is not compiled at all:
  - `audio`: `audio` module, `Platform::AudioB`, `Ctx::audio`, sound opcodes 0xB9–0xBC (else bad opcode, fault 2), the `pump` / `stop` calls in `runtime`.
  - `jpeg`: `jpeg` module, `Platform::JpegB`, `Ctx::jpeg`, the FI mode-3 paths in `image.rs` (else a mode-3 image doesn't load). `ImageList::find_or_load` takes the `&JpegImpl` argument only with it.
  - `sdcard`: `sdcard` (block-device trait) + `fat`. There is no `Platform::SdCardB` slot: the BSP owns its card driver and passes it to `sdboot`.
  - `sdboot` (implies `sdcard`): `sdboot::sd_boot_check::<P, S>`.
  - Enabled by: nextion `sdboot` (BSP feature `sdboot`, default on; `--no-default-features` drops it, −5 KB), n32903 `audio` + `jpeg` + `sdboot`, exynos4412 `audio`, sim `audio` + `jpeg`, epaper none (BSP feature `sdcard`, off: its SD driver is unused), tdo_y13 none.
  - A BSP's `Platform` impl / `Ctx` literal must match the features it enables, so don't build several BSPs in one cargo invocation (`--workspace`): feature unification would turn on `audio` for every member.
  - Tests: `cargo test -p ferrite-core --features host,mock --target x86_64-pc-windows-msvc` (features off), and again with `,audio,jpeg,sdboot` added.
  - `ferrite_build` fails a program that calls `playSound`/`stopSound`/`setVolume`/`soundPlaying` when the device lacks `caps.audio` (`AUDIO_BUILTINS`).
- `CAPS` bits are mirrored in `tools/devices/<id>.json` `caps.*` (+ the `_FALLBACK` table in `ferrite_devices.py`) — keep in sync. The build tools gate features on them.
- **JPEG (`CAP_JPEG`, `caps.jpeg`):** FI image mode 3 = FI header + baseline JPEG stream. `ferrite_img.py` always re-encodes (`"mode": "jpeg"`, `"quality"` 1–100, default 85) to baseline 4:2:0, 3 components, standard Huffman tables, alpha flattened onto black. On a device without `caps.jpeg`, `ferrite_build` warns and falls back to `"auto"`. The designer defaults `.jpg`/`.jpeg` imports to `"jpeg"` on such devices.
  - Firmware: `ImageList::find_or_load` decodes once, at image load (`Image::decode_jpeg`), into an `AlignedBuf` (32-byte aligned, whole cache lines, stride/rows rounded up to 16) on the heap; drawing (`draw`/`draw_region`, scissored) copies from it. Out of heap / bad stream → the image doesn't load. `ImageList::clear` frees the buffers. RAM = stride × rows × 2 per image (800×480 = 750 KB), so RAM-rich boards only.
  - N32903: `n32903_rs::jpeg` (HAL port of the vendor `JPEG/Src/jpegcodec.c`, polled, decode only: packet RGB565/RGB555/YUV422/ARGB8888, stride, down-scale, `probe` stream check, bounded waits, cache maintenance) ← `bsp/n32903/src/jpeg` (`HwJpeg`). Sim: `bsp/sim/src/jpeg.rs` (`jpeg-decoder` crate, accepts only what the engine accepts). Example: `examples/n32903/jpeg_demo`.
- **Audio (`CAP_AUDIO`, `caps.audio`):** `"sounds": [{"name", "source": "x.wav"}]` → `RES_SOUND` (6): header magic "SN", version 1, channels u8, rate u32, then PCM16 LE (`ferrite_build.convert_sound`: 8/16/24/32-bit WAV → 16-bit). Builtins `playSound(name [, loop])` (0xB9 → 1/0), `stopSound()` (0xBA), `setVolume(0..100)` (0xBB), `soundPlaying()` (0xBC). One sound at a time.
  - Core: `AudioImpl` holds the player; `play` opens the resource and `backend.start(rate, channels)`, `runtime::run` calls `ctx.audio.pump(&ctx.flash)` every main-loop pass (fills `free_space()` from flash, whole frames), and a one-shot sound ends when `pending()` reaches 0. `stop` on program reload / writefs.
  - N32903 speaker: the DAC feeds an 8002-type amplifier whose SHUTDOWN pin is GPB1, active high (`AMP_*` in `bsp/n32903/src/audio`; low = on, the opposite of the vendor boards' `SPK_EN`): off at boot, on from `start` to `stop`. Driving it high during playback = silence with pops.
  - N32903: `n32903_rs::spu` (port of the vendor `SPU/Src/DrvSPU.c` + `SPU.c`: DAC clock = UPLL ÷ `REG_CLKDIV1` ADO_N0/N1 = 128 × rate, DFA fixes the rounding, stereo PCM16 on channel 0, threshold/end interrupts refill the halves of a ring buffer) ← `bsp/n32903/src/audio` (`SpuAudio`: 8 KB DMA ring through the uncached alias, 32 K-sample queue filled by `write` and drained by the ISR, mono duplicated to L/R, `pending` = 0 after two silent halves). SPU open + `dac_on(1)` (~0.75 s ramp) happen on the first `start`; `stop` keeps the DAC powered. Queue indices are volatile, no fences (armv5te has none: even `compiler_fence` links `__sync_synchronize`).
  - Exynos 4412: `bsp/exynos4412/src/audio.rs` (`I2sAudio`): WM8758 on I2S0, fixed 48 kHz stereo, HAL `DmaPlayback` (pdma0, ring of two 85 ms halves) refilled from the `PDMA0_IRQ` ISR out of a 32 K-frame queue; `write` resamples linearly from the sound's rate. Codec + clocks + DMA + speaker amp come up at boot (`I2sAudio::power_on` in main.rs, ~0.5 s VMID charge; the pop is at boot), then the DMA plays silence while idle. A sound starting on an idle ring is written just ahead of the DMA read position (`prime`, ~3 ms latency instead of up to 2 halves). Speakers when no jack is in (`wm8758::jack_in`, checked at each `start`). Volume 1–100 → −48…0 dB, 0 = mute.
  - Sim: `bsp/sim/src/audio.rs` (`SimAudio`): silent, consumes at the sample rate, logs start/stop, `--audio-out out.wav` records the output. Example: `examples/n32903/audio_demo`.

### SD Card Boot
- `ferrite_core::fat` (read-only FAT16/32, generic over `SdCardBackend`, `sdcard` feature) + `ferrite_core::sdboot::sd_boot_check(ctx, sd, present)` (`sdboot` feature): `EMPTY.BIN` → recovery, `PROGRAM.BIN` → copied to `P::FS_BASE`. The BSP detects/inits the card; `present` makes the progress text visible (n32903: `lcd::flush_cache`).
- **Nextion:** SPI SD (`sdcard/hw.rs`, PC13 CS).
- **N32903:** the driver is in the HAL submodule, `n32903_rs::sd`: a Rust port of Nuvoton's N3290x SD driver (SDLoader `sd.c`), with init on any of the 3 SD ports, multi-block DMA read/write and card size from the CSD. SD cards only, no MMC. The HAL is meant to be a port of the vendor C library, so new peripheral drivers go there and the BSP only adapts them. `bsp/n32903/src/sdcard` adapts it: port 0 (GPE2–GPE7), plus a 32-byte-aligned bounce buffer because DMA needs cache-line alignment. There is no card-detect pin, so `init()` is the probe (~100 ms without a card). The SD host shares the FMI and DMAC with NAND: each op sets FMICR itself, and the FMI is never reset (that drops the NAND config). After an update, `flash::flush()` writes the shadow to NAND.

### Wi-Fi / Network (epaper)
- **No credentials in firmware:** the program passes SSID/password to `SYS_WIFI_CONNECT` (e.g. read from a `RES_FILE`); URLs, NTP server and the optional TLS CA also come from the program. Syscalls 0x10–0x15 (Wi-Fi) and 0x30–0x3D (HTTP(S), SNTP) in `bsp/epaper/src/platform.rs`. Ids, `NET_ERR_*` codes and wrappers (`wifiConnect`, `httpGet`, `httpJsonNum`, `ntpSync`…) are duplicated in `lib/epaper.fl`, so keep the two in sync. Table in `docs/ferrite-lang.md`. Example: `examples/lilygo-t5/weather`.
- **Two cores:** `esp-radio` 0.18 (matches esp-hal 1.1) and `embassy-net`/`reqwless` are async-only, so `net::task::start` (from `main`) starts `esp-rtos` (TIMG0 + SW_INTERRUPT 0/1) and runs an embassy `Executor` on **core 1** (32 KB stack). The VM super-loop on core 0 is unchanged.
- **Mailbox:** `net::SHARED` (critical-section mutex) + `net::WAKE` signal. Syscalls only write requests or read results and return at once; programs poll `SYS_WIFI_STATUS`/`SYS_HTTP_STATUS`/`SYS_NTP_STATUS`. One HTTP request + one NTP sync at a time. A finished NTP sync is written to the RTC in `frame_end` (core 0 owns the I2C bus).
- **Radio lifetime:** off until the first connect. A `session` = `esp_radio::wifi::new` + embassy-net stack (DHCP, DNS, TCP, UDP), with auto-reconnect every 5 s. `SYS_WIFI_DISCONNECT` (or new credentials) bumps `generation`, the session returns and dropping the controller powers the radio down.
- **HTTP(S):** `reqwless` + `embedded-tls` (TLS 1.3, AES-128-GCM only; `rsa` feature for RSA certs). Response ≤ 64 KB, received into PSRAM (`esp_alloc::ExternalMemory`, `allocator-api2` Vec) and kept until `SYS_HTTP_FREE`. TLS buffers 16.6 KB + 4 KB in PSRAM. No CA = encrypted but unauthenticated. `SYS_HTTPS_CA(file)` loads a DER CA from a user file (no expiry check, `NoClock`). `net/json.rs`: dotted-path JSON lookup (`SYS_HTTP_JSON`, `_NUM`), so the VM needs no JSON parser.
- **Syscall signature:** `PlatformRuntime::syscall` takes `&mut ObjPool` (all BSPs), so handlers can return new strings. `syscallStr(id, …)` in the compiler = the same `OP_SYSCALL`, typed `string`.
- **Memory/flash:** internal heap is now 64 KB `#[ram(reclaimed)]` + 96 KB, because Wi-Fi buffers must be internal. The app grew 200 KB → 1.07 MB, so `partition.csv` has `factory` = 2 MB and the `ferrite` FS partition moved **0x110000 → 0x210000** (`flash/epaper.rs`, `epaper.ps1`). Existing boards need the FS image reflashed at the new offset.
- `der` is a direct dependency only to enable its `heapless` feature (embedded-tls 0.18 needs `SequenceOf`/`SetOf`, gated since der 0.8.3).
- **DNS:** `task::resolve` asks the DHCP-offered server (5 s cap), then 1.1.1.1 and 8.8.8.8 with its own UDP A-record query (`dns_query_udp`). Used by NTP and, through the `Resolver` `embedded_nal_async::Dns` impl, by reqwless. `SYS_WIFI_DNS_STR` (0x16) shows the DHCP server.
- **Serial tools:** the T5 talks over the S3's USB-Serial-JTAG, which resets the chip on esptool-style DTR/RTS changes. `ferrite_cli.open_port` (also used by the designer) keeps both deasserted; pyserial's default open/close rebooted the board.
- ADC2 (battery) is shared with Wi-Fi: `SYS_BATTERY_VOLTAGE` may return -1 while the radio is on.

### Touch Gestures
- **Swipe (OnSwipe = 10):** fires on release when displacement ≥ 40 px and dominant axis ≥ 2× minor; args: `(direction, widget_id, dominant_delta)`
  - Directions: 0=left, 1=right, 2=up, 3=down (use `lib/touch.fl` constants `SWIPE_LEFT`…`SWIPE_DOWN`)
  - Mutually exclusive with `on_click` — a touch that becomes a swipe never fires `on_click`
- **Long press (OnLongPress = 11):** fires once after 500 ms of holding while finger stays within 20 px of press origin; args: `(widget_id, x, y)`
  - Suppresses `on_click` on the subsequent release
- **Gesture state** tracked in main loop locals: `gesture_start_x/y`, `press_ms`, `long_press_fired`
- **Constants:** `LONG_PRESS_MS = 500`, `LONG_PRESS_MAX_MOVE_SQ = 400`, `SWIPE_MIN_DIST_SQ = 1600`, `SWIPE_AXIS_RATIO = 2`
- **Language library:** `lib/touch.fl` — include in `.fl` programs to get named `SWIPE_*` constants

### Property Animation
- **Time-based tweening** of any scalar widget property via its integer `PROP_*` id. Reuses the `set_scalar_prop` → `mark_dirty` → dirty-render path — no separate render path.
- **Opcodes:** `OP_ANIMATE` (0xAD), `OP_STOP_ANIM` (0xAE), `OP_STOP_ANIM_PROP` (0xAF) — all args on stack.
- **VM builtins:**
  - `animate(widget, prop, to, durationMs, easing [, @onEnd])` — tween from the property's *current* value to `to`. Starting a tween on an already-animating property replaces it.
  - `stopAnim(widget)` / `stopAnimProp(widget, prop)` — cancel.
- **easing byte** (`ctrl`): low 4 bits = curve (`EASE_LINEAR/IN/OUT/IN_OUT`); bit4 `REPEAT_LOOP`, bit5 `REPEAT_YOYO`. OR a repeat flag onto a curve.
- **Color props** (BG/BORDER/TEXT/PRESS/GRADIENT) interpolate channel-wise via `lerp_rgb565`; others linearly.
- **onAnimEnd:** optional `@func` (a func_id) fired with `(widget, prop)` via `enqueue_callback` when a *one-shot* tween completes (never for `REPEAT_*`).
- **Tick:** `Vm::tick_animations()` runs once per main-loop iteration (in `runtime::run`, before render), independent of VM state, and only re-marks dirty when the integer value actually changes (avoids redundant redraws on the vsync-free device loop).
- **Pool size:** `Platform::ANIM_SLOTS` const — default 8 (nextion, epaper), tdo_y13 = 32. Over-cap tweens are dropped.
- **Language library:** `lib/anim.fl` — `EASE_*`, `REPEAT_*`, and animatable `PROP_*` constants. Opcode numbers are duplicated in `vm.rs`, `tools/ferrite_cc.py` (`Op`/`OP_NAMES`/`_NO_ARG_OPS`) — keep in sync.

### USART RX Resync
- The protocol has no frame markers: a stray byte that looks like a payload tag (noise, an aborted transfer) would leave `Protocol` swallowing every later byte as payload. The main loop calls `protocol.resync()` once the RX ring is drained, the parser is mid-message and no byte has arrived for `protocol::RX_RESYNC_MS` (500 ms). The timestamp is taken after `feed`, so a flash erase it triggers doesn't count as a stall.
- Nextion: PA10 (USART0 RX) is input pull-up, not floating, so an unplugged line doesn't make noise.
- Hosts: `ferrite_cli` `request()` resends once after a 0.6 s resync wait (`writefs` restarts from the header). The designer's device dialog retries ping and the writefs header, and so does its build-and-upload action.
- **Adaptive chunks (host):** `ferrite_cli` halves a failing chunk (4096 → `MIN_CRC_CHUNK` 256; the device takes any length) and tries 256 B up to `MIN_CHUNK_TRIES` 32 times, 0.2 s apart. With sequence ACKs it may halve at any time. Plain CRC mode (older firmware) never halves after a missing ACK (the device may hold it as a lost-pong duplicate): it first asks with `_probe_taken` (empty chunk numbered seq − 1 → pong only if seq isn't taken).
- **CRC chunks (writefs):** a 16-byte WriteFs header (flags bit 0 + header CRC) asks for them; new firmware ACKs with FsCaps (`3A 01 01`), older firmware with a pong (host falls back to plain chunks). Chunk = `seq u32` + `CRC-32(seq + data)` + data, held in RAM until checked; bad header/chunk → error 10 (`ERR_TRANSFER_CRC`), nothing written, host resends (`CHUNK_TRIES` = 8). A resend of the chunk just written (lost pong; same seq and same CRC) is ACKed without writing; `resync` drops a chunk cut short. A 16-byte header is CRC-checked whatever its flags. **In a transfer** (header taken, image not complete, < `TRANSFER_IDLE_MS` 10 s quiet) only header/chunk tags start a message: a damaged chunk tag/length used to run image bytes as commands (`0x18` Restart → reboot mid-transfer → host resends stopped U-Boot autoboot). **Sequence ACKs** (flags bit 1, FsCaps `3A 01 03`): every chunk is answered by FsAck (`0x42`, `next_seq` + `last_crc` + CRC-32), read by content, so stale extra replies to a damaged chunk can't push the host one reply behind (false "done", shifted image). `protocol.rs` (`FS_FLAG_CRC`, `FS_FLAG_SEQ_ACK`, `ERR_TRANSFER_CRC`, `send_fs_ack`) and `ferrite_cli.py` (`FS_FLAG_CRC`, `FS_FLAG_SEQ_ACK`, `ERR_TRANSFER_CRC`, `TAG_FSCAPS_RESP`, `TAG_FSACK_RESP`) — keep in sync. The designer still uploads with plain chunks.
- **Exynos 4412 RX on CPU1:** the BSP runs Embassy (`exynos_4412_rs` `embassy` feature, one executor per core). CPU1 runs only `usart::rx_task` (HAL async `UartRx`, UART2's interrupt targeted to CPU1) and fills a 64 KB cross-core SPSC ring. CPU0 runs `ui_task` = `runtime::Runtime::step` + `yield_now`. CPU0 may then mask IRQs for a while without losing RX bytes. `lcd::flush` polls VIDCON1 for vsync instead of the HAL's `Fimd::wait_vsync` (`wfi` with IRQs masked; under Embassy no deadline alarm, MCT G0 belongs to embassy-time): with the HAL wait the runtime went silent during writefs. If CPU1 doesn't start, the task runs on CPU0. `handle_irq` checks PDMA0 (audio) before `embassy::on_interrupt`, because the HAL's pl330 hook would take and disable it.
- **exynos4412 FS upload (`ferrite_cli.py bootflash FILE`):** the flash is a RAM shadow loaded from `FERRITE/FLASH.BIN` on the eMMC, so `writefs` only changes it until the next boot. `bootflash` saves it through the vendor loader uart_boot instead: UART with `-p` (header `EXYNFLSH` + len + CRC-32, 1 KB chunks, sent only after a uart_boot ready line, like `vendor/exynos_4412_rs/tools/uartboot.py`), or WiFi with `--host [HOST]` (`POST /flash` + `X-CRC32`, default `$EXYNOS_HOST`/`exynos`, like `httpboot.py`; give the host before the subcommand or it takes its name). An image starting with `FERR` gets `--fs-base` (0x1000) bytes of 0xFF in front. `--reset` sends the ferrite Restart first (over `-p`, also with `--host`), `--wait S` waits for a manual reset, `--app IMAGE [--save]` then sends an application image to `/boot`/`/save`; otherwise, with `-p`, uart_boot's `r` key restarts into autoboot (`--no-run` leaves it waiting).
- **exynos4412 WiFi (BSP feature `wifi`, default on; `bsp/exynos4412/src/net/`, `bridge.rs`):** the HAL's `uart_boot`/`wifi_async_test` stack as a service on **CPU3** (otherwise parked), so the UI core never waits for the network.
  - Config: `RES_FILE`s in the FS image (`"files"` in project.json, uploaded with writefs/bootflash): `wifi_cfg` (text: `ssid=`, `password=`, optional `band=2.4|5`, `hostname=`, default `exynos` like uart_boot), and, unless the firmware embeds the chip firmware (`WIFI_FIRMWARE_DIR` at build, `build.rs`), `wifi_fw` + `wifi_nvram` (the HAL's `brcmfmac4334-sdio.samsung,mehmet-board.bin/.txt`). An image carrying them makes the firmware keep a copy on CACHE (`FERRITE/WIFI.CFG`, `WIFIFW.BIN`, `WIFINV.TXT`, written only when they differ); an image without them uses that copy, so WiFi survives uploading apps that don't carry them. Last fallback: build-time `WIFI_SSID`/`WIFI_PASSWORD`/`WIFI_BAND`. Nothing found → no WiFi, no boot delay. `wifi_cfg` holds the passphrase: the running program can `fileOpen` it, and it shouldn't be committed.
  - Boot: CPU0 reads the config and does `bcm4334::power_on` (PMIC on I2C0, which CPU0 owns; ~0.5 s), then starts CPU3: polled SDIO + firmware + scan + WPA2 join (retried every 10 s), then an Embassy executor with the WiFi runner (host wake EINT retargeted to CPU3), embassy-net DHCP, `http` (3 servers) and the bridge. A lost link is not rejoined (restart).
  - **Protocol bridge (`tcp://HOST:7000`):** `ferrite_cli -p tcp://exynos …` (`open_port` → pyserial `socket://`; `BRIDGE_PORT` in `ferrite_cli.py` = `net::BRIDGE_PORT`, keep in sync) runs every command (writefs, memdetail, perf, settime…) over WiFi. `bridge::Session` (one at a time: TCP client or an HTTP API request) switches `usart::HwUart`'s RX/TX to two SPSC rings (`bridge::RX` 64 KB / `TX` 16 KB); UART2 bytes wait meanwhile. A new session makes CPU0 drop the last one's leftovers (`RX_DISCARD`) before sending. `reset()` drains TX + 200 ms so the final pong leaves.
  - **HTTP (port 80, `net/http.rs`):** `GET /` dashboard, `/api/status` (link, heap, passes/frames), `/api/mem` (memdetail as JSON), `/api/perf?ms=N` (perf over N ms; both through the bridge → 409 while a `tcp://` client is connected), `POST /flash` (or `/fs`, `PUT`; same request as uart_boot's, so `ferrite_cli bootflash --host` works against a running ferrite too), `POST /restart`. An upload goes to CPU0 through a mailbox (`net::service` in `frame_end`): an `FERR` image gets the region's current config sector in front (like writefs), the result must mount + pass its FS checksum, then it is saved straight to `FLASH.BIN` (`flash::forget_changes`, so the restart doesn't overwrite it) and the board restarts after the HTTP answer. Pool / phase names in `http.rs` mirror `MEM_POOLS`/`PERF_PHASES` — keep in sync.
- **exynos4412 power-key standby (`platform::enter_standby`):** a short press of the power key (HAL `standby::PowerButton`, alongside `PowerKeys`, which still acts on 2 s holds: off / + vol up restart / + vol down fastboot) calls HAL `standby::enter` from `frame_end`: backlight, LVDS and FIMD off, CPU at 200 MHz, CPU0 in `wfi` with IRQs masked until the next press (held 2 s while asleep = power off). Before it: `lcd::flush_wait` (no present in flight: its completion is a CPU0 interrupt) and `audio.stop` (the DMA keeps looping the ring while its CPU0 refill interrupt can't run). WiFi stays joined: while CPU3's executor runs, the MCT comparator interrupt is targeted to CPU0 + CPU3 (HAL `Gic::set_targets`), so CPU3 services embassy time and CPU0 only wakes on it (which also lets the HAL time a long press); back to CPU0 alone after. Bridge / `/flash` requests wait for the wake; UART RX (CPU1) keeps filling its ring; touch is polled, so nothing touched meanwhile is delivered.
- `ferrite_cli.py -p COMx monitor [--hex] [--no-time]` prints what the device sends with timestamps and reopens the port if it disappears; `-l` before a command (e.g. `-l writefs`) does the same after it, to watch the restart.

### Recovery Mode
- **Hold top-left corner for 3 seconds at boot** → recovery mode (nextion, n32903, 50×50 px). tdo_y13 (round panel, no reachable corners): hold the centre 120×120 px; the progress bar runs under it (`bsp/tdo_y13/src/platform.rs` `recovery_touch`/`recovery_hold`)
- Red progress bar fills to indicate progress, release to cancel
- In recovery mode: program is not loaded, only USART is active
- New program can be flashed via `writefs`
- **PENIRQ (PC14): touch is interrupt-driven** (`bsp/nextion/src/touch/hw.rs`, `IRQ_TOUCH`; `false` = old main-loop polling).
  - EXTI14 falling edge (IRQ 40 `EXTI10_15`) masks itself, then the SysTick ISR samples every 10 ms into a ring; 3 pen-up samples queue `UP` and re-arm.
  - Armed lazily on the first main-loop poll, so the boot recovery check still bit-bangs the XPT2046 directly; calibration pauses the sampler.
  - EXTI14 + SysTick run at priority 0xC0, below USART0 (a ~0.3 ms sample > one RX byte time).
  - The old EXTI attempt hung because `irq.rs`'s hand-written `__INTERRUPTS` had slot 40 = 0: an IRQ must be added there (and to `device.x`) or it jumps to address 0.
- **RAM cost:** ~18 bytes (8 × WidgetId + count + active)

### Font Renderer (Adafruit GFX compatible, sparse/UTF-8)
- **Format:** Sparse font — single combined RES_FONT flash resource:
  - `[0..2]` num_glyphs u16 LE, `[2]` y_advance u8, `[3]` font_id u8
  - `[4..4+N*2]` codepoints[] (N × u16 LE, sorted — for binary search)
  - `[4+N*2..4+N*9]` glyphs[] (N × 7B: bitmapOffset u16, w, h, xAdv, xOff i8, yOff i8)
  - `[4+N*9..]` bitmap data (1-bit packed, MSB first)
- **GfxGlyph (7B):** bitmapOffset(u16) + width + height + xAdvance + xOffset(i8) + yOffset(i8)
- **Glyph lookup:** binary search on sorted codepoints[] — O(log N), supports arbitrary Unicode BMP subsets
- **Loading:** `Font::load(fs, flash, name)` — codepoints + glyphs in RAM, bitmap stays in flash
- **Draw modes:**
  - Opaque: `begin_pixels` + stream (fast, fg+bg)
  - Transparent: only fg pixels via `fill_rect(1,1)` (slow, background preserved)
- **API:** `draw_char(ch: char)`, `draw_str(text: &[u8])` (UTF-8), `char_width()`, `char_width_cp(u16)`, `text_width()`, `line_height()`
- **UTF-8:** `font::utf8_next(bytes, pos)` decodes one BMP codepoint; returns `Some(0xFFFF)` for invalid sequences
- **Tool:** `tools/ferrite_font_converter.py` — TrueType → sparse binary; supports `-r` range spec (e.g. `32-127,0x011E-0x011F`)
- **RAM cost:** N × 9B (codepoints + glyphs) per font; bitmap read in 128B chunks

## Project Name

**ferrite-ui** — no trademark issues.
Nextion is a registered trademark of ITEAD — this project is fully independent, clean-room implementation.

## File Structure

Cargo **virtual workspace** (root `Cargo.toml` = `[workspace]` only). The framework is
generic over `P: Platform`; each board is a thin BSP crate that owns its hardware
backends + entry point and calls `ferrite_core::runtime::run::<P>()`. See `HANDOFF.md`
for the platform-abstraction design.

A single base library, **`ferrite-core`** (crate `ferrite_core`), holds the HAL backend
traits, the generic wrappers and the entire device-agnostic framework — there is no
separate framework crate.

```
ferrite-ui/                  (workspace root — virtual manifest + shared release profile)
├── Cargo.toml               — [workspace] members + [profile.release]
├── .cargo/config.toml       — default target thumbv7m (nextion) + gd32-link.x
├── gd32-{link,memory,device}.x — GD32 linker scripts (used for nextion-from-root)
├── ferrite-core/            — THE base library (crate `ferrite_core`): HAL backend
│   │                          traits (LcdBackend…) + generic wrappers (LcdImpl<B>…) +
│   │                          Platform + MockPlatform (feature "mock") + the whole
│   │                          device-agnostic framework. Features: mock / host / epaper.
│   └── src/
│       ├── lib.rs           — module tree + mock monomorphization guard (cfg "mock")
│       ├── runtime.rs       — run::<P>() loop + PlatformRuntime + InputHandler
│       │                      (FullInput / BasicInput), error screen
│       ├── ctx.rs           — Ctx<P> (all hardware-backed fields use P's backends)
│       ├── vm.rs            — bytecode interpreter   ├── render.rs — painter's algorithm
│       ├── pool.rs          — ObjPool (object pool)  ├── clip.rs   — ClipRegion
│       ├── widget.rs        — widgets on the pool
│       ├── font.rs/embedded_font.rs — sparse UTF-8 font + ROM font
│       ├── fs.rs/image.rs/strpool.rs/keyboard.rs/config.rs/proto.rs/protocol.rs/heap.rs
│       ├── fat.rs/sdboot.rs — FAT reader + shared SD boot actions
│       ├── platform.rs/mock.rs/gfx_glyph.rs
│       ├── types.rs         — Rect, Offset, Size, Edges, Color (RGB565)
│       └── {lcd,flash,rtc,backlight,sdcard,touch,usart,systick,gpio}.rs
│                            — backend trait + generic Impl<B> + cfg(mock) type alias
│                              (touch.rs also has hit_test)
└── bsp/
    ├── nextion/  — GD32F103 + NX8048K070. main.rs (clock/port bring-up), platform.rs,
    │              panic.rs, gpio.rs (FPGA bus), user_gpio.rs (header), irq.rs,
    │              <periph>/{mod.rs,hw.rs}
    ├── epaper/   — ESP32-S3 + ED047TC1. main.rs, platform.rs, panic.rs, battery.rs,
    │              lcd/{mod,epaper,display,ed047tc1,rmt,error}.rs, <periph>/{mod,epaper}.rs
    └── sim/      — host (minifb). main.rs, platform.rs (window pump via frame_end),
                   <periph>/{mod.rs,sim.rs}
```

## Memory Usage

- Object pool: arena (default 8 KB, `pool_large` 2 MB) + one table entry per object pointer (default 320 × 2 B, `pool_large` 8192 × 4 B) + DFS cache, all allocated at boot
- Widget: 4 B block header + 24 B + 8 B per used section (a label with text ≈ 36 B, a styled button ≈ 52 B; a gauge adds 12 B `SEC_DRAWN` once drawn)
- String: 4 B block header + 4 B length + bytes rounded up to 4; array: 4 B header + 4 B per element
- Clip region: 32 × 8 bytes = 256 bytes
- VM: ~150 bytes (stack + vars + call stack)
- Fs header: 12 bytes
- Font (per font): ~900 bytes (128 glyphs × 7B + meta)

## Current Status

- [x] FPGA protocol decoded (Ghidra reverse engineering)
- [x] LCD driver working (rectangle drawing tested)
- [x] Double buffer mechanism understood
- [x] Rust no_std skeleton setup
- [x] GPIO driver ported to Rust
- [x] Clip region implementation
- [x] Widget system (core: nested widgets, border, padding, dirty redraw)
- [x] XPT2046 touch driver (SPI bit-bang, Z-pressure, median filter, debounce)
- [x] Bytecode interpreter (37 opcodes, protobuf tag, varint/zigzag, property R/W, Builder)
- [x] Flash driver (W25Q256 SPI bit-bang, 4-byte addr, read/write/erase, VM integration)
- [x] Flash filesystem (TOC, resource access by name, mount/find/read)
- [x] Font rendering (Adafruit GFX compatible, header in RAM, bitmap read from flash)
- [x] Widget types (Label, Button, Progress, Slider, Checkbox, Radio)
- [x] Image format (FI: raw/rle/indexed+rle, streaming decode, Python converter)
- [x] JPEG images (FI mode 3, N32903 hardware decoder + sim; `CAP_JPEG`)
- [x] Backlight PWM (TIMER0_CH0, PA8, 10kHz, 0-100%)
- [x] USART0 RX interrupt + 128B ring buffer
- [x] Interrupt vector table (device.x + irq.rs)
- [x] Embedded font (FreeMono9pt7b, in ROM, no flash required)
- [x] USART protobuf protocol (ping/pong, execute, restart, fs write, meminfo, stackinfo)
- [x] Startup sequence (backlight → display → recovery check → font → fs → vm)
- [x] Error protocol (display + USART, 9 error codes; 9 = flash image corrupted)
- [x] RES_SYSCFG — per-image pool sizes (UART RX, object arena/pointers, VM stacks, anim slots), heap-allocated at boot
- [x] Object pool — widgets, strings and arrays in one compacting arena with stable u16 pointers
- [x] Touch event → VM callback (on_click, on_tap, on_paint, on_touch_down/up/move)
- [x] Touch gestures (swipe left/right/up/down, long press — global callbacks OnSwipe=10, OnLongPress=11)
- [x] Iterative render (flat DFS instead of recursive, DFS cache)
- [x] PENIRQ: interrupt-driven touch sampling (EXTI14 + SysTick, ring buffer)
- [x] SPI timing constants (SPI_HALF_CLK=54, ~1MHz)
- [x] Recovery mode (hold top-left corner 3s at boot → USART-only mode)
- [x] @func_name syntax (compiler callback references)
- [x] W_ALLTAR opcode (alloc+target+store combined)
- [x] Sparse variable map (Vec<VmVar>, max 256, removed 32 limit)
- [x] Compound assignments (+=, -=, *=, /=, %=) in compiler
- [x] Pre/post increment/decrement (++i, i++, --i, i--) in compiler
- [x] Ternary operator (cond ? then : else) in compiler
- [x] Buffered render mode (double-buffered full redraw, project.json selectable)
- [x] Image header v3 (flags field: render_mode)
- [x] Custom panic handler (LCD error display + USART output, replaces panic-halt)
- [ ] SD card boot (SPI0 bus sharing issue needs to be resolved)
