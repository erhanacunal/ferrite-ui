/// Simple linked-list heap allocator for bare-metal use.
///
/// 16KB static heap buffer. First-fit allocation with free-block coalescing.
/// Single-threaded (no locking) — safe for Cortex-M3 bare-metal.
///
/// Usage:
///   heap::init() at startup, then use Box/Vec from `extern crate alloc`.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

const HEAP_SIZE: usize = 16 * 1024;

/// Minimum block size (header + at least 1 usable byte, aligned)
const MIN_BLOCK: usize = HEADER_SIZE + 4;

static mut HEAP_MEM: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

/// Block header — sits immediately before the user data.
/// When free: part of the free list (next points to next free block).
/// When allocated: size is preserved, next is unused.
#[repr(C)]
struct BlockHeader {
    /// Total block size including this header
    size: usize,
    /// Next free block (null = end of list). Only valid when block is free.
    next: *mut BlockHeader,
}

const HEADER_SIZE: usize = core::mem::size_of::<BlockHeader>();

/// Global allocator state
struct FerriHeap {
    free_list: *mut BlockHeader,
    initialized: bool,
}

static mut HEAP: FerriHeap = FerriHeap {
    free_list: ptr::null_mut(),
    initialized: false,
};

/// Initialize the heap. Must be called once at startup before any allocation.
pub fn init() {
    unsafe {
        let start = (&raw mut HEAP_MEM) as *mut u8 as *mut BlockHeader;
        (*start).size = HEAP_SIZE;
        (*start).next = ptr::null_mut();
        HEAP.free_list = start;
        HEAP.initialized = true;
    }
}

/// Return (total_free_bytes, largest_free_block) for diagnostics.
pub fn stats() -> (usize, usize) {
    unsafe {
        let mut total = 0usize;
        let mut largest = 0usize;
        let mut block = HEAP.free_list;
        while !block.is_null() {
            let size = (*block).size;
            total += size;
            if size > largest {
                largest = size;
            }
            block = (*block).next;
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

    // Required size: header + user data, aligned
    let align = layout.align().max(core::mem::align_of::<BlockHeader>());
    let size = align_up(layout.size() + HEADER_SIZE, align).max(MIN_BLOCK);

    // First-fit search
    let mut prev: *mut BlockHeader = ptr::null_mut();
    let mut current = HEAP.free_list;

    while !current.is_null() {
        if (*current).size >= size {
            let remaining = (*current).size - size;

            if remaining >= MIN_BLOCK {
                // Split: create new free block after this allocation
                let new_free = (current as *mut u8).add(size) as *mut BlockHeader;
                (*new_free).size = remaining;
                (*new_free).next = (*current).next;
                (*current).size = size;

                // Remove current from free list, insert new_free
                if prev.is_null() {
                    HEAP.free_list = new_free;
                } else {
                    (*prev).next = new_free;
                }
            } else {
                // Use entire block (no split)
                if prev.is_null() {
                    HEAP.free_list = (*current).next;
                } else {
                    (*prev).next = (*current).next;
                }
            }

            // Return pointer past the header
            return (current as *mut u8).add(HEADER_SIZE);
        }

        prev = current;
        current = (*current).next;
    }

    ptr::null_mut() // OOM
}

unsafe fn dealloc_inner(ptr: *mut u8, _layout: Layout) {
    if ptr.is_null() {
        return;
    }

    // Recover header (sits just before the user pointer)
    let block = (ptr as *mut BlockHeader).sub(1);

    // Insert into free list sorted by address (enables coalescing)
    let mut prev: *mut BlockHeader = ptr::null_mut();
    let mut current = HEAP.free_list;

    while !current.is_null() && (current as usize) < (block as usize) {
        prev = current;
        current = (*current).next;
    }

    // Insert block between prev and current
    (*block).next = current;
    if prev.is_null() {
        HEAP.free_list = block;
    } else {
        (*prev).next = block;
    }

    // Coalesce with next block if adjacent
    if !current.is_null() {
        let block_end = (block as *mut u8).add((*block).size);
        if block_end == current as *mut u8 {
            (*block).size += (*current).size;
            (*block).next = (*current).next;
        }
    }

    // Coalesce with previous block if adjacent
    if !prev.is_null() {
        let prev_end = (prev as *mut u8).add((*prev).size);
        if prev_end == block as *mut u8 {
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
