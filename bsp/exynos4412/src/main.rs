//! Exynos 4412 tablet BSP — 4 × Cortex-A9, 1 GB DRAM, 1280×800 LVDS panel
//! (LTN101AL03, FIMD), mXT1664S touch, MAX77686 PMIC/RTC, UART2 console.
//!
//! Runs from RAM: `uart_boot` on the boot partition receives the image
//! (`rust-objcopy -O binary` + `tools/uartboot.py` in the HAL repo) and starts
//! it; the image relocates itself to 0x40008000 (memory.x).
//!
//! Resources persist in `FERRITE/FLASH.BIN` on the eMMC CACHE partition
//! (FAT32), loaded into RAM at boot and saved after `writefs` (storage.rs).
//!
//! Single core: CPU0 runs the ferrite main loop; the other cores stay parked
//! (network/audio can go there later, as epaper does with Wi-Fi on core 1).
//!
//! Build (from this directory): `cargo build --release`

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;

use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::{BootArgs, cpu, heap, mmu, println};

use ferrite_core::backlight::BacklightImpl;
use ferrite_core::ctx::Ctx;
use ferrite_core::flash::FlashImpl;
use ferrite_core::font::FontList;
use ferrite_core::image::ImageList;
use ferrite_core::lcd::LcdImpl;
use ferrite_core::pool::ObjPool;
use ferrite_core::rtc::RtcImpl;
use ferrite_core::systick::SystickImpl;
use ferrite_core::touch::TouchImpl;
use ferrite_core::usart::UsartImpl;

mod backlight;
mod flash;
mod lcd;
mod platform;
mod rtc;
mod storage;
mod systick;
mod touch;
mod usart;

use platform::Exynos4412Platform;

exynos_4412_rs::entry!(main);
exynos_4412_rs::global_heap!();

#[unsafe(no_mangle)]
extern "C" fn handle_irq(_frame: &mut ExceptionFrame) {
    let gic = Gic::new();
    let Some(irq) = gic.acknowledge() else { return };
    if Some(irq.id()) == usart::UART.irq() {
        usart::rx_isr();
    }
    gic.end_of_interrupt(irq);
}

fn fail(what: &str, error: impl core::fmt::Debug) -> ! {
    println!("ferrite: {} ERROR {:?}", what, error);
    exynos_4412_rs::console::flush();
    cpu::halt()
}

fn main(_args: BootArgs) -> ! {
    println!("\nferrite-ui (exynos4412)");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    if let Err(e) = heap::init() {
        fail("heap", e);
    }

    // PMIC bus (I2C0): touch power rails, RTC.
    let pmic_bus = I2c::new(Bus::I2c0);
    if let Err(e) = pmic_bus.init(400_000, 100) {
        fail("PMIC bus", e);
    }
    if let Err(e) = touch::init(Max77686::new(pmic_bus), mct) {
        // Not fatal: the UI runs without touch (USART still works).
        println!("ferrite: touch ERROR {:?}", e);
    }

    let lcd = match lcd::init() {
        Ok(lcd) => lcd,
        Err(e) => fail("display", e),
    };
    // Resources: the CACHE partition's FAT32 (FERRITE/FLASH.BIN) → RAM region.
    storage::init();
    flash::init();

    let gic = Gic::new();
    gic.init();
    usart::init();
    if let Some(id) = usart::UART.irq() {
        gic.enable(id);
    }
    cpu::enable_irq();
    println!("ferrite: up, entering runtime");

    let ctx = Box::new(Ctx::<Exynos4412Platform> {
        lcd: LcdImpl::with_backend(lcd),
        flash: FlashImpl::with_backend(flash::RamFlash),
        fonts: FontList::new(),
        images: ImageList::new(),
        objs: ObjPool::new(),
        fs: None,
        backlight: BacklightImpl::with_backend(backlight::GpioBacklight),
        rtc: RtcImpl::with_backend(rtc::new()),
        usart: UsartImpl::with_backend(usart::HwUart),
        systick: SystickImpl::init(),
        gpio: ferrite_core::gpio::GpioImpl::none(),
        eeprom: ferrite_core::eeprom::EepromImpl::none(),
        cursor_visible: false,
    });
    let touch = TouchImpl::with_backend(touch::MxtTouch);
    ferrite_core::runtime::run::<Exynos4412Platform>(ctx, touch)
}
