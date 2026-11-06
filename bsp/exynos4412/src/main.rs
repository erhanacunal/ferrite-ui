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
//! Embassy, one executor per core (`exynos_4412_rs::embassy`):
//! - CPU0: [`ui_task`], the ferrite runtime (VM + input + render) one pass at
//!   a time (`Runtime::step`), yielding between passes;
//! - CPU1: `usart::rx_task`, UART2 RX on its own core and interrupt, so
//!   nothing CPU0 does can overrun the 16-byte FIFO.
//! CPU2/3 stay parked. Audio is refilled from the `pdma0` interrupt (audio.rs).
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
use exynos_4412_rs::drivers::pl330::PDMA0_IRQ;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::{BootArgs, cpu, embassy, heap, mmu, println, smp};

use ferrite_core::backlight::BacklightImpl;
use ferrite_core::ctx::Ctx;
use ferrite_core::flash::FlashImpl;
use ferrite_core::font::FontList;
use ferrite_core::image::ImageList;
use ferrite_core::lcd::LcdImpl;
use ferrite_core::pool::ObjPool;
use ferrite_core::rtc::RtcImpl;
use ferrite_core::runtime::Runtime;
use ferrite_core::systick::SystickImpl;
use ferrite_core::touch::TouchImpl;
use ferrite_core::usart::UsartImpl;

mod audio;
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
    // PDMA0 first: the HAL's `on_interrupt` would take it for its async DMA
    // waits (and disable it).
    if irq.id() == PDMA0_IRQ {
        audio::dma_isr();
    } else {
        // MCT (embassy time), UART2 RX (CPU1), …; the rest, e.g. the wake
        // SGI a secondary core gets at its start, is ignored.
        embassy::on_interrupt(irq.id());
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
    usart::report_clock();
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
    let pmic = Max77686::new(pmic_bus);
    if let Err(e) = touch::init(pmic, mct) {
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
    // MCT comparator 0 → embassy-time, to CPU0.
    embassy::init();
    // UART2 RX interrupt to CPU1 (falls back to CPU0 below).
    let rx = usart::start(1);
    unsafe { *(&raw mut RX) = Some(rx) };
    // Codec + I2S + DMA up now (~0.5 s), so the first sound starts at once
    // and the power-up pop is at boot. Not fatal: the UI runs without sound.
    let mut audio = audio::new(pmic);
    if !audio.power_on() {
        println!("ferrite: audio off");
    }
    cpu::enable_irq();
    let rx_on_cpu0 = match smp::start_cpu(1, cpu1) {
        Ok(()) => false,
        Err(e) => {
            println!("ferrite: CPU1 ERROR {:?}, UART RX on CPU0", e);
            if let Some(id) = usart::UART.irq() {
                gic.set_target(id, 0);
            }
            true
        }
    };
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
        audio: ferrite_core::audio::AudioImpl::with_backend(audio),
        cursor_visible: false,
        tasks: ferrite_core::task::TaskTable::new(),
    });
    let touch = TouchImpl::with_backend(touch::MxtTouch);
    embassy::run(move |spawner| {
        spawner.spawn(ui_task(ctx, touch).unwrap());
        if rx_on_cpu0 {
            spawner.spawn(usart::rx_task(rx).unwrap());
        }
    })
}

/// The UART RX handle for CPU1's task, set before CPU1 starts.
static mut RX: Option<exynos_4412_rs::embassy::uart::UartRx> = None;

/// CPU1: UART2 RX only.
fn cpu1(_core: usize) -> ! {
    Gic::new().init_this_cpu();
    cpu::enable_irq();
    // SAFETY: written once by CPU0 before it started this core.
    let rx = unsafe { (*(&raw const RX)).expect("RX set before CPU1 starts") };
    embassy::run(|spawner| spawner.spawn(usart::rx_task(rx).unwrap()))
}

/// CPU0: the ferrite runtime, one main-loop pass per poll. Yields between
/// passes so other CPU0 tasks (if any) get their turn.
#[embassy_executor::task]
async fn ui_task(ctx: Box<Ctx<Exynos4412Platform>>, touch: TouchImpl<touch::MxtTouch>) {
    let mut rt = Runtime::<Exynos4412Platform>::new(ctx, touch);
    loop {
        rt.step();
        embassy_futures::yield_now().await;
    }
}
