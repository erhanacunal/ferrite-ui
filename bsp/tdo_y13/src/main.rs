//! TDO Y13 BSP — Allwinner F1C100s (ARM926EJ-S) + TL021WVC04 480×480 LCD.
//!
//! Multi-threaded RTOS environment using the f1c100s HAL preemptive scheduler.
//!
//! ## Thread layout
//!
//! | Thread     | Priority | Tick | Purpose                                    |
//! |------------|----------|------|--------------------------------------------|
//! | idle       | 31       | 10   | Background idle loop (sleeps forever)      |
//! | ui         | 1        | 5    | Ferrite-UI main loop (owns all peripherals) |
//!
//! Build:
//!   cargo build -p bsp-tdo-y13 --release

#![no_std]
#![no_main]
#![allow(dead_code)]
#![feature(linkage)]

extern crate alloc;

use alloc::boxed::Box;
use f1c100s::thread;
use f1c100s::timer;
use f1c100s::interrupt;
use f1c100s::gpio;


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

use platform::TdoY13Platform;

// === Global allocator ===
//
// The f1c100s HAL allocator guards every alloc/free with interrupt
// disable/enable, so it is safe under the preemptive scheduler this target
// runs. ferrite-core's `external_alloc` feature keeps its own single-threaded
// static heap from also claiming `#[global_allocator]`.
#[global_allocator]
static GLOBAL: f1c100s::allocator::Allocator = f1c100s::allocator::ALLOCATOR;

/// Top of usable DRAM. The MMU maps 0x8000_0000..=0x81FF_FFFF (32 MB); reserve a
/// 64 KB guard below the top so the heap never runs into the supervisor stacks.
const DRAM_END: usize = 0x8000_0000 + 32 * 1024 * 1024 - 64 * 1024;

// === Thread stacks ===

#[unsafe(link_section = ".bss.idle_stack")]
static mut IDLE_STACK: [u8; 1024] = [0u8; 1024];

#[unsafe(link_section = ".bss.ui_stack")]
static mut UI_STACK: [u8; 32768] = [0u8; 32768];

fn idle_task(_: *mut ()) {
    // The idle thread must ALWAYS be runnable so the scheduler has something to
    // switch to whenever every other thread blocks. If idle instead blocks
    // (e.g. thread_sleep), then when the UI thread waits on an IPC event — such
    // as the interrupt-driven I2C transfer for touch — `ready_bitmap` becomes
    // empty, `do_schedule()` can't switch away, `block_current()` is a no-op,
    // and the wait returns `Timeout` immediately (the ISR never gets to run).
    // Wait-for-interrupt parks the core until the next IRQ without blocking.
    loop {
        f1c100s::cpu::wfi();
    }
}

fn ui_task(_: *mut ()) {

    unsafe { touch::init(); }

    let ctx = Box::new(Ctx::<TdoY13Platform> {
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
    ferrite_core::runtime::run::<TdoY13Platform>(ctx, touch);
}

// ── Scheduler tick ISR (called from TIMER1 every 10 ms) ──────────────────────

fn sched_tick_isr() {
    thread::sched_tick();
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_main() -> ! {
    timer::avs_init();
    

    // use f1c100s::mmu::{MemDesc, RW_CB, RW_NCNB};
    // f1c100s::mmu::init(&[
    //     MemDesc::new(0x00000000, 0xFFFF_FFFF, 0x00000000, RW_NCNB),
    //     MemDesc::new(0x80000000, 0x81FF_FFFF, 0x80000000, RW_CB),
    // ]);

    init_gpio_mux();

    f1c100s::interrupt::init();

    // Initialize the heap over all DRAM above the loaded image/.bss (`_end`),
    // up to the 64 KB guard below the top of mapped DRAM.
    unsafe {
        unsafe extern "C" {
            static _end: u8;
        }
        let heap_start = core::ptr::addr_of!(_end) as usize;
        f1c100s::allocator::init_heap(heap_start as *mut u8, DRAM_END - heap_start);
    }

    
    unsafe { flash::init(); }    
    unsafe { backlight::init(); }
    unsafe { lcd::init(); }
    unsafe { usart::init(); }    
    
    thread::sched_init();

    thread::thread_create(
        "idle", idle_task, core::ptr::null_mut(),
        unsafe { &mut *core::ptr::addr_of_mut!(IDLE_STACK) }, 31, 10,
    );
    thread::thread_create(
        "ui", ui_task, core::ptr::null_mut(),
        unsafe { &mut *core::ptr::addr_of_mut!(UI_STACK) }, 1, 5,
    );
    
    systick::init();

    thread::sched_start();
}

fn init_gpio_mux() {
    use f1c100s::gpio::{self, Port, DriveLevel};
    
    // ── Touch controller GPIOs ───────────────────────────────────────────────
    // Note: I2C comms (init, probe) happen in the ui thread after sched_start.
    // Calling TWI/I2C here would panic — EventFlags::wait() needs a current thread.
    gpio::set_function(Port::E, 10, gpio::function::OUTPUT); // touch reset
    gpio::set_drive_level(Port::E, 10, DriveLevel::Level2);
    gpio::set_function(Port::E, 9,  gpio::function::INPUT);  // touch IRQ
    gpio::set_pull_mode(Port::E, 9,  gpio::PullMode::Disable);

    // Assert / de-assert reset (bring chip out of reset before I2C init)
    gpio::set_value(Port::E, 10, false); timer::delay_ms(10);
    gpio::set_value(Port::E, 10, true);  timer::delay_ms(10);
    
}
