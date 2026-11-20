# CLAUDE.md — ferrite-ui

Bare-metal HMI framework in Rust: a bytecode VM runs `.fl` programs on a widget tree. First target: Nextion NX8048K070 (GD32F103, Cortex-M3, 20 KB RAM, 800×480 through an FPGA frame buffer); also N32903, tdo_y13, epaper (ESP32-S3) and a host sim. The Exynos 4412 tablet lives in ../ferrite-ui-xl, which uses this repo's `tools/` and `lib/`.

## Detailed notes (read the relevant one before changing a subsystem)

- `docs/notes/render.md` — widget objects and sections, kinds, box model, dirty rendering, restores/moves, scroll containers, clipping, gauge partial redraw, board renderer hook, portrait mode, anti-aliasing, row blends.
- `docs/notes/runtime.md` — main loop, VM (callbacks, faults, program switching, widget page, image header, code addresses), object pool, flash FS + checksum, RES_SYSCFG, memdetail, file tree, memory usage.
- `docs/notes/features.md` — user files, GPIO, EEPROM, CAPS/cargo features, JPEG, audio, SD boot, Wi-Fi (epaper), gestures, animation, fonts.
- `docs/notes/boards.md` — hardware, FPGA protocol and double buffer, W25Q256, recovery mode + PENIRQ touch, sim tools, what moved to ferrite-ui-xl.
- `docs/notes/compiler.md` — `.fl` pitfalls, compiler changes (float locals, classes, widget constructors).
- `docs/notes/tools.md` — USART resync, writefs CRC chunks / sequence ACKs, `ferrite_cli` upload paths (bootflash, bridge, monitor), editor support (`ferrite_analysis`: designer code editor + LSP).
- `docs/notes/history.md` — the original feature checklist.
- Language/bytecode/protocol references: `docs/ferrite-lang.md`, `docs/bytecode.md`, `docs/protocol.md`. Platform abstraction design: `HANDOFF.md`.

## Layout

Cargo virtual workspace. `ferrite-core` = HAL backend traits + generic `*Impl<B>` wrappers + the whole device-agnostic framework, generic over `P: Platform`. Each board is a thin BSP crate under `bsp/` (`nextion`, `n32903`, `tdo_y13`, `epaper`, `sim`) owning its backends and entry point. Tools (compiler `ferrite_lang.py`/`ferrite_cc.py`, `ferrite_build.py`, `ferrite_cli.py`, designer, font/image converters, LSP) in `tools/`; `.fl` library in `lib/`; examples in `examples/<board>/`.

## Commands

- Tests: `cargo test -p ferrite-core --features host,mock --target x86_64-pc-windows-msvc`, and again with `,audio,jpeg,sdboot` added.
- Firmware: `build.bat` (Nextion, build + objcopy), `n32903.ps1`, `tdo_y13.ps1`, `epaper.ps1`. Sim: `sim.bat <project>`.
- Device tool: `python tools/ferrite_cli.py -p COMx <cmd>` (`writefs`, `memdetail`, `monitor`, …).

## Rules and invariants

- `#![no_std]`, Rust 2024, no hidden runtime. Panic handler draws with the ROM font only.
- Don't build several BSPs in one cargo invocation (`--workspace`): feature unification breaks `Platform` impls / `Ctx` literals.
- Use `lcd.width()/height()`, not `LcdBackend::WIDTH/HEIGHT`, for screen bounds (portrait mode).
- Draw through the renderer / `LcdImpl`; board renderer overrides stay inside `clip ∩ abs` and draw the same whatever the clip (`renderer.rs`). Default renderer keeps GD32 firmware byte-identical.
- Object lifetimes are manual (widget subtree destroy, `strFree`/`strClear`, `arrFree`, `free(obj)`); no GC.
- Compiler changes: check that every example still compiles to identical bytecode unless the change is meant to alter it.
- FS v3 images (CRC-32) need current firmware: update firmware before uploading images built by new tools.
- Nextion: a new IRQ must be added to `irq.rs` `__INTERRUPTS` and `device.x`, or it jumps to address 0. FPGA writes need `spin(3)`; leave CMD 0x0D at 0.
- **Keep in sync** (duplicated numbers/layouts): opcodes (`vm.rs` ↔ `ferrite_cc.py` `Op`/`Builtin`/`OP_NAMES`/`_NO_ARG_OPS` ↔ `docs/bytecode.md`); widget page ops (`PG_*` in `vm.rs` ↔ `PAGE_*` in `ferrite_lang.py`); widget kinds (`widget.rs` ↔ `ferrite_designer.py` ↔ `lib/core.fl`); VM fault names (`ferrite_cli.py` `VM_FAULT_NAMES`); syscfg (`syscfg.rs` ↔ `ferrite_build.py` `SYSCFG_FIELDS`); mem pools (`protocol::mem_pool` ↔ `ferrite_cli.py` `MEM_POOLS`); `CAPS` bits ↔ `tools/devices/*.json` `caps.*` + `ferrite_devices.py` `_FALLBACK`; flex bit layout (`lib/flex.fl` ↔ docs); epaper syscall ids / `NET_ERR_*` (`bsp/epaper/src/platform.rs` ↔ `lib/epaper.fl`); writefs flags/tags (`protocol.rs` ↔ `ferrite_cli.py`).

## `.fl` pitfalls

- A generated `on_click` dispatcher calls every handler registered for a widget; arrays go to functions only as `: array` parameters (an array's bare name is accepted only where an array is expected; since 2026-11-20); a global may not share a name with any function's param/local.
- `fileOpen` returns `0xFF` on error and every later file call on that handle faults: always check.
