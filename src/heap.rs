/// Simple linked-list heap allocator for bare-metal use.
///
/// 14KB static heap buffer. First-fit allocation with free-block coalescing.
/// Single-threaded (no locking) — safe for Cortex-M3 bare-metal.
///
/// Usage:
///   heap::init() at startup, then use Box/Vec from `extern crate alloc`.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

const HEAP_SIZE: usize = 14 * 1024;

/// Minimum block size (header + at least 1 usable byte, aligned to 4)
const MIN_BLOCK: usize = HEADER_SIZE + 4;

/// Sentinel: free_list / next == NULL_OFFSET means end of list.
/// Safe because valid offsets are always < HEAP_SIZE (14336) < 0xFFFF.
const NULL_OFFSET: u16 = 0xFFFF;

#[repr(C, align(4))]
struct HeapStorage([u8; HEAP_SIZE]);
static mut HEAP_MEM: HeapStorage = HeapStorage([0; HEAP_SIZE]);

/// Block header — 4 bytes total (was 8 with usize+ptr on a 32-bit target).
///
/// `size` is the total block size including this header (max = 14336 < u16::MAX).
/// `next` is the byte offset from the start of HEAP_MEM to the next free block,
/// or NULL_OFFSET when this is the last free block. Only meaningful when free.
#[repr(C)]
struct BlockHeader {
    size: u16,
    next: u16,
}

const HEADER_SIZE: usize = core::mem::size_of::<BlockHeader>(); // 4

/// Global allocator state
struct FerriHeap {
    free_list: u16, // offset of first free block; NULL_OFFSET = empty
    initialized: bool,
}

static mut HEAP: FerriHeap = FerriHeap {
    free_list: 0, // value unused until initialized = true
    initialized: false,
};

// === Offset helpers ===

#[inline]
unsafe fn heap_base() -> *mut u8 {
    (&raw mut HEAP_MEM).cast::<u8>()
}

#[inline]
unsafe fn block_at(offset: u16) -> *mut BlockHeader {
    heap_base().add(offset as usize).cast()
}

#[inline]
unsafe fn offset_of(block: *mut BlockHeader) -> u16 {
    (block as usize - heap_base() as usize) as u16
}

/// Initialize the heap. Must be called once at startup before any allocation.
pub fn init() {
    unsafe {
        let first = block_at(0);
        (*first).size = HEAP_SIZE as u16;
        (*first).next = NULL_OFFSET;
        HEAP.free_list = 0;
        HEAP.initialized = true;
    }
}

/// Return (total_free_bytes, largest_free_block) for diagnostics.
pub fn stats() -> (usize, usize) {
    unsafe {
        let mut total = 0usize;
        let mut largest = 0usize;
        let mut off = HEAP.free_list;
        while off != NULL_OFFSET {
            let block = block_at(off);
            let size = (*block).size as usize;
            total += size;
            if size > largest { largest = size; }
            off = (*block).next;
        }
        (total, largest)
    }
}

fn align_up(val: usize, align: usize) -> usize {
    (val + align - 1) & !(align - 1)
}

unsafe fn alloc_inner(layout: Layout) -> *mut u8 {
    if !HEAP.initialized {
        return ptr::null_mut();
    }

    // Required size: header + user data, aligned to at least BlockHeader alignment
    let align = layout.align().max(4);
    let size = align_up(layout.size() + HEADER_SIZE, align).max(MIN_BLOCK);

    if size > HEAP_SIZE {
        return ptr::null_mut();
    }
    let size16 = size as u16;

    // First-fit search using offsets
    let mut prev_off = NULL_OFFSET;
    let mut cur_off = HEAP.free_list;

    while cur_off != NULL_OFFSET {
        let current = block_at(cur_off);
        let cur_size = (*current).size;

        if cur_size >= size16 {
            let remaining = cur_size - size16;

            if remaining >= MIN_BLOCK as u16 {
                // Split: create new free block after this allocation
                let new_off = cur_off + size16;
                let new_free = block_at(new_off);
                (*new_free).size = remaining;
                (*new_free).next = (*current).next;
                (*current).size = size16;

                if prev_off == NULL_OFFSET {
                    HEAP.free_list = new_off;
                } else {
                    (*block_at(prev_off)).next = new_off;
                }
            } else {
                // Use entire block (no split)
                if prev_off == NULL_OFFSET {
                    HEAP.free_list = (*current).next;
                } else {
                    (*block_at(prev_off)).next = (*current).next;
                }
            }

            // Return pointer past the header
            return heap_base().add(cur_off as usize + HEADER_SIZE);
        }

        prev_off = cur_off;
        cur_off = (*current).next;
    }

    ptr::null_mut() // OOM
}

unsafe fn dealloc_inner(ptr: *mut u8, _layout: Layout) {
    if ptr.is_null() {
        return;
    }

    // Recover block start: sits HEADER_SIZE bytes before the user pointer
    let block_off = (ptr as usize - heap_base() as usize - HEADER_SIZE) as u16;
    let block = block_at(block_off);

    // Insert into free list sorted by offset (enables coalescing with neighbours)
    let mut prev_off = NULL_OFFSET;
    let mut cur_off = HEAP.free_list;

    while cur_off != NULL_OFFSET && cur_off < block_off {
        prev_off = cur_off;
        cur_off = (*block_at(cur_off)).next;
    }

    (*block).next = cur_off;
    if prev_off == NULL_OFFSET {
        HEAP.free_list = block_off;
    } else {
        (*block_at(prev_off)).next = block_off;
    }

    // Coalesce with next block if adjacent
    if cur_off != NULL_OFFSET {
        let block_end = block_off + (*block).size;
        if block_end == cur_off {
            (*block).size += (*block_at(cur_off)).size;
            (*block).next = (*block_at(cur_off)).next;
        }
    }

    // Coalesce with previous block if adjacent
    if prev_off != NULL_OFFSET {
        let prev = block_at(prev_off);
        let prev_end = prev_off + (*prev).size;
        if prev_end == block_off {
            (*prev).size += (*block).size;
            (*prev).next = (*block).next;
        }
    }
}

// === Global allocator ===

struct FerriAllocator;

unsafe impl GlobalAlloc for FerriAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        alloc_inner(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        dealloc_inner(ptr, layout)
    }
}

#[global_allocator]
static ALLOCATOR: FerriAllocator = FerriAllocator;
