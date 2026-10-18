/// System configuration resource (`RES_SYSCFG`, entry name `"sys"`).
///
/// An optional 32-byte record in the flash filesystem that sizes the runtime
/// pools at boot. Every field uses 0 = "built-in default", so an image without
/// the resource — or with a zeroed field — behaves exactly like one built
/// before this resource existed. It is applied once, right after `Fs::mount`;
/// a filesystem write reboots the device, so pools are never resized while a
/// program is running.
///
/// All pools are allocated from the heap, so their total is bounded by the
/// linker-sized heap rather than by fixed arrays in `.bss`.
///
/// Layout (little-endian, 32 bytes):
///   0  u16  magic "SC"
///   2  u8   version (1)
///   3  u8   reserved
///   4  u32  strpool_bytes   string pool buffer
///   8  u16  max_strings     string pool slots
///  10  u16  uart_rx_bytes   USART RX ring (rounded up to a power of two)
///  12  u16  max_widgets     widget cap (≤ widget::MAX_WIDGETS)
///  14  u16  max_ext         widget extension cap (≤ widget::MAX_EXTENSIONS)
///  16  u8   eval_stack      VM evaluation stack depth
///  17  u8   call_stack      VM call stack depth
///  18  u8   cb_queue        VM callback queue length
///  19  u8   anim_slots      property animation slots
///  20  ..   reserved (0) up to byte 32
extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::flash::{FlashBackend, FlashImpl};
use crate::fs::{Fs, RES_SYSCFG};

/// Resource table name of the system configuration entry.
pub const SYSCFG_NAME: &[u8] = b"sys";
/// Size of the serialized record.
pub const SYSCFG_SIZE: usize = 32;

const MAGIC: u16 = 0x4353; // "SC" LE
const VERSION: u8 = 1;

/// Pool sizes requested by the image. 0 in any field = platform default.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct SysCfg {
    pub strpool_bytes: u32,
    pub max_strings: u16,
    pub uart_rx_bytes: u16,
    pub max_widgets: u16,
    pub max_ext: u16,
    pub eval_stack: u8,
    pub call_stack: u8,
    pub cb_queue: u8,
    pub anim_slots: u8,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SysCfgError {
    /// Entry is shorter than the record or has the wrong resource kind.
    BadEntry,
    BadMagic,
    BadVersion,
}

impl SysCfg {
    /// Decode a serialized record.
    pub fn parse(b: &[u8]) -> Result<Self, SysCfgError> {
        if b.len() < SYSCFG_SIZE {
            return Err(SysCfgError::BadEntry);
        }
        let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        if u16_at(0) != MAGIC {
            return Err(SysCfgError::BadMagic);
        }
        if b[2] != VERSION {
            return Err(SysCfgError::BadVersion);
        }
        Ok(Self {
            strpool_bytes: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            max_strings: u16_at(8),
            uart_rx_bytes: u16_at(10),
            max_widgets: u16_at(12),
            max_ext: u16_at(14),
            eval_stack: b[16],
            call_stack: b[17],
            cb_queue: b[18],
            anim_slots: b[19],
        })
    }

    /// Read the `"sys"` entry from a mounted filesystem. `Ok(None)` when the
    /// image has no such entry (all defaults apply).
    pub fn load<F: FlashBackend>(
        fs: &Fs,
        flash: &FlashImpl<F>,
    ) -> Result<Option<Self>, SysCfgError> {
        let Some(entry) = fs.find(flash, SYSCFG_NAME) else {
            return Ok(None);
        };
        if entry.kind != RES_SYSCFG || (entry.size as usize) < SYSCFG_SIZE {
            return Err(SysCfgError::BadEntry);
        }
        let mut buf = [0u8; SYSCFG_SIZE];
        fs.read_resource(flash, &entry, 0, &mut buf);
        Self::parse(&buf).map(Some)
    }
}

/// Allocate a `len`-element slice filled with `fill`, returning `None` instead
/// of aborting when the heap cannot satisfy it. Used for every pool that a
/// [`SysCfg`] can size, so an oversized request surfaces as a boot error.
pub(crate) fn try_alloc_slice<T: Clone>(len: usize, fill: T) -> Option<Box<[T]>> {
    let mut v = Vec::new();
    v.try_reserve_exact(len).ok()?;
    v.resize(len, fill);
    Some(v.into_boxed_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> [u8; SYSCFG_SIZE] {
        let mut b = [0u8; SYSCFG_SIZE];
        b[0..2].copy_from_slice(&MAGIC.to_le_bytes());
        b[2] = VERSION;
        b[4..8].copy_from_slice(&4096u32.to_le_bytes());
        b[8..10].copy_from_slice(&96u16.to_le_bytes());
        b[10..12].copy_from_slice(&512u16.to_le_bytes());
        b[12..14].copy_from_slice(&200u16.to_le_bytes());
        b[14..16].copy_from_slice(&150u16.to_le_bytes());
        b[16] = 32;
        b[17] = 12;
        b[18] = 16;
        b[19] = 4;
        b
    }

    #[test]
    fn parse_all_fields() {
        let cfg = SysCfg::parse(&record()).ok().unwrap();
        assert_eq!(
            cfg,
            SysCfg {
                strpool_bytes: 4096,
                max_strings: 96,
                uart_rx_bytes: 512,
                max_widgets: 200,
                max_ext: 150,
                eval_stack: 32,
                call_stack: 12,
                cb_queue: 16,
                anim_slots: 4,
            }
        );
    }

    #[test]
    fn parse_rejects_bad_records() {
        let mut b = record();
        assert_eq!(SysCfg::parse(&b[..16]), Err(SysCfgError::BadEntry));
        b[2] = 2;
        assert_eq!(SysCfg::parse(&b), Err(SysCfgError::BadVersion));
        b[0] = 0;
        assert_eq!(SysCfg::parse(&b), Err(SysCfgError::BadMagic));
    }
}
