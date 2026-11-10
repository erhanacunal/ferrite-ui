//! Object pool: one compacting arena for every object a VM program can see —
//! widgets, strings and arrays.
//!
//! Objects are named by a *pointer* (`ObjPtr`: u16, u32 with `wide_obj`). A pointer never changes
//! while the object lives; it indexes the pointer table, which holds the
//! object's current word offset in the arena:
//!
//! ```text
//! table: [Off; max_objects]   ptr → word offset of the block, or FREE_TAG | next free ptr
//! arena: [u32; arena_words]   blocks packed from 0 up to `top`
//! block: header u32 = ptr (bits 0-13) | type (bits 14-15) | payload words (bits 16-31)
//!        payload ...
//! wide_obj: header u32 = type (bits 14-15) | payload words (bits 16-31), then ptr u32
//! hole:  header u32 = T_FREE | (hole words - 1) -- always a single header word
//! ```
//!
//! Allocation bumps `top`. Freeing marks the block `T_FREE` (or lowers `top`
//! when it is the last block). When an allocation does not fit, `compact`
//! slides the live blocks down in one pass — each block names its own pointer,
//! so the table entry is updated as the block moves — and the allocation is
//! retried. Nothing outside the pool ever holds an offset, so moving is
//! invisible to the VM and to Rust code (`compact` needs `&mut self`, so no
//! borrow of an object can survive it).
//!
//! Freed pointers are reused in FIFO order (a queue linked through the table),
//! so re-entrant builders such as modal dialogs get the same ids back.
//!
//! Lifetimes are manual: `destroy_subtree` (widgets), `str_free` / the string
//! sweep at `strClear()` (strings), `arr_free` (arrays).
//!
//! The arena and the table are allocated once (at construction, then again by
//! [`ObjPool::configure`] from `RES_SYSCFG`), so the pool never fragments the
//! linker heap. Widget operations live in `widget.rs`, string operations in
//! `strpool.rs` (both `impl ObjPool`).
extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::syscfg::try_alloc_slice;
use crate::widget::{Widget, WidgetId};

/// Object pointer: table index. `PTR_NONE` = no object.
#[cfg(not(feature = "wide_obj"))]
pub type ObjPtr = u16;
/// Object pointer: table index. `PTR_NONE` = no object.
#[cfg(feature = "wide_obj")]
pub type ObjPtr = u32;
pub const PTR_NONE: ObjPtr = ObjPtr::MAX;

/// Changed widgets `ObjPool` lists per frame (see `ObjPool::changed`).
pub(crate) const CHANGED_MAX: usize = 8;

/// Block types (header bits 14-15).
pub const T_FREE: u8 = 0;
pub const T_WIDGET: u8 = 1;
pub const T_STRING: u8 = 2;
pub const T_ARRAY: u8 = 3;

/// Largest pointer count: the block header stores the pointer in 14 bits.
#[cfg(not(feature = "wide_obj"))]
pub const MAX_OBJECTS: usize = 0x3FFF;
/// Largest pointer count: the pointer has its own header word; the table
/// entry's free tag (bit 31) caps it -- in practice the heap does first.
#[cfg(feature = "wide_obj")]
pub const MAX_OBJECTS: usize = 0x7FFF_FFFE;
/// Largest payload of one object, in words. The header field has 16 bits,
/// and a freed block becomes a hole whose field holds its whole length - 1
/// (`HDR - 1` words more than the payload), which must fit too.
pub const MAX_OBJ_WORDS: usize = 0xFFFF - (HDR - 1);

#[cfg(not(feature = "pool_large"))]
mod profile {
    /// Table entry: word offset. 15 bits address 128 KB of arena.
    pub type Off = u16;
    pub const DEFAULT_ARENA_BYTES: usize = 8 * 1024;
    pub const DEFAULT_MAX_OBJECTS: usize = 320;
}
#[cfg(feature = "pool_large")]
mod profile {
    /// Table entry: word offset, for arenas past 128 KB.
    pub type Off = u32;
    pub const DEFAULT_ARENA_BYTES: usize = 2 * 1024 * 1024;
    pub const DEFAULT_MAX_OBJECTS: usize = 8192;
}
use profile::*;

/// Top bit of a table entry marks a free pointer; the rest links the FIFO.
const FREE_TAG: Off = 1 << (Off::BITS - 1);
/// Largest arena (words) a table entry can address.
const MAX_ARENA_WORDS: usize = (FREE_TAG as usize) - 1;

/// Header words of a live block (a hole always has one).
#[cfg(not(feature = "wide_obj"))]
pub(crate) const HDR: usize = 1;
#[cfg(feature = "wide_obj")]
pub(crate) const HDR: usize = 2;

#[cfg(feature = "wide_obj")]
const _: () = assert!(cfg!(feature = "pool_large"), "wide_obj needs pool_large (u32 table entries)");

/// Write a live block header at `at`.
#[inline]
fn put_header(arena: &mut [u32], at: usize, ptr: ObjPtr, ty: u8, words: usize) {
    let h = ((ty as u32) << 14) | ((words as u32) << 16);
    #[cfg(not(feature = "wide_obj"))]
    {
        arena[at] = h | (ptr as u32 & 0x3FFF);
    }
    #[cfg(feature = "wide_obj")]
    {
        arena[at] = h;
        arena[at + 1] = ptr;
    }
}
/// Header of a hole `len` words long (header included).
#[inline]
fn hole(len: usize) -> u32 {
    ((T_FREE as u32) << 14) | (((len - 1) as u32) << 16)
}
/// Pointer of the live block at `at`.
#[inline]
fn hdr_ptr(arena: &[u32], at: usize) -> ObjPtr {
    #[cfg(not(feature = "wide_obj"))]
    {
        (arena[at] & 0x3FFF) as ObjPtr
    }
    #[cfg(feature = "wide_obj")]
    {
        arena[at + 1]
    }
}
/// Words of the block with header `h`, header included.
#[inline]
fn blk_len(h: u32) -> usize {
    (if hdr_type(h) == T_FREE { 1 } else { HDR }) + hdr_words(h)
}
#[inline]
fn hdr_type(h: u32) -> u8 {
    ((h >> 14) & 3) as u8
}
#[inline]
fn hdr_words(h: u32) -> usize {
    (h >> 16) as usize
}

pub struct ObjPool {
    pub(crate) arena: Box<[u32]>,
    table: Box<[Off]>,
    /// First unused arena word.
    top: usize,
    /// Pointers handed out so far (table entries `0..hwm` are initialised).
    hwm: ObjPtr,
    /// FIFO of freed pointers, linked through the table.
    free_head: ObjPtr,
    free_tail: ObjPtr,
    /// Words held by freed blocks below `top` (what `compact` would reclaim).
    garbage: usize,
    /// Live objects per type (`[_, widgets, strings, arrays]`).
    live: [u32; 4],
    /// Task that allocated each object (`multi_program`): it is freed when
    /// that task ends.
    #[cfg(feature = "multi_program")]
    pub(crate) owner: Box<[u8]>,
    /// Task that handles each widget's events (`multi_program`): the one that
    /// allocated it, or that set its last on_click / on_tap / on_paint.
    #[cfg(feature = "multi_program")]
    pub(crate) handler: Box<[u8]>,
    /// Task the next allocations belong to (`multi_program`). The runtime sets
    /// it around everything a task's VM runs; 0 (main) otherwise.
    #[cfg(feature = "multi_program")]
    pub cur_task: u8,

    // --- Widget tree state (see widget.rs) ---
    pub root: WidgetId,
    pub(crate) dfs_cache: Vec<WidgetId>,
    pub(crate) dfs_valid: bool,
    /// Scratch widget returned by `get_mut` for an id that names no widget,
    /// so a stale id behaves like a destroyed widget instead of panicking.
    pub(crate) dead: Widget,
    /// Bumped whenever something outside the widget renderer may have drawn
    /// over widgets (VM drawing builtins, erased areas). A widget redrawn
    /// incrementally (gauge) trusts its on-screen pixels only if they were
    /// drawn in the current epoch.
    pub(crate) draw_epoch: u32,
    /// Some widget may have a dirty bit set. Raised by everything that sets
    /// one (`mark_dirty`, `alloc_widget`); lowered only by a render scan that
    /// found no dirty widget. While it is down the render passes skip their
    /// DFS scans, so an idle main-loop pass doesn't walk the tree.
    pub(crate) maybe_dirty: bool,
    /// Widgets `mark_dirty` was called on since the last render (the roots
    /// of what changed; a move, `mark_moved`, is not listed). `changed_n`
    /// past `CHANGED_MAX` = more than fit: anything may have changed. A
    /// moved subtree with nothing listed in it only moved, so its pixels can
    /// be shifted instead of redrawn (`render::shift_moved`).
    pub(crate) changed: [WidgetId; CHANGED_MAX],
    pub(crate) changed_n: u8,
    /// `draw_epoch` when the last dirty render ended: equal at the start of
    /// the next one = nothing drew over the screen between them, so every
    /// widget still shows what it last drew.
    pub(crate) settled_epoch: u32,
    /// This render's shifted pixels: (DFS index of the moved subtree root,
    /// destination rect). Pass 2 leaves them alone. Empty on boards that
    /// cannot move pixels (never allocated there).
    pub(crate) shifted: Vec<(usize, crate::types::Rect)>,
    /// Scratch for pass 2: the part of a shifted subtree's box still to draw.
    pub(crate) shift_keep: Vec<crate::types::Rect>,
    /// What is drawn over the widgets from outside the tree (the on-screen keyboard), set by
    /// the input handler: a board renderer that repaints whole areas
    /// (`WidgetRenderer::compose`) leaves areas that touch it to the built-in passes.
    pub overlay: Option<crate::types::Rect>,
    /// Retained layers: roots, plane alphas, the running pass's scope (`layer.rs`).
    #[cfg(feature = "layers")]
    pub layers: crate::layer::Layers,
    /// Geometry by DFS index for the current render pass (`geo.rs`).
    #[cfg(feature = "render_cache")]
    pub(crate) geo: Vec<crate::geo::Geo>,
    #[cfg(feature = "render_cache")]
    pub(crate) geo_stack: Vec<u32>,
    #[cfg(feature = "render_cache")]
    pub(crate) geo_valid: bool,
}

impl ObjPool {
    /// Pool with the compile-time default capacity.
    pub fn new() -> Self {
        let mut pool = Self::empty();
        // Boot-time allocation on an empty heap; failing here is fatal anyway.
        assert!(pool.alloc_storage(DEFAULT_ARENA_BYTES, DEFAULT_MAX_OBJECTS));
        pool
    }

    fn empty() -> Self {
        Self {
            arena: Box::new([]),
            table: Box::new([]),
            top: 0,
            hwm: 0,
            free_head: PTR_NONE,
            free_tail: PTR_NONE,
            garbage: 0,
            live: [0; 4],
            #[cfg(feature = "multi_program")]
            owner: Box::new([]),
            #[cfg(feature = "multi_program")]
            handler: Box::new([]),
            #[cfg(feature = "multi_program")]
            cur_task: 0,
            root: WidgetId::NONE,
            dfs_cache: Vec::new(),
            dfs_valid: false,
            dead: Widget::DEAD,
            draw_epoch: 0,
            maybe_dirty: false,
            changed: [WidgetId::NONE; CHANGED_MAX],
            changed_n: 0,
            settled_epoch: u32::MAX,
            shifted: Vec::new(),
            overlay: None,
            shift_keep: Vec::new(),
            #[cfg(feature = "layers")]
            layers: crate::layer::Layers::new(),
            #[cfg(feature = "render_cache")]
            geo: Vec::new(),
            #[cfg(feature = "render_cache")]
            geo_stack: Vec::new(),
            #[cfg(feature = "render_cache")]
            geo_valid: false,
        }
    }

    /// Resize to `arena_bytes` / `max_objects` (0 = compile-time default),
    /// clamped to what the table can address. Drops every object. Returns
    /// false if the heap cannot hold the request; the pool then falls back to
    /// the defaults (or stays empty if even that fails).
    pub fn configure(&mut self, arena_bytes: usize, max_objects: usize) -> bool {
        let bytes = if arena_bytes == 0 { DEFAULT_ARENA_BYTES } else { arena_bytes };
        let objects = if max_objects == 0 { DEFAULT_MAX_OBJECTS } else { max_objects };
        let words = (bytes / 4).min(MAX_ARENA_WORDS);
        let objects = objects.min(MAX_OBJECTS);
        self.clear();
        if words == self.arena.len() && objects == self.table.len() {
            return true;
        }
        // Release the old storage first so the allocator can reuse it.
        *self = Self::empty();
        if self.alloc_storage(words * 4, objects) {
            return true;
        }
        self.alloc_storage(DEFAULT_ARENA_BYTES, DEFAULT_MAX_OBJECTS);
        false
    }

    fn alloc_storage(&mut self, bytes: usize, objects: usize) -> bool {
        let objects = objects.min(MAX_OBJECTS);
        // The DFS cache holds at most one entry per object; reserving it here
        // keeps it from growing (and fragmenting the heap) at render time.
        let mut dfs = Vec::new();
        let (Some(arena), Some(table), Ok(())) = (
            try_alloc_slice((bytes / 4).min(MAX_ARENA_WORDS), 0u32),
            try_alloc_slice(objects, FREE_TAG),
            dfs.try_reserve_exact(objects),
        ) else {
            return false;
        };
        #[cfg(feature = "multi_program")]
        {
            let (Some(owner), Some(handler)) = (try_alloc_slice(objects, 0u8), try_alloc_slice(objects, 0u8))
            else {
                return false;
            };
            self.owner = owner;
            self.handler = handler;
        }
        self.arena = arena;
        self.table = table;
        self.dfs_cache = dfs;
        true
    }

    /// Drop every object (program reload).
    pub fn clear(&mut self) {
        self.top = 0;
        self.hwm = 0;
        self.free_head = PTR_NONE;
        self.free_tail = PTR_NONE;
        self.garbage = 0;
        self.live = [0; 4];
        #[cfg(feature = "multi_program")]
        {
            self.cur_task = 0;
        }
        self.root = WidgetId::NONE;
        self.dfs_cache.clear();
        self.dfs_valid = false;
        self.maybe_dirty = false;
        self.changed_n = 0;
        self.settled_epoch = self.draw_epoch.wrapping_sub(1);
        self.shifted.clear();
        #[cfg(feature = "layers")]
        {
            // The surfaces stay shown until the next render hides them.
            self.layers = crate::layer::Layers::new();
        }
        #[cfg(feature = "render_cache")]
        {
            self.geo_valid = false;
        }
    }

    // --- Diagnostics ---

    /// Arena capacity in bytes.
    pub fn capacity_bytes(&self) -> usize {
        self.arena.len() * 4
    }
    /// Arena bytes below `top` (live objects + reclaimable holes).
    pub fn used_bytes(&self) -> usize {
        self.top * 4
    }
    /// Bytes in freed blocks that `compact` would reclaim.
    pub fn garbage_bytes(&self) -> usize {
        self.garbage * 4
    }
    /// Pointer table size (`max_objects`).
    pub fn capacity_objects(&self) -> usize {
        self.table.len()
    }
    /// Live objects of type `ty`, or all types for `T_FREE`.
    pub fn live_objects(&self, ty: u8) -> usize {
        if ty == T_FREE {
            self.live.iter().map(|&n| n as usize).sum()
        } else {
            self.live[ty as usize] as usize
        }
    }
    /// Arena bytes (headers included) held by live objects of type `ty`.
    pub fn bytes_of(&self, ty: u8) -> usize {
        let mut words = 0;
        let mut at = 0;
        while at < self.top {
            let h = self.arena[at];
            if hdr_type(h) == ty {
                words += blk_len(h);
            }
            at += blk_len(h);
        }
        words * 4
    }
    /// Heap bytes held by the pool (arena + table + DFS cache).
    pub fn heap_bytes(&self) -> usize {
        use core::mem::size_of;
        let tasks = if cfg!(feature = "multi_program") { 2 * self.table.len() } else { 0 };
        #[cfg(feature = "render_cache")]
        let geo = self.geo.capacity() * size_of::<crate::geo::Geo>() + self.geo_stack.capacity() * 4;
        #[cfg(not(feature = "render_cache"))]
        let geo = 0;
        self.arena.len() * 4
            + self.table.len() * size_of::<Off>()
            + self.dfs_cache.capacity() * size_of::<WidgetId>()
            + tasks
            + geo
    }
    /// Pointers handed out so far: every live pointer is below this. Used as
    /// the iteration bound and as a loop guard for tree walks.
    pub fn ptr_count(&self) -> usize {
        self.hwm as usize
    }

    // --- Core object operations ---

    /// Word offset of the block for `ptr` if it is a live object of type `ty`.
    #[inline]
    pub(crate) fn block(&self, ptr: ObjPtr, ty: u8) -> Option<usize> {
        let e = *self.table.get(ptr as usize)?;
        if (ptr as usize) >= self.hwm as usize || e & FREE_TAG != 0 {
            return None;
        }
        let at = e as usize;
        (hdr_type(self.arena[at]) == ty).then_some(at)
    }

    /// Payload words of a live object of type `ty`.
    #[inline]
    pub(crate) fn payload(&self, ptr: ObjPtr, ty: u8) -> Option<&[u32]> {
        let at = self.block(ptr, ty)?;
        let n = hdr_words(self.arena[at]);
        Some(&self.arena[at + HDR..at + HDR + n])
    }

    #[inline]
    pub(crate) fn payload_mut(&mut self, ptr: ObjPtr, ty: u8) -> Option<&mut [u32]> {
        let at = self.block(ptr, ty)?;
        let n = hdr_words(self.arena[at]);
        Some(&mut self.arena[at + HDR..at + HDR + n])
    }

    /// Type of the object `ptr` names, or `T_FREE` for none.
    pub fn type_of(&self, ptr: ObjPtr) -> u8 {
        match self.table.get(ptr as usize) {
            Some(&e) if (ptr as usize) < self.hwm as usize && e & FREE_TAG == 0 => {
                hdr_type(self.arena[e as usize])
            }
            _ => T_FREE,
        }
    }

    /// Take a pointer: the oldest freed one, else the next unused one.
    fn take_ptr(&mut self) -> Option<ObjPtr> {
        if self.free_head != PTR_NONE {
            let p = self.free_head;
            if p == self.free_tail {
                self.free_head = PTR_NONE;
                self.free_tail = PTR_NONE;
            } else {
                self.free_head = (self.table[p as usize] & !FREE_TAG) as ObjPtr;
            }
            return Some(p);
        }
        if (self.hwm as usize) < self.table.len() {
            let p = self.hwm;
            self.hwm += 1;
            return Some(p);
        }
        None
    }

    /// Queue `p` for reuse (FIFO tail).
    fn put_ptr(&mut self, p: ObjPtr) {
        self.table[p as usize] = FREE_TAG;
        if self.free_tail == PTR_NONE {
            self.free_head = p;
        } else {
            self.table[self.free_tail as usize] = FREE_TAG | p as Off;
        }
        self.free_tail = p;
    }

    /// Make `words` free words at `top`, compacting if needed.
    fn reserve_top(&mut self, words: usize) -> bool {
        if self.top + words <= self.arena.len() {
            return true;
        }
        if self.garbage > 0 {
            self.compact();
        }
        self.top + words <= self.arena.len()
    }

    /// Allocate an object with `words` zeroed payload words.
    pub(crate) fn alloc_obj(&mut self, ty: u8, words: usize) -> Option<ObjPtr> {
        if words > MAX_OBJ_WORDS || !self.reserve_top(HDR + words) {
            return None;
        }
        let p = self.take_ptr()?;
        #[cfg(feature = "multi_program")]
        {
            self.owner[p as usize] = self.cur_task;
            self.handler[p as usize] = self.cur_task;
        }
        let at = self.top;
        put_header(&mut self.arena, at, p, ty, words);
        self.arena[at + HDR..at + HDR + words].fill(0);
        self.top += HDR + words;
        self.table[p as usize] = at as Off;
        self.live[ty as usize] += 1;
        Some(p)
    }

    /// Free an object of type `ty` (no-op if `ptr` names none).
    pub(crate) fn free_obj(&mut self, ptr: ObjPtr, ty: u8) {
        let Some(at) = self.block(ptr, ty) else {
            return;
        };
        let len = blk_len(self.arena[at]);
        self.release_block(at, len);
        self.live[ty as usize] -= 1;
        self.put_ptr(ptr);
    }

    /// Turn the `len` words at `at` into a hole, or drop them when they end
    /// at `top`.
    fn release_block(&mut self, at: usize, len: usize) {
        if len == 0 {
            return;
        }
        if at + len == self.top {
            self.top = at;
        } else {
            self.arena[at] = hole(len);
            self.garbage += len;
        }
    }

    /// Resize an object's payload to `words`, keeping its pointer and the
    /// first `min(old, new)` words; new words are zeroed. Grows in place when
    /// the object is the last block, else moves it to `top`.
    pub(crate) fn resize_obj(&mut self, ptr: ObjPtr, ty: u8, words: usize) -> bool {
        let Some(at) = self.block(ptr, ty) else {
            return false;
        };
        let old = hdr_words(self.arena[at]);
        if words == old {
            return true;
        }
        if words > MAX_OBJ_WORDS {
            return false;
        }
        if words < old {
            put_header(&mut self.arena, at, ptr, ty, words);
            // The cut-off words become a hole (a 1-word hole is just a header).
            self.release_block(at + HDR + words, old - words);
            return true;
        }
        // The last block only needs the added words (compaction keeps it
        // last); any other block needs room for a copy at `top`.
        let need = if at + HDR + old == self.top { words - old } else { HDR + words };
        if !self.reserve_top(need) {
            return false;
        }
        // Compaction may have moved the block (and may have left it last).
        let at = self.table[ptr as usize] as usize;
        if at + HDR + old == self.top {
            self.arena[at + HDR + old..at + HDR + words].fill(0);
            put_header(&mut self.arena, at, ptr, ty, words);
            self.top += words - old;
            return true;
        }
        let dst = self.top;
        self.arena.copy_within(at + HDR..at + HDR + old, dst + HDR);
        self.arena[dst + HDR + old..dst + HDR + words].fill(0);
        put_header(&mut self.arena, dst, ptr, ty, words);
        self.top += HDR + words;
        self.table[ptr as usize] = dst as Off;
        self.arena[at] = hole(HDR + old);
        self.garbage += HDR + old;
        true
    }

    /// Slide every live block down over the holes, updating the table.
    pub fn compact(&mut self) {
        let mut read = 0;
        let mut write = 0;
        while read < self.top {
            let h = self.arena[read];
            let n = blk_len(h);
            if hdr_type(h) != T_FREE {
                if read != write {
                    self.arena.copy_within(read..read + n, write);
                    let p = hdr_ptr(&self.arena, write);
                    self.table[p as usize] = write as Off;
                }
                write += n;
            }
            read += n;
        }
        self.top = write;
        self.garbage = 0;
    }

    /// True if `p` belongs to the running task: always without
    /// `multi_program`. Scopes the string sweep and literal sharing.
    #[inline]
    pub(crate) fn mine(&self, p: ObjPtr) -> bool {
        #[cfg(feature = "multi_program")]
        {
            self.owner.get(p as usize) == Some(&self.cur_task)
        }
        #[cfg(not(feature = "multi_program"))]
        {
            let _ = p;
            true
        }
    }

    /// Give object `obj` to the task that owns object `like` (a string the
    /// runtime made for a task's widget, e.g. keyboard input). No-op without
    /// `multi_program`.
    #[inline]
    pub(crate) fn adopt(&mut self, obj: ObjPtr, like: ObjPtr) {
        #[cfg(feature = "multi_program")]
        if let (Some(&o), true) = (self.owner.get(like as usize), (obj as usize) < self.owner.len()) {
            self.owner[obj as usize] = o;
        }
        #[cfg(not(feature = "multi_program"))]
        let _ = (obj, like);
    }

    /// Pointers of the live objects of type `ty`, in pointer order.
    pub(crate) fn ptrs_of(&self, ty: u8) -> impl Iterator<Item = ObjPtr> + '_ {
        (0..self.hwm).filter(move |&p| self.block(p, ty).is_some())
    }

    // --- Arrays (VM `int[]`: one i32 per payload word) ---

    /// New zeroed array of `len` elements.
    pub fn arr_alloc(&mut self, len: usize) -> Option<ObjPtr> {
        self.alloc_obj(T_ARRAY, len)
    }

    /// New array holding `values`.
    pub fn arr_from(&mut self, values: &[i32]) -> Option<ObjPtr> {
        let p = self.arr_alloc(values.len())?;
        if let Some(a) = self.arr_mut(p) {
            a.copy_from_slice(values);
        }
        Some(p)
    }

    /// New array with one element per byte of `data`.
    pub fn arr_from_bytes(&mut self, data: &[u8]) -> Option<ObjPtr> {
        let p = self.arr_alloc(data.len())?;
        if let Some(a) = self.arr_mut(p) {
            for (v, &b) in a.iter_mut().zip(data) {
                *v = b as i32;
            }
        }
        Some(p)
    }

    pub fn arr_free(&mut self, id: ObjPtr) {
        self.free_obj(id, T_ARRAY);
    }

    /// Elements of array `id` (None when `id` names no array).
    pub fn arr(&self, id: ObjPtr) -> Option<&[i32]> {
        let w = self.payload(id, T_ARRAY)?;
        // SAFETY: u32 and i32 have the same size and alignment.
        Some(unsafe { core::slice::from_raw_parts(w.as_ptr() as *const i32, w.len()) })
    }

    pub fn arr_mut(&mut self, id: ObjPtr) -> Option<&mut [i32]> {
        let w = self.payload_mut(id, T_ARRAY)?;
        // SAFETY: u32 and i32 have the same size and alignment.
        Some(unsafe { core::slice::from_raw_parts_mut(w.as_mut_ptr() as *mut i32, w.len()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(bytes: usize, objects: usize) -> ObjPool {
        let mut p = ObjPool::empty();
        assert!(p.alloc_storage(bytes, objects));
        p
    }

    #[test]
    fn alloc_free_reuses_pointers_fifo() {
        let mut p = pool(256, 8);
        let a = p.arr_alloc(2).unwrap();
        let b = p.arr_alloc(2).unwrap();
        let c = p.arr_alloc(2).unwrap();
        assert_eq!((a, b, c), (0, 1, 2));
        p.arr_free(b);
        p.arr_free(a);
        // Oldest freed first.
        assert_eq!(p.arr_alloc(1), Some(b));
        assert_eq!(p.arr_alloc(1), Some(a));
        assert_eq!(p.arr_alloc(1), Some(3));
    }

    #[test]
    fn compaction_keeps_pointers_and_contents() {
        // Three (header + 3)-word blocks, one more block of room left.
        let blk = HDR + 3;
        let mut p = pool(4 * 4 * blk, 8);
        let a = p.arr_from(&[1, 2, 3]).unwrap();
        let b = p.arr_from(&[4, 5, 6]).unwrap();
        let c = p.arr_from(&[7, 8, 9]).unwrap();
        p.arr_free(b);
        assert_eq!(p.garbage_bytes(), 4 * blk);
        // header + 5 words don't fit at top (one block left) until the hole is reclaimed.
        let d = p.arr_from(&[10, 11, 12, 13, 14]).unwrap();
        assert_eq!(p.garbage_bytes(), 0);
        assert_eq!(p.arr(a), Some(&[1, 2, 3][..]));
        assert_eq!(p.arr(c), Some(&[7, 8, 9][..]));
        assert_eq!(p.arr(d), Some(&[10, 11, 12, 13, 14][..]));
        assert_eq!(d, b, "the freed pointer is reused");
        assert!(p.arr_alloc(2).is_none(), "arena full");
    }

    #[test]
    fn freeing_the_last_block_lowers_top() {
        let mut p = pool(64, 8);
        let a = p.arr_alloc(3).unwrap();
        let b = p.arr_alloc(3).unwrap();
        p.arr_free(b);
        assert_eq!((p.used_bytes(), p.garbage_bytes()), (4 * (HDR + 3), 0));
        p.arr_free(a);
        assert_eq!(p.used_bytes(), 0);
    }

    #[test]
    fn resize_grows_in_place_or_moves() {
        let mut p = pool(128, 8);
        let a = p.arr_from(&[1, 2]).unwrap();
        assert!(p.resize_obj(a, T_ARRAY, 4), "last block grows in place");
        assert_eq!(p.arr(a), Some(&[1, 2, 0, 0][..]));
        let b = p.arr_from(&[9]).unwrap();
        assert!(p.resize_obj(a, T_ARRAY, 6), "moves past b");
        assert_eq!(p.arr(a), Some(&[1, 2, 0, 0, 0, 0][..]));
        assert_eq!(p.arr(b), Some(&[9][..]));
        assert_eq!(p.garbage_bytes(), 4 * (HDR + 4));
        assert!(p.resize_obj(a, T_ARRAY, 1), "shrink");
        assert_eq!(p.arr(a), Some(&[1][..]));
        p.compact();
        assert_eq!(p.arr(b), Some(&[9][..]));
        assert_eq!(p.arr(a), Some(&[1][..]));
        assert_eq!(p.used_bytes(), 4 * (2 * HDR + 2));
    }

    #[test]
    fn last_block_grows_after_compaction_without_room_for_a_copy() {
        // a, b and one spare word: b needs 2 more, a's hole has them, but a
        // second copy of b would not fit.
        let blk = HDR + 3;
        let mut p = pool(4 * (2 * blk + 1), 8);
        let a = p.arr_from(&[1, 2, 3]).unwrap();
        let b = p.arr_from(&[4, 5, 6]).unwrap();
        p.arr_free(a);
        assert!(p.resize_obj(b, T_ARRAY, 5));
        assert_eq!(p.arr(b), Some(&[4, 5, 6, 0, 0][..]));
        assert_eq!((p.used_bytes(), p.garbage_bytes()), (4 * (HDR + 5), 0));
        assert!(!p.resize_obj(b, T_ARRAY, blk + 4 + 1), "past the arena");
        assert_eq!(p.arr(b), Some(&[4, 5, 6, 0, 0][..]));
    }

    /// The arena of a narrow build is smaller than the largest object.
    #[cfg(feature = "pool_large")]
    #[test]
    fn largest_object_leaves_a_walkable_hole() {
        let mut p = pool(4 * (2 * (HDR + MAX_OBJ_WORDS) + 2 * (HDR + 1)), 8);
        assert!(p.arr_alloc(MAX_OBJ_WORDS + 1).is_none());
        let a = p.arr_alloc(MAX_OBJ_WORDS).unwrap();
        let b = p.arr_from(&[7]).unwrap();
        // Freed in the middle: a hole of the object's whole length.
        p.arr_free(a);
        assert_eq!(p.garbage_bytes(), 4 * (HDR + MAX_OBJ_WORDS));
        assert_eq!(p.bytes_of(T_ARRAY), 4 * (HDR + 1));
        // A resize up to the limit that has to compact and move the block.
        let c = p.arr_alloc(MAX_OBJ_WORDS - 1).unwrap();
        let d = p.arr_from(&[8]).unwrap();
        assert!(p.resize_obj(c, T_ARRAY, MAX_OBJ_WORDS));
        assert_eq!(p.bytes_of(T_ARRAY), 4 * (2 * (HDR + 1) + HDR + MAX_OBJ_WORDS));
        p.compact();
        assert_eq!(p.arr(b), Some(&[7][..]));
        assert_eq!(p.arr(d), Some(&[8][..]));
        assert_eq!(p.arr(c).map(|s| s.len()), Some(MAX_OBJ_WORDS));
        assert_eq!(p.used_bytes(), 4 * (2 * (HDR + 1) + HDR + MAX_OBJ_WORDS));
    }

    #[cfg(feature = "wide_obj")]
    #[test]
    fn wide_pointers_past_14_bits() {
        let n = 0x4100usize;
        let mut p = pool(n * 4 * (HDR + 1), n);
        let ptrs: Vec<ObjPtr> = (0..n).map(|i| p.arr_from(&[i as i32]).unwrap()).collect();
        assert_eq!(*ptrs.last().unwrap(), (n - 1) as ObjPtr);
        // Free every other one, then compact: survivors keep id and value.
        for &q in ptrs.iter().step_by(2) {
            p.arr_free(q);
        }
        p.compact();
        for (i, &q) in ptrs.iter().enumerate().skip(1).step_by(2) {
            assert_eq!(p.arr(q), Some(&[i as i32][..]));
        }
        assert_eq!(p.live_objects(T_ARRAY), n / 2);
    }

    #[test]
    fn type_checks_and_capacity() {
        let mut p = pool(64, 2);
        let a = p.arr_alloc(1).unwrap();
        assert_eq!(p.type_of(a), T_ARRAY);
        assert_eq!(p.type_of(1), T_FREE);
        assert!(p.payload(a, T_STRING).is_none());
        p.arr_alloc(1).unwrap();
        assert!(p.arr_alloc(1).is_none(), "out of pointers");
        assert_eq!(p.live_objects(T_ARRAY), 2);
    }
}
