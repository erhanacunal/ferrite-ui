# Program-facing features

User files, GPIO, EEPROM, optional hardware (CAPS, JPEG, audio), SD boot, Wi-Fi (epaper), gestures, animation, fonts.

## User Files (RES_FILE)
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

## User GPIO Header
- **Backend:** `gpio::GpioBackend` (`PIN_COUNT`, `set_mode`, `write`, `read`) → `Platform::GpioB` → `ctx.gpio` (`GpioImpl`, range-checks pin/mode). Boards without a header use `NoGpio` (0 pins).
- **Pins 0..7, same numbers on every board:** GD32 Nextion PC2–PC9 (`bsp/nextion/src/user_gpio.rs`), N32903 GPD3, GPD2, GPB2–GPB6, GPA0 (`bsp/n32903/src/gpio`). Sim: 8 virtual pins that log to stdout. epaper: 1 input-only pin, 0 = BUTTON_1 (GPIO21, `bsp/epaper/src/user_gpio.rs`; pull-up on from boot, `pinMode` switches the IO_MUX pulls, output → fault 14); it has no touch backend (`touch::NoTouch`). tdo_y13: `NoGpio`.
- **VM builtins:** `pinMode(pin, mode)` (`OP_PIN_MODE` 0xB0), `pinWrite(pin, value)` (0xB1, non-zero = high), `pinRead(pin)` (0xB2 → 0/1). Modes: 0 input, 1 output push-pull, 2 input pull-up, 3 input pull-down (GD32 only). `lib/gpio.fl` has the `GPIO_*` constants and pin table.
- Pins keep their power-on state until `pinMode`. Writing a non-output pin is ignored (on GD32 the ODR would flip an input's pull). Unknown pin / unsupported mode → VM fault 14 (`GPIO`). No pin-change callback: poll from `loop()`.

## User EEPROM
- **Backend:** `eeprom::EepromBackend` (`SIZE`, `read`, `write`, `flush`) → `Platform::EepromB` → `ctx.eeprom` (`EepromImpl`: range checks, and it skips 16-byte chunks that already hold the data, to save write cycles and wear). Boards without one use `NoEeprom` (size 0).
- **GD32 Nextion:** 24C08 (1 KB) on the RTC's I2C bus (SCL = PC0, SDA = PC1). The bit-bang moved to `bsp/nextion/src/i2c.rs`, which is shared with `rtc/hw.rs`. The A2 pin is at VCC, so the 4 × 256 B blocks answer at 0x54–0x57 (A0/A1 at GND, unused: on the 24C08 those bits select the block). The RTC is at 0x51. `eeprom/hw.rs` splits reads at block boundaries and writes into 16-byte pages, with bounded ACK polling after each page (`POLL_MAX`, ~30 ms, so a missing chip faults instead of hanging). No ISR uses the bus.
- **N32903:** 1 KB in the last 4 KB sector (0x7F000) of the BY25Q40 SPI NOR flash (512 KB). SPI0: GPD12 = CLK, GPD13 = CS (SS0), GPD14 = DI (flash SO), GPD15 = DO (flash SI). Drivers in the HAL: `n32903_rs::spi` + `n32903_rs::spiflash`, ported from the vendor BSP (`D:\repos\N32905_NonOS_BSP-master\SPI\Src\spi.c`, `spiflash.c`). The waits are bounded, there is an added 4 KB sector erase (0x20; the vendor's `spiFlashEraseSector` is really a 64 KB 0xD8 erase), and page splitting. `bsp/n32903/src/eeprom` keeps a RAM copy: `write` only updates it, and `flush` commits. If only bits were cleared, it programs just the changed pages; otherwise it does one sector erase plus programming. Every commit is read back and compared. A dead chip or a failed commit → every access faults 16.
- **`flush`:** the runtime calls `ctx.eeprom.flush()` once per main-loop pass and before the USART resets (Restart, FsWriteComplete). All writes in one callback (e.g. the relay board's save) cost one erase. Write-through backends (24C08, sim) keep the default no-op.
- **Sim:** 1 KB `SimEeprom`, and `--eeprom <file>` loads it at start and saves it after each write. tdo_y13 / epaper: `NoEeprom`.
- **VM builtins:** `eepromSize()` (0xB3), `eepromRead(addr)` (0xB4), `eepromWrite(addr, value)` (0xB5, low byte), `eepromReadArr(addr, arr, count)` (0xB6), `eepromWriteArr(addr, arr, count)` (0xB7). Bad range, no EEPROM or a bus error → VM fault 16 (`EEPROM`). Bad array → 9, `count` > array length → 13. Opcodes are duplicated in `vm.rs` and `tools/ferrite_cc.py` (`Op`, `Builtin`, `OP_NAMES`, `_NO_ARG_OPS`), fault names in `ferrite_cli.py` `VM_FAULT_NAMES`. Keep them in sync.
- **Examples:** `examples/nextion/eeprom_demo` (boot counter + reset; `examples/n32903/eeprom_demo` builds the same source for the N32903). `examples/nextion/relay_board` saves its buttons on OK and loads them at startup (format in `main.fl`: header + 16-byte slots, checksummed, header written last).
- `strByte(s, i)` (0xB8) returns byte `i` of a string, or -1 past the end. It is the counterpart of `arrToStr`, used to save names byte by byte.

## Device Features (CAPS)
- **Pattern for optional hardware** (audio, JPEG, later video): a ferrite-core cargo feature gating a backend trait (null impl `NoAudio`/`NoJpeg` for tests), a `Platform::*B` slot, an `*Impl<B>` field in `Ctx`, the VM opcodes / runtime calls that use it, and a `Platform::CAPS` bit (`CAP_AUDIO`, `CAP_VIDEO` reserved, `CAP_JPEG`). Several boards can share one feature through their own backends.
- **ferrite-core features** — a BSP enables only what its board has; with a feature off the code is not compiled at all:
  - `audio`: `audio` module, `Platform::AudioB`, `Ctx::audio`, sound opcodes 0xB9–0xBC (else bad opcode, fault 2), the `pump` / `stop` calls in `runtime`.
  - `jpeg`: `jpeg` module, `Platform::JpegB`, `Ctx::jpeg`, the FI mode-3 paths in `image.rs` (else a mode-3 image doesn't load). `ImageList::find_or_load` takes the `&JpegImpl` argument only with it.
  - `sdcard`: `sdcard` (block-device trait) + `fat`. There is no `Platform::SdCardB` slot: the BSP owns its card driver and passes it to `sdboot`.
  - `sdboot` (implies `sdcard`): `sdboot::sd_boot_check::<P, S>`.
  - Enabled by: nextion `sdboot` (BSP feature `sdboot`, default on; `--no-default-features` drops it, −5 KB), n32903 `audio` + `jpeg` + `sdboot`, sim `audio` + `jpeg`, epaper none (BSP feature `sdcard`, off: its SD driver is unused), tdo_y13 none.
  - A BSP's `Platform` impl / `Ctx` literal must match the features it enables, so don't build several BSPs in one cargo invocation (`--workspace`): feature unification would turn on `audio` for every member.
  - Tests: `cargo test -p ferrite-core --features host,mock --target x86_64-pc-windows-msvc` (features off), and again with `,audio,jpeg,sdboot` added.
  - `ferrite_build` fails a program that calls `playSound`/`stopSound`/`setVolume`/`soundPlaying` when the device lacks `caps.audio` (`AUDIO_BUILTINS`).
- **Widget kinds (`widget-*`, default feature `widgets` = all):** `widget-progress`, `-slider`, `-checkbox`, `-radio` (+ button groups), `-input` (+ on-screen keyboard), `-gauge`, `-dropdown`, `-graph`, `-image` (the image widget; background images are always built), `-shapes` (circle/ellipse/polygon/line). Base, label, button and rectangle are always built. A BSP short on flash depends on ferrite-core with `default-features = false` and lists the kinds it keeps; every BSP takes the default today.
  - Code checks a kind with `widget::is_kind(k, KIND_X)` (or `kind_built(KIND_X)`), which folds to `false` when it is left out, so the drawing and touch code drop out. Compare with constants: a `match` on a runtime value through a helper does not fold (tried: graph code stayed in).
  - A kind left out still exists (the tools and VM don't know about it) and acts as a base widget: box, background, border, children, callbacks; shapes become boxes. The keyboard goes with `widget-input` (`Keyboard::shown`/`key_down`).
  - Nextion (2026-11-19, 120 660 B with all): −5.3 KB input, −2.7 KB gauge, −2.2 KB graph, −1.8 KB shapes, −680 dropdown, −512 checkbox, −500 radio, −380 slider, −376 image, −52 progress; all off 103 196 B (−17.5 KB).
- `CAPS` bits are mirrored in `tools/devices/<id>.json` `caps.*` (+ the `_FALLBACK` table in `ferrite_devices.py`) — keep in sync. The build tools gate features on them.
- **JPEG (`CAP_JPEG`, `caps.jpeg`):** FI image mode 3 = FI header + baseline JPEG stream. `ferrite_img.py` always re-encodes (`"mode": "jpeg"`, `"quality"` 1–100, default 85) to baseline 4:2:0, 3 components, standard Huffman tables, alpha flattened onto black. On a device without `caps.jpeg`, `ferrite_build` warns and falls back to `"auto"`. The designer defaults `.jpg`/`.jpeg` imports to `"jpeg"` on such devices.
  - Firmware: `ImageList::find_or_load` decodes once, at image load (`Image::decode_jpeg`), into an `AlignedBuf` (32-byte aligned, whole cache lines, stride/rows rounded up to 16) on the heap; drawing (`draw`/`draw_region`, scissored) copies from it. Out of heap / bad stream → the image doesn't load. `ImageList::clear` frees the buffers. RAM = stride × rows × 2 per image (800×480 = 750 KB), so RAM-rich boards only.
  - N32903: `n32903_rs::jpeg` (HAL port of the vendor `JPEG/Src/jpegcodec.c`, polled, decode only: packet RGB565/RGB555/YUV422/ARGB8888, stride, down-scale, `probe` stream check, bounded waits, cache maintenance) ← `bsp/n32903/src/jpeg` (`HwJpeg`). Sim: `bsp/sim/src/jpeg.rs` (`jpeg-decoder` crate, accepts only what the engine accepts). Example: `examples/n32903/jpeg_demo`.
- **Audio (`CAP_AUDIO`, `caps.audio`):** `"sounds": [{"name", "source": "x.wav"}]` → `RES_SOUND` (6): header magic "SN", version 1, channels u8, rate u32, then PCM16 LE (`ferrite_build.convert_sound`: 8/16/24/32-bit WAV → 16-bit). Builtins `playSound(name [, loop])` (0xB9 → 1/0), `stopSound()` (0xBA), `setVolume(0..100)` (0xBB), `soundPlaying()` (0xBC). One sound at a time.
  - Core: `AudioImpl` holds the player; `play` opens the resource and `backend.start(rate, channels)`, `runtime::run` calls `ctx.audio.pump(&ctx.flash)` every main-loop pass (fills `free_space()` from flash, whole frames), and a one-shot sound ends when `pending()` reaches 0. `stop` on program reload / writefs.
  - N32903 speaker: the DAC feeds an 8002-type amplifier whose SHUTDOWN pin is GPB1, active high (`AMP_*` in `bsp/n32903/src/audio`; low = on, the opposite of the vendor boards' `SPK_EN`): off at boot, on from `start` to `stop`. Driving it high during playback = silence with pops.
  - N32903: `n32903_rs::spu` (port of the vendor `SPU/Src/DrvSPU.c` + `SPU.c`: DAC clock = UPLL ÷ `REG_CLKDIV1` ADO_N0/N1 = 128 × rate, DFA fixes the rounding, stereo PCM16 on channel 0, threshold/end interrupts refill the halves of a ring buffer) ← `bsp/n32903/src/audio` (`SpuAudio`: 8 KB DMA ring through the uncached alias, 32 K-sample queue filled by `write` and drained by the ISR, mono duplicated to L/R, `pending` = 0 after two silent halves). SPU open + `dac_on(1)` (~0.75 s ramp) happen on the first `start`; `stop` keeps the DAC powered. Queue indices are volatile, no fences (armv5te has none: even `compiler_fence` links `__sync_synchronize`).
  - Sim: `bsp/sim/src/audio.rs` (`SimAudio`): silent, consumes at the sample rate, logs start/stop, `--audio-out out.wav` records the output. Example: `examples/n32903/audio_demo`.

## SD Card Boot
- `ferrite_core::fat` (read-only FAT16/32, generic over `SdCardBackend`, `sdcard` feature) + `ferrite_core::sdboot::sd_boot_check(ctx, sd, present)` (`sdboot` feature): `EMPTY.BIN` → recovery, `PROGRAM.BIN` → copied to `P::FS_BASE`. The BSP detects/inits the card; `present` makes the progress text visible (n32903: `lcd::flush_cache`).
- **Nextion:** SPI SD (`sdcard/hw.rs`, PC13 CS).
- **N32903:** the driver is in the HAL submodule, `n32903_rs::sd`: a Rust port of Nuvoton's N3290x SD driver (SDLoader `sd.c`), with init on any of the 3 SD ports, multi-block DMA read/write and card size from the CSD. SD cards only, no MMC. The HAL is meant to be a port of the vendor C library, so new peripheral drivers go there and the BSP only adapts them. `bsp/n32903/src/sdcard` adapts it: port 0 (GPE2–GPE7), plus a 32-byte-aligned bounce buffer because DMA needs cache-line alignment. There is no card-detect pin, so `init()` is the probe (~100 ms without a card). The SD host shares the FMI and DMAC with NAND: each op sets FMICR itself, and the FMI is never reset (that drops the NAND config). After an update, `flash::flush()` writes the shadow to NAND.

## Wi-Fi / Network (epaper)
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

## Touch Gestures
- **Swipe (OnSwipe = 10):** fires on release when displacement ≥ 40 px and dominant axis ≥ 2× minor; args: `(direction, widget_id, dominant_delta)`
  - Directions: 0=left, 1=right, 2=up, 3=down (use `lib/touch.fl` constants `SWIPE_LEFT`…`SWIPE_DOWN`)
  - Mutually exclusive with `on_click` — a touch that becomes a swipe never fires `on_click`
- **Long press (OnLongPress = 11):** fires once after 500 ms of holding while finger stays within 20 px of press origin; args: `(widget_id, x, y)`
  - Suppresses `on_click` on the subsequent release
- **Gesture state** tracked in main loop locals: `gesture_start_x/y`, `press_ms`, `long_press_fired`
- **Constants:** `LONG_PRESS_MS = 500`, `LONG_PRESS_MAX_MOVE_SQ = 400`, `SWIPE_MIN_DIST_SQ = 1600`, `SWIPE_AXIS_RATIO = 2`
- **Language library:** `lib/touch.fl` — include in `.fl` programs to get named `SWIPE_*` constants

## Property Animation
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

## Font Renderer (Adafruit GFX compatible, sparse/UTF-8)
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
- **Tool:** `tools/ferrite_font_converter.py` — TrueType → sparse binary; supports `-r` range spec (e.g. `32-127,0x011E-0x011F`). `--v3`: V3 layout (magic `00 03`, 14-byte glyphs: offset u32, w/h/advance u16, offsets i16; u16 line height) for fonts past V1/V2's limits (~130pt at 96 DPI); read only by ferrite-ui-xl (`caps.font_v3`: `ferrite_build` repacks every font of those projects as V3, the Designer generates V3). V3 header bytes 8..10 hold the bold / italic / bold italic variants' font ids (project font entries' `"bold"`/`"italic"`/`"bold_italic"`, set by `ferrite_build`): rich text (`rich_text` prop 0x38, `caps.rich_text`, `tools/ferrite_rich.py` = ferrite-ui-xl `rich.rs`)
- **RAM cost:** N × 9B (codepoints + glyphs) per font; bitmap read in 128B chunks
