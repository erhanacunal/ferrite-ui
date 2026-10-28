//! `EpaperPlatform` — binds the ESP32-S3 / ED047TC1 backends to the framework
//! and implements the runtime hooks (`PlatformRuntime`) that `ferrite_core::run`
//! calls.

use ferrite_core::platform::Platform;
use ferrite_core::touch::TouchImpl;

use ferrite_core::ctx::Ctx;
use ferrite_core::runtime::{BasicInput, PlatformRuntime};
use ferrite_core::pool::ObjPool;
use ferrite_core::types::{COLOR_BLACK, COLOR_WHITE};
use ferrite_core::vm::{RenderMode, Vm};
use ferrite_core::{render};

use core::cell::RefCell;

use critical_section::Mutex;
use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::Rtc;
use esp_hal::rtc_cntl::sleep::TimerWakeupSource;
use core::time::Duration;

use crate::{battery, lcd, net, systick};

// === Ferrite syscall ids (see docs/ferrite-lang.md) ===

// --- System (0x00–0x0F) ---
const SYS_UPTIME: u8 = 0x00; // () → milliseconds as i32
const SYS_REBOOT: u8 = 0x01; // () → does not return
const SYS_REPAIR_SCREEN: u8 = 0x02; // () → 0
const SYS_EPD_POWER_OFF: u8 = 0x03; // () → 0
const SYS_EPD_POWER_ON: u8 = 0x04; // () → 0
const SYS_BATTERY_VOLTAGE: u8 = 0x05; // () → battery millivolts (e.g. 3750)
const SYS_DEEP_SLEEP: u8 = 0x06; // (duration_ms) → does not return; -1 if duration <= 0

// --- Wi-Fi station (0x10–0x1F), see net/mod.rs ---
const SYS_WIFI_CONNECT: u8 = 0x10; // (ssid, pass) → 0, or -2 bad argument
const SYS_WIFI_DISCONNECT: u8 = 0x11; // () → 0; radio off
const SYS_WIFI_STATUS: u8 = 0x12; // () → 0 off, 1 connecting, 2 connected, 3 failed (retrying)
const SYS_WIFI_RSSI: u8 = 0x13; // () → dBm, 0 if not connected
const SYS_WIFI_IP: u8 = 0x14; // () → IPv4 big-endian, 0 if not connected
const SYS_WIFI_IP_STR: u8 = 0x15; // () → string "a.b.c.d"
const SYS_WIFI_DNS_STR: u8 = 0x16; // () → string, DNS server from DHCP

// --- Network requests (0x30–0x3F) ---
const SYS_HTTP_GET: u8 = 0x30; // (url) → 0 started, < 0 error
const SYS_HTTP_STATUS: u8 = 0x31; // () → 0 idle, 1 running, 2 done, < 0 error
const SYS_HTTP_CODE: u8 = 0x32; // () → HTTP status code
const SYS_HTTP_LEN: u8 = 0x33; // () → body length
const SYS_HTTP_BODY: u8 = 0x34; // (start, len) → string
const SYS_HTTP_BYTE: u8 = 0x35; // (index) → byte, -1 past the end
const SYS_HTTP_FIND: u8 = 0x36; // (needle, from) → index or -1
const SYS_HTTP_JSON: u8 = 0x37; // (path) → string ("" if missing)
const SYS_HTTP_JSON_NUM: u8 = 0x38; // (path, scale) → number × scale, i32::MIN if missing
const SYS_HTTP_FREE: u8 = 0x39; // () → 0
const SYS_HTTPS_CA: u8 = 0x3A; // (file name) → 0 / -2; "" = no verification
const SYS_NTP_SYNC: u8 = 0x3B; // (server, tz_minutes) → 0 started, < 0 error
const SYS_NTP_STATUS: u8 = 0x3C; // () → 0 idle, 1 running, 2 done, < 0 error
const SYS_NTP_TIME: u8 = 0x3D; // () → UTC Unix seconds, 0 if never synced

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
    type TouchB = ferrite_core::touch::NoTouch;
    type BacklightB = crate::backlight::epaper::EpdBacklight;
    type RtcB = crate::rtc::epaper::EpdRtc;
    type UsartB = crate::usart::epaper::EspUart;
    type SystickB = crate::systick::EpdSystick;
    type GpioB = crate::user_gpio::EpdGpio;
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

    fn syscall(id: u8, args: &[i32], strpool: &mut ObjPool) -> Option<i32> {
        let arg = |i: usize| args.get(i).copied().unwrap_or(0);
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
            SYS_DEEP_SLEEP => deep_sleep(arg(0)),

            SYS_WIFI_CONNECT => Some(net::wifi_connect(strpool, arg(0), arg(1))),
            SYS_WIFI_DISCONNECT => Some(net::wifi_disconnect()),
            SYS_WIFI_STATUS => Some(net::wifi_status()),
            SYS_WIFI_RSSI => Some(net::wifi_rssi()),
            SYS_WIFI_IP => Some(net::wifi_ip()),
            SYS_WIFI_IP_STR => net::wifi_ip_str(strpool),
            SYS_WIFI_DNS_STR => net::wifi_dns_str(strpool),

            SYS_HTTP_GET => Some(net::http_get(strpool, arg(0))),
            SYS_HTTP_STATUS => Some(net::http_status()),
            SYS_HTTP_CODE => Some(net::http_code()),
            SYS_HTTP_LEN => Some(net::http_len()),
            SYS_HTTP_BODY => net::http_body(strpool, arg(0), arg(1)),
            SYS_HTTP_BYTE => Some(net::http_byte(arg(0))),
            SYS_HTTP_FIND => Some(net::http_find(strpool, arg(0), arg(1))),
            SYS_HTTP_JSON => net::http_json(strpool, arg(0)),
            SYS_HTTP_JSON_NUM => Some(net::http_json_num(strpool, arg(0), arg(1))),
            SYS_HTTP_FREE => Some(net::http_free()),
            SYS_HTTPS_CA => Some(net::https_ca(strpool, arg(0))),
            SYS_NTP_SYNC => Some(net::ntp_sync(strpool, arg(0), arg(1))),
            SYS_NTP_STATUS => Some(net::ntp_status()),
            SYS_NTP_TIME => Some(net::ntp_time(systick::millis() as u32)),
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
            render::render_all(ctx);
            ctx.lcd.flush_dirty();
        }
    }

    fn frame_end(ctx: &mut Ctx<Self>) {
        // A finished SNTP sync (core 1) is written to the RTC here, on the
        // core that owns the I2C bus.
        if let Some(dt) = net::take_rtc_update(systick::millis() as u32) {
            ctx.rtc.set_time(&dt);
        }
    }
}
