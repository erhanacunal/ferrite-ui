# Platform-abstraction migration — Handoff (updated 2026-06-09, PR-5b + PR-4 DONE)

## Goal (achieved)

`Platform` (ferrite-core/src/platform.rs) + `PlatformRuntime` (ferrite-ui/src/runtime.rs)
are now the single real seam between the device-agnostic framework and the per-device
BSP crates:

- `ferrite-core` — HAL backend traits + generic wrappers (`LcdImpl<B>`…) + `Platform`.
- `ferrite-ui`   — device-agnostic **framework library**, generic over `P: Platform`.
  Owns the unified `runtime::run::<P>()` loop. **No `[[bin]]`s, no device deps**
  (except `esp-alloc`, used only by `heap.rs` under the `epaper` feature).
- `bsp/{nextion,epaper,sim}` — thin binary crates: concrete backends + `impl Platform`
  + `impl PlatformRuntime` + a `main` that builds `Ctx<P>` and calls `ferrite_ui::runtime::run`.

## ✅ Status — all three BSPs build/check on `run::<P>()`

| BSP            | target                    | gate (from workspace root)                                              | result |
|----------------|---------------------------|------------------------------------------------------------------------|--------|
| bsp-nextion    | `thumbv7m-none-eabi`      | `cargo build -p bsp-nextion --release`                                  | LINKS  |
| bsp-epaper     | `xtensa-esp32s3-none-elf` | `cargo +esp check -p bsp-epaper --target xtensa-esp32s3-none-elf -Zbuild-std=core,alloc` | OK |
| bsp-sim        | host (`x86_64-*`)         | `cargo build -p bsp-sim --target x86_64-pc-windows-msvc`                | LINKS  |

Framework gates (host, no embedded toolchain):
```
cargo check -p ferrite-core --features mock                              # OK
cargo check -p ferrite-ui   --features mock --lib --no-default-features  # OK (run::<MockPlatform>)
cargo check -p ferrite-ui   --lib --no-default-features                  # OK (ZERO device features)
```

### Why the explicit `--target` / `-Zbuild-std`
The workspace-root `.cargo/config.toml` pins `[build] target = thumbv7m-none-eabi`
(for nextion). epaper and sim therefore need an explicit `--target` when invoked
from the root. The Espressif fork ships **no precompiled xtensa `core`/`alloc`**, so
epaper also needs `-Zbuild-std=core,alloc` (now also encoded in
`bsp/epaper/.cargo/config.toml`, effective when cargo runs from that dir — e.g. the
`epaper.ps1` script). Source `~/export-esp.ps1` before any `cargo +esp` invocation.

## What `run::<P>()` is (ferrite-ui/src/runtime.rs)

`pub fn run<P: PlatformRuntime>(ctx: Box<Ctx<P>>, touch: TouchImpl<P::TouchB>) -> !`
owns the shared loop: embedded-font add, `P::boot`, FS mount, root widget, VM build
(`vm.syscall_fn = Some(P::syscall)`, `vm.forced_render_mode = P::FORCED_RENDER_MODE`),
program load, setup/on_program_start/loop, then the forever loop (modal resume → VM
step → USART protocol → input → render phase → drain callbacks → **`P::frame_end`**).

Two traits carry all device variance — **no `#[cfg]` in the loop**:
- **`PlatformRuntime: Platform`** — consts `BG_COLOR`/`FG_COLOR`/`FORCED_RENDER_MODE`,
  `type Input`, and hooks `reset()`, `syscall()`, `stack_info()`, `boot()`,
  `initial_render()`, `on_extra_rx()`, and **`frame_end()`** (host window pump; no-op
  on device). Sensible defaults so a minimal BSP overrides only `reset`+consts+`Input`.
- **`InputHandler<P>`** — `FullInput` (keyboard + scrollbars + sliders + gestures;
  nextion + sim) or `BasicInput` (press→on_click; epaper). Monomorphization dead-strips
  the unused one.

## BSP recipe (all three follow it)

- `Cargo.toml`: `ferrite-ui = { path="../..", default-features=false [, features=[…]] }`.
  nextion: no features (lib heap is the allocator). epaper: `features=["epaper"]`
  (disables lib heap → `esp_alloc`; `heap::stats` reports via `esp_alloc`). sim:
  `features=["host"]` (lib built as std).
- Backend `mod.rs` files re-export the core trait (`pub use ferrite_core::<p>::*`),
  `#[path]`-redirect the device backend file from the **root `src/<periph>/{hw,epaper,sim}.rs`**,
  alias `pub type X = XImpl<Backend>`, and expose **free-fn constructors**
  (`pub fn new()/init() -> X`). Inherent `impl` on the framework alias is **E0116**
  (foreign type) — always use a free fn wrapping `with_backend`.
- Systick gets a ZST `impl SystickBackend` (`Gd32Systick`/`EpdSystick`/`SimSystick`) +
  `pub type Systick`; hardware init that needs a peripheral handle stays a free fn
  called from `main` (Ctx uses `Systick::handle()`).
- `src/platform.rs`: the `impl Platform` (8 assoc types) + `impl PlatformRuntime`.
- `src/main.rs`: device bring-up (clocks/ports/peripherals), build `Ctx<P>` + `touch`,
  call `ferrite_ui::runtime::run::<P>`.
- `panic.rs` is a real BSP file (names concrete `Lcd`/`Flash`).

Device-specific notes:
- **epaper** forces `RenderMode::EPaper` (`FORCED_RENDER_MODE`); `EpdLcd::pre_full_redraw`
  ghost-erases (drive all-white); `EspUart::rx_read_byte` drains the USB-Serial-JTAG
  FIFO; `boot` = `alloc_buffers`+`clear`+`probe_ferrite_fs_preamble`. bsp-epaper has its
  own `epaper` feature so the `#![cfg(feature="epaper")]` guards in the redirected backend
  files compile; `#![allow(unsafe_op_in_unsafe_fn)]` covers the pre-2024 backend style.
- **sim** uses `frame_end` to pump the minifb window (sample mouse → present framebuffer)
  via a thread-local `HOST` in `platform.rs` (minifb `Window` is `!Send`). `type Input =
  FullInput` (richer than the old sim_body).

## ferrite-core / framework changes carried in this migration
- `UsartBackend::rx_read_byte()` (default `None`); the loop drains via `ctx.usart.rx_read_byte()`.
- `FlashImpl<B>: Clone` + `Platform::FlashB: FlashBackend + Clone` (VM clones a flash
  handle for flash-exec cache refills). All flash backends derive `Clone`.
- `heap.rs` global allocator is now `#[cfg(all(not(epaper), not(host)))]` — the 14 KB
  static heap is the allocator **only** on bare-metal nextion; epaper uses `esp_alloc`,
  host uses std.
- `runtime::PlatformRuntime::frame_end` added (host window pump).

## Layout (the root `src/` is gone)

The workspace root is now a **virtual manifest** (`Cargo.toml` = `[workspace]` + the
shared `[profile.release]`). Every crate is a member directory:
```
ferrite-core/      HAL backend traits + generic wrappers + Platform
ferrite-ui/        device-agnostic framework lib (was the root src/) — owns runtime::run
bsp/nextion/       GD32 backends + entry + panic   (gpio.rs, irq.rs, fat.rs, */hw.rs)
bsp/epaper/        ESP32-S3 backends + entry + panic (battery.rs, lcd/{epaper,display,
                   ed047tc1,rmt,error}.rs, */epaper.rs)
bsp/sim/           host backends + entry            (*/sim.rs)
```
Each BSP now physically OWNS its device backend files (no more `#[path]` redirects into
a shared `src/`). The epaper backends lost their `#![cfg(feature="epaper")]` guards (only
compiled in bsp-epaper now), and bsp-epaper dropped its private `epaper` feature (it still
enables ferrite-ui's `epaper` feature for the heap). The GD32 linker scripts
(`gd32-{link,memory,device}.x`) stay at the workspace root because the root
`.cargo/config.toml` (`-Tgd32-link.x`, CWD = root) is what links nextion when built from
the root.

## REMAINING (optional polish — nothing blocks a build)

- `bsp/nextion/src/gpio.rs` has Turkish comments (repo is English-only) — clean up.
- **sim runtime** still hits the upstream `scoped-tls` Rust-2024 issue at *run* time
  (it compiles + links fine). Resolve when running the sim end-to-end.
- Reduce BSP warnings (unused `pub use` re-exports in some BSP `usart`/`systick` mods; a
  few unused vars in backends).
- nextion has two overlapping linker setups: the root `.cargo/config.toml`
  (`-Tgd32-link.x`, used when building `-p bsp-nextion` from the root) and
  `bsp/nextion/.cargo/config.toml` + `memory.x`/`device.x`/`build.rs` (used when building
  from inside the crate dir). Consolidate if desired.

## Risks (carry forward)
- **No `dyn` in tight draw loops** — keep static generics for `LcdBackend`. The one
  `Box<dyn FlashBackend>` in `VmCode` is the only acceptable dyn (cold SPI path).
- **Behavioral parity not yet runtime-tested** — `run()` is a faithful transcription of
  the old nextion/epaper main loops (compiled, not yet run on hardware). Watch the
  on_paint double-drain and the buffered-mode keyboard overlay cadence when flashing.
  epaper now *forces* EPaper render mode (old body honoured the image header) — confirm
  this matches the deployed images.
