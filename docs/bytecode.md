# VM Bytecode Reference

This document describes the Ferrite VM image format and opcode set used by
`src/vm.rs`. The compiler and disassembler live in `tools/ferrite_lang.py` and
`tools/ferrite_cc.py`.

## VM Model

- Instruction width: 1-byte opcode plus fixed-size immediate operands.
- Value type: signed `i32`. Float values are stored as IEEE-754 `f32` bits in an
  `i32` stack slot.
- Eval stack: 16 entries.
- Call stack: 8 frames.
- Variables: global slots plus stack-frame local slots. Slot bytes with bit 7
  set refer to locals (`0x80 + local_index`); other slot bytes refer to globals.
- Arrays: heap-backed VM arrays of `i32`.
- Widgets: VM keeps a current widget target for `W_SET`, `W_GET`, `W_PARENT`,
  `W_DIRTY`, and `W_RENDER`.

Multi-byte immediates are little-endian. Jump and call targets are byte offsets
inside the opcode stream, not the full image file.

## Image Format

Current compiler output is image version 4:

```text
offset  size  field
0       1     version = 4
1       2     function_count, u16 LE
3       2     global_count, u16 LE
5       2     flags, u16 LE
7       1     widget_count
8       1     ext_count
9       ...   function table, function_count * 12 bytes
...     ...   opcode stream
```

Flags:

| Bit | Meaning |
| --- | --- |
| `0` | Render mode: `0` = dirty, `1` = buffered |

Function table entry:

```text
offset  size  field
0       2     func_id, u16 LE
2       1     function kind
3       1     padding
4       4     offset into opcode stream, u32 LE
8       4     function length in bytes, u32 LE
```

Function kinds:

| Value | Kind |
| --- | --- |
| `0` | `setup` |
| `1` | `loop` |
| `2` | user function |
| `3` | `on_program_start` |
| `4` | `on_page_changing` |
| `5` | `on_page_changed` |
| `6` | `on_user_message` |
| `7` | `on_touch_down` |
| `8` | `on_touch_up` |
| `9` | `on_touch_move` |

Older images are still parsed:

- v1/v2: 5-byte base header.
- v3: adds `flags`.
- v4: adds `widget_count` and `ext_count`.

## Core Opcodes

### Stack, Arithmetic, Logic, Control

| Opcode | Mnemonic | Operands | Stack effect / behavior |
| --- | --- | --- | --- |
| `0x00` | `HALT` | - | stop VM |
| `0x01` | `POP` | - | pop one value |
| `0x02` | `DUP` | - | duplicate top value |
| `0x03` | `SWAP` | - | swap top two values |
| `0x04` | `ADD` | - | `a b -> a+b` |
| `0x05` | `SUB` | - | `a b -> a-b` |
| `0x06` | `MUL` | - | `a b -> a*b` |
| `0x07` | `DIV` | - | `a b -> a/b`, pushes `0` on divide by zero |
| `0x08` | `MOD` | - | `a b -> a%b`, pushes `0` on divide by zero |
| `0x09` | `NEG` | - | `a -> -a` |
| `0x0A` | `AND` | - | boolean `&&`, result `0` or `1` |
| `0x0B` | `OR` | - | boolean `||`, result `0` or `1` |
| `0x0C` | `NOT` | - | boolean not |
| `0x0D` | `EQ` | - | equality comparison |
| `0x0E` | `NE` | - | inequality comparison |
| `0x0F` | `LT` | - | signed less-than |
| `0x10` | `LE` | - | signed less-or-equal |
| `0x11` | `GT` | - | signed greater-than |
| `0x12` | `GE` | - | signed greater-or-equal |
| `0x13` | `RET` | - | return from call/callback |
| `0x14` | `YIELD` | - | yield to main loop |
| `0x35` | `JMP` | `u16 target` | set `pc = target` |
| `0x36` | `JZ` | `u16 target` | pop condition; jump if zero |
| `0x37` | `JNZ` | `u16 target` | pop condition; jump if nonzero |
| `0x38` | `CALL` | `u16 target` | call function at target |
| `0x1D` | `FRAME` | `u8 local_count` | function prologue; reserves locals |
| `0x9F` | `critical` | - | run without cooperative yielding until next explicit yield |

### Constants And Variables

| Opcode | Mnemonic | Operands | Stack effect / behavior |
| --- | --- | --- | --- |
| `0x20` | `PUSH_0` | - | push `0` |
| `0x21` | `PUSH_1` | - | push `1` |
| `0x22` | `PUSH_2` | - | push `2` |
| `0x23` | `PUSH_M1` | - | push `-1` |
| `0x24` | `LOAD_0` | - | push local `0` |
| `0x25` | `LOAD_1` | - | push local `1` |
| `0x26` | `LOAD_2` | - | push local `2` |
| `0x27` | `LOAD_3` | - | push local `3` |
| `0x28` | `LOAD_4` | - | push local `4` |
| `0x29` | `STORE_0` | - | pop into local `0` |
| `0x2A` | `STORE_1` | - | pop into local `1` |
| `0x2B` | `STORE_2` | - | pop into local `2` |
| `0x2C` | `STORE_3` | - | pop into local `3` |
| `0x2D` | `STORE_4` | - | pop into local `4` |
| `0x30` | `PUSH_I8` | `i8` | push signed 8-bit immediate |
| `0x31` | `PUSH_I16` | `i16 LE` | push signed 16-bit immediate |
| `0x32` | `PUSH_I32` | `i32 LE` | push signed 32-bit immediate |
| `0x33` | `LOAD` | `u8 slot` | push global/local slot |
| `0x34` | `STORE` | `u8 slot` | pop into global/local slot |

### Widgets

| Opcode | Mnemonic | Operands | Stack effect / behavior |
| --- | --- | --- | --- |
| `0x15` | `W_DIRTY` | - | mark current target dirty |
| `0x16` | `W_RENDER` | - | render dirty widgets |
| `0x1A` | `W_ALLOC` | - | allocate widget, push widget id |
| `0x1C` | `W_ALLTAR` | `u8 slot` | allocate widget, store id in slot, set as target |
| `0x39` | `W_TARGET` | `u8 widget_id` | set fixed widget target |
| `0x42` | `W_TARGET_S` | - | pop widget id and set target |
| `0x3A` | `W_SET` | `u8 prop_id` | pop value and set scalar property |
| `0x3B` | `W_GET` | `u8 prop_id` | push scalar property value |
| `0x3C` | `W_PARENT` | `u8 parent_id` | parent current target under fixed parent |
| `0x3D` | `W_SET_LEN` | `u8 prop_id, u8 len, bytes` | set compound/text property |

`W_SET_LEN` uses either UTF-8 bytes for `TEXT` or packed ZigZag varints for
compound numeric properties.

### Arrays And Flash

| Opcode | Mnemonic | Operands | Stack effect / behavior |
| --- | --- | --- | --- |
| `0x17` | `ARR_LOAD` | - | pop index, pop array id, push value |
| `0x18` | `ARR_STORE` | - | pop value, index, array id |
| `0x19` | `ARR_LEN` | - | pop array id, push length |
| `0x1B` | `ARR_FREE` | - | pop array id and free it |
| `0x3E` | `ARR_ALLOC` | `u8 size` | allocate zero-filled array, push array id |
| `0x3F` | `ARR_INIT` | `u8 count, count*i32 LE` | allocate initialized array, push array id |
| `0x40` | `F_READ` | `u32 addr, u16 len` | read flash bytes into array, push array id |
| `0x41` | `F_WRITE` | `u32 addr, u8 len, bytes` | write inline bytes to flash |

## Builtin Opcodes

Builtins are single-byte opcodes. Operands are passed on the stack unless noted.

| Opcode | Name | Behavior |
| --- | --- | --- |
| `0x80` | `fillRect` | draw filled rectangle |
| `0x81` | `rect` | draw rectangle outline |
| `0x82` | `line` | draw line |
| `0x83` | `circle` | draw circle outline |
| `0x84` | `fillCircle` | draw filled circle |
| `0x85` | `drawImage` | draw image resource |
| `0x86` | `drawTextLit` | `u8 len, bytes`; draw inline literal text |
| `0x87` | `delay` | delay milliseconds |
| `0x88` | `strLit` | `u8 len, bytes`; allocate literal string, push string id |
| `0x89` | `itos` | int to string id |
| `0x8A` | `ftos` | float bits to string id |
| `0x8B` | `concat` | concatenate two string ids, push new string id |
| `0x8C` | `parseInt` | parse string id as integer |
| `0x8D` | `parseFloat` | parse string id as float bits |
| `0x8E` | `strLen` | push string length |
| `0x8F` | `setText` | pop string id, set target text |
| `0x90` | `drawStr` | draw string-pool string |
| `0x91` | `strClear` | clear temporary strings, preserving widget text |
| `0x92` | `strFree` | free string id |
| `0x93` | `roundedRect` | draw rounded rectangle outline |
| `0x94` | `fillRoundedRect` | draw filled rounded rectangle |
| `0x95` | `arc` | draw arc |
| `0x96` | `beginFrame` | begin explicit frame; no-op in framework buffered mode |
| `0x97` | `endFrame` | end explicit frame; no-op in framework buffered mode |
| `0x98` | `sendUsart` | send one byte/debug byte |
| `0x99` | `sendUsartStr` | send string bytes/debug string |
| `0x9A` | `rtcRead` | push array `[sec,min,hour,day,weekday,month,year]` |
| `0x9B` | `rtcWrite` | pop array id and write RTC fields |
| `0x9C` | `millis` | push system milliseconds |
| `0x9D` | `fpgaCmd` | pop `cmd`, `data`; send raw FPGA command/data |
| `0x9E` | `fpgaData` | pop data; send raw FPGA data |
| `0xA0` | `setBrightness` | pop percent and set LCD backlight |
| `0xA1` | `brightness` | push current backlight percent |
| `0xA2` | `fileOpen` | pop string id, push handle `1`, `2`, or `0xFF` |
| `0xA3` | `fileRead` | pop handle, push byte `0..255` or `-1` on EOF |
| `0xA4` | `fileSize` | pop handle, push file size |
| `0xA5` | `fileClose` | pop handle and release file slot |
| `0xA6` | `arrToStr` | pop length and array id; allocate string from low bytes |
| `0xA7` | `showModal` | pop `click_fn`, `builder_fn`; suspend until `setDialogResult`; push result |
| `0xA8` | `setDialogResult` | pop result and record on innermost modal frame |
| `0xA9` | `sprintf` | `u8 argc`; pop `argc` args then fmt str_id; push formatted str_id |

`sprintf` has an inline `u8 argc` byte (number of format arguments after the format string). Stack layout before execution: `[... fmt, arg0, arg1, ..., argN-1]` (fmt pushed first).

## Float Opcodes

Float operands and results are `f32` bit patterns stored in `i32` slots.

| Opcode | Name | Behavior |
| --- | --- | --- |
| `0xC0` | `itof` | int to float bits |
| `0xC1` | `ftoi` | float bits to int |
| `0xC2` | `fadd` | `a + b` |
| `0xC3` | `fsub` | `a - b` |
| `0xC4` | `fmul` | `a * b` |
| `0xC5` | `fdiv` | `a / b`, pushes `0.0` on divide by zero |
| `0xC6` | `fneg` | `-a` |
| `0xC7` | `feq` | float equality |
| `0xC8` | `flt` | float less-than |
| `0xC9` | `fle` | float less-or-equal |
| `0xCA` | `fgt` | float greater-than |
| `0xCB` | `fge` | float greater-or-equal |
| `0xCC` | `fne` | float inequality |

## Float Math Opcodes

Trig, square root, and rounding. All operate on f32 bit patterns. Integer stack values must be converted to float first (`itof`). The compiler handles auto-promotion automatically when these functions are called from Ferrite programs.

| Opcode | Name | Behavior |
| --- | --- | --- |
| `0xCD` | `fsin` | `sin(a)` — input in radians |
| `0xCE` | `fcos` | `cos(a)` — input in radians |
| `0xCF` | `fsqrt` | `sqrt(a)` |
| `0xD0` | `fabs` | `|a|` — absolute value |
| `0xD1` | `fatan2` | pop `x`, pop `y`; push `atan2(y, x)` in radians |
| `0xD2` | `ffloor` | round toward −∞ |
| `0xD3` | `fceil` | round toward +∞ |

## Widget Properties

Scalar properties are used by `W_SET` and `W_GET`.

| ID | Name | Notes |
| --- | --- | --- |
| `0x01` | `LOC_X` | widget `location.x` |
| `0x02` | `LOC_Y` | widget `location.y` |
| `0x03` | `SIZE_W` | widget width |
| `0x04` | `SIZE_H` | widget height |
| `0x05` | `VISIBLE` | flag |
| `0x06` | `ENABLED` | flag |
| `0x07` | `CLICKABLE` | flag |
| `0x08` | `BG_COLOR` | RGB565 |
| `0x09` | `BORDER_COLOR` | RGB565 |
| `0x0E` | `KIND` | widget kind |
| `0x0F` | `TEXT_COLOR` | RGB565 |
| `0x10..0x13` | `MARGIN_T/R/B/L` | edge fields |
| `0x14..0x17` | `BORDER_T/R/B/L` | edge fields |
| `0x18..0x1B` | `PADDING_T/R/B/L` | edge fields |
| `0x1C` | `FONT_ID` | font resource id |
| `0x1D` | `TEXT_ALIGN` | `0=left`, `1=center`, `2=right` |
| `0x1E` | `PRESS_COLOR` | RGB565 |
| `0x1F` | `IMAGE_ID` | image resource id |
| `0x20` | `ON_CLICK` | function id |
| `0x21` | `ON_PAINT` | function id |
| `0x22` | `ON_TAP` | function id |
| `0x23` | `BORDER_RADIUS` | pixels |
| `0x24` | `VALUE` | widget-specific signed value |
| `0x25` | `CHECKED` | flag |
| `0x26` | `MAX_LENGTH` | input max length |
| `0x27` | `CURSOR_POS` | aliases extension `value` |
| `0x28` | `ON_CHANGE` | aliases `ON_TAP` storage for input |
| `0x29` | `SCROLL_Y` | aliases extension `value` |
| `0x2A` | `CLIP_CHILDREN` | scroll/clip flag |
| `0x2B` | `GRADIENT_COLOR` | RGB565 gradient end color |
| `0x2C` | `GRADIENT_DIR` | `0=none`, `1=horizontal`, `2=vertical` |

Compound properties are used by `W_SET_LEN`:

| ID | Name | Payload |
| --- | --- | --- |
| `0x40` | `LOCATION` | two packed ZigZag varints: `x`, `y` |
| `0x41` | `SIZE` | two packed ZigZag varints: `w`, `h` |
| `0x42` | `MARGIN` | four packed ZigZag varints: `top`, `right`, `bottom`, `left` |
| `0x43` | `BORDER_EDGES` | four packed ZigZag varints |
| `0x44` | `PADDING` | four packed ZigZag varints |
| `0x45` | `TEXT` | UTF-8 bytes; allocated in `StringPool` |

## Widget Kinds

| Value | Kind |
| --- | --- |
| `0` | Base/container |
| `1` | Label |
| `2` | Button |
| `3` | Progress |
| `4` | Slider |
| `5` | Checkbox |
| `6` | Radio |
| `7` | Input |
| `8` | Gauge |
| `9` | Dropdown |

## Disassembly

Use the compiler tool to inspect bytecode:

```bash
python tools/ferrite_lang.py examples/dropdown/main.fl --disasm
python tools/ferrite_cc.py disasm program.bin
```
