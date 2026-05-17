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
- **RTC:** AT8563T (I2C)

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
- **WidgetId:** `u8` index, `0xFF` = NONE sentinel
- **Split struct: Widget (18B base) + WidgetExt (32B on-demand)**
  - **Widget (base):** tree links, flags, kind, location, size, background_color, border_color, ext index
  - **WidgetExt:** margin/border/padding edges, text fields, press_color, border_radius, image_id, callbacks, value
  - Extensions allocated lazily via `ensure_ext()` — pure containers stay at 18B
  - Accessed via `WidgetTree` accessor methods: `tree.margin(id)`, `tree.press_color(id)`, etc.
- **Box model (CSS border-box style):**
  - `margin` → outer spacing (not included in size)
  - `border` → border line (included in size)
  - `padding` → inner spacing (included in size)
  - `location` → relative offset from parent content area
  - `size` → border box dimensions
- **Flags:** `VISIBLE` (0x01), `ENABLED` (0x02), `CLICKABLE` (0x04), `DIRTY` (0x08), `PRESSED` (0x10), `CHECKED` (0x20)
- **Widget types:** `KIND_BASE` (0, container), `KIND_LABEL` (1, text), `KIND_BUTTON` (2, clickable container), `KIND_PROGRESS` (3, progress bar), `KIND_SLIDER` (4, slider), `KIND_CHECKBOX` (5, checkbox), `KIND_RADIO` (6, radio button), `KIND_INPUT` (7, text input), `KIND_GAUGE` (8, gauge), `KIND_DROPDOWN` (9, dropdown)
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

### Render
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
- **Vm struct:** eval stack (16-deep), vars (sparse Vec, max 256), call stack (8-deep)
- **Property R/W:** scalar (W_SET wt=0, single value from stack) and compound (W_SET wt=2, LEN payload with multiple zigzag varints)
- **Builder:** builds bytecode in RAM, forward jump patching, writes to `&mut [u8]` buffer
- **Execution:** `vm.run(&code[..len], &mut tree, &mut lcd, &flash)` — F_READ/F_WRITE operate via flash
- **Control flow:** if/while/for — via JZ/JNZ/JMP combinations
- **Image header v3:** version(u8=3) + func_count(u16) + global_count(u16) + flags(u16) + func_table
  - flags bit 0: render_mode (0=dirty, 1=buffered)
  - Backward compatible: v1/v2 images parsed with 5-byte header, v3 with 7-byte header

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
- **Resource types:** Font=0, Image=1, Program=2, Page=3, File=4
- **API:** `mount()`, `find(name)`, `read_resource()`, `count_by_kind()`, `find_nth_by_kind()`, `verify_checksum()`
- **RAM cost:** 12 bytes (header cache only — table stays in flash)

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

### Recovery Mode
- **Hold top-left corner for 3 seconds at boot** → recovery mode
- Red progress bar fills to indicate progress, release to cancel
- In recovery mode: program is not loaded, only USART is active
- New program can be flashed via `writefs`
- **PENIRQ (PC14):** touch detection via GPIO polling (EXTI interrupt NOT USED — SPI conflict)
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

```
ferrite-ui/
├── .cargo/config.toml  — thumbv7m-none-eabi target + linker flags
├── Cargo.toml          — cortex-m, cortex-m-rt (device feature)
├── memory.x            — GD32F103RBT6 linker script (128K Flash, 20K RAM)
├── device.x            — Interrupt vector definitions (USART0)
├── build.rs            — device.x → linker search path
└── src/
    ├── main.rs         — entry point, startup sequence, USART command loop
    ├── gpio.rs         — GPIOA/B init, 16-bit data bus, clock pulse
    ├── lcd.rs          — FPGA protocol, fill_rect, begin_pixels/write_pixel
    ├── types.rs        — Rect, Offset, Size, Edges, Color (RGB565)
    ├── widget.rs       — Widget struct (7 kinds), WidgetId, WidgetTree (DFS cache)
    ├── clip.rs         — ClipRegion (32 rect pool, subtract algorithm)
    ├── flash.rs        — W25Q256 SPI flash driver (hardware SPI0, 4-byte addr)
    ├── font.rs         — Adafruit GFX bitmap font renderer (flash + embedded)
    ├── embedded_font.rs — Embedded FreeMono9pt7b font data (in ROM)
    ├── fs.rs           — Flash filesystem (TOC, resource access by name)
    ├── image.rs        — Ferrite Image (FI) format decoder (raw/rle/indexed+rle)
    ├── render.rs       — render_all + render_dirty + render_buffered (painter's algorithm)
    ├── panic.rs        — Custom panic handler (LCD error display + USART output)
    ├── touch.rs        — XPT2046 SPI bit-bang, hit test, debounce, PENIRQ GPIO, recovery
    ├── sdcard.rs       — SD card SPI driver (SPI0 shared, Mode 0)
    ├── fat.rs          — FAT16/32 filesystem reader
    ├── vm.rs           — Bytecode interpreter (57+ opcodes, sparse vars, W_ALLTAR)
    ├── backlight.rs    — LCD backlight PWM (TIMER0_CH0, PA8)
    ├── usart.rs        — USART0 serial + RX interrupt ring buffer
    ├── irq.rs          — GD32F103 interrupt vector table (__INTERRUPTS)
    └── protocol.rs     — USART protobuf protocol (ping/pong, execute, restart, fs write)
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
- [x] Widget system (core: nested widgets, border, margin, padding, dirty redraw)
- [x] XPT2046 touch driver (SPI bit-bang, Z-pressure, median filter, debounce)
- [x] Bytecode interpreter (37 opcodes, protobuf tag, varint/zigzag, property R/W, Builder)
- [x] Flash driver (W25Q256 SPI bit-bang, 4-byte addr, read/write/erase, VM integration)
- [x] Flash filesystem (TOC, resource access by name, mount/find/read)
- [x] Font rendering (Adafruit GFX compatible, header in RAM, bitmap read from flash)
- [x] Widget types (Label, Button, Progress, Slider, Checkbox, Radio)
- [x] Image format (FI: raw/rle/indexed+rle, streaming decode, Python converter)
- [x] Backlight PWM (TIMER0_CH0, PA8, 10kHz, 0-100%)
- [x] USART0 RX interrupt + 128B ring buffer
- [x] Interrupt vector table (device.x + irq.rs)
- [x] Embedded font (FreeMono9pt7b, in ROM, no flash required)
- [x] USART protobuf protocol (ping/pong, execute, restart, fs write, meminfo, stackinfo)
- [x] Startup sequence (backlight → display → recovery check → font → fs → vm)
- [x] Error protocol (display + USART, 7 error codes)
- [x] Touch event → VM callback (on_click, on_tap, on_paint, on_touch_down/up/move)
- [x] Iterative render (flat DFS instead of recursive, DFS cache)
- [x] PENIRQ GPIO polling (skip SPI when idle)
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
