# Ferrite Language Reference

Ferrite Language (`.fl`) is a simple C-like language that compiles to VM bytecode for the ferrite-ui bare-metal HMI framework. It runs on a GD32F103 (Cortex-M3, 108MHz) driving an 800x480 LCD.

## Toolchain

```bash
# Compile to VM image and print disassembly
python ferrite_lang.py source.fl --image --disasm

# Compile to VM image binary
python ferrite_lang.py source.fl --image -o program.bin

# Compile as page (with background color, no image header)
python ferrite_lang.py source.fl --page 0x0000 -o page_main.bin

# Upload to device
python ferrite_cli.py -p COM3 execute program.bin
```

## Program Structure

A ferrite program requires two functions: `fn setup()` and `fn loop()`. Global variables are declared at the top level. No top-level imperative code is allowed outside of function bodies.

```c
// Global variables (accessible from all functions)
var counter;
var label;

// setup() runs once at startup. Must return 0 for success.
fn setup() {
    counter = 0;
    label = alloc();
    target(label);
    set(kind, 1);
    set(size, 200, 40);
    set(text_color, 0xFFFF);
    set(font_id, 0);
    parent(0);
    render();
    return 0;
}

// loop() runs repeatedly. The compiler wraps it in while(1){...yield;}
fn loop() {
    counter = counter + 1;
    var s = itos(counter);
    target(label);
    setText(s);
    dirty();
    render();
    delay(100);
    strClear();
}

// Helper functions
fn rgb565(r, g, b) {
    return (r & 0xF8) * 256 + (g & 0xFC) * 8 + b / 8;
}
```

### setup()

Called once at startup after global variable initialization. Widget creation and initial UI layout go here. **Must return 0** for success — any other return value stops execution and shows an error on screen.

### loop()

Called repeatedly in an infinite loop. The compiler automatically wraps the body in `while(1) { <body>; yield; }`, so the function never returns. The `yield` allows the main loop to process touch events, USART messages, and callbacks between iterations.

### Global Variables

Variables declared at the top level are global — visible from `setup()`, `loop()`, all callbacks, and helper functions. Global variable initializers (including array literals) are automatically emitted at the start of `setup()`.

```c
var x;                  // initialized to 0 in setup
var color = 0xF800;     // initialized to 0xF800 in setup
var lut[4] = [10, 20, 30, 40];  // array initialized in setup
```

The VM has 32 variable slots total. Global variables use slots 0..N, function locals use slots N+.

## Comments

```c
// Single-line comment

/* Multi-line
   comment */
```

## Variables

```c
var x = 0;          // integer, initialized to 0
var color = 0xF800;  // hex literal (red in RGB565)
var mask = 0b1010;   // binary literal
var big = 1_000_000; // underscores for readability
```

All values are **signed 32-bit integers** (`i32`).

### Arrays

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
3.14159     // float (stored as f32 bit pattern)
```

### Boolean Values

`true` evaluates to `1`, `false` to `0`. Any non-zero value is truthy.

### String Literals

String literals are used as arguments to `drawText()` and `str()`.

```c
drawText(10, 20, 0, 0xFFFF, 0x0000, "Hello World");
var s = str("Hello");
```

Escape sequences: `\\`, `\"`, `\n`, `\t`.

### RGB565 Colors

The display uses 16-bit RGB565 color format. Common colors:

| Color   | Value    |
|---------|----------|
| Black   | `0x0000` |
| White   | `0xFFFF` |
| Red     | `0xF800` |
| Green   | `0x07E0` |
| Blue    | `0x001F` |
| Yellow  | `0xFFE0` |
| Cyan    | `0x07FF` |
| Magenta | `0xF81F` |

## Operators

### Arithmetic

| Operator | Description |
|----------|-------------|
| `+` | Addition |
| `-` | Subtraction / Negation |
| `*` | Multiplication |
| `/` | Integer division |
| `%` | Modulo |

### Comparison

| Operator | Description |
|----------|-------------|
| `==` | Equal |
| `!=` | Not equal |
| `<`  | Less than |
| `<=` | Less or equal |
| `>`  | Greater than |
| `>=` | Greater or equal |

### Logical / Bitwise

| Operator | Description |
|----------|-------------|
| `&&` | Logical AND (short-circuit) |
| `\|\|` | Logical OR (short-circuit) |
| `!`  | Logical NOT |
| `&` | Bitwise AND |
| `\|` | Bitwise OR |

## Control Flow

### if / else

```c
if (x > 10) {
    set(bg_color, 0xF800);
} else if (x > 5) {
    set(bg_color, 0x07E0);
} else {
    set(bg_color, 0x001F);
}
```

### while

```c
var i = 0;
while (i < 10) {
    i = i + 1;
}
```

### for

```c
for (var i = 0; i < 4; i = i + 1) {
    target(btn);
    set(bg_color, colors[i]);
}
```

### break / continue

```c
while (true) {
    if (i >= 10) { break; }
    if (i % 2 == 0) { i = i + 1; continue; }
    i = i + 1;
}
```

## Functions

Functions are defined with `fn`. Parameters are passed by value. Implicit `return 0` if omitted.

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
```

The VM has an 8-deep call stack.

## Widget System

Widgets form a tree structure (parent-child) with a CSS-like box model.

### Widget Lifecycle

```c
var btn;

fn setup() {
    btn = alloc();
    target(btn);
    set(location, 100, 50);
    set(size, 200, 60);
    set(bg_color, 0x001F);
    set(border, 2, 2, 2, 2);
    set(border_color, 0xFFFF);
    set(border_radius, 8);
    set(clickable, 1);
    parent(0);  // root widget is always id 0
    render();
    return 0;
}
```

### alloc()

Allocates a new widget (max 254). Returns the widget ID.

### target(widget)

Sets the target widget for subsequent `set()`, `get()`, `dirty()`, `parent()` calls.

### parent(widget)

Attaches the current target as a child of the given parent.

### set(property, value...)

Sets a property on the current target widget.

**Scalar properties:**

```c
set(bg_color, 0xF800);       // background color
set(border_color, 0xFFFF);    // border color
set(border_radius, 12);       // rounded corners (0 = sharp)
set(text_color, 0x0000);      // text color (labels)
set(visible, 1);              // show/hide
set(enabled, 1);              // enable/disable
set(clickable, 1);            // receives touch events
set(kind, 1);                 // 0=base, 1=label, 2=button, 3=progress, 4=slider, 5=checkbox, 6=radio
set(font_id, 0);              // font index (0=embedded)
set(text_align, 1);           // 0=left, 1=center, 2=right
set(press_color, 0x7BEF);     // pressed highlight color
set(image_id, 1);             // background image from flash
set(value, 50);               // progress/slider value (0-100)
set(checked, 1);              // checkbox/radio checked state (0 or 1)
set(on_click, 1);             // click callback func_id
set(on_paint, 2);             // custom paint callback func_id
set(on_tap, 3);               // tap-with-coords callback func_id
```

**Compound properties:**

```c
set(location, 100, 50);         // x, y
set(size, 200, 60);             // width, height
set(margin, 5, 5, 5, 5);       // top, right, bottom, left
set(border, 2, 2, 2, 2);       // top, right, bottom, left
set(padding, 10, 10, 10, 10);  // top, right, bottom, left
```

**Individual edge properties:**

```c
set(margin_top, 10);
set(border_left, 3);
set(padding_bottom, 8);
```

### border_radius

When `border_radius > 0`, the widget background and border are drawn as rounded rectangles instead of sharp-cornered rectangles. The radius is in pixels.

```c
set(border, 2, 2, 2, 2);
set(border_color, 0xFFFF);
set(border_radius, 12);    // 12px rounded corners
```

### Progress Bar & Slider

Widgets with `kind=3` (progress) or `kind=4` (slider) display a horizontal fill bar.

- `value` (0-100): fill percentage
- `press_color`: fill bar color
- `border_color`: slider thumb color (slider only)

```c
var bar = alloc();
target(bar);
set(kind, 3);                // progress bar
set(size, 200, 20);
set(bg_color, 0x2104);       // track color
set(press_color, 0x07E0);    // fill color (green)
set(value, 75);              // 75% filled
set(border_radius, 4);       // rounded fill
parent(0);

var slider = alloc();
target(slider);
set(kind, 4);                // slider (draggable)
set(size, 200, 30);
set(bg_color, 0x2104);
set(press_color, 0x001F);    // fill color (blue)
set(border_color, 0xFFFF);   // thumb color
set(clickable, 1);           // required for touch drag
set(value, 50);
parent(0);
```

Slider `on_click` callback receives `(widget_id, new_value)` when dragged.

### Checkbox & Radio

Widgets with `kind=5` (checkbox) or `kind=6` (radio) display a check/radio indicator inside the widget area. Touch toggles the `checked` state automatically.

- `checked` (0 or 1): current state, readable via `get(checked)`
- `text_color`: indicator outline color
- `press_color`: indicator fill color when checked (defaults to `text_color` if 0)
- `border_radius`: when > 0, checkbox uses rounded indicator box

**Checkbox** toggles on each tap (on/off). **Radio** buttons auto-uncheck siblings — tapping a radio unchecks all other `kind=6` widgets under the same parent, then checks itself.

```c
// Checkbox
var cb = alloc();
target(cb);
set(kind, 5);                // checkbox
set(location, 20, 20);
set(size, 30, 30);
set(bg_color, 0x2104);
set(text_color, 0xFFFF);     // indicator outline
set(press_color, 0x07E0);    // green check fill
set(clickable, 1);
set(checked, 1);             // start checked
parent(0);

// Radio group — children of the same parent
var r1 = alloc();
target(r1);
set(kind, 6);                // radio
set(location, 20, 60);
set(size, 30, 30);
set(bg_color, 0x2104);
set(text_color, 0xFFFF);
set(press_color, 0x001F);    // blue dot when selected
set(clickable, 1);
set(checked, 1);             // selected by default
parent(0);

var r2 = alloc();
target(r2);
set(kind, 6);
set(location, 20, 100);
set(size, 30, 30);
set(bg_color, 0x2104);
set(text_color, 0xFFFF);
set(press_color, 0x001F);
set(clickable, 1);
parent(0);
```

Read checked state in callbacks:

```c
fn handle_click(widget_id) {
    target(widget_id);
    if (get(checked)) {
        // widget is now checked
    }
}
```

### get(property)

Reads a property value from the current target widget.

```c
var w = get(width);
var color = get(bg_color);
var r = get(border_radius);
var is_on = get(checked);    // checkbox/radio state
var val = get(value);        // progress/slider value
```

### dirty() / render()

`dirty()` marks the target widget subtree for redraw. `render()` redraws all dirty widgets.

### halt() / yield_op()

`halt()` stops the VM. `yield_op()` yields to the main loop for one cycle.

## Drawing Primitives

Drawing primitives render directly to the LCD framebuffer. All coordinates are screen pixels (0,0 = top-left, 799x479 = bottom-right).

### fillRect(x, y, w, h, color)

```c
fillRect(0, 0, 800, 480, 0x0000);   // clear screen
fillRect(350, 190, 100, 100, 0xF800); // red square
```

### rect(x, y, w, h, color)

Rectangle outline (1px).

### line(x0, y0, x1, y1, color)

Line between two points (Bresenham's algorithm).

### circle(cx, cy, r, color) / fillCircle(cx, cy, r, color)

Circle outline / filled circle (midpoint algorithm).

### roundedRect(x, y, w, h, r, color) / fillRoundedRect(x, y, w, h, r, color)

Rounded rectangle outline / filled. Radius clamped to half the smallest dimension.

### arc(cx, cy, r, start, end, color)

Arc (portion of circle). Angles in degrees: 0=right, 90=down, 180=left, 270=up.

### drawImage(x, y, image_id)

Draw a flash-stored image at the given position.

### drawText(x, y, font_id, fg, bg, "text")

Draw a string literal. `bg=0` for transparent.

```c
drawText(10, 30, 0, 0xFFFF, 0x0000, "Hello World");
```

### delay(ms)

Non-blocking pause. Touch and USART continue processing during delay.

### millis()

Returns the system uptime in milliseconds (32-bit, wraps at ~49 days).

```c
var start = millis();
// ... do work ...
var elapsed = millis() - start;
```

### critical()

Enter a critical section — the VM keeps stepping without yielding until the next `yield` or `delay()`. Use for atomic UI updates (e.g., hiding one panel and showing another without a visible intermediate state).

```c
critical();
target(panel_a);
set(visible, 0);
dirty();
target(panel_b);
set(visible, 1);
dirty();
render();
// yield happens at end of loop() body, ending the critical section
```

### arrFree(arr)

Free a heap-allocated array. Required in loops to prevent memory exhaustion.

```c
var time = rtcRead();  // allocates a 7-element array each call
var h = time[2];
arrFree(time);         // free immediately after use
```

### setBrightness(percent) / brightness()

Control LCD backlight (0-100%).

```c
setBrightness(100);       // full brightness
var current = brightness(); // read current level
```

### fpgaCmd(cmd, data) / fpgaData(data)

Send raw commands/data to the FPGA display controller. For advanced use only.

### Double Buffering

```c
beginFrame();  // start drawing to back buffer
// ... draw operations ...
endFrame();    // swap buffers (tear-free)
```

## Float32 Operations

Software-emulated 32-bit float (Cortex-M3 has no FPU). Floats stored as IEEE 754 bit patterns in i32 variables.

```c
var pi = 3.14159;              // float literal
var f = itof(42);              // int → float
var i = ftoi(3.14);            // float → int (truncate)
var sum = fadd(1.5, 2.3);     // 3.8
var diff = fsub(10.0, 3.5);   // 6.5
var prod = fmul(pi, fmul(r, r)); // pi*r^2
var quot = fdiv(5.0, 9.0);    // 0.555...
var neg = fneg(3.14);          // -3.14
```

Float comparisons return i32 (0 or 1):

```c
if (fgt(temp, 30.0)) { /* hot */ }
if (flt(temp, 10.0)) { /* cold */ }
// Also: feq, fne, fle, fge
```

## String Operations

Runtime string pool for dynamic text. Strings are immutable, referenced by ID.

```c
var s = str("Hello");          // create from literal
var n = itos(42);              // int → string "42"
var f = ftos(3.14);            // float → string "3.14"
var msg = concat(s, n);        // concatenate
var len = strLen(s);           // byte length
var val = parseInt(str("42")); // parse int
var fv = parseFloat(str("3.14")); // parse float

// Display on widget
target(label);
setText(msg);

// Draw directly to screen
drawStr(10, 460, 0, 0x07E0, 0x0000, msg);

// Reclaim pool (preserves widget text)
strClear();

// Free individual string
strFree(s);
```

## RTC (Real-Time Clock)

Read and set the AT8563T hardware clock.

### rtcRead()

Reads the current date/time. Returns an array ID with 7 elements:

| Index | Field   | Range  |
|-------|---------|--------|
| 0     | second  | 0-59   |
| 1     | minute  | 0-59   |
| 2     | hour    | 0-23   |
| 3     | day     | 1-31   |
| 4     | weekday | 0-6 (0=Sunday) |
| 5     | month   | 1-12   |
| 6     | year    | 0-99 (from 2000) |

```c
var time = rtcRead();
var hour = time[2];
var minute = time[1];
var second = time[0];
```

### rtcWrite(arr)

Sets the date/time from an array with the same 7-element layout.

```c
// Set to 2025-06-15 14:30:00 (Sunday)
var t[7] = [0, 30, 14, 15, 0, 6, 25];
rtcWrite(t);
```

### Example: Digital Clock

```c
var time;
var h;
var m;
var s;

fn setup() {
    return 0;
}

fn loop() {
    time = rtcRead();
    h = time[2];
    m = time[1];
    s = time[0];

    beginFrame();
    fillRect(300, 200, 200, 50, 0x0000);

    var hs = itos(h);
    var ms = itos(m);
    var ss = itos(s);
    if (h < 10) { hs = concat(str("0"), hs); }
    if (m < 10) { ms = concat(str("0"), ms); }
    if (s < 10) { ss = concat(str("0"), ss); }
    var text = concat(hs, concat(str(":"), concat(ms, concat(str(":"), ss))));
    drawStr(340, 235, 0, 0xFFFF, 0x0000, text);
    strClear();
    endFrame();

    delay(1000);
}
```

## Events and Callbacks

Callbacks are functions called by the system in response to events. They are defined as regular functions with special names. The compiler automatically detects them and assigns the correct function kind in the VM image header.

### System Callbacks

Define these functions to handle system events:

| Function | Trigger | Arguments |
|----------|---------|-----------|
| `fn on_program_start()` | After setup, before loop starts | none |
| `fn on_touch_down(x, y)` | Touch press | screen x, y |
| `fn on_touch_up()` | Touch release | none |
| `fn on_touch_move(x, y)` | Touch held + moving | screen x, y |
| `fn on_user_message(arr_id)` | USART message received | array of bytes |

```c
fn on_touch_down(x, y) {
    fillCircle(x, y, 5, 0xF800);
}

fn on_user_message(arr_id) {
    var cmd = arr_id[0];
    if (cmd == 1) {
        target(0);
        set(bg_color, arr_id[1] * 256 + arr_id[2]);
        dirty();
        render();
    }
}
```

### Widget Callbacks

Widget event handlers are regular functions referenced by name using the `@` syntax: `@function_name` resolves to the function's ID at compile time.

```c
var btn;

fn handle_click(widget_id) {
    target(btn);
    var color = get(bg_color);
    if (color == 0xF800) {
        set(bg_color, 0x07E0);
    } else {
        set(bg_color, 0xF800);
    }
    dirty();
    render();
}

fn setup() {
    btn = alloc();
    target(btn);
    set(size, 200, 80);
    set(bg_color, 0xF800);
    set(clickable, 1);
    set(on_click, @handle_click);  // @name resolves to func_id
    parent(0);
    render();
    return 0;
}

fn loop() {}
```

**Widget event properties:**

| Property | Event | Callback args |
|----------|-------|---------------|
| `on_click` | Press + release on same widget | `(widget_id)` |
| `on_paint` | After widget is rendered | `(widget_id)` |
| `on_tap` | Tap with coordinates | `(widget_id, packed_xy)` |

For `on_tap`, extract coordinates: `x = coords / 65536`, `y = coords & 0xFFFF`.

### Function References (`@name`)

Use `@function_name` to reference a function by name. The compiler resolves it to the function's ID at compile time. This is the recommended way to assign callbacks — avoids fragile hardcoded numbers.

```c
fn my_handler(widget_id) { ... }
fn my_painter(widget_id) { ... }

// Reference by name — compiler resolves to func_id
set(on_click, @my_handler);
set(on_paint, @my_painter);
```

`@name` can be used anywhere an integer expression is expected — it evaluates to the function's numeric ID.

### Callback Queue

All callbacks are queued and executed in FIFO order between main loop iterations. This ensures callbacks never nest and the VM state remains predictable. The queue holds up to 8 pending callbacks.

## USART Communication

### sendUsart(data)

Send data via USART. Accepts an array (sends raw bytes) or a string ID (sends text).

```c
var buf[3] = [0x01, 0x02, 0x03];
sendUsart(buf);

var msg = str("hello");
sendUsart(msg);
```

## VM Image Format

The compiler produces a binary image with an embedded function table header:

```
version:        u8 (= 1)
function_count: u16 LE
reserved:       u16
[func_id: u16, kind: u8, pad: u8, offset: u32, length: u32] × N
opcodes...
```

Function kinds: Setup=0, Loop=1, UserFunction=2, OnProgramStart=3, OnUserMessage=6, OnTouchDown=7, OnTouchUp=8, OnTouchMove=9.

## VM Limits

| Resource | Limit |
|----------|-------|
| Widget arena | 254 widgets |
| Variable slots | 256 (sparse map, shared globals + function locals) |
| Eval stack | 16 deep |
| Call stack | 8 deep |
| Callback queue | 8 pending |
| String pool | 2,048 bytes, 32 slots |
| Bytecode | limited by flash resource or 2KB via USART |
| Clip rects | 32 rectangles |

## Complete Example

```c
// Global state
var root;
var panel;
var btn;
var counter;

fn setup() {
    counter = 0;

    root = alloc();
    target(root);
    set(size, 800, 480);
    set(bg_color, 0x0000);

    panel = alloc();
    target(panel);
    set(location, 100, 100);
    set(size, 600, 280);
    set(bg_color, 0x10A2);
    set(border, 2, 2, 2, 2);
    set(border_color, 0x4A69);
    set(border_radius, 16);
    parent(root);

    btn = alloc();
    target(btn);
    set(location, 200, 80);
    set(size, 200, 80);
    set(bg_color, 0xF800);
    set(press_color, 0x7800);
    set(border, 2, 2, 2, 2);
    set(border_color, 0xFFFF);
    set(border_radius, 8);
    set(clickable, 1);
    set(on_click, 1);  // func_id for handle_click
    parent(panel);

    render();
    return 0;
}

fn loop() {
    // Update display every second
    var time = rtcRead();
    var h = time[2];
    var m = time[1];

    beginFrame();
    var hs = itos(h);
    var ms = itos(m);
    if (h < 10) { hs = concat(str("0"), hs); }
    if (m < 10) { ms = concat(str("0"), ms); }
    var text = concat(hs, concat(str(":"), ms));
    drawStr(360, 30, 0, 0xFFFF, 0x0000, text);
    strClear();
    endFrame();

    delay(1000);
}

fn handle_click(widget_id) {
    counter = counter + 1;
    target(widget_id);
    if (counter % 2 == 0) {
        set(bg_color, 0xF800);
    } else {
        set(bg_color, 0x07E0);
    }
    dirty();
    render();
}

fn on_touch_down(x, y) {
    fillCircle(x, y, 3, 0xFFE0);
}

fn rgb565(r, g, b) {
    return (r & 0xF8) * 256 + (g & 0xFC) * 8 + b / 8;
}
```
