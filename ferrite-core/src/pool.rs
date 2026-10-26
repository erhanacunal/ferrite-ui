//! Object pool: one compacting arena for every object a VM program can see —
//! widgets, strings and arrays.
//!
//! Objects are named by a u16 *pointer* (`ObjPtr`). A pointer never changes
//! while the object lives; it indexes the pointer table, which holds the
//! object's current word offset in the arena:
//!
//! ```text
//! table: [Off; max_objects]   ptr → word offset of the block, or FREE_TAG | next free ptr
//! arena: [u32; arena_words]   blocks packed from 0 up to `top`
//! block: header u32 = ptr (bits 0-13) | type (bits 14-15) | payload words (bits 16-31)
//!        payload ...
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
pub type ObjPtr = u16;
pub const PTR_NONE: ObjPtr = 0xFFFF;

/// Block types (header bits 14-15).
pub const T_FREE: u8 = 0;
pub const T_WIDGET: u8 = 1;
pub const T_STRING: u8 = 2;
pub const T_ARRAY: u8 = 3;

/// Largest pointer count: the block header stores the pointer in 14 bits.
pub const MAX_OBJECTS: usize = 0x3FFF;
/// Largest payload of one object, in words (16-bit header field).
pub const MAX_OBJ_WORDS: usize = 0xFFFF;

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

#[inline]
fn header(ptr: ObjPtr, ty: u8, words: usize) -> u32 {
    (ptr as u32 & 0x3FFF) | ((ty as u32) << 14) | ((words as u32) << 16)
}
#[inline]
fn hdr_ptr(h: u32) -> ObjPtr {
    (h & 0x3FFF) as ObjPtr
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
    hwm: u16,
    /// FIFO of freed pointers, linked through the table.
    free_head: ObjPtr,
    free_tail: ObjPtr,
    /// Words held by freed blocks below `top` (what `compact` would reclaim).
    garbage: usize,
    /// Live objects per type (`[_, widgets, strings, arrays]`).
    live: [u16; 4],

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
            root: WidgetId::NONE,
            dfs_cache: Vec::new(),
            dfs_valid: false,
            dead: Widget::DEAD,
            draw_epoch: 0,
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
        self.root = WidgetId::NONE;
        self.dfs_cache.clear();
        self.dfs_valid = false;
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
                words += 1 + hdr_words(h);
            }
            at += 1 + hdr_words(h);
        }
        words * 4
    }
    /// Heap bytes held by the pool (arena + table + DFS cache).
    pub fn heap_bytes(&self) -> usize {
        use core::mem::size_of;
        self.arena.len() * 4
            + self.table.len() * size_of::<Off>()
            + self.dfs_cache.capacity() * size_of::<WidgetId>()
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
        Some(&self.arena[at + 1..at + 1 + n])
    }

    #[inline]
    pub(crate) fn payload_mut(&mut self, ptr: ObjPtr, ty: u8) -> Option<&mut [u32]> {
        let at = self.block(ptr, ty)?;
        let n = hdr_words(self.arena[at]);
        Some(&mut self.arena[at + 1..at + 1 + n])
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
            let next = (self.table[p as usize] & !FREE_TAG) as ObjPtr;
            self.free_head = if p == self.free_tail { PTR_NONE } else { next };
            if self.free_head == PTR_NONE {
                self.free_tail = PTR_NONE;
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
        if words > MAX_OBJ_WORDS || !self.reserve_top(1 + words) {
            return None;
        }
        let p = self.take_ptr()?;
        let at = self.top;
        self.arena[at] = header(p, ty, words);
        self.arena[at + 1..at + 1 + words].fill(0);
        self.top += 1 + words;
        self.table[p as usize] = at as Off;
        self.live[ty as usize] += 1;
        Some(p)
    }

    /// Free an object of type `ty` (no-op if `ptr` names none).
    pub(crate) fn free_obj(&mut self, ptr: ObjPtr, ty: u8) {
        let Some(at) = self.block(ptr, ty) else {
            return;
        };
        let words = hdr_words(self.arena[at]);
        self.release_block(at, words);
        self.live[ty as usize] -= 1;
        self.put_ptr(ptr);
    }

    /// Turn the block at `at` (`words` payload) into a hole, or drop it when
    /// it ends at `top`.
    fn release_block(&mut self, at: usize, words: usize) {
        if at + 1 + words == self.top {
            self.top = at;
        } else {
            self.arena[at] = header(0, T_FREE, words);
            self.garbage += 1 + words;
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
            self.arena[at] = header(ptr, ty, words);
            let tail = at + 1 + words;
            // The cut-off words become a hole (a 1-word hole is just a header).
            self.release_block(tail, old - words - 1);
            return true;
        }
        // Last block: extend in place.
        if at + 1 + old == self.top && self.top + (words - old) <= self.arena.len() {
            self.arena[at + 1 + old..at + 1 + words].fill(0);
            self.arena[at] = header(ptr, ty, words);
            self.top += words - old;
            return true;
        }
        // Move to top. Compaction may move the block (and may leave it last).
        if !self.reserve_top(1 + words) {
            return false;
        }
        let at = self.table[ptr as usize] as usize;
        if at + 1 + old == self.top {
            self.arena[at + 1 + old..at + 1 + words].fill(0);
            self.arena[at] = header(ptr, ty, words);
            self.top += words - old;
            return true;
        }
        let dst = self.top;
        self.arena.copy_within(at + 1..at + 1 + old, dst + 1);
        self.arena[dst + 1 + old..dst + 1 + words].fill(0);
        self.arena[dst] = header(ptr, ty, words);
        self.top += 1 + words;
        self.table[ptr as usize] = dst as Off;
        self.arena[at] = header(0, T_FREE, old);
        self.garbage += 1 + old;
        true
    }

    /// Slide every live block down over the holes, updating the table.
    pub fn compact(&mut self) {
        let mut read = 0;
        let mut write = 0;
        while read < self.top {
            let h = self.arena[read];
            let n = 1 + hdr_words(h);
            if hdr_type(h) != T_FREE {
                if read != write {
                    self.arena.copy_within(read..read + n, write);
                    self.table[hdr_ptr(h) as usize] = write as Off;
                }
                write += n;
            }
            read += n;
        }
        self.top = write;
        self.garbage = 0;
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
        // 16 words: three 4-word blocks (header + 3) fill 12.
        let mut p = pool(64, 8);
        let a = p.arr_from(&[1, 2, 3]).unwrap();
        let b = p.arr_from(&[4, 5, 6]).unwrap();
        let c = p.arr_from(&[7, 8, 9]).unwrap();
        p.arr_free(b);
        assert_eq!(p.garbage_bytes(), 16);
        // 6 words don't fit at top (4 left) until the hole is reclaimed.
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
        assert_eq!((p.used_bytes(), p.garbage_bytes()), (16, 0));
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
        assert_eq!(p.garbage_bytes(), 20);
        assert!(p.resize_obj(a, T_ARRAY, 1), "shrink");
        assert_eq!(p.arr(a), Some(&[1][..]));
        p.compact();
        assert_eq!(p.arr(b), Some(&[9][..]));
        assert_eq!(p.arr(a), Some(&[1][..]));
        assert_eq!(p.used_bytes(), 16);
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
