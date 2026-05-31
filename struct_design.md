# Struct Feature — Ferrite Language & VM

## Design Summary

**Value-type structs** backed by **consecutive var slots**. Field access reuses existing `LOAD`/`STORE` with compiler-computed offsets. Zero heap cost.

## Language Syntax

```fl
// Top-level declarations (before any fn)
struct Point { x, y }
struct Theme { bg, fg, accent }
struct Sensor { id, temp, humidity, pressure }

// Usage
fn demo() {
    var p = struct Point;     // allocates 2 consecutive slots → base slot id in p
    p.x = 100;                // STORE(p + 0, 100)
    p.y = 200;                // STORE(p + 1, 200)

    // Copy on assignment (value semantics)
    var q = p;                // copies all N fields
    q.x = 300;                // only q changes, p stays (100,200)

    var t = struct Theme;
    t.bg = color(255,255,255);
    t.fg = color(0,0,0);
    t.accent = color(255,0,0);
}

// Pass by value — all fields copied to consecutive param slots
fn apply_theme(container, theme) {
    target(container);
    set(bg_color, theme.bg);
    set(border_color, theme.accent);
    ...
}
```

## Returning Structs

Functions return a single `i32` (existing RET contract stays). Use out-parameter pattern:

```fl
fn init_point(out_p) {
    out_p.x = 10;
    out_p.y = 20;
}
// caller:
var p = struct Point;
init_point(p);
```

## VM Changes (minimal — 2 new opcodes)

### New opcodes

```
OP_STRUCT_ALLOC  0xAD  + u8 n_fields
  → Reserves n_fields consecutive var slots
  → Pushes base slot id onto eval stack

OP_STRUCT_INIT   0xAE  + u8 n_fields  
  → Pops n_fields values from stack (bottom = field 0)
  → Allocates + stores into consecutive slots
  → Pushes base slot id
```

### VM state

Add `u16 next_dynamic_slot` to `Vm` struct — initialized to the image header's `max_var_count`. Runtime-allocated struct/var slots live above compiler-assigned slots. RAM cost: **2 bytes**.

## Compiler Changes

| Step | What |
|---|---|
| Parse | `struct Name { field1, ... }` at top level, build offset map |
| Resolve | `p.field` → look up struct type of `p`, find field offset `k` |
| Emit field read | `p.x` → `LOAD(p_slot + k)` (existing opcode) |
| Emit field write | `p.x = v` → `STORE(p_slot + k)` (existing opcode) |
| Emit alloc | `struct Point` → `STRUCT_ALLOC 2` (new opcode) |
| Emit init | `struct Point { 10, 20 }` → `STRUCT_INIT 2` (new opcode) |
| Emit copy | `q = p` → `STRUCT_ALLOC n` + copy loop per field |
| Emit pass | Struct arg → allocate N param slots, copy fields before CALL |

## Compatibility

- All existing bytecode runs unchanged
- Field access reuses `LOAD`/`STORE` with computed offsets — no new opcodes needed per access
- `STRUCT_ALLOC` (0xAD) and `STRUCT_INIT` (0xAE) in the free 0xA0+ opcode range
- Image header v5 adds `max_var_count` (u8 or u16) to reserve space for runtime allocations
