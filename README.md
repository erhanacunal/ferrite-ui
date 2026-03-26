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

## Features

- **Widget system** -- HTML-like tree structure with CSS box model (margin, border, padding), arena-allocated (64 widgets, no heap)
- **Widget types** -- Base (container), Label (text + font + alignment), Button (press state + children)
- **Dirty redraw** -- Clip-based painter's algorithm, only repaints changed widgets
- **Bytecode VM** -- 55 opcodes, protobuf tag encoding, 16 variables, 8-deep call stack
- **Drawing primitives** -- fillRect, rect, line, circle, fillCircle, drawImage, drawText
- **Float32 arithmetic** -- Software float (add/sub/mul/div/neg + comparisons + conversion)
- **String pool** -- 2KB static buffer for runtime string operations (itos, ftos, concat, parse)
- **Touch input** -- Debounced press/hold/release events, hit testing, on_click and on_tap callbacks
- **Custom paint** -- on_paint callback for widget custom drawing
- **Flash filesystem** -- TOC-based, named resources (fonts, images, programs, pages)
- **Font rendering** -- Adafruit GFX compatible bitmap fonts (flash + embedded)
- **Image format** -- Ferrite Image (FI): raw/RLE/indexed+RLE, streaming decode
- **Page manager** -- Multiple full-screen pages, show/hide, flash loading
- **USART protocol** -- Protobuf-style serial communication (ping, execute, flash write, user messages)
- **Backlight PWM** -- 0-100% brightness via hardware timer
- **SysTick timer** -- 1ms tick counter for non-blocking delays

## Ferrite Language

Programs are written in a C-like language (`.fl` files) that compiles to VM bytecode:

```c
var root = alloc();
target(root);
set(size, 800, 480);
set(bg_color, 0x0000);

var btn = alloc();
target(btn);
set(kind, 2);
set(location, 300, 200);
set(size, 200, 80);
set(bg_color, 0xF800);
set(press_color, 0x7800);
set(clickable, 1);
parent(root);

// Drawing primitives
line(0, 0, 799, 479, 0xFFFF);
circle(400, 240, 100, 0x07E0);

// Dynamic text
var count = 42;
var s = concat(str("Count: "), itos(count));
drawStr(10, 10, 0, 0xFFFF, 0x0000, s);

// Float math
var temp = fdiv(fsub(98.6, 32.0), 1.8);
var msg = concat(str("Temp: "), ftos(temp));

halt();
```

See [docs/ferrite-lang.md](docs/ferrite-lang.md) for the full language reference.

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
```

## Building

Requires Rust nightly with the `thumbv7m-none-eabi` target:

```bash
rustup target add thumbv7m-none-eabi
cargo build --release --target thumbv7m-none-eabi
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
| Widget arena (64 widgets) | ~3.5 KB |
| String pool | ~2.1 KB |
| Text pool (labels) | 256 B |
| Clip region (32 rects) | 256 B |
| VM (x2: main + callback) | ~300 B |
| Array pool (per VM) | 256 B |
| USART RX ring buffer | 128 B |

## Project Structure

```
ferrite-ui/
├── src/
│   ├── main.rs          Entry point, startup, main loop
│   ├── vm.rs            Bytecode VM (55 opcodes, builtins, f32)
│   ├── widget.rs        Widget tree, arena allocator, box model
│   ├── render.rs        Painter's algorithm, dirty redraw, clip
│   ├── lcd.rs           FPGA display protocol, drawing primitives
│   ├── clip.rs          Clip region (rect subtract algorithm)
│   ├── touch.rs         XPT2046 driver, hit test, debounce
│   ├── flash.rs         W25Q256 SPI flash driver
│   ├── font.rs          Adafruit GFX bitmap font renderer
│   ├── image.rs         Ferrite Image (FI) decoder
│   ├── fs.rs            Flash filesystem (TOC, named resources)
│   ├── page.rs          Page manager (multi-page UI)
│   ├── strpool.rs       Static string pool (2KB, itos/ftos/concat)
│   ├── systick.rs       SysTick 1ms timer
│   ├── callback.rs      Callback metadata (function table)
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
│   └── ferrite_img.py   PNG to Ferrite Image converter
├── docs/
│   └── ferrite-lang.md  Language reference
├── memory.x             Linker script (128KB Flash, 20KB RAM)
└── CLAUDE.md            Internal development notes
```

## Architecture

- **No heap, no allocator** -- all memory is statically allocated
- **No OS, no runtime** -- `#![no_std]`, `#![no_main]`, `cortex-m-rt` entry point
- **No frame buffer in CPU RAM** -- pixels are written directly to the FPGA over a 16-bit GPIO bus
- **Dirty redraw only** -- the clip-based painter's algorithm redraws only changed widgets
- **Bytecode VM** -- UI logic runs as bytecode programs, uploaded via USART or loaded from flash

## License

This project is an independent, clean-room implementation. It is not affiliated with or endorsed by ITEAD (Nextion).
