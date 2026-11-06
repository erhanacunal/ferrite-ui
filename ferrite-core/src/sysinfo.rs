//! System information syscalls (0x40..=0x47), for status and monitor apps.
//!
//! A BSP that answers them calls [`syscall`] from its
//! `PlatformRuntime::syscall` with what it knows ([`Info`]); the object pool
//! figures come from the pool itself. The ids are duplicated in
//! `lib/sysinfo.fl` — keep them in sync.

use crate::pool::{ObjPool, T_ARRAY, T_FREE, T_STRING, T_WIDGET};

/// Main-loop passes since boot (wraps).
pub const SYS_PASSES: u8 = 0x40;
/// Frames shown on the display since boot (wraps): presents, or passes on
/// a board that doesn't count them.
pub const SYS_FRAMES: u8 = 0x41;
/// Heap in use, KiB.
pub const SYS_HEAP_USED_KB: u8 = 0x42;
/// Heap size, KiB.
pub const SYS_HEAP_TOTAL_KB: u8 = 0x43;
/// Object arena in use (live objects, without reclaimable garbage), KiB.
pub const SYS_OBJ_USED_KB: u8 = 0x44;
/// Object arena size, KiB.
pub const SYS_OBJ_TOTAL_KB: u8 = 0x45;
/// Live objects: arg 0 = all, 1 = widgets, 2 = strings, 3 = arrays.
pub const SYS_OBJ_COUNT: u8 = 0x46;
/// CPU clock, MHz (0 = unknown).
pub const SYS_CPU_MHZ: u8 = 0x47;

/// What the board knows; `0` = unknown.
pub struct Info {
    pub passes: u32,
    pub frames: u32,
    pub heap_used: usize,
    pub heap_total: usize,
    pub cpu_mhz: u32,
}

/// Answers a system information syscall; `None` for any other id.
pub fn syscall(id: u8, args: &[i32], objs: &ObjPool, info: impl FnOnce() -> Info) -> Option<i32> {
    let kb = |b: usize| (b / 1024) as i32;
    Some(match id {
        SYS_OBJ_USED_KB => kb(objs.used_bytes().saturating_sub(objs.garbage_bytes())),
        SYS_OBJ_TOTAL_KB => kb(objs.capacity_bytes()),
        SYS_OBJ_COUNT => {
            let ty = match args.first().copied().unwrap_or(0) {
                1 => T_WIDGET,
                2 => T_STRING,
                3 => T_ARRAY,
                _ => T_FREE,
            };
            objs.live_objects(ty) as i32
        }
        SYS_PASSES | SYS_FRAMES | SYS_HEAP_USED_KB | SYS_HEAP_TOTAL_KB | SYS_CPU_MHZ => {
            let i = info();
            match id {
                SYS_PASSES => i.passes as i32,
                SYS_FRAMES => i.frames as i32,
                SYS_HEAP_USED_KB => kb(i.heap_used),
                SYS_HEAP_TOTAL_KB => kb(i.heap_total),
                _ => i.cpu_mhz as i32,
            }
        }
        _ => return None,
    })
}
