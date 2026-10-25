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
- **Heap-allocated Vec** — widgets grow on demand, max 254 (WidgetId is u8)
- **Tree structure:** left-child right-sibling (parent + first_child + next_sibling)
- **WidgetId:** `u16` index, `0xFFFF` = NONE sentinel (max 1024 widgets)
- **Split struct: Widget (18B base) + WidgetExt (32B on-demand)**
  - **Widget (base):** tree links, flags, kind, location, size, background_color, border_color, ext index
  - **WidgetExt:** group_id (+ 3 reserved bytes, the former margin), border/padding edges, text fields, press_color, border_radius, image_id, callbacks, value
  - Extensions allocated lazily via `ensure_ext()` — pure containers stay at 18B
  - Accessed via `WidgetTree` accessor methods: `tree.border(id)`, `tree.press_color(id)`, etc.
- **Box model (CSS border-box style):**
  - `border` → border line (included in size)
  - `padding` → inner spacing (included in size)
  - `location` → relative offset from parent content area
  - `size` → border box dimensions
- **Flags:** `VISIBLE` (0x01), `ENABLED` (0x02), `CLICKABLE` (0x04), `DIRTY` (0x08), `PRESSED` (0x10), `CHECKED` (0x20)
- **Widget types:** `KIND_BASE` (0, container), `KIND_LABEL` (1, text), `KIND_BUTTON` (2, clickable container), `KIND_PROGRESS` (3, progress bar), `KIND_SLIDER` (4, slider), `KIND_CHECKBOX` (5, checkbox), `KIND_RADIO` (6, radio button), `KIND_INPUT` (7, text input), `KIND_GAUGE` (8, gauge), `KIND_DROPDOWN` (9, dropdown), `KIND_GRAPH` (10, spline graph), `KIND_IMAGE` (11, image)
- **Shape widgets:** `KIND_RECTANGLE` (12), `KIND_CIRCLE` (13), `KIND_POLYGON` (14), `KIND_LINE` (15), `KIND_ELLIPSE` (16) — fill = `bg_color`, stroke = `border_color`, stroke/line thickness = border width. Polygon points come from `text` as `"x,y x,y …"` (relative to the widget's top-left); Line draws the widget-box diagonal; Circle/Ellipse fill the widget box (ellipse uses the full w×h). Kind numbers are duplicated in `widget.rs` and `tools/ferrite_designer.py` — keep in sync. `lib/shapes.fl` provides named `KIND_*` constants.
- **Button Group:** `KIND_BUTTON_GROUP` (17) — exclusive toggle button. Widgets of this kind with the same `group_id` (`PROP_GROUP_ID` 0x32, `WidgetExt.group_id`, tree-wide) form one group; only one is CHECKED. A tap or `set(checked, 1)` → `WidgetTree::select_in_group`. Re-tapping the selected one keeps it selected. Selected = drawn with `press_color` (`render::shows_press`). Set `group_id` before `checked`. Kind number is in `widget.rs`, `tools/ferrite_designer.py`, `lib/core.fl`.
- **Radio groups:** `KIND_RADIO` uses the same `group_id` via `select_in_group`, with one difference: `group_id` 0 (the default) = sibling radios with group 0 (the original behaviour), ≠ 0 = tree-wide like Button Group. `set(checked, 1)` enforces the group only for ≠ 0. Radio and Button Group groups are separate even with equal ids. The designer mirrors this in `DesignerModel.enforce_exclusive_group`.
- **Margin removed:** prop ids 0x10–0x13 / 0x42 are reserved, and its 4 `WidgetExt` bytes became `group_id` + `_reserved[3]`.
- **Label:** text_color, font_id, text_align (LEFT/CENTER/RIGHT), text (from StringPool)
- **Button:** press_color (background when pressed), accepts child widgets
- **Color:** RGB565 (`background_color`, `border_color`, `text_color`, `press_color`)
- **Painter's algorithm** — z-order: DFS pre-order (lower index = behind)
- **Clip region** (inspired by ReactOS Region API)
  - Static rect pool: `MAX_CLIP_RECTS = 32`
  - Each `subtract` operation produces max 4 new rects (top/bottom/left/right strips)
  - Pool full fallback: draw entire dirty rect (overdraw, no tearing)
- **Dirty redraw flow:**
  1. `mark_dirty(id)` → marks widget + entire subtree as dirty
  2. `render_dirty()` → computes DFS order
  3. For each dirty widget, collects occluders (later in DFS, not descendants)
  4. Subtracts occluder rects from ClipRegion
  5. Widget is drawn over remaining visible rects
  6. Child widgets are drawn recursively with the same occluder list
- **Three render functions:**
  - `render_all()` — full screen, DFS pre-order, no clipping (initial draw)
  - `render_dirty()` — iterative (not recursive), uses DFS cache, clipped (partial update)
  - `render_buffered()` — double-buffered full redraw: begin_frame + render_all + end_frame (flicker-free, only fires when dirty)
  - **DFS cache:** `WidgetTree.dfs_cache` — not recomputed unless tree changes (alloc/add_child/clear invalidate it)
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
- `fill_rect` → `set_address` + burst pixel write (hardware speed on FPGA)
- No frame buffer in CPU RAM — writes directly to FPGA
- **Dirty mode:** partial update — only dirty widgets are redrawn (front buffer)
- **Buffered mode:** full redraw to back buffer, FPGA atomic swap (no tearing)

### Bytecode Interpreter (VM)
- **Protobuf tag encoding:** `tag = (opcode << 3) | wire_type`
- **Wire types:** 0=varint, 1=i16 fixed (2B LE), 2=LEN (varint len + payload), 5=no-arg
- **ZigZag varint:** signed integer encoding (protobuf compatible)
- **37 opcodes:** stack ops (PUSH/POP/DUP/SWAP), arithmetic (ADD/SUB/MUL/DIV/MOD/NEG), comparison (EQ/NE/LT/LE/GT/GE), logic (AND/OR/NOT), control (JMP/JZ/JNZ/CALL/RET/YIELD/HALT), widget (W_TARGET/W_SET/W_GET/W_DIRTY/W_RENDER/W_ALLOC/W_PARENT), flash (F_READ/F_WRITE)
- **Opcode 0–15:** 1-byte tag (frequent), **16+:** 2-byte tag (rare)
- **W_ALLTAR opcode (0x1C):** combined alloc + store + target (saves 5 bytes per widget)
- **Vm struct:** eval stack (32-deep), vars (sparse Vec, max 256), call stack (16-deep)
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
- **Fault codes:** every `VmState::Error` goes through `Vm::fail(fault::X)` — `vm.fault()` (u8, `vm::fault::*`) + `vm.fault_pc()` (start of the faulting instruction; first fault wins). Shown on the Code 6 error screen as `VM n: <name>` + PC, and sent over USART as a 4-byte error payload `[code, fault, pc_lo, pc_hi]` (`ferrite_cli.py` `VM_FAULT_NAMES` — keep in sync)
- **Control flow:** if/while/for — via JZ/JNZ/JMP combinations
- **Widget page (RES_PAGE):** `"widget_page": true` in project.json (top level = default, per-program overrides) → `ferrite_lang.build_image(widget_page=True)` moves static widget setup (runs of `W_ALLTAR`/target/parent/constant `set`/constant stores in `setup()` and functions only `setup()` reaches) into a `RES_PAGE` resource named like the program. Each run becomes `OP_W_LOAD` (0x44) + u16 block offset; `Vm::load_page_block` reads the block from flash and replays the same ops (`PG_*` in vm.rs = `PAGE_*` in ferrite_lang.py, keep in sync) through the shared `w_alltar`/`w_parent`/`w_set`/`w_set_len`, so ids and tree order are unchanged. `runtime::load_main` attaches it via `Fs::find_kind(b"main", RES_PAGE)` → `vm.set_page`. Bad/missing page → fault 15. Format in `docs/bytecode.md`.
- **Image header v3:** version(u8=3) + func_count(u16) + global_count(u16) + flags(u16) + func_table
  - flags bit 0: render_mode (0=dirty, 1=buffered)
  - flags bit 2: wide_addr — JMP/JZ/JNZ/CALL targets are u32 instead of u16. The compiler sets it automatically only when bytecode exceeds 64 KB (smaller images are byte-identical to before).
  - flags bit 3: portrait — `"portrait": true` in project.json (see Portrait mode)
  - Backward compatible: v1/v2 images parsed with 5-byte header, v3 with 7-byte header
- **Code addresses (`vm::CodeAddr`):** `u16` by default; `u32` with the ferrite-core `wide_code` feature (enabled by tdo_y13, n32903, sim). Widens pc, call frames, callback queue, snapshots. A narrow build (nextion, epaper) rejects wide images and >64 KB code at load → Code 6. VM error USART payload carries a u32 PC (6 bytes) once PC > 0xFFFF.

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
- **Resource types:** Font=0, Image=1, Program=2, Page=3, File=4, SysCfg=5
- **API:** `mount()`, `find(name)`, `read_resource()`, `count_by_kind()`, `find_nth_by_kind()`, `verify()`
- **Checksum (FS v3):** CRC-32 (IEEE = `zlib.crc32`, nibble table) of header bytes 0..12 + table + data up to the furthest resource end — the whole image except the checksum field. `mount` still accepts v2 (table byte sum); older firmware rejects v3 images (BadVersion), so update firmware before uploading images built by the new tools.
  - Checked after every `writefs` (before the pong; a bad image → error reply 9 + Code 9 screen, no reboot, so the host can resend) and after SD `PROGRAM.BIN` flashing.
  - At boot only where `Platform::VERIFY_FS_ON_BOOT` (n32903, sim: RAM-shadowed, a few ms). Off on Nextion: the bit-banged SPI reads ~125 KB/s, ~8 s per MB.
  - `ferrite_build.py` writes it (`fs_checksum`), `--disasm` and `ferrite_analyze.py` check it.
- **RAM cost:** 12 bytes (header cache only — table stays in flash)

### System Config (RES_SYSCFG)
- **Purpose:** Per-image sizing of the runtime pools, applied once at boot right after `Fs::mount` (FS writes reboot, so pools never resize under a running program). No heap-size field — the heap is linker-sized (`__sheap`..`_heap_end`, `_stack_size` in `memory.x`), and every configurable pool is allocated from it.
- **Embedding:** `project.json` → `"memory": {"strpool_bytes", "max_strings", "uart_rx_bytes", "max_widgets", "max_ext", "eval_stack", "call_stack", "cb_queue", "anim_slots"}` → 32-byte entry named `"sys"`. Omitted key / 0 = firmware default; no `"memory"` block = no entry = all defaults.
- **Record (LE):** magic `"SC"` u16, version u8 (=1), rsvd u8, strpool_bytes u32, max_strings u16, uart_rx_bytes u16, max_widgets u16, max_ext u16, eval_stack u8, call_stack u8, cb_queue u8, anim_slots u8, 12 B reserved. Layout duplicated in `syscfg.rs` and `tools/ferrite_build.py` (`SYSCFG_FIELDS`) — keep in sync.
- **Apply (`runtime::apply_syscfg`):** `usart.set_rx_capacity` (`UsartBackend` hook; nextion: heap ring, power of two 16 B–32 KB; other BSPs ignore it), `strpool.configure`, `tree.set_limits` (+ `reserve_tree` preallocates to the caps), `vm.configure` (eval/call ≤ 255, queue ≥ 2; snapshots save only the live stack prefix).
- **Designer:** Resources panel → "Memory (system config)" group, one spin box per key (0 shown as "default"). Stored as `"memory"` in the `.fui`; exported to project.json only for non-zero keys. Field list comes from `ferrite_build.SYSCFG_FIELDS` (labels: `MEMORY_LABELS` in `ferrite_designer.py`).
- **Errors:** malformed entry → Code 8 (invalid system config); a pool the heap can't hold → Code 7 (insufficient memory).

### Memory Report (MemDetail)
- `ferrite_cli.py -p COMx memdetail` → per-pool table: heap (used/total, largest free block), CPU stack (+ peak), string pool bytes + slots, UART RX ring (waiting/size), widgets + extensions (live/cap, storage bytes), eval/call stack, callback queue, animations (each with the heap bytes it holds).
- Protocol: host field 11 varint (`0x58`) → device field 6 payload (`0x32`): version u8 (=1) + count u8 + count × [id u8, used u32, capacity u32, extra u32] LE. Pool ids `protocol::mem_pool` = `MEM_POOLS` in `ferrite_cli.py` (keep in sync). capacity 0 = board doesn't report that pool (CLI prints n/a).
- Gathered in `runtime::mem_detail` from `heap_stats`/`heap_total`/`stack_info` (PlatformRuntime), `UsartBackend::rx_stats` (nextion only), `StringPool::used_bytes/used_strings/heap_bytes`, `WidgetTree::mem_stats`, `Vm::mem_stats`. VM stack depths are the current values (≈0 between iterations), not high-water marks. `meminfo` is unchanged.

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
- **Pins 0..7, same numbers on every board:** GD32 Nextion PC2–PC9 (`bsp/nextion/src/user_gpio.rs`), N32903 GPD3, GPD2, GPB2–GPB6, GPA0 (`bsp/n32903/src/gpio`). Sim: 8 virtual pins that log to stdout. epaper / tdo_y13: `NoGpio`.
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
- **Pattern for optional hardware** (audio, JPEG, later video): a backend trait in ferrite-core with a null impl (`NoAudio`, `NoJpeg`), a `Platform::*B` slot, an `*Impl<B>` field in `Ctx`, and a `Platform::CAPS` bit (`CAP_AUDIO`, `CAP_VIDEO` reserved, `CAP_JPEG`). Every board fills the slot; boards without the hardware use the null impl, which compiles away (Nextion flash unchanged in practice). Several boards can share one feature through their own backends.
- `CAPS` bits are mirrored in `tools/devices/<id>.json` `caps.*` (+ the `_FALLBACK` table in `ferrite_devices.py`) — keep in sync. The build tools gate features on them.
- **JPEG (`CAP_JPEG`, `caps.jpeg`):** FI image mode 3 = FI header + baseline JPEG stream. `ferrite_img.py` always re-encodes (`"mode": "jpeg"`, `"quality"` 1–100, default 85) to baseline 4:2:0, 3 components, standard Huffman tables, alpha flattened onto black. On a device without `caps.jpeg`, `ferrite_build` warns and falls back to `"auto"`. The designer defaults `.jpg`/`.jpeg` imports to `"jpeg"` on such devices.
  - Firmware: `ImageList::find_or_load` decodes once, at image load (`Image::decode_jpeg`), into an `AlignedBuf` (32-byte aligned, whole cache lines, stride/rows rounded up to 16) on the heap; drawing (`draw`/`draw_region`, scissored) copies from it. Out of heap / bad stream → the image doesn't load. `ImageList::clear` frees the buffers. RAM = stride × rows × 2 per image (800×480 = 750 KB), so RAM-rich boards only.
  - N32903: `n32903_rs::jpeg` (HAL port of the vendor `JPEG/Src/jpegcodec.c`, polled, decode only: packet RGB565/RGB555/YUV422/ARGB8888, stride, down-scale, `probe` stream check, bounded waits, cache maintenance) ← `bsp/n32903/src/jpeg` (`HwJpeg`). Sim: `bsp/sim/src/jpeg.rs` (`jpeg-decoder` crate, accepts only what the engine accepts). Example: `examples/n32903/jpeg_demo`.

### SD Card Boot
- `ferrite_core::fat` (read-only FAT16/32, generic over `SdCardBackend`) + `ferrite_core::sdboot::sd_boot_check(ctx, sd, present)`: `EMPTY.BIN` → recovery, `PROGRAM.BIN` → copied to `P::FS_BASE`. The BSP detects/inits the card; `present` makes the progress text visible (n32903: `lcd::flush_cache`).
- **Nextion:** SPI SD (`sdcard/hw.rs`, PC13 CS).
- **N32903:** the driver is in the HAL submodule, `n32903_rs::sd`: a Rust port of Nuvoton's N3290x SD driver (SDLoader `sd.c`), with init on any of the 3 SD ports, multi-block DMA read/write and card size from the CSD. SD cards only, no MMC. The HAL is meant to be a port of the vendor C library, so new peripheral drivers go there and the BSP only adapts them. `bsp/n32903/src/sdcard` adapts it: port 0 (GPE2–GPE7), plus a 32-byte-aligned bounce buffer because DMA needs cache-line alignment. There is no card-detect pin, so `init()` is the probe (~100 ms without a card). The SD host shares the FMI and DMAC with NAND: each op sets FMICR itself, and the FMI is never reset (that drops the NAND config). After an update, `flash::flush()` writes the shadow to NAND.

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

### Recovery Mode
- **Hold top-left corner for 3 seconds at boot** → recovery mode
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
│       ├── widget.rs        — Widget/WidgetTree      ├── clip.rs   — ClipRegion
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

- Widget base: N × 18 bytes (tree links, layout, colors)
- Widget ext: M × 32 bytes (edges, text, callbacks — only for widgets that need them)
- Clip region: 32 × 8 bytes = 256 bytes
- VM: ~150 bytes (stack + vars + call stack + array pool)
- Fs header: 12 bytes
- Font (per font): ~900 bytes (128 glyphs × 7B + meta)
- Example: 25 widgets, 22 with ext = 462B + 716B = 1.2KB (was 1.2KB at 48B/widget)

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
- [x] RES_SYSCFG — per-image pool sizes (UART RX, strpool, VM stacks, anim/widget caps), heap-allocated at boot
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
