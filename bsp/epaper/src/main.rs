//! E-Paper BSP — ESP32-S3 + ED047TC1 (LilyGo T5 V2.3).
//!
//! This binary owns only device bring-up (clock, PSRAM, I8080/RMT, peripheral
//! construction); the entire application loop lives in `ferrite_core::run`.
//!
//! Build:  cargo +esp build -p bsp-epaper --release
//! Flash:  espflash flash --chip esp32s3 --monitor \
//!             target/xtensa-esp32s3-none-elf/release/ferrite-ui

#![no_std]
#![no_main]
#![feature(asm_experimental_arch)]
// The device backends (`sdcard/epaper.rs`, etc.) use the pre-2024 unsafe-fn
// convention.
#![allow(dead_code, unsafe_op_in_unsafe_fn)]

extern crate alloc;
use esp_alloc as _; // activates esp_alloc as the global allocator

use alloc::boxed::Box;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    peripherals::Peripherals,
};

use ferrite_core::ctx::Ctx;
use ferrite_core::font::FontList;
use ferrite_core::image::ImageList;
use ferrite_core::strpool::StringPool;
use ferrite_core::widget::WidgetTree;

// --- Concrete hardware backends (type aliases + free-fn constructors) ---
mod backlight;
mod flash;
mod lcd;
mod rtc;
mod sdcard;
mod systick;
mod touch;
mod usart;

// --- Device-only modules (binary-local) ---
mod battery;
mod panic;
mod platform;

use platform::EpaperPlatform;

// --- GPIO pin ownership (kept alive for the lifetime of the program) ---

struct EpaperPins {
    _sd_sclk: Output<'static>,
    _sd_mosi: Output<'static>,
    _sd_miso: Input<'static>,
    _sd_cs: Output<'static>,
    _button_1: Input<'static>,
    // GPIO14 and ADC2 are consumed by battery::init() — not stored here.
}

/// Bring up PSRAM, the ED047TC1 (I8080 + RMT + config SR), SYSTIMER, RTC and the
/// battery ADC, then configure the SD-card / button GPIOs.
fn init_gpio(peripherals: Peripherals) -> EpaperPins {
    // PSRAM heap — must be first so large allocations (framebuffer, LUT) land in
    // PSRAM (octal mode for the LilyGo T5 V2.3).
    let psram_config = esp_hal::psram::PsramConfig {
        mode: esp_hal::psram::PsramMode::OctalSpi,
        ..Default::default()
    };
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram, psram_config);

    let output = OutputConfig::default();
    let input_pu = InputConfig::default().with_pull(Pull::Up);

    // Initialise ED047TC1: I8080, RMT, and config shift-register in one call.
    lcd::epaper::init(lcd::epaper::EpdHwPins {
        cfg_data: peripherals.GPIO13,
        cfg_clk: peripherals.GPIO12,
        cfg_str: peripherals.GPIO0,
        data0: peripherals.GPIO8,
        data1: peripherals.GPIO1,
        data2: peripherals.GPIO2,
        data3: peripherals.GPIO3,
        data4: peripherals.GPIO4,
        data5: peripherals.GPIO5,
        data6: peripherals.GPIO6,
        data7: peripherals.GPIO7,
        lcd_dc: peripherals.GPIO40,
        lcd_wrx: peripherals.GPIO41,
        dma_ch: peripherals.DMA_CH0,
        lcd_cam: peripherals.LCD_CAM,
        rmt: peripherals.RMT,
    });

    // System timer (SYSTIMER UNIT0, 16 MHz).
    systick::init(peripherals.SYSTIMER);
    rtc::epaper::init(peripherals.I2C0, peripherals.GPIO17, peripherals.GPIO18);
    battery::init(peripherals.GPIO14, peripherals.ADC2);

    EpaperPins {
        _sd_sclk: Output::new(peripherals.GPIO11, Level::Low, output),
        _sd_mosi: Output::new(peripherals.GPIO15, Level::Low, output),
        _sd_miso: Input::new(peripherals.GPIO16, input_pu),
        _sd_cs: Output::new(peripherals.GPIO42, Level::High, output),
        _button_1: Input::new(peripherals.GPIO21, input_pu),
    }
}

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_hal::main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::_80MHz);
    let peripherals = esp_hal::init(config);

    // 64 KB DRAM heap for small allocations; PSRAM (8 MB) is registered inside
    // init_gpio() before any large allocation is made.
    esp_alloc::heap_allocator!(size: 64 * 1024);

    // Configure GPIO, PSRAM heap, I8080/RMT, SYSTIMER, RTC, battery. Held for
    // the lifetime of the program (main never returns, so never dropped).
    let _pins = init_gpio(peripherals);

    // Build the application context on the heap (too large for the stack). The
    // PSRAM framebuffer is allocated inside `run` via `EpaperPlatform::boot`.
    let ctx = Box::new(Ctx::<EpaperPlatform> {
        lcd: lcd::new(),
        flash: flash::new(),
        tree: WidgetTree::new(),
        fonts: FontList::new(),
        images: ImageList::new(),
        strpool: StringPool::new(),
        fs: None,
        backlight: backlight::init(),
        rtc: rtc::init(),
        usart: usart::init(),
        systick: systick::Systick::handle(),
        audio: ferrite_core::audio::AudioImpl::none(),
        cursor_visible: false,
    });

    let touch = touch::init();

    ferrite_core::runtime::run::<EpaperPlatform>(ctx, touch)
}
