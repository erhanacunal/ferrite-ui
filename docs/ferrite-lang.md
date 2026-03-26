# Ferrite Language Reference

Ferrite Language (`.fl`) is a simple C-like language that compiles to VM bytecode for the ferrite-ui bare-metal HMI framework. It runs on a GD32F103 (Cortex-M3, 108MHz) driving an 800x480 LCD.

## Toolchain

```bash
# Compile and print disassembly
python ferrite_lang.py source.fl --disasm

# Compile to binary
python ferrite_lang.py source.fl -o program.bin

# Compile as page (with background color)
python ferrite_lang.py source.fl --page 0x0000 -o page_main.bin

# Upload to device
python ferrite_cli.py -p COM3 execute program.bin
```

## Program Structure

A ferrite program consists of **function definitions** and **top-level statements**. Functions are defined first (order doesn't matter), then top-level statements execute sequentially as the main program.

```c
// Functions are defined at the top
fn helper(x, y) {
    return x + y;
}

// Top-level statements = main program
var result = helper(10, 20);
halt();
```

The program runs on a stack-based VM with 16 variable slots (shared between main code and function calls) and a 16-deep eval stack.

## Comments

```c
// Single-line comment

/* Multi-line
   comment */
```

## Variables

Variables are declared with `var`. The VM has 16 variable slots total (shared between main program and function parameters).

```c
var x = 0;          // integer, initialized to 0
var color = 0xF800;  // hex literal (red in RGB565)
var mask = 0b1010;   // binary literal
var big = 1_000_000; // underscores for readability
```

All values are **signed 32-bit integers** (`i32`).

### Arrays

Arrays are allocated from a fixed pool (64 elements total, max 8 arrays).

```c
var colors[4] = [0xF800, 0x07E0, 0x001F, 0xFFE0];  // init with values
var buffer[16];                                       // zero-filled
var dynamic[8] = [1, x * 2, y + 1, 0, 0, 0, 0, 0]; // mixed const + expr

// Access
var first = colors[0];
colors[2] = 0xFFFF;
```

## Data Types

There is only one data type: **i32** (signed 32-bit integer). Colors are RGB565 unsigned 16-bit values stored in i32.

### Number Literals

```c
42          // decimal
0xFF00      // hexadecimal
0b1100_0011 // binary
1_000_000   // decimal with underscores
```

### Boolean Values

`true` evaluates to `1`, `false` to `0`. Any non-zero value is truthy.

```c
var enabled = true;   // 1
var disabled = false;  // 0
```

### String Literals

String literals are only used as arguments to `drawText()`. They cannot be assigned to variables.

```c
drawText(10, 20, 0, 0xFFFF, 0x0000, "Hello World");
```

Escape sequences: `\\`, `\"`, `\n`, `\t`.

### RGB565 Colors

The display uses 16-bit RGB565 color format. Common colors:

| Color   | Value    | RGB565 |
|---------|----------|--------|
| Black   | `0x0000` | R=0, G=0, B=0 |
| White   | `0xFFFF` | R=31, G=63, B=31 |
| Red     | `0xF800` | R=31, G=0, B=0 |
| Green   | `0x07E0` | R=0, G=63, B=0 |
| Blue    | `0x001F` | R=0, G=0, B=31 |
| Yellow  | `0xFFE0` | R=31, G=63, B=0 |
| Cyan    | `0x07FF` | R=0, G=63, B=31 |
| Magenta | `0xF81F` | R=31, G=0, B=31 |

You can write a helper to compute RGB565 at runtime:

```c
fn rgb565(r, g, b) {
    return (r & 0xF8) * 256 + (g & 0xFC) * 8 + b / 8;
}

var orange = rgb565(255, 165, 0);
```

## Operators

### Arithmetic

| Operator | Description | Example |
|----------|-------------|---------|
| `+` | Addition | `a + b` |
| `-` | Subtraction | `a - b` |
| `*` | Multiplication | `a * b` |
| `/` | Division (integer) | `a / b` |
| `%` | Modulo | `a % b` |
| `-` | Negation (unary) | `-x` |

### Comparison

| Operator | Description | Example |
|----------|-------------|---------|
| `==` | Equal | `a == b` |
| `!=` | Not equal | `a != b` |
| `<`  | Less than | `a < b` |
| `<=` | Less or equal | `a <= b` |
| `>`  | Greater than | `a > b` |
| `>=` | Greater or equal | `a >= b` |

### Logical

| Operator | Description | Example |
|----------|-------------|---------|
| `&&` | Logical AND (short-circuit) | `a && b` |
| `\|\|` | Logical OR (short-circuit) | `a \|\| b` |
| `!`  | Logical NOT | `!a` |

### Bitwise

| Operator | Description | Example |
|----------|-------------|---------|
| `&` | Bitwise AND | `a & b` |
| `\|` | Bitwise OR | `a \| b` |

## Control Flow

### if / else

```c
if (x > 10) {
    set(bg_color, 0xF800);
}

if (count == 0) {
    set(visible, 0);
} else {
    set(visible, 1);
}

// Chained
if (mode == 1) {
    set(bg_color, 0xF800);
} else if (mode == 2) {
    set(bg_color, 0x07E0);
} else {
    set(bg_color, 0x001F);
}
```

### while

```c
var i = 0;
while (i < 10) {
    target(btn);
    set(bg_color, colors[i]);
    dirty();
    render();
    delay(100);
    i = i + 1;
}
```

### for

```c
for (var i = 0; i < 4; i = i + 1) {
    target(btn);
    set(bg_color, colors[i]);
    dirty();
    render();
}
```

### break / continue

```c
var i = 0;
while (true) {
    if (i >= 10) { break; }
    if (i % 2 == 0) {
        i = i + 1;
        continue;
    }
    // odd numbers only
    i = i + 1;
}
```

## Functions

Functions are defined with `fn`. Parameters are passed by value. Functions can return a value with `return` (implicit `return 0` if omitted).

```c
fn clamp(val, lo, hi) {
    if (val < lo) { return lo; }
    if (val > hi) { return hi; }
    return val;
}

fn max(a, b) {
    if (a > b) { return a; }
    return b;
}

var x = clamp(500, 0, 255);
var bigger = max(10, 20);
```

The VM has an 8-deep call stack, so recursion depth is limited.

## Widget System

Widgets are the UI building blocks. They form a tree structure (parent-child) and use a CSS-like box model with margin, border, padding, and content area.

### Widget Lifecycle

```c
// 1. Allocate
var btn = alloc();

// 2. Set target (all property operations apply to the target)
target(btn);

// 3. Configure properties
set(location, 100, 50);
set(size, 200, 60);
set(bg_color, 0x001F);
set(border, 2, 2, 2, 2);
set(border_color, 0xFFFF);

// 4. Attach to parent
parent(root);

// 5. Mark dirty and render
dirty();
render();
```

### alloc()

Allocates a new widget from the arena (max 64 widgets). Returns the widget ID.

```c
var panel = alloc();
```

### target(widget)

Sets the target widget. All subsequent `set()`, `get()`, `dirty()`, `parent()` calls operate on this target.

```c
target(panel);
set(bg_color, 0xF800);  // sets panel's background to red
```

### parent(widget)

Attaches the current target widget as a child of the given parent.

```c
var root = alloc();
target(root);
set(size, 800, 480);

var child = alloc();
target(child);
set(location, 10, 10);
set(size, 100, 50);
parent(root);  // child is now inside root
```

### set(property, value...)

Sets a property on the current target widget.

**Scalar properties** (single value):

```c
set(bg_color, 0xF800);     // background color
set(border_color, 0xFFFF);  // border color
set(text_color, 0x0000);    // text color (for labels)
set(visible, 1);            // show/hide
set(enabled, 1);            // enable/disable
set(clickable, 1);          // make clickable (receives touch events)
set(kind, 1);               // 0=base, 1=label, 2=button
set(font_id, 0);            // font index (0 = embedded font)
set(text_align, 1);         // 0=left, 1=center, 2=right
set(press_color, 0x7BEF);   // button press highlight color
set(image_id, 1);           // background image (from flash)
```

**Compound properties** (multiple values):

```c
set(location, 100, 50);         // x, y position
set(size, 200, 60);             // width, height
set(margin, 5, 5, 5, 5);       // top, right, bottom, left
set(border, 2, 2, 2, 2);       // top, right, bottom, left
set(padding, 10, 10, 10, 10);  // top, right, bottom, left
```

**Individual edge properties:**

```c
set(margin_top, 10);
set(margin_right, 5);
set(border_left, 3);
set(padding_bottom, 8);
```

### get(property)

Reads a property value from the current target widget. Returns the value.

```c
target(btn);
var w = get(width);
var h = get(height);
var color = get(bg_color);
```

### dirty()

Marks the current target widget (and its subtree) as needing redraw.

```c
target(btn);
set(bg_color, 0xF800);
dirty();   // mark for redraw
render();  // actually redraw dirty widgets
```

### render()

Triggers a partial redraw of all dirty widgets using the clip-based painter's algorithm.

```c
dirty();
render();  // only redraws what changed
```

### halt()

Stops the VM. Use at the end of page-building programs.

```c
// Build UI...
halt();
```

### yield_op()

Yields execution back to the main loop for one cycle. The VM resumes next iteration. Useful in long-running programs to allow touch handling and USART processing.

```c
while (true) {
    // do work...
    yield_op();  // let main loop handle events
}
```

## Drawing Primitives

Drawing primitives render directly to the LCD framebuffer, bypassing the widget system. They are useful for custom graphics in `on_paint` callbacks or standalone drawing programs.

All coordinates are in screen pixels (0,0 = top-left, 799x479 = bottom-right).

### fillRect(x, y, w, h, color)

Draws a filled rectangle.

```c
// Black background
fillRect(0, 0, 800, 480, 0x0000);

// Red square at center
fillRect(350, 190, 100, 100, 0xF800);
```

### rect(x, y, w, h, color)

Draws a rectangle outline (1px border).

```c
// White border rectangle
rect(50, 50, 200, 100, 0xFFFF);

// Selection highlight
rect(10, 10, 780, 460, 0x07E0);
```

### line(x0, y0, x1, y1, color)

Draws a line between two points using Bresenham's algorithm.

```c
// Diagonal line
line(0, 0, 799, 479, 0xFFFF);

// Horizontal line
line(100, 240, 700, 240, 0xF800);

// Vertical line
line(400, 50, 400, 430, 0x07E0);
```

### circle(cx, cy, r, color)

Draws a circle outline using the midpoint circle algorithm.

```c
// Circle at center of screen
circle(400, 240, 100, 0xFFFF);

// Small indicator dot outline
circle(50, 50, 10, 0x07E0);
```

### fillCircle(cx, cy, r, color)

Draws a filled circle.

```c
// Filled circle
fillCircle(400, 240, 100, 0x001F);

// Small dot
fillCircle(50, 50, 5, 0xF800);
```

### drawImage(x, y, image_id)

Draws a flash-stored image at the given position. The `image_id` must match an image loaded in the flash filesystem (Ferrite Image format).

```c
// Draw image #1 at top-left
drawImage(0, 0, 1);

// Draw icon at button position
drawImage(110, 55, 2);
```

### drawText(x, y, font_id, fg, bg, "text")

Draws a text string at the given position. The text argument must be a string literal (quoted).

- `font_id`: 0 = embedded font (FreeMono 9pt), 1+ = flash-loaded fonts
- `fg`: foreground (text) color
- `bg`: background color, 0 = transparent

```c
// White text on black background
drawText(10, 30, 0, 0xFFFF, 0x0000, "Hello World");

// Red text, transparent background
drawText(100, 100, 0, 0xF800, 0, "Warning!");

// Using flash font #1
drawText(200, 200, 1, 0x07E0, 0x0000, "Status: OK");
```

### delay(ms)

Pauses the VM for the given number of milliseconds. **This is non-blocking** -- the main loop continues to process touch events and USART messages while the VM waits.

```c
// Simple animation: flash a rectangle
for (var i = 0; i < 5; i = i + 1) {
    fillRect(300, 200, 200, 80, 0xF800);  // red
    delay(500);
    fillRect(300, 200, 200, 80, 0x0000);  // black
    delay(500);
}
```

## Float32 Operations

The VM supports 32-bit floating-point arithmetic via software emulation (Cortex-M3 has no FPU). Floats are stored on the i32 stack as their IEEE 754 bit representation.

### Float Literals

Float literals use a decimal point:

```c
var pi = 3.14159;
var half = 0.5;
var temp = 23.7;
```

Float literals are automatically converted to their f32 bit pattern at compile time. They can be stored in regular variables and passed to float functions.

### Conversion

#### itof(value)

Converts an integer to a float.

```c
var count = 42;
var f_count = itof(count);    // 42 -> 42.0 (as f32 bits)
```

#### ftoi(value)

Converts a float to an integer (truncates toward zero).

```c
var f = 3.14159;
var i = ftoi(f);   // -> 3

var neg = ftoi(-2.7);  // -> -2
```

### Arithmetic

#### fadd(a, b)

Float addition.

```c
var a = 1.5;
var b = 2.3;
var sum = fadd(a, b);   // 3.8
```

#### fsub(a, b)

Float subtraction (a - b).

```c
var delta = fsub(10.0, 3.5);   // 6.5
```

#### fmul(a, b)

Float multiplication.

```c
var area = fmul(3.14159, fmul(r, r));   // pi * r^2

// Scale an integer value
var pct = 0.75;
var scaled = ftoi(fmul(itof(total), pct));
```

#### fdiv(a, b)

Float division (a / b). Returns 0.0 if b is zero.

```c
var ratio = fdiv(itof(part), itof(whole));

// Temperature conversion: C = (F - 32) * 5/9
var celsius = fmul(fsub(fahrenheit, 32.0), fdiv(5.0, 9.0));
```

#### fneg(a)

Float negation.

```c
var x = 3.14;
var neg_x = fneg(x);   // -3.14
```

### Comparison

Float comparisons return an integer (0 or 1) that can be used directly in `if` and `while` conditions.

#### feq(a, b) / fne(a, b)

```c
if (feq(x, 0.0)) {
    // x is exactly zero
}

if (fne(a, b)) {
    // a and b differ
}
```

#### flt(a, b) / fle(a, b) / fgt(a, b) / fge(a, b)

```c
var temp = 23.7;

if (fgt(temp, 30.0)) {
    set(bg_color, 0xF800);  // red = hot
} else if (flt(temp, 10.0)) {
    set(bg_color, 0x001F);  // blue = cold
} else {
    set(bg_color, 0x07E0);  // green = ok
}
```

### Complete Float Example

A gauge that interpolates color from green to red based on a percentage:

```c
fn lerp(a, b, t) {
    // a + (b - a) * t  where t is 0.0..1.0
    return fadd(a, fmul(fsub(b, a), t));
}

fn gauge_color(pct) {
    // pct: 0.0 = green, 1.0 = red
    var r = ftoi(fmul(lerp(0.0, 31.0, pct), 1.0));
    var g = ftoi(lerp(63.0, 0.0, pct));
    // RGB565: (r << 11) | (g << 5)
    return r * 2048 + g * 32;
}

// Draw gauge at 75%
var pct = 0.75;
var color = gauge_color(pct);
var bar_w = ftoi(fmul(itof(400), pct));
fillRect(100, 200, bar_w, 30, color);
```

### Performance Note

All float operations use software emulation (~20-70 cycles per op at 108MHz). This is fast enough for UI calculations but avoid tight inner loops with heavy float math.

## String Operations

The VM has a static string pool (2KB buffer, 16 string slots) for runtime string manipulation. Strings are immutable once created. The pool is append-only -- use `strClear()` to reset it when needed.

String IDs are regular integers stored in variables. They reference data in the global pool.

### str("literal")

Creates a string from a literal and returns a string ID.

```c
var greeting = str("Hello");
var unit = str(" C");
```

### itos(value)

Converts an integer to its decimal string representation.

```c
var count = 42;
var s = itos(count);     // "42"

var neg = itos(-7);      // "-7"
```

### ftos(value)

Converts a float to its string representation (2 decimal places).

```c
var temp = 23.7;
var s = ftos(temp);      // "23.70"

var pi = 3.14159;
var s2 = ftos(pi);       // "3.14"
```

### concat(a, b)

Concatenates two strings. Returns a new string ID.

```c
var name = str("Temperature: ");
var val = itos(25);
var unit = str(" C");

var temp_str = concat(name, val);      // "Temperature: 25"
var full = concat(temp_str, unit);     // "Temperature: 25 C"
```

### parseInt(str_id)

Parses a string as an integer. Supports decimal and hex (`0x` prefix). Returns 0 on failure.

```c
var s = str("42");
var n = parseInt(s);       // 42

var hex = str("0xFF");
var h = parseInt(hex);     // 255
```

### parseFloat(str_id)

Parses a string as a float. Returns f32 bits (use with float operations).

```c
var s = str("3.14");
var f = parseFloat(s);     // f32 bits of 3.14

var temp = fmul(f, 2.0);  // 6.28
```

### strLen(str_id)

Returns the byte length of a string.

```c
var s = str("Hello");
var len = strLen(s);       // 5
```

### setText(str_id)

Sets the text of the current target widget (label) from a string ID. The string data is copied to the widget's text pool, so the string ID can be reused or freed.

```c
// Display a counter on a label widget
var label = alloc();
target(label);
set(kind, 1);  // KIND_LABEL
set(font_id, 0);
set(text_color, 0xFFFF);
set(size, 200, 30);
parent(root);

// Update label text dynamically
var count = 0;
while (count < 100) {
    var s = itos(count);
    target(label);
    setText(s);
    dirty();
    render();
    delay(100);
    count = count + 1;
    strClear();  // reclaim pool space each iteration
}
```

### drawStr(x, y, font_id, fg, bg, str_id)

Draws a string from the pool directly to the LCD at the given position. Unlike `drawText()` which takes a string literal, `drawStr()` takes a string ID for dynamic content.

- `font_id`: 0 = embedded font, 1+ = flash fonts
- `fg`: foreground color
- `bg`: background color (0 = transparent)

```c
var fps = itos(60);
var label = concat(fps, str(" FPS"));
drawStr(10, 460, 0, 0x07E0, 0x0000, label);   // "60 FPS" in green
```

### strClear()

Clears the string pool while **preserving strings referenced by widget text**. Temporary strings (from `itos`, `concat`, etc.) are freed, but any string assigned to a widget via `set(text, ...)` or `setText()` survives.

This works by scanning all widget `text_id` fields, compacting survivors to the front of the pool, and updating widget references. It's safe to call in a loop without losing label text.

```c
// Label text survives strClear — no need to re-set it each iteration
var label = alloc();
target(label);
set(kind, 1);
set(text, "Permanent title");  // this string survives strClear

var i = 0;
while (true) {
    var s = concat(str("Count: "), itos(i));  // temp strings
    drawStr(10, 50, 0, 0xFFFF, 0x0000, s);
    delay(100);
    i = i + 1;
    strClear();  // frees "Count: ", itos result, concat result
                 // keeps "Permanent title" on the label
}
```

### Complete String Example

A real-time counter display:

```c
var root = alloc();
target(root);
set(size, 800, 480);
set(bg_color, 0x0000);

var label = alloc();
target(label);
set(kind, 1);
set(location, 300, 200);
set(size, 200, 40);
set(bg_color, 0x0000);
set(text_color, 0xFFFF);
set(font_id, 0);
set(text_align, 1);
parent(root);

dirty();
render();

var i = 0;
while (true) {
    var prefix = str("Count: ");
    var num = itos(i);
    var text = concat(prefix, num);

    target(label);
    setText(text);
    dirty();
    render();

    delay(50);
    i = i + 1;
    strClear();
}
```

### strFree(str_id)

Marks a single string for reclamation. The string is discarded on the next `strClear()` call, even if a widget references it. Use this to explicitly release strings you no longer need.

```c
var tmp1 = itos(sensor_value);
var tmp2 = str(" mV");
var msg = concat(tmp1, tmp2);

// Done with intermediates, mark them for cleanup
strFree(tmp1);
strFree(tmp2);

// msg is still usable until strClear
drawStr(10, 10, 0, 0xFFFF, 0x0000, msg);

// Now reclaim — msg stays if not freed, tmp1/tmp2 are gone
strFree(msg);
strClear();
```

`strFree` only marks the string — actual space is reclaimed when `strClear()` compacts the pool. Between calls, freed strings still occupy buffer space but their slots are flagged for removal.

### Pool Limits

| Resource | Limit |
|----------|-------|
| Pool buffer | 2,048 bytes |
| Max strings | 32 simultaneous |
| ftos precision | 2 decimal places |

When the pool is full, string operations set the VM error state. Call `strClear()` in loops to reclaim space -- widget text is automatically preserved.

## Events and Callbacks

Callbacks are functions that the system calls in response to events. They are registered using the Compiler API (ferrite_cc.py) and stored in the `.meta` file alongside the program bytecode.

### Widget Properties for Events

Set these on widgets to link them to callback functions:

```c
set(on_click, 1);   // func_id for click event
set(on_paint, 2);   // func_id for custom paint event
set(on_tap, 3);     // func_id for tap-with-coordinates event
```

The `func_id` values are assigned by the compiler when functions are defined in the callback metadata.

### on_click

Fires when a clickable widget is pressed and released (touch press + release on the same widget). The callback receives the **widget_id** as an argument.

```c
// In the .fl file, the callback function:
fn handle_click(widget_id) {
    target(widget_id);
    var color = get(bg_color);
    if (color == 0xF800) {
        set(bg_color, 0x07E0);
    } else {
        set(bg_color, 0xF800);
    }
    dirty();
    render();
    return 0;
}
```

The widget must have `set(clickable, 1)` and the `on_click` property set to the function's ID.

### on_paint

Fires after a widget is rendered (background, border, image drawn), allowing custom drawing on top. The callback receives the **widget_id** as an argument.

Use drawing primitives (`fillRect`, `line`, `circle`, etc.) inside `on_paint` to draw custom content within the widget's area.

```c
fn draw_gauge(widget_id) {
    // Draw a custom gauge on the widget
    // Get widget position from target
    target(widget_id);
    var x = get(loc_x);
    var y = get(loc_y);
    var w = get(width);
    var h = get(height);

    // Draw gauge background
    fillRect(x + 10, y + 20, w - 20, 30, 0x4208);

    // Draw gauge fill (60%)
    var fill_w = (w - 20) * 60 / 100;
    fillRect(x + 10, y + 20, fill_w, 30, 0x07E0);

    return 0;
}
```

### on_tap

Fires when a widget is tapped, providing the touch coordinates. The callback receives two arguments: **widget_id** and a **packed coordinate** value.

The packed coordinate encodes `(x << 16) | y` -- extract with bitwise operations:

```c
fn handle_tap(widget_id, coords) {
    var x = coords / 65536;       // upper 16 bits
    var y = coords & 0xFFFF;      // lower 16 bits

    // Draw a dot where the user tapped
    fillCircle(x, y, 5, 0xF800);

    return 0;
}
```

### on_user_message

A system-level callback that fires when a UserMessage (USART field 6) is received from the host. The callback receives an **array_id** containing the message bytes.

```c
fn handle_message(arr_id) {
    // Read first byte as command
    var cmd = arr_id[0];

    if (cmd == 1) {
        // Command 1: change background color
        // Bytes 1-2: RGB565 color (high byte, low byte)
        var color = arr_id[1] * 256 + arr_id[2];
        target(0);  // root widget
        set(bg_color, color);
        dirty();
        render();
    }

    return 0;
}
```

The host sends messages using the CLI:

```bash
# Send raw bytes
python ferrite_cli.py -p COM3 send 0x01 0xF8 0x00

# Send text
python ferrite_cli.py -p COM3 send "hello"
```

### System Callbacks

These are registered in the callback metadata (via the Compiler API), not in the `.fl` language directly:

| Callback | Trigger | Arguments |
|----------|---------|-----------|
| `on_program_start` | Program loaded and first page shown | none |
| `on_page_changing` | Before page switch | old_index, new_index |
| `on_page_changed` | After page switch | new_index |
| `on_user_message` | USART field 6 received | array_id |

## Compiler API (Python)

For advanced use cases (callback registration, page building, meta generation), use the Python Compiler class directly:

```python
from ferrite_cc import Compiler, Prop, rgb565

cc = Compiler(base_id=1)  # base_id=1: root is widget 0

# Define callback functions FIRST (before main code)
fid_click = cc.define_func("on_btn_click", arg_count=1)
# ... callback body (use cc.asm for low-level ops) ...
cc.ret()

fid_paint = cc.define_func("on_custom_paint", arg_count=1)
cc.ret()

fid_msg = cc.define_func("on_msg", arg_count=1)
cc.ret()

# Main code: build widgets
cc.alloc("panel")
cc.target("panel")
cc.set_prop("size", 800, 480)
cc.set_prop("bg_color", 0x0000)
cc.set_prop("on_click", fid_click)
cc.set_prop("on_paint", fid_paint)

cc.halt()

# Register system callbacks
cc.on_program_start("on_btn_click")
cc.on_user_message("on_msg")

# Save bytecode and metadata
cc.save("main.bin")
with open("main.meta", "wb") as f:
    f.write(cc.build_meta())
```

## Complete Example

A full program that creates a button panel with click handling and custom drawing:

```c
// Color helper
fn rgb565(r, g, b) {
    return (r & 0xF8) * 256 + (g & 0xFC) * 8 + b / 8;
}

// --- Build UI ---

// Root widget (full screen)
var root = alloc();
target(root);
set(size, 800, 480);
set(bg_color, 0x0000);

// Header bar
var header = alloc();
target(header);
set(location, 0, 0);
set(size, 800, 60);
set(bg_color, 0x10A2);
parent(root);

// Title label
var title = alloc();
target(title);
set(kind, 1);  // KIND_LABEL
set(location, 10, 5);
set(size, 780, 50);
set(bg_color, 0x10A2);
set(text_color, 0xFFFF);
set(font_id, 0);
set(text_align, 1);  // center
parent(header);

// Button row
var btn1 = alloc();
target(btn1);
set(kind, 2);  // KIND_BUTTON
set(location, 50, 150);
set(size, 200, 80);
set(bg_color, 0xF800);
set(press_color, 0x7800);
set(border, 2, 2, 2, 2);
set(border_color, 0xFFFF);
set(clickable, 1);
parent(root);

var btn2 = alloc();
target(btn2);
set(kind, 2);
set(location, 300, 150);
set(size, 200, 80);
set(bg_color, 0x07E0);
set(press_color, 0x03E0);
set(border, 2, 2, 2, 2);
set(border_color, 0xFFFF);
set(clickable, 1);
parent(root);

// Draw some custom graphics below buttons
fillRect(50, 300, 700, 2, 0x4208);  // separator line
circle(400, 400, 50, 0xFFFF);
drawText(350, 440, 0, 0xFFFF, 0, "ferrite-ui");

halt();
```

## VM Limits

| Resource | Limit |
|----------|-------|
| Widget arena | 64 widgets |
| Text pool | 256 bytes |
| Variable slots | 16 (shared main + functions) |
| Eval stack | 16 deep |
| Call stack | 8 deep |
| Array pool | 64 elements, max 8 arrays |
| Bytecode size | 1024 bytes (USART execute) |
| Flash code | limited by flash resource size |
| Clip rects | 32 rectangles |
