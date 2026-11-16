# Runtime, VM and storage

Main loop, bytecode VM, callbacks, program switching, object pool, flash filesystem, system config, memory report, file layout.

## Runtime (`runtime.rs`)
- `run::<P>(ctx, touch)` = `Runtime::<P>::new(ctx, touch)` (startup: ROM font, `P::boot`, mount, syscfg, root, `main`, first frame) then `step()` forever. One `step` = one main-loop pass (VM burst, USART protocol, input, animations, audio pump, render, callbacks, `P::frame_end`).
- **Frame pacing (`PlatformRuntime::render_due`, dirty mode):** the render phase is skipped while the BSP says a render would not reach the screen sooner (default: always due).
- A BSP with its own scheduler calls `Runtime::new` + `step` from a task instead. The VM and render stay in the same pass on the same core.

## Bytecode Interpreter (VM)
- **Protobuf tag encoding:** `tag = (opcode << 3) | wire_type`
- **Wire types:** 0=varint, 1=i16 fixed (2B LE), 2=LEN (varint len + payload), 5=no-arg
- **ZigZag varint:** signed integer encoding (protobuf compatible)
- **37 opcodes:** stack ops (PUSH/POP/DUP/SWAP), arithmetic (ADD/SUB/MUL/DIV/MOD/NEG), comparison (EQ/NE/LT/LE/GT/GE), logic (AND/OR/NOT), control (JMP/JZ/JNZ/CALL/RET/YIELD/HALT), widget (W_TARGET/W_SET/W_GET/W_DIRTY/W_RENDER/W_ALLOC/W_PARENT), flash (F_READ/F_WRITE)
- **Opcode 0–15:** 1-byte tag (frequent), **16+:** 2-byte tag (rare)
- **W_ALLTAR opcode (0x1C):** combined alloc + store + target (saves 5 bytes per widget)
- **Vm struct:** eval stack (32-deep), vars (sparse Vec, max 256), call stack (16-deep). Arrays are `T_ARRAY` objects in `ctx.objs` (array id = pointer); pool full → fault 17 (`ARR_ALLOC`). `ARR_ALLOC` takes a u8 size: bigger arrays compile to `PUSH size; ARR_ALLOC_S` (0x2E), and a constant initializer over 255 elements to that + stores of its non-zero values (`ARR_INIT`'s count is a u8 too)
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
- **Program switching:** an image holds any number of `RES_PROGRAM`s (`"programs"` in project.json); `main` starts at boot. `runProgram(name [, arg])` (`OP_RUN_PROGRAM` 0xBD → 0 if missing) sets `vm.next_program` and `VmState::Exit`: nothing of the program runs after it (callbacks, modals, the interrupted main code: `can_take_callback` is false, every run loop stops on a non-`Running` state; `start_program` returns early if setup/on_program_start exited). The main loop then calls `runtime::switch_program`: objs + images cleared, VM reset, sound stopped, input reset, new root, `load_program` (program + its same-named `RES_PAGE`), `start_program`. `programArg()` (0xBE) = the `arg` (`vm.program`, 0 at boot), `programName(i)` (0xBF, `Fs::nth_name_by_kind`) lists programs, `-1` = own name. Only the int arg, EEPROM and files survive a switch; syscfg is per image.
- **Widget page (RES_PAGE):** `"widget_page": true` in project.json (top level = default, per-program overrides) → `ferrite_lang.build_image(widget_page=True)` moves static widget setup (runs of `W_ALLTAR`/target/parent/constant `set`/constant stores in `setup()` and functions only `setup()` reaches) into a `RES_PAGE` resource named like the program. Each run becomes `OP_W_LOAD` (0x44) + u16 block offset; `Vm::load_page_block` reads the block from flash and replays the same ops (`PG_*` in vm.rs = `PAGE_*` in ferrite_lang.py, keep in sync) through the shared `w_alltar`/`w_parent`/`w_set`/`w_set_len`, so ids and tree order are unchanged. `runtime::load_main` attaches it via `Fs::find_kind(b"main", RES_PAGE)` → `vm.set_page`. Bad/missing page → fault 15. Format in `docs/bytecode.md`.
- **Image header v3:** version(u8=3) + func_count(u16) + global_count(u16) + flags(u16) + func_table
  - flags bit 0: render_mode (0=dirty, 1=buffered)
  - flags bit 2: wide_addr — JMP/JZ/JNZ/CALL targets are u32 instead of u16. The compiler sets it automatically only when bytecode exceeds 64 KB (smaller images are byte-identical to before).
  - flags bit 3: portrait — `"portrait": true` in project.json (see Portrait mode)
  - Backward compatible: v1/v2 images parsed with 5-byte header, v3 with 7-byte header
- **Code addresses (`vm::CodeAddr`):** `u16` by default; `u32` with the ferrite-core `wide_code` feature (enabled by tdo_y13, n32903, sim). Widens pc, call frames, callback queue, snapshots. A narrow build (nextion, epaper) rejects wide images and >64 KB code at load → Code 6. VM error USART payload carries a u32 PC (6 bytes) once PC > 0xFFFF.

## Object Pool (`pool.rs`)
- **One allocator for widgets, strings and VM arrays:** `ObjPool` = word arena (`Box<[u32]>`) + pointer table (`Box<[Off]>`, `Off` = u16 word offset, u32 with the `pool_large` feature). An object id (`ObjPtr`, u16) indexes the table; the table gives the block's arena offset, so ids stay valid while blocks move.
- **Block:** u32 header = owning pointer (bits 0–13) | type (14–15: `T_FREE`/`T_WIDGET`/`T_STRING`/`T_ARRAY`) | payload words (16–31), then the payload. Max 0x3FFF objects, 0xFFFF payload words each (`MAX_OBJ_WORDS`).
- **Allocation:** bump at `top`. Freeing a block marks it `T_FREE` (garbage) or lowers `top` if it is the last one. When a request doesn't fit, `compact()` slides live blocks down and rewrites their table entries via the header pointer. `resize_obj` grows in place if the block is last, else moves it.
- **Pointers:** freed pointers are reused FIFO (free list threaded through the table, top-bit tag), so a stale id isn't handed out again right away. Typed access (`payload(p, ty)`) returns None for a free or wrong-type pointer.
- **Lifetimes are manual:** `destroy` frees a widget subtree, `strFree`/`strClear` (sweep: frees every string no live widget shows), `arrFree`. No tracing GC.
- **Strings:** payload word 0 = length (bits 0–29) | `KEEP` sweep mark (bit 30) | interned (bit 31), then the bytes. Interned literals are shared: `str_free` ignores them, byte edits copy first.
- **Sizing:** syscfg `arena_bytes` / `max_objects` (0 = default: 8 KB / 320, `pool_large` 2 MB / 8192). `pool_large` is enabled by n32903 and tdo_y13.

## Flash Filesystem (Fs)
- **Simple TOC structure** — access resources by name
- **Layout:**
  - `0x000000 - 0x000FFF`: Reserved (4KB = 1 sector, erase guard)
  - `0x001000 - 0x00100F`: Header (16B: magic "FERR" + version + screen W/H + resource count + checksum)
  - `0x001010 - 0x001FFF`: Resource Table (max 127 entries × 32B)
  - `0x002000+`: Resource data (packed)
- **Entry format (32B):** name[16] + kind(1) + pad(3) + offset(4) + size(4) + reserved(4)
- **Resource types:** Font=0, Image=1, Program=2, Page=3, File=4, SysCfg=5, Sound=6, Mesh=7 (ferrite-ui-xl; not read here)
- **API:** `mount()`, `find(name)`, `read_resource()`, `count_by_kind()`, `find_nth_by_kind()`, `verify()`
- **Checksum (FS v3):** CRC-32 (IEEE = `zlib.crc32`, nibble table) of header bytes 0..12 + table + data up to the furthest resource end — the whole image except the checksum field. `mount` still accepts v2 (table byte sum); older firmware rejects v3 images (BadVersion), so update firmware before uploading images built by the new tools.
  - Checked after every `writefs` (before the pong; a bad image → error reply 9 + Code 9 screen, no reboot, so the host can resend) and after SD `PROGRAM.BIN` flashing.
  - At boot only where `Platform::VERIFY_FS_ON_BOOT`: n32903, sim (RAM-shadowed, a few ms), tdo_y13 (50 MHz SPI NOR, ~0.2 s per MB). Off on Nextion: the bit-banged SPI reads ~125 KB/s, ~8 s per MB.
  - `ferrite_build.py` writes it (`fs_checksum`), `--disasm` and `ferrite_analyze.py` check it.
- **RAM cost:** 12 bytes (header cache only — table stays in flash)

## System Config (RES_SYSCFG)
- **Purpose:** Per-image sizing of the runtime pools, applied once at boot right after `Fs::mount` (FS writes reboot, so pools never resize under a running program). No heap-size field — the heap is linker-sized (`__sheap`..`_heap_end`, `_stack_size` in `memory.x`), and every configurable pool is allocated from it.
- **Embedding:** `project.json` → `"memory": {"arena_bytes", "max_objects", "uart_rx_bytes", "eval_stack", "call_stack", "cb_queue", "anim_slots"}` → 32-byte entry named `"sys"`. Omitted key / 0 = firmware default; no `"memory"` block = no entry = all defaults.
- **Record (LE):** magic `"SC"` u16, version u8 (=2; v1 records are rejected → Code 8), rsvd u8, arena_bytes u32, max_objects low u16, uart_rx_bytes u16, max_objects high u16 (bytes 12–13, 0 on old images), 2 B reserved, eval_stack u8, call_stack u8, cb_queue u8, anim_slots u8, 12 B reserved. Layout duplicated in `syscfg.rs` and `tools/ferrite_build.py` (`SYSCFG_FIELDS`) — keep in sync.
- **Apply (`runtime::apply_syscfg`):** `usart.set_rx_capacity` (`UsartBackend` hook; nextion: heap ring, power of two 16 B–32 KB; other BSPs ignore it), `objs.configure(arena_bytes, max_objects)`, `vm.configure` (eval/call ≤ 255, queue ≥ 2; snapshots save only the live stack prefix).
- **Designer:** Resources panel → "Memory (system config)" group, one spin box per key (0 shown as "default"). Stored as `"memory"` in the `.fui`; exported to project.json only for non-zero keys. Field list comes from `ferrite_build.SYSCFG_FIELDS` (labels: `MEMORY_LABELS` in `ferrite_designer.py`).
- **Errors:** malformed entry → Code 8 (invalid system config); a pool the heap can't hold → Code 7 (insufficient memory).
- **Board defaults:** `Platform::DEFAULT_ARENA_BYTES` / `DEFAULT_MAX_OBJECTS` / `DEFAULT_EVAL_STACK` / `DEFAULT_CALL_STACK` / `DEFAULT_CB_QUEUE` (0 = built-in) fill syscfg fields left at 0, and apply even without a `"sys"` entry.

## Memory Report (MemDetail)
- `ferrite_cli.py -p COMx memdetail` → per-pool table: heap (used/total, largest free block), CPU stack (+ peak), object arena (used/size, reclaimable garbage), object pointers (live/max, heap bytes of arena + table + DFS cache), UART RX ring (waiting/size), widgets / strings / arrays (live/max objects, bytes in the arena), eval/call stack, callback queue, animations (each with the heap bytes it holds). epaper's heap row is esp-alloc over all regions (internal + PSRAM), with no largest-block figure.
- Protocol: host field 11 varint (`0x58`) → device field 6 payload (`0x32`): version u8 (=1) + count u8 + count × [id u8, used u32, capacity u32, extra u32] LE. Pool ids `protocol::mem_pool` = `MEM_POOLS` in `ferrite_cli.py` (keep in sync). capacity 0 = board doesn't report that pool (CLI prints n/a).
- Gathered in `runtime::mem_detail` from `heap_stats`/`heap_total`/`stack_info` (PlatformRuntime), `UsartBackend::rx_stats` (nextion only), `ObjPool::used_bytes/garbage_bytes/live_objects/bytes_of/heap_bytes`, `Vm::mem_stats`. VM stack depths are the current values (≈0 between iterations), not high-water marks. `meminfo` is unchanged.

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
