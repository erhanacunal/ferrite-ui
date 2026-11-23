# Boards and hardware

GD32 Nextion hardware, the FPGA bus, board bring-up, recovery, the sim and the ferrite-ui-xl split.

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

## Rust no_std
- `#![no_std]` + `#![no_main]`
- Rust 2024 edition (`edition = "2024"` — stable with Rust 1.85)
- `cortex-m-rt` crate — startup, interrupt table, `#[entry]` macro
- Custom panic handler — displays error on LCD + sends via USART (no heap, ROM font only)
- No newlib, no hidden runtime, RAM fully owned by user
- Target: `thumbv7m-none-eabi`, built through the custom spec `bsp/nextion/thumbv7m-none-eabi.json` (`"frame-pointer": "may-omit"`, −4 KB; `-Cforce-frame-pointers=no` can't lower a built-in target's "always") with `-Zbuild-std` (`build.bat`, CI). The n32903 / tdo_y13 specs are may-omit too, and their build-std division is replaced by assembly (`ferrite-core` feature `arm_div`); Cortex-M3 divides in hardware.

## External Flash (W25Q256, 32MB)
- **Pin assignment:** PA4=CS, PA5=CLK, PA6=MISO, PA7=MOSI (bit-bang SPI)
- **4-byte address mode:** activated with 0xB7 at init (full 32MB access)
- **API:** `read(addr, buf)`, `write(addr, data)`, `erase_sector(addr)`, `read_id()`
- `write()` automatically splits across page boundaries (256B page program)
- `erase_sector()` and `page_program()` busy-wait until complete

## Recovery Mode
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

## Sim tools
- **Sim clock:** `FERRITE_SIM_CLOCK=HH:MM[:SS]` starts the sim's RTC at that time of day (reproducing a clock-face state).
- **Sim CPU profiler (`bsp/sim/src/prof.rs`, Windows x64):** `--prof SECS[,DELAY_MS] [--prof-lines]` samples the main thread every ~1 ms (SuspendThread + RtlVirtualUnwind, dbghelp names; no admin needed), prints self/inclusive/line tops and exits. Build with `CARGO_PROFILE_RELEASE_DEBUG=true CARGO_PROFILE_RELEASE_OPT_LEVEL=3`. Most samples are the 60 fps wait; compare render entries.

## ferrite-ui-xl (exynos4412)
- The exynos4412 board and the runtime features only it used live in the ferrite-ui-xl repo (its own ferrite-core, sim, BSP and examples; it uses this repo's `tools/` and `lib/`). Removed from this runtime: `wide_obj`, `multi_program` (tasks), `fast_vm`, `superops`, `render_cache` (geometry table), `layers`, `perf` (main-loop profiler), the pixel shift (`move_rects`), the whole-area compose and `on_paint` capture renderer hooks, model3d / sysinfo / battery syscalls, the sim's `tablet` size.
- Here: opcodes 0x45–0x4D (superops) and 0xD4–0xDA (tasks) are bad opcodes, `PROP_LAYER`/`PROP_LAYER_ALPHA` (0x36/0x37) are ignored (read 0 / 255), `CAP_*` bits 3/4 are XL's, kind 18 (`KIND_MODEL3D`) and `RES_MESH` (7) are XL's, `on_task_message`/`on_task_exit` (FunctionKind 15/16) never fire, a `perf` query gets no reply.
- Tools keep supporting XL: `tools/devices/exynos4412-xl.json`, `caps.superops`/`caps.multi_program` gating, `ferrite_cli` `perf`, `bootflash`, `savefw`, `--host`, `-p tcp://HOST` (bridge port 7000), `KIND_MODEL3D` in the designer, `"meshes"` in project.json, `lib/model3d.fl`, `lib/sysinfo.fl`, `lib/battery.fl`, `lib/gps.fl` (2026-11-20).

## Project Name

**ferrite-ui** — no trademark issues.
Nextion is a registered trademark of ITEAD — this project is fully independent, clean-room implementation.
