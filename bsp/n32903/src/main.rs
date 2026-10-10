//! Nextion N32903 BSP — Nuvoton N32903 (W55FA93, ARM926EJ-S) + 800×480 RGB LCD.
//!
//! Booted by the NAND loader (`n32903_nand_loader_rs`): the loader copies this
//! image from NAND block 4 to SDRAM 0x0 and jumps to it, so DDR and the 96 MHz
//! system clock are already configured. Single-threaded, like the Nextion
//! GD32 target: two interrupts (1 ms tick, UART RX), everything else polled.
//!
//! Memory (8 MB SDRAM): image + .bss (two 800×480 RGB565 frame buffers) from
//! 0x0, heap up to 0x7BE000, exception stacks above (startup.s).
//!
//! Build:
//!   .\n32903.ps1          (from the workspace root)

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;

use ferrite_core::backlight::BacklightImpl;
use ferrite_core::ctx::Ctx;
use ferrite_core::flash::FlashImpl;
use ferrite_core::font::FontList;
use ferrite_core::image::ImageList;
use ferrite_core::lcd::LcdImpl;
use ferrite_core::rtc::RtcImpl;
use ferrite_core::strpool::StringPool;
use ferrite_core::touch::TouchImpl;
use ferrite_core::usart::UsartImpl;
use ferrite_core::widget::WidgetTree;

use n32903_rs::aic::{self, LocalIntState};
use n32903_rs::cache::{self, CacheMode};

mod backlight;
mod flash;
mod lcd;
mod rtc;
mod sdcard;
mod systick;
mod touch;
mod usart;

mod panic;
mod platform;

use platform::N32903Platform;

unsafe extern "C" {
    static mut __bss_start__: u32;
    static mut __bss_end__: u32;
    static __heap_start__: u8;
    static __heap_end__: u8;
}

/// Entry point, called from `Reset_Go` in the HAL's startup.s (SVC mode,
/// IRQ/FIQ masked).
#[unsafe(no_mangle)]
pub extern "C" fn rust_main() -> ! {
    unsafe {
        // startup.s doesn't zero .bss, and SDRAM still holds whatever the
        // loader left there. No statics have been touched yet.
        zero_bss();

        let start = core::ptr::addr_of!(__heap_start__) as usize;
        let end = core::ptr::addr_of!(__heap_end__) as usize;
        n32903_rs::heap::init(start, end - start);

        // Identity map, SDRAM cacheable (write-back), peripherals uncached.
        // The section table lives in .bss, so this comes after zero_bss.
        cache::cache_enable(CacheMode::WriteBack);

        // Vectors at 0x18/0x1C → AIC dispatch; then the two IRQ sources.
        aic::aic_initialize();
        usart::init();
        systick::init();
        aic::aic_set_local_interrupt(LocalIntState::EnableIrq);
    }

    usart::log("\r\nferrite-ui: N32903 BSP\r\n");
    if !systick::is_ticking() {
        usart::log("WARNING: timer IRQ not ticking, delays will hang\r\n");
    }

    unsafe {
        backlight::init(); // GPD0/GPD1: JTAG is gone from here on
        lcd::init();
        touch::init();
        rtc::init();
        flash::init();
    }

    let ctx = Box::new(Ctx::<N32903Platform> {
        lcd: LcdImpl::with_backend(lcd::new()),
        flash: FlashImpl::with_backend(flash::new()),
        tree: WidgetTree::new(),
        fonts: FontList::new(),
        images: ImageList::new(),
        strpool: StringPool::new(),
        fs: None,
        backlight: BacklightImpl::with_backend(backlight::new()),
        rtc: RtcImpl::with_backend(rtc::new()),
        usart: UsartImpl::with_backend(usart::new()),
        systick: systick::handle(),
        audio: ferrite_core::audio::AudioImpl::none(),
        cursor_visible: false,
    });
    let touch = TouchImpl::with_backend(touch::new());
    ferrite_core::runtime::run::<N32903Platform>(ctx, touch);
}

unsafe fn zero_bss() {
    let mut p = core::ptr::addr_of_mut!(__bss_start__);
    let end = core::ptr::addr_of_mut!(__bss_end__);
    while p < end {
        unsafe {
            core::ptr::write_volatile(p, 0);
            p = p.add(1);
        }
    }
}
