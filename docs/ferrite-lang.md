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
    label.kind = 1;
    label.size = [200, 40];
    label.text_color = 0xFFFF;
    label.font_id = 0;
    parent(0);
    render();
    return 0;
}

// loop() runs repeatedly. The compiler wraps it in while(1){...yield;}
fn loop() {
    counter++;
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

Global variables use slots 0..127, function locals use slots 0..127 (separate namespace via high bit). Max 128 globals + 128 locals per function.

## Comments

```c
// Single-line comment

/* Multi-line
   comment */
```

## Variables

```c
var x = 0;          // int, initialized to 0
var color = 0xF800;  // int, hex literal (red in RGB565)
var speed = 1.5;     // float, inferred from float literal
var angle = 0.0;     // float, decimal point makes it float
var count;           // int (default when no initializer)
```

The compiler tracks each variable's type (`int` or `float`) based on its initializer. A variable initialized with a float literal or float expression becomes a float variable. Subsequent arithmetic on float variables automatically uses float instructions.

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

Two value types: **int** (signed 32-bit integer) and **float** (IEEE 754 f32, stored as bit pattern in i32). The compiler infers types at compile time — no runtime overhead.

### Number Literals

```c
42          // int: decimal
0xFF00      // int: hexadecimal
0b1100_0011 // int: binary
1_000_000   // int: decimal with underscores
3.14159     // float: decimal point makes it float
0.0         // float: zero as float
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

All arithmetic and comparison operators are **type-aware**: when either operand is float, the compiler automatically promotes the int operand and uses float instructions. No manual `fadd()`/`flt()` calls needed.

### Arithmetic

| Operator | int + int | float involved |
|----------|-----------|----------------|
| `+` | ADD | FADD (auto-promote) |
| `-` | SUB / NEG | FSUB / FNEG |
| `*` | MUL | FMUL |
| `/` | integer division | FDIV |
| `%` | modulo | not supported for float |

```c
var a = 1.0;
var b = 2;
var c = a + b;  // b auto-promoted to float, uses FADD → c is float
var d = -a;     // uses FNEG
```

### Comparison

| Operator | int + int | float involved |
|----------|-----------|----------------|
| `==` | EQ | FEQ |
| `!=` | NE | FNE |
| `<`  | LT | FLT |
| `<=` | LE | FLE |
| `>`  | GT | FGT |
| `>=` | GE | FGE |

Comparisons always return int (0 or 1), even with float operands.

```c
var temp = 25.5;
if (temp > 30.0) { /* hot */ }    // uses FGT
if (temp < 10) { /* cold */ }     // 10 auto-promoted, uses FLT
```

### Logical / Bitwise

| Operator | Description |
|----------|-------------|
| `&&` | Logical AND (short-circuit) |
| `\|\|` | Logical OR (short-circuit) |
| `!`  | Logical NOT |
| `&` | Bitwise AND (int only) |
| `\|` | Bitwise OR (int only) |
| `<<` | Left shift (int only) |
| `>>` | Arithmetic right shift, sign-preserving (int only) |

Shift operators follow C precedence: lower than `+`/`-`, higher than `<`/`>`. The shift amount is masked to `[0, 31]`.

```c
var flags = 0;
flags = flags | (1 << 4);   // set bit 4
var bit4 = (flags >> 4) & 1; // extract bit 4
var sign = -8 >> 1;          // -4  (arithmetic: sign bit preserved)
```

### Compound Assignment

| Operator | Equivalent |
|----------|------------|
| `x += y` | `x = x + y` |
| `x -= y` | `x = x - y` |
| `x *= y` | `x = x * y` |
| `x /= y` | `x = x / y` |
| `x %= y` | `x = x % y` |

Works on variables, array elements, and widget properties (via dot syntax). Float-aware — uses float instructions when either side is float.

```c
var speed = 1.5;
speed += 0.5;          // FADD (float compound)

var colors[4] = [0, 0, 0, 0];
colors[2] += 0x0100;   // array element compound

btn.value += 10;       // widget property compound (target + get + add + set)
```

### Increment / Decrement

| Operator | Description | Value of expression |
|----------|-------------|---------------------|
| `++x` | Pre-increment | value **after** increment |
| `x++` | Post-increment | value **before** increment |
| `--x` | Pre-decrement | value **after** decrement |
| `x--` | Post-decrement | value **before** decrement |

Works on variables and array elements. Float-aware — increments by `1.0` for float variables.

```c
var i = 0;
var a = ++i;    // i=1, a=1 (pre: increment first, then use)
var b = i++;    // b=1, i=2 (post: use first, then increment)

var angle = 0.0;
++angle;        // increments by 1.0 (float)

for (var i = 0; i < 10; i++) {
    // most common use case
}
```

### Ternary Operator

```c
var x = (a > b) ? a : b;          // like if/else but as an expression
var color = pressed ? 0xF800 : 0x07E0;
```

Right-associative and nestable:

```c
var level = (temp > 80) ? 2 : (temp > 50) ? 1 : 0;
```

Float-aware — if either branch is float, the result is float:

```c
var speed = 1.5;
var v = (fast) ? speed : 0;  // 0 auto-promoted to 0.0, result is float
```

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
    i++;
}
```

### for

```c
for (var i = 0; i < 4; i++) {
    target(btn);
    set(bg_color, colors[i]);
}
```

### break / continue

```c
while (true) {
    if (i >= 10) { break; }
    if (i % 2 == 0) { i++; continue; }
    i++;
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

### Parameter Type Annotations

Parameters default to `int`. Use `: float` to declare float parameters — this enables the compiler to use float operations inside the function body:

```c
fn lerp(a: float, b: float, t: float) {
    return a + (b - a) * t;  // compiles to FSUB, FMUL, FADD
}

fn distance(x1: float, y1: float, x2: float, y2: float) {
    var dx = x2 - x1;  // FSUB (all params are float)
    var dy = y2 - y1;
    return dx * dx + dy * dy;  // FMUL, FMUL, FADD
}
```

Int parameters don't need annotation — `fn foo(x)` and `fn foo(x: int)` are equivalent.

The VM has an 8-deep call stack.

## Widget System

Widgets form a tree structure (parent-child) with a CSS-like box model.

### Widget Lifecycle

```c
var btn;

fn setup() {
    btn = alloc();
    btn.location = [100, 50];
    btn.size = [200, 60];
    btn.bg_color = 0x001F;
    btn.border = [2, 2, 2, 2];
    btn.border_color = 0xFFFF;
    btn.border_radius = 8;
    btn.clickable = 1;
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

### Property Access (dot syntax)

Widget variables support dot syntax as shorthand for `target()` + `set()`/`get()`:

**Write:**

```c
var btn = alloc();
btn.bg_color = 0x001F;         // scalar property
btn.text_color = 0xFFFF;
btn.size = [200, 60];          // compound property
btn.border = [2, 2, 2, 2];
btn.text = "Click me";         // string literal (no heap alloc)
btn.text = str("Click me");    // also works (optimized to same as above)
btn.on_click = @handle_click;  // callback
btn.clickable = 1;
```

**Read:**

```c
var c = btn.bg_color;          // scalar read
var v = slider.value;
if (cb.checked) { /* ... */ }
```

**How it works:** `btn.bg_color = 0x001F` compiles to `target(btn)` + `set(bg_color, 0x001F)`. Consecutive property accesses on the same widget emit only one `target()` — the compiler tracks the current target and skips redundant switches.

```c
var btn = alloc();       // W_ALLTAR: alloc + target + store
btn.bg_color = 0x001F;   // W_SET only (already targeted)
btn.size = [200, 60];    // W_SET only (still targeted)

var lbl = alloc();       // W_ALLTAR: switches target to lbl
lbl.text = "OK";         // W_SET only

btn.bg_color = 0xF800;   // W_TARGET(btn) + W_SET (target switched back)
```

Compound properties (location, size, margin, border, padding) require `[...]` array syntax. Reading compound properties directly is not supported — use the individual components instead:

```c
var w = btn.width;       // OK: reads size_w
var h = btn.height;      // OK: reads size_h
// var s = btn.size;     // Error: cannot read compound property
```

The dot syntax and `set()`/`get()` can be mixed freely:

```c
var btn = alloc();
btn.bg_color = 0x001F;    // dot syntax
target(btn);
set(size, 200, 60);       // traditional syntax
var c = btn.bg_color;     // dot read
```

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

### Gradient Fill

Any widget can have a linear gradient background instead of a solid color.  Two
properties control it:

| Property | Values | Description |
|---|---|---|
| `gradient_color` | RGB565 color | End color of the gradient |
| `gradient_dir` | `0` / `1` / `2` | Direction: 0=solid (no gradient), 1=horizontal (left→right), 2=vertical (top→bottom) |

`bg_color` is always the *start* color (top for vertical, left for horizontal).
`gradient_color` is the *end* color.  Setting `gradient_dir = 0` (default) disables
the gradient and falls back to a solid `bg_color` fill.

```c
fn rgb565(r, g, b) {
    return (r & 0xF8) * 256 + (g & 0xFC) * 8 + b / 8;
}

fn setup() {
    // Panel with a vertical gradient (dark blue → cyan)
    var panel = alloc();
    panel.size = [400, 200];
    panel.bg_color = rgb565(0, 20, 80);       // top color
    panel.gradient_color = rgb565(0, 180, 220); // bottom color
    panel.gradient_dir = 2;                    // vertical
    parent(0);

    // Button with a horizontal gradient
    var btn = alloc();
    btn.location = [50, 50];
    btn.size = [200, 60];
    btn.bg_color = rgb565(200, 0, 0);          // left color
    btn.gradient_color = rgb565(255, 180, 0);  // right color
    btn.gradient_dir = 1;                      // horizontal
    btn.border_radius = 8;
    btn.clickable = 1;
    parent(panel);

    render();
    return 0;
}
```

Gradient direction constants (define in your program for readability):

```c
const GRADIENT_NONE = 0;
const GRADIENT_H    = 1;   // horizontal
const GRADIENT_V    = 2;   // vertical
```

**project.json syntax:**

```json
{
  "type": "base",
  "size": [400, 200],
  "background_color": "0x0014FF",
  "gradient_color": "0x00B4DC",
  "gradient_dir": 2
}
```

**Performance notes:**
- Vertical gradients render nearly as fast as solid fills — one `fill_rect` call per row.
- Horizontal gradients use per-pixel writes — slower for wide widgets; prefer vertical for large areas.
- Gradient is suppressed when the widget is in pressed state (`press_color` takes priority).
- In **dirty render mode**, widgets disappearing from a gradient-background parent may leave a solid-color artifact.  Use **buffered render mode** (`render_mode: "buffered"`) for UIs where widgets are frequently shown/hidden over gradient backgrounds.

### Progress Bar & Slider

Widgets with `kind=3` (progress) or `kind=4` (slider) display a horizontal fill bar.

- `value` (0-100): fill percentage
- `press_color`: fill bar color
- `border_color`: slider thumb color (slider only)

```c
var bar = alloc();
bar.kind = 3;                // progress bar
bar.size = [200, 20];
bar.bg_color = 0x2104;       // track color
bar.press_color = 0x07E0;    // fill color (green)
bar.value = 75;              // 75% filled
bar.border_radius = 4;       // rounded fill
parent(0);

var slider = alloc();
slider.kind = 4;             // slider (draggable)
slider.size = [200, 30];
slider.bg_color = 0x2104;
slider.press_color = 0x001F; // fill color (blue)
slider.border_color = 0xFFFF; // thumb color
slider.clickable = 1;        // required for touch drag
slider.value = 50;
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
cb.kind = 5;                 // checkbox
cb.location = [20, 20];
cb.size = [30, 30];
cb.bg_color = 0x2104;
cb.text_color = 0xFFFF;      // indicator outline
cb.press_color = 0x07E0;     // green check fill
cb.clickable = 1;
cb.checked = 1;              // start checked
parent(0);

// Radio group — children of the same parent
var r1 = alloc();
r1.kind = 6;                 // radio
r1.location = [20, 60];
r1.size = [30, 30];
r1.bg_color = 0x2104;
r1.text_color = 0xFFFF;
r1.press_color = 0x001F;     // blue dot when selected
r1.clickable = 1;
r1.checked = 1;              // selected by default
parent(0);

var r2 = alloc();
r2.kind = 6;
r2.location = [20, 100];
r2.size = [30, 30];
r2.bg_color = 0x2104;
r2.text_color = 0xFFFF;
r2.press_color = 0x001F;
r2.clickable = 1;
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

Or with dot syntax when the widget variable is known:

```c
if (cb.checked) {
    // checkbox is checked
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

With dot syntax (equivalent, but doesn't require manual `target()`):

```c
var w = btn.width;
var color = btn.bg_color;
var val = slider.value;
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

Software-emulated 32-bit float (Cortex-M3 has no FPU). The compiler tracks types at compile time and automatically selects float or integer instructions — just use normal operators.

### Basic Usage

```c
var pi = 3.14159;       // float (decimal point)
var r = 5.0;
var area = pi * r * r;  // FMUL, FMUL — all float

var speed = 1.5;
var pos = 0.0;
pos = pos + speed;      // FADD

if (pos > 100.0) {      // FGT
    pos = 0.0;
}
```

### Mixed int/float (Auto-Promotion)

When an operator has one float and one int operand, the int is automatically promoted:

```c
var x = 1.0;
var y = 2;
var z = x + y;     // y promoted via ITOF, then FADD → z is float
var half = y / 2.0; // y promoted, then FDIV
```

Assignment to a float variable also auto-promotes:

```c
var angle = 0.0;   // float
angle = 42;        // 42 auto-promoted via ITOF
```

### Conversion Functions

```c
var f = itof(42);       // int → float (explicit)
var i = ftoi(3.14);     // float → int (truncate toward zero)
```

### Float Functions with Type Annotations

```c
fn lerp(a: float, b: float, t: float) {
    return a + (b - a) * t;
}

fn clampf(val: float, lo: float, hi: float) {
    if (val < lo) { return lo; }
    if (val > hi) { return hi; }
    return val;
}

fn setup() {
    var mid = lerp(0.0, 100.0, 0.5);   // 50.0
    var clamped = clampf(1.5, 0.0, 1.0); // 1.0
}
```

### Explicit Builtins (Legacy)

The explicit float builtins still work for backward compatibility:

```c
var sum = fadd(1.5, 2.3);     // same as 1.5 + 2.3
var diff = fsub(10.0, 3.5);   // same as 10.0 - 3.5
var prod = fmul(2.0, 3.0);    // same as 2.0 * 3.0
var quot = fdiv(5.0, 9.0);    // same as 5.0 / 9.0
var neg = fneg(3.14);          // same as -3.14

// Comparisons: feq, fne, flt, fle, fgt, fge
if (fgt(temp, 30.0)) { }      // same as: if (temp > 30.0) { }
```

### Type Inference Rules

| Expression | Inferred type |
|------------|---------------|
| `42`, `0xFF` | int |
| `3.14`, `0.0` | float |
| `var x = 1.0;` | x is float |
| `var x = 1;` | x is int |
| `var x;` | x is int (default) |
| `float + int` | float (int auto-promoted) |
| `float + float` | float |
| `int + int` | int |
| `float < float` | int (comparison result) |
| `itof(x)` | float |
| `ftoi(x)` | int |
| `get(prop)` | int (widget properties are int) |
| `sin(x)`, `cos(x)`, `sqrt(x)`, `abs(x)`, `floor(x)`, `ceil(x)` | float |
| `atan2(y, x)` | float |

## Math Functions

Trigonometry, rounding, and geometric helpers. All work on float values; integer arguments are auto-promoted via `itof`. Results are float.

```c
var angle = 0.785;      // radians (≈ 45°)
var s = sin(angle);     // → 0.707...
var c = cos(angle);     // → 0.707...

var dist = sqrt(fadd(fmul(dx, dx), fmul(dy, dy)));  // Euclidean distance

var bearing = atan2(dy, dx);   // angle from point A to B, in radians

var a = abs(-3.5);      // → 3.5
var lo = floor(1.9);    // → 1.0
var hi = ceil(1.1);     // → 2.0
```

### Gauge needle example

```c
// Draw a needle from center (cx, cy) at angle deg (0=right, clockwise)
fn needle(cx, cy, r: float, deg: float, color) {
    var rad = deg * 3.14159 / 180.0;
    var x2 = ftoi(itof(cx) + r * cos(rad));
    var y2 = ftoi(itof(cy) + r * sin(rad));
    line(cx, cy, x2, y2, color);
}
```

### Function reference

| Function | Args | Returns | Description |
|----------|------|---------|-------------|
| `sin(x)` | float (radians) | float | sine |
| `cos(x)` | float (radians) | float | cosine |
| `sqrt(x)` | float | float | square root |
| `abs(x)` | float | float | absolute value |
| `atan2(y, x)` | float, float | float (radians) | arctangent of y/x, full quadrant |
| `floor(x)` | float | float | round toward −∞ |
| `ceil(x)` | float | float | round toward +∞ |

All angles are in **radians**. Multiply by `3.14159 / 180.0` to convert from degrees.

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

// Build a string from a byte buffer (e.g. after fileRead loop)
var buf[64];
buf[0] = 72; buf[1] = 105;     // 'H', 'i'
var hi = arrToStr(buf, 2);     // → "Hi"  (takes low byte of each element)
var full = arrToStr(buf);      // whole array (pads with NUL from unused slots)

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

### sprintf(fmt, ...)

Format one or more values into a new string using printf-style format specifiers. Returns a string ID. Supports up to 8 format arguments. Output is capped at 128 bytes.

```c
var s = sprintf("Temp: %d C", temp);
var s = sprintf("V = %.2f", voltage);
var s = sprintf("%02d:%02d:%02d", h, m, sec);
var s = sprintf("x=%.1f y=%.1f", px, py);
var s = sprintf("0x%X", flags);
```

**Supported specifiers:**

| Specifier | Argument type | Output |
|-----------|--------------|--------|
| `%d` / `%i` | int | signed decimal |
| `%u` | int | unsigned decimal |
| `%x` | int | lowercase hex |
| `%X` | int | uppercase hex |
| `%f` | float | decimal, 2 places by default |
| `%.Nf` | float | decimal with N places (0–9) |
| `%s` | str_id | inline string |
| `%%` | — | literal `%` |

The `%f` specifier reads a float value (stored as bits in an int, same as all float variables). The `%s` specifier reads a string pool ID — pass another str variable or a `str("...")` literal.

```c
// Sensor readout: float + unit
var voltage = 3.28;       // float
var s = sprintf("Batt: %.2f V", voltage);
target(lbl);
setText(s);
strClear();

// Mixed types
var name = str("MCU");
var freq = 108;
var info = sprintf("%s @ %d MHz", name, freq);

// Zero-pad integers: manual approach (sprintf has no %0Nd width yet)
var h = 9;
var m = 5;
// Use conditional concat for zero-padding, or:
var clock = sprintf("%d:%d", h, m);    // → "9:5"

// Hex dump
var reg = 0xDEAD;
var s2 = sprintf("REG=0x%X", reg);    // → "REG=0xDEAD"
```

> **Note:** `sprintf` is the preferred way to build formatted strings. It is more compact than chained `concat(itos(...), ...)` calls and produces a single string pool entry instead of several intermediates.

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
fn setup() {
    return 0;
}

fn loop() {
    var time = rtcRead();
    var h = time[2];
    var m = time[1];
    var s = time[0];
    arrFree(time);

    beginFrame();
    fillRect(300, 200, 200, 50, 0x0000);
    var text = sprintf("%d:%02d:%02d", h, m, s);
    drawStr(340, 235, 0, 0xFFFF, 0x0000, text);
    strClear();
    endFrame();

    delay(1000);
}
```

> **Note:** `%02d` zero-pads to 2 digits (e.g. `9` → `"09"`). Width-and-pad is supported for the `%d`/`%u` family.

## Files (Flash Filesystem)

Read user data bundled into the flash image at build time. Files are declared in `project.json` under `"files": [...]` and stored in flash as `RES_FILE` resources. The VM exposes a minimal handle-based API for sequential byte-by-byte reading.

### Embedding Files

In `project.json`:

```json
{
  "files": [
    { "name": "config",  "source": "data/config.bin" },
    { "name": "palette", "source": "data/palette.raw" }
  ]
}
```

Names are limited to **15 ASCII characters**. Source paths are relative to the project directory. Files are stored verbatim — no compression, no transformation. There is no directory listing at runtime: the program must know the file name it wants to open.

### Constraints

- Files are **read-only** (flash-backed).
- At most **2 files open simultaneously** — handle values are `1` and `2`.
- `fileRead` returns **one byte per call**. Heavy reads are slow; buffer into your own array if you need to process large data repeatedly.
- Passing an **invalid handle** (`0xFF`, a closed slot, or a value other than 1/2) to `fileRead` / `fileSize` / `fileSeek` / `fileClose` puts the VM into the **Error** state. Programs must check the return of `fileOpen` before using the handle.

### fileOpen(name) → handle

Opens a file by name. Returns a handle (`1` or `2`) on success, or `0xFF` (255) on error. Errors are returned for any of:

- the filesystem is not mounted,
- the name is not found,
- the named resource is not a file (e.g. it's a font or image),
- both file slots are already in use.

The `name` argument must be a string id (produced by `str("...")` or equivalent).

```c
var h = fileOpen(str("config"));
if (h == 255) {
    // open failed — do not touch h further
    return;
}
```

### fileRead(handle) → byte | -1

Reads the next byte and advances the internal read position. Returns a value in `0..255`, or `-1` when the end of the file is reached. Subsequent calls past EOF keep returning `-1`.

```c
var b;
while (1) {
    b = fileRead(h);
    if (b < 0) { break; }   // EOF
    // ... process b ...
}
```

### fileSize(handle) → int

Returns the total size of the open file in bytes. Does not change the read position.

```c
var total = fileSize(h);
```

### fileSeek(handle, pos)

Sets the read position to `pos` bytes from the start of the file. The position is clamped to `[0, fileSize(handle)]` — seeking past the end lands exactly at EOF, and a negative `pos` is treated as `0`. Does not return a value.

```c
fileSeek(h, 0);   // rewind to start
fileSeek(h, 16);  // skip first 16-byte header
```

Seeking to the end and calling `fileRead` immediately returns `-1` (EOF). There is no seek-from-end or seek-from-current — `pos` is always an absolute byte offset.

### fileClose(handle)

Releases the slot so it can be reused by another `fileOpen`. Does not return a value.

```c
fileClose(h);
```

### Example: Read a Config File

```c
fn setup() {
    var h = fileOpen(str("config"));
    if (h == 255) {
        return;
    }
    var total = fileSize(h);
    var sum = 0;
    var b;
    while (1) {
        b = fileRead(h);
        if (b < 0) { break; }
        sum = sum + b;
    }
    fileClose(h);
    // use total / sum ...
}
```

### Example: Copying Bytes Into an Array

```c
fn setup() {
    var h = fileOpen(str("palette"));
    if (h == 255) { return; }

    var n = fileSize(h);
    var buf[256];
    var i = 0;
    var b;
    while (i < n) {
        b = fileRead(h);
        if (b < 0) { break; }
        buf[i] = b;
        i = i + 1;
    }
    fileClose(h);
}
```

### Example: Display File Contents as a Label

Use `arrToStr(arr, len)` to turn a byte buffer into a string you can pass to
`setText` or `drawStr`. The low byte of each array element becomes one
character — exactly what `fileRead` produces.

```c
var label;

fn setup() {
    label = alloc();
    parent(0);
    target(label);
    set(loc, 10, 10);
    set(size, 400, 40);

    var h = fileOpen(str("message"));
    if (h == 255) { return; }

    var buf[256];
    var i = 0;
    var b;
    while (i < 256) {
        b = fileRead(h);
        if (b < 0) { break; }
        buf[i] = b;
        i = i + 1;
    }
    fileClose(h);

    var s = arrToStr(buf, i);   // convert first `i` bytes to a string
    target(label);
    setText(s);
}
```

Passing `arrToStr(buf)` without a length uses the full array — any unused
slots contribute NUL bytes, so prefer the two-argument form when the valid
byte count is known (as it is after a `fileRead` loop).

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

### Lambda Expressions

Lambdas are anonymous functions defined inline. They compile to a regular function and evaluate to the function's ID — syntactic sugar for defining a named function + using `@name`.

```c
// Instead of:
fn handle_click(widget_id) {
    btn.bg_color = 0xF800;
    dirty();
    render();
}
btn.on_click = @handle_click;

// Write:
btn.on_click = |widget_id| {
    btn.bg_color = 0xF800;
    dirty();
    render();
};
```

**Syntax:** `|params| { body }` — parameters use the same type annotations as regular functions:

```c
var f = |a, b| { return a + b; };
var g = |x: float, y: float| { return x * y; };
var h = || { counter++; };              // zero parameters
```

**No captures.** Lambdas can only access global variables and their own parameters/locals. Referencing a local variable from the enclosing function is a compile error:

```c
fn setup() {
    var local_x = 42;
    var f = |y| { return local_x + y; };  // ERROR: captures 'local_x'
    return 0;
}
```

Move the variable to a global to fix:

```c
var x;

fn setup() {
    x = 42;
    var f = |y| { return x + y; };  // OK: 'x' is global
    return 0;
}
```

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

## Syscall Interface

Device-specific operations are exposed through `syscall()`. The VM dispatches to a handler registered by the host firmware — the VM itself stays device-agnostic.

```c
var result = syscall(id, arg0, arg1, ...);
```

The first argument is the syscall **ID** — it must be a compile-time integer literal or a named `const`. Arguments after the ID are runtime values (int, float bits, or str_ids). The call always pushes one `i32` result. If the handler signals an error (by returning `None` internally), the VM enters the **Error** state.

### Using the e-paper device library

Include `lib/epaper.fl` to get the predefined constants:

```c
#include "epaper.fl"

fn setup() {
    var ssid = str("MyNetwork");
    var pass = str("secret123");
    var rc = syscall(SYS_WIFI_CONNECT, ssid, pass);
    strClear();
    if (rc != 0) {
        // connection failed
    }
    return 0;
}

fn loop() {
    var status = syscall(SYS_WIFI_STATUS);
    if (status == 2) {
        // connected — do work
    }
    delay(1000);
}
```

### Syscall ID ranges (epaper device)

| Range | Category |
|-------|----------|
| `0x00–0x0F` | System |
| `0x10–0x1F` | WiFi |
| `0x20–0x2F` | Bluetooth |

| ID | Name | Arguments | Returns |
|----|------|-----------|---------|
| `0x00` | `SYS_UPTIME` | — | milliseconds |
| `0x01` | `SYS_REBOOT` | — | does not return |
| `0x10` | `SYS_WIFI_CONNECT` | `ssid: str_id, pass: str_id` | `0`=ok, `-1`=error |
| `0x11` | `SYS_WIFI_DISCONNECT` | — | `0` |
| `0x12` | `SYS_WIFI_STATUS` | — | `0`=idle, `1`=connecting, `2`=connected, `3`=failed |
| `0x13` | `SYS_WIFI_RSSI` | — | RSSI in dBm, or `0` if not connected |
| `0x14` | `SYS_WIFI_IP` | — | IPv4 as `u32` big-endian |
| `0x20` | `SYS_BT_START` | `name: str_id` | `0`=ok, `-1`=error |
| `0x21` | `SYS_BT_STOP` | — | `0` |
| `0x22` | `SYS_BT_STATUS` | — | `0`=off, `1`=advertising, `2`=connected |
| `0x23` | `SYS_BT_SEND` | `data: str_id` | bytes sent, or `-1` on error |

### Adding new syscalls

Define new IDs in your own include file — no VM changes needed:

```c
// mydevice.fl
const SYS_READ_SENSOR = 0x30;  // () → sensor reading as i32
const SYS_SET_GPIO    = 0x31;  // (pin: int, level: int) → 0

var temp = syscall(SYS_READ_SENSOR);
syscall(SYS_SET_GPIO, 5, 1);
```

The host registers the handler once before running the VM:

```rust
vm.syscall_fn = Some(|id, args, strpool| match id {
    0x30 => Some(read_sensor()),
    0x31 => { set_gpio(args[0] as u8, args[1] != 0); Some(0) }
    _    => None,  // unknown id → VM Error
});
```

## VM Image Format

The compiler produces a binary image with an embedded function table header (current version: 4):

```
version:        u8  (= 4)
function_count: u16 LE
global_count:   u16 LE
flags:          u16 LE  (bit 0: render_mode — 0=dirty, 1=buffered)
widget_count:   u8      (pre-allocation hint)
ext_count:      u8      (pre-allocation hint)
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
| String pool | 2,048 bytes, 64 slots |
| Open files | 2 simultaneous (handles 1 and 2) |
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
    root.size = [800, 480];
    root.bg_color = 0x0000;

    panel = alloc();
    panel.location = [100, 100];
    panel.size = [600, 280];
    panel.bg_color = 0x10A2;
    panel.border = [2, 2, 2, 2];
    panel.border_color = 0x4A69;
    panel.border_radius = 16;
    parent(root);

    btn = alloc();
    btn.location = [200, 80];
    btn.size = [200, 80];
    btn.bg_color = 0xF800;
    btn.press_color = 0x7800;
    btn.border = [2, 2, 2, 2];
    btn.border_color = 0xFFFF;
    btn.border_radius = 8;
    btn.clickable = 1;
    btn.on_click = @handle_click;
    parent(panel);

    render();
    return 0;
}

fn loop() {
    // Update display every second
    var time = rtcRead();
    var h = time[2];
    var m = time[1];
    arrFree(time);

    beginFrame();
    var text = sprintf("%02d:%02d", h, m);
    drawStr(360, 30, 0, 0xFFFF, 0x0000, text);
    strClear();
    endFrame();

    delay(1000);
}

fn handle_click(widget_id) {
    counter++;
    if (counter % 2 == 0) {
        btn.bg_color = 0xF800;
    } else {
        btn.bg_color = 0x07E0;
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
