//! `EpaperPlatform` — binds the ESP32-S3 / ED047TC1 backends to the framework
//! and implements the runtime hooks (`PlatformRuntime`) that `ferrite_core::run`
//! calls.

use ferrite_core::platform::Platform;
use ferrite_core::touch::TouchImpl;

use ferrite_core::ctx::Ctx;
use ferrite_core::runtime::{BasicInput, PlatformRuntime};
use ferrite_core::strpool::StringPool;
use ferrite_core::types::{COLOR_BLACK, COLOR_WHITE};
use ferrite_core::vm::{RenderMode, Vm};
use ferrite_core::{render};

use core::cell::RefCell;

use critical_section::Mutex;
use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::Rtc;
use esp_hal::rtc_cntl::sleep::TimerWakeupSource;
use core::time::Duration;

use crate::{battery, lcd, systick};

// === Ferrite syscall ids (see docs/ferrite-lang.md) ===

// --- System (0x00–0x0F) ---
const SYS_UPTIME: u8 = 0x00; // () → milliseconds as i32
const SYS_REBOOT: u8 = 0x01; // () → does not return
const SYS_REPAIR_SCREEN: u8 = 0x02; // () → 0
const SYS_EPD_POWER_OFF: u8 = 0x03; // () → 0
const SYS_EPD_POWER_ON: u8 = 0x04; // () → 0
const SYS_BATTERY_VOLTAGE: u8 = 0x05; // () → battery millivolts (e.g. 3750)
const SYS_DEEP_SLEEP: u8 = 0x06; // (duration_ms) → does not return; -1 if duration <= 0

/// RTC controller handle, needed to enter deep sleep. Set once by `init_sleep`.
static RTC_CNTL: Mutex<RefCell<Option<Rtc<'static>>>> = Mutex::new(RefCell::new(None));

/// Take ownership of the LPWR (RTC_CNTL) peripheral so `SYS_DEEP_SLEEP` can
/// drive the sleep controller. Called once from `main::init_gpio`.
pub fn init_sleep(lpwr: LPWR<'static>) {
    critical_section::with(|cs| {
        RTC_CNTL.borrow_ref_mut(cs).replace(Rtc::new(lpwr));
    });
}

/// Power down the panel and enter deep sleep for `duration_ms` milliseconds.
/// The chip resets on wake-up (RAM is lost, `main` runs again), so this never
/// returns on success. Returns `Some(-1)` when `duration_ms <= 0` or the RTC
/// controller was never handed over via `init_sleep`.
fn deep_sleep(duration_ms: i32) -> Option<i32> {
    if duration_ms <= 0 {
        return Some(-1);
    }
    let rtc = critical_section::with(|cs| RTC_CNTL.borrow_ref_mut(cs).take());
    let Some(mut rtc) = rtc else {
        return Some(-1);
    };
    // Cut the e-paper high-voltage rails first: the image is retained without
    // power and the panel would otherwise keep drawing current while asleep.
    lcd::epaper::power_off();
    let timer = TimerWakeupSource::new(Duration::from_millis(duration_ms as u64));
    rtc.sleep_deep(&[&timer])
}

/// Trigger an ESP32-S3 system reset via the RTC_CNTL_OPTIONS0 SW_SYS_RST bit.
pub unsafe fn esp_restart() -> ! {
    const RTC_CNTL_OPTIONS0: *mut u32 = 0x6000_8000u32 as *mut u32;
    let v = core::ptr::read_volatile(RTC_CNTL_OPTIONS0);
    core::ptr::write_volatile(RTC_CNTL_OPTIONS0, v | (1 << 31));
    loop {}
}

/// Marker type implementing `Platform` + `PlatformRuntime` for the LilyGo T5.
pub struct EpaperPlatform;

impl Platform for EpaperPlatform {
    type LcdB = crate::lcd::epaper::EpdLcd;
    type FlashB = crate::flash::epaper::EspFlash;
    type TouchB = crate::touch::epaper::EpdButtons;
    type BacklightB = crate::backlight::epaper::EpdBacklight;
    type RtcB = crate::rtc::epaper::EpdRtc;
    type UsartB = crate::usart::epaper::EspUart;
    type SystickB = crate::systick::EpdSystick;
    type GpioB = ferrite_core::gpio::NoGpio;
    type EepromB = ferrite_core::eeprom::NoEeprom;
    // E-paper is white when idle; anti-aliased text on transparent labels
    // blends against this instead of the raster-board default (black).
    const DEFAULT_BG_COLOR: u16 = COLOR_WHITE;
}

impl PlatformRuntime for EpaperPlatform {
    const BG_COLOR: u16 = COLOR_WHITE;
    const FG_COLOR: u16 = COLOR_BLACK;
    // The e-paper panel only updates on explicit `render()` calls; force EPaper
    // mode regardless of the image header so the per-frame loop never drives it.
    const FORCED_RENDER_MODE: Option<RenderMode> = Some(RenderMode::EPaper);
    type Input = BasicInput;

    fn reset() -> ! {
        unsafe { esp_restart() }
    }

    fn syscall(id: u8, args: &[i32], _strpool: &StringPool) -> Option<i32> {
        let _ = args;
        match id {
            SYS_UPTIME => Some(systick::millis() as i32),
            SYS_REBOOT => unsafe { esp_restart() },
            SYS_REPAIR_SCREEN => {
                lcd::epaper::repair_screen();
                Some(0)
            }
            SYS_EPD_POWER_OFF => {
                lcd::epaper::power_off();
                Some(0)
            }
            SYS_EPD_POWER_ON => {
                lcd::epaper::power_on();
                Some(0)
            }
            SYS_BATTERY_VOLTAGE => battery::read_mv().or(Some(-1)),
            SYS_DEEP_SLEEP => deep_sleep(args.first().copied().unwrap_or(0)),
            _ => None, // unknown id → VM Error
        }
    }

    fn stack_info() -> (u32, u32) {
        (0, 0)
    }

    fn boot(ctx: &mut Ctx<Self>, _touch: &mut TouchImpl<Self::TouchB>) -> u8 {
        // Allocate the PSRAM framebuffer (must run after `psram_allocator!` in
        // main), then ghost-erase any image left from the previous session.
        lcd::epaper::alloc_buffers();
        lcd::epaper::clear();
        let _ = ctx;
        // Map logical FS addresses into the ferrite partition before `run`
        // mounts the filesystem from XIP-mapped internal flash.
        crate::flash::probe_ferrite_fs_preamble();
        0
    }

    fn initial_render(ctx: &mut Ctx<Self>, vm: &Vm) {
        // EPaper mode draws only on explicit `render()`; other modes get a full
        // initial frame so the panel is verifiable even without a program.
        if vm.render_mode != RenderMode::EPaper {
            render::render_all(ctx, vm);
            ctx.lcd.flush_dirty();
        }
    }
}
