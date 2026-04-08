# ferrite-ui

Bare-metal HMI framework for the Nextion NX8048K070 display, written in Rust. Replaces the original Nextion firmware with a clean-room implementation.

For the some bricked Nextion Displays :)

## Hardware

| Component | Part | Details |
|-----------|------|---------|
| CPU | GD32F103RBT6 | Cortex-M3, 108MHz, 128KB Flash, 20KB RAM |
| Display | NX8048K070 | 800x480 TFT LCD |
| Display controller | FPGA | 2MB framebuffer, double-buffered, CPU writes directly |
| Touch | XPT2046 | SPI, Z-pressure detection, median filter |
| Flash | W25Q256JVFQ | 32MB SPI, stores fonts/images/bytecode |
| RTC | AT8563T | I2C |

![Ferrite Clock Example](docs/images/ferrite_clock.jpeg)

### ST-Link SWD Pinout

![ST-Link SWD Pins](docs/images/st-link-pins.jpeg)

## Features

- **Widget system** -- HTML-like tree structure with CSS box model, split-struct design (18B base + 32B extension on demand)
- **Widget types** -- Base (container), Label, Button, Progress, Slider, Checkbox, Radio
- **Dirty redraw** -- Two-pass clip-based painter's algorithm (erase hidden, then draw visible)
- **Double buffering** -- FPGA back/front buffer swap for tear-free rendering
- **Bytecode VM** -- 57+ opcodes, stack-based locals (FRAME prologue), 128 globals + 128 locals per frame, 8-deep call stack
- **Drawing primitives** -- fillRect, rect, line, circle, fillCircle, roundedRect, fillRoundedRect, arc, drawImage, drawText
- **Float32 arithmetic** -- Software float (add/sub/mul/div/neg + comparisons + conversion)
- **String pool** -- Heap-allocated strings with auto-incrementing IDs, smart GC preserving widget text
- **Touch input** -- Debounced press/hold/release events, hit testing, on_click and on_tap callbacks
- **Custom paint** -- on_paint callback for widget custom drawing
- **Flash filesystem** -- TOC-based, named resources (fonts, images, programs, pages)
- **Font rendering** -- Adafruit GFX compatible bitmap fonts (flash + embedded)
- **Image format** -- Ferrite Image (FI): raw/RLE/indexed+RLE, streaming decode
- **Recovery mode** -- Hold top-left corner 3s at boot for USART-only reflash mode (see below)
- **USART protocol** -- Protobuf-style serial communication (ping, execute, flash write, user messages)
- **Backlight PWM** -- 0-100% brightness via hardware timer
- **SysTick timer** -- 1ms tick counter, blocking and non-blocking delays

### Recovery Mode

![Recovery Mode](docs/images/recovery_mode.jpeg)

Hold the top-left corner for 3 seconds at boot. A red progress bar fills while holding. Release to cancel. In recovery mode, no program is loaded -- use `writefs` via USART to flash a new program.

### Widget Demo

| Buttons | Sliders |
|---------|---------|
| ![Buttons Tab](docs/images/widgets_demo_1.jpeg) | ![Sliders Tab](docs/images/widgets_demo_2.jpeg) |

| Checkbox / Radio | Canvas (on_paint) |
|------------------|-------------------|
| ![Check Tab](docs/images/widgets_demo_3.jpeg) | ![Canvas Tab](docs/images/widgets_demo_4.jpeg) |

## Ferrite Language

Programs are written in a C-like language (`.fl` files) that compiles to VM bytecode. Here's an [analog clock](examples/clock/standalone.fl) that demonstrates drawing primitives, string operations, and the animation loop:

```c
// Hand positions: 12 entries (R=120), dx = R*sin(i*30°)
var sdx[12] = [0, 60, 104, 120, 104, 60, 0, -60, -104, -120, -104, -60];
var sdy[12] = [-120, -104, -60, 0, 60, 104, 120, 104, 60, 0, -60, -104];

var h = 10;
var m = 10;
var s = 0;

// Clock face
fillCircle(400, 210, 170, 0x2945);
arc(400, 210, 170, 0, 359, 0x4A69);
drawText(392, 68, 0, 0xFFFF, 0, "12");

// Digital time panel
fillRoundedRect(250, 420, 300, 50, 12, 0x2945);
roundedRect(250, 420, 300, 50, 12, 0x4A69);

while (true) {
    // Second hand (red)
    var i = s / 5;
    line(400, 210, 400 + sdx[i], 210 + sdy[i], 0xF800);

    // Center dot
    fillCircle(400, 210, 6, 0xF800);
    fillCircle(400, 210, 3, 0xFFFF);

    // Digital time "HH:MM:SS"
    var ts = concat(itos(h), concat(str(":"),
             concat(itos(m), concat(str(":"), itos(s)))));
    drawStr(345, 450, 0, 0xFFFF, 0, ts);
    strClear();  // free temp strings, widget text preserved

    delay(1000);
    s = s + 1;
    if (s >= 60) { s = 0; m = m + 1; }
    if (m >= 60) { m = 0; h = h + 1; }
    if (h >= 24) { h = 0; }
}
```

See [docs/ferrite-lang.md](docs/ferrite-lang.md) for the full language reference and [examples/](examples/) for complete programs.

## Toolchain

```bash
# Compile .fl source to bytecode
python tools/ferrite_lang.py program.fl -o program.bin

# Disassemble bytecode
python tools/ferrite_cc.py disasm program.bin

# Upload and execute on device
python tools/ferrite_cli.py -p COM3 execute program.bin

# Write flash filesystem image
python tools/ferrite_cli.py -p COM3 writefs flash.bin

# Send user message to device
python tools/ferrite_cli.py -p COM3 send 0x01 0x02 0x03

# Convert PNG to Ferrite Image format
python tools/ferrite_img.py icon.png -o icon.fi

# Analyze bytecode memory and instruction profile
python tools/ferrite_analyze.py program.fl -v
```

## Building

Requires Rust nightly with the `thumbv7m-none-eabi` target:

```bash
rustup target add thumbv7m-none-eabi
cargo build --release --target thumbv7m-none-eabi
cargo objcopy --release -- -O binary firmware.bin 

# flash to device

st-flash.exe write .\firmware.bin 0x08000000
```

Binary output: `target/thumbv7m-none-eabi/release/ferrite-ui`

## Resource Usage

| Resource | Used | Total | % |
|----------|------|-------|---|
| Flash (text) | 31 KB | 128 KB | 24% |
| RAM (bss) | 6.3 KB | 20 KB | 31% |

### RAM Breakdown

| Component | Size |
|-----------|------|
| Heap (linked-list allocator) | 14 KB |
| Clip region (32 rects) | 256 B |
| USART RX ring buffer | 128 B |

Heap-allocated: Ctx (WidgetTree + extensions, FontList, ImageList, StringPool), VM (stack-based locals, arrays, callback queue), fonts.

## Project Structure

```
ferrite-ui/
├── src/
│   ├── main.rs          Entry point, startup, main loop
│   ├── ctx.rs           Shared application context (Ctx struct)
│   ├── vm.rs            Bytecode VM (57+ opcodes, stack frames, f32)
│   ├── widget.rs        Widget + WidgetExt split, tree, box model
│   ├── render.rs        Painter's algorithm, dirty redraw, clip
│   ├── lcd.rs           FPGA display protocol, double buffer, drawing primitives
│   ├── clip.rs          Clip region (rect subtract algorithm)
│   ├── touch.rs         XPT2046 driver, hit test, debounce
│   ├── flash.rs         W25Q256 SPI flash driver
│   ├── font.rs          Adafruit GFX bitmap font renderer
│   ├── image.rs         Ferrite Image (FI) decoder
│   ├── fs.rs            Flash filesystem (TOC, named resources)
│   ├── strpool.rs       Heap string pool (itos/ftos/concat/GC)
│   ├── heap.rs          Linked-list heap allocator (14KB)
│   ├── systick.rs       SysTick 1ms timer
│   ├── protocol.rs      USART protobuf protocol
│   ├── usart.rs         USART0 driver + RX interrupt
│   ├── backlight.rs     LCD backlight PWM
│   ├── gpio.rs          GPIO, 16-bit data bus, clock
│   ├── proto.rs         Protobuf encoding/decoding primitives
│   ├── irq.rs           Interrupt vector table
│   └── embedded_font.rs Built-in FreeMono 9pt font
├── tools/
│   ├── ferrite_lang.py  Language compiler (.fl -> bytecode)
│   ├── ferrite_cc.py    Assembler / disassembler / compiler API
│   ├── ferrite_cli.py   Serial communication tool
│   ├── ferrite_img.py   PNG to Ferrite Image converter
│   ├── ferrite_analyze.py  Bytecode memory/instruction profiler
│   └── ferrite_build.py JSON project builder (flash image)
├── docs/
│   └── ferrite-lang.md  Language reference
├── memory.x             Linker script (128KB Flash, 20KB RAM)
└── CLAUDE.md            Internal development notes
```

## Architecture

- **Custom heap allocator** -- 14KB linked-list allocator, all large structures heap-allocated via Box/Vec
- **No OS, no runtime** -- `#![no_std]`, `#![no_main]`, `cortex-m-rt` entry point
- **No frame buffer in CPU RAM** -- pixels are written directly to the FPGA over a 16-bit GPIO bus
- **Double buffered** -- FPGA handles front/back buffer swap, tear-free rendering
- **Two-pass dirty redraw** -- erase hidden widgets first, then draw visible ones with clip-based painter's algorithm
- **Split-struct widgets** -- 18B base (tree links, layout, colors) + 32B extension on demand (edges, text, callbacks)
- **Stack-based VM locals** -- FRAME prologue per function, locals isolated across CALL/RET, supports recursion
- **Shared context** -- Ctx struct bundles LCD, Flash, WidgetTree, FontList, ImageList, StringPool, Fs

## License

MIT License. See [LICENSE](LICENSE) for details.

This project is an independent, clean-room implementation. It is not affiliated with or endorsed by ITEAD (Nextion).
