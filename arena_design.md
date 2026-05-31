# Handle-Arena Memory System

## Motivation

- **Eliminate StringPool's 2KB pre-allocation** — the pool exists solely because the linked-list heap fragments; it already does its own compaction internally
- **Lift WidgetId from u8 to u16** — no more 254-widget limit
- **System-wide defragmentation** — generalize StringPool's compaction pattern to all dynamically-allocated data

## Core Idea

A **generational handle arena**: all allocations return a 2-byte `Handle` instead of a raw pointer. A separate slot table maps handles to arena offsets. Compaction moves blocks without invalidating handles.

```
┌──────────────────────────────────────┐
│  ARENA  (remaining RAM after globals) │
│  ┌────┬──────┬───┬────────┬───────┐  │
│  │ Hdr│widget│str│ widget │ array │  │
│  │ 4B │ 24B  │12B│  24B   │ 40B   │  │
│  └────┴──────┴───┴────────┴───────┘  │
│                                      │
│  Slot Table (separate, fixed 1 KB)   │
│  [0] → {off:0x0040, sz:24, gen:1}   │
│  [1] → {off:0x0058, sz:12, gen:3}   │
│  [2] → FREE                          │
│  ...                                 │
└──────────────────────────────────────┘
```

## Slot Table

Fixed 1 KB array of `Slot` entries (8 bytes each, max 128 entries):

```rust
struct Slot {
    offset: u16,   // byte offset of block within arena
    size: u16,     // total block size including 4-byte header
    gen: u16,      // generation — incremented on free; catch use-after-free
    flags: u16,    // KIND bits + FREE sentinel (0x8000 = free, 0x7FFF = type tag)
}
```

| Field | Bits | Purpose |
|---|---|---|
| offset | 16 | Position in arena data region |
| size | 16 | Block size (header + payload), max 65535 |
| gen | 16 | Generation counter, bumped on free → stale handles fail lookup |
| flags | 16 | Hi bit = free sentinel; lo 15 bits = allocator-visible type tag |

## Handle (u16, 2 bytes)

```
bits [15:9] → slot_index  (0-127, indexes slot table)
bits [8:0]  → generation  (0-511, must match slot.gen)
```

Null handle = `0xFFFF`. Validation on every access:

```rust
fn resolve(h: Handle) -> Option<*mut u8> {
    let slot = &slots[h.index()];
    if slot.is_free() || h.gen() != slot.gen { return None; }
    Some(arena_base.add(slot.offset + HEADER_SIZE))
}
```

## Block Header (4 bytes, inside arena)

```rust
struct BlockHeader {
    size: u16,   // payload bytes (not including this header)
    kind: u16,   // type tag for debugging / compaction relocation callbacks
}
```

Each allocation: `[BlockHeader 4B] [user data …]`. The slot's `size` = `HEADER_SIZE + payload_size`.

## Compaction

Same algorithm as StringPool today, but arena-wide:

1. Walk the slot table (sorted by `offset`) — skip free slots
2. Compute compacted `offset` for each live block
3. `memmove` each block from old offset → new offset
4. Update `slot.offset` to new position
5. All free space coalesced at arena tail

Handles are **stable** through compaction — only `slot.offset` changes. No handle needs updating anywhere in the system.

Trigger: after any free, or when a bump-alloc fails with enough total free space but insufficient contiguous.

## What Gets Replaced

| Current | → | New |
|---|---|---|
| `StringPool` — 2KB pre-allocated, internal compaction | | **Removed** — strings are arena blocks, handle = string id |
| `WidgetTree` — `Vec<Widget>` + `Vec<WidgetExt>` | | Widgets are arena blocks, `WidgetId` = `Handle` |
| `Vm::vars` — `Vec<VmVar>` | | Variable slots are arena blocks |
| `Vm::arrays` — `Vec<VmArray>` (each holds `Vec<i32>`) | | Arrays are arena blocks |
| `FontList` — `Vec<Font>` | | Fonts are arena blocks |
| `ImageList` — `Vec<Image>` | | Images are arena blocks |

## RAM Budget (14 KB total)

| Component | Size |
|---|---|
| Globals / statics / stack | ~1 KB |
| Slot table (128 entries × 8 B) | 1,024 B |
| Arena data region | ~12 KB |
| **Worst-case fragmentation waste** | **none** (compaction) |

**StringPool savings**: 2 KB freed → used by arena for actual data.

## Trade-offs

| Pro | Con |
|---|---|
| No 2KB StringPool pre-allocation | 1KB slot table is permanent overhead |
| No u8 widget limit (now u16 handles) | Extra load: handle → slot → offset → data (2 extra reads vs direct pointer) |
| System-wide defragmentation | Major refactor — touches widget.rs, vm.rs, font.rs, render.rs, strpool.rs, ctx.rs, image.rs |
| Handles survive compaction + flash serialization | Must write custom container types (`ArenaSlice`, `ArenaVec`, etc.) — no `Box`/`Vec` from `alloc` |
| Generational indices catch use-after-free | Each allocation costs 12 bytes overhead (4B header + 8B slot) vs 4B today |
| Single ownership model — no Rc/Arc needed | |

## Architecture Note

This is a **fundamental change** to the memory model. It replaces `extern crate alloc` with a custom handle-based allocator. Every data structure that currently uses `Vec`, `Box`, or raw pointers must be rewritten to use handles and arena accessors. The payoff is substantial (2KB RAM, no widget limit, guaranteed defrag) but the migration touches virtually every file in `src/`.
