#[cfg(feature = "firmware")]
mod hw {
    use core::ptr::write_volatile;

    const IWDG_KR:  *mut u32 = 0x4003_0000 as *mut u32;
    const IWDG_PR:  *mut u32 = 0x4003_0004 as *mut u32;
    const IWDG_RLR: *mut u32 = 0x4003_0008 as *mut u32;

    pub fn init() {
        unsafe {
            write_volatile(IWDG_KR, 0x5555);  // unlock PR/RLR
            write_volatile(IWDG_PR, 4);        // prescaler /64 → 625 Hz at 40 kHz LSI
            write_volatile(IWDG_RLR, 1249);   // reload → ~2 s timeout (1250 / 625)
            write_volatile(IWDG_KR, 0xCCCC); // start IWDG
            write_volatile(IWDG_KR, 0xAAAA); // feed immediately
        }
    }

    #[inline]
    pub fn feed() {
        unsafe { write_volatile(IWDG_KR, 0xAAAA); }
    }
}

#[cfg(feature = "firmware")]
pub use hw::{init, feed};

#[cfg(not(feature = "firmware"))]
pub fn init() {}
#[cfg(not(feature = "firmware"))]
pub fn feed() {}
