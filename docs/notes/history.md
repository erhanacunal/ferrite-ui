# Feature history

The original bring-up checklist (moved from CLAUDE.md).

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
