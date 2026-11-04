//! `Exynos4412Platform` — binds the backends to ferrite-core and implements
//! the runtime hooks.

use exynos_4412_rs::drivers::power::{self, BootMode, PowerKeys};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::heap;
use ferrite_core::ctx::Ctx;
use ferrite_core::platform::Platform;
use ferrite_core::pool::ObjPool;
use ferrite_core::runtime::{ERR_EMPTY_RUN, FullInput, PlatformRuntime};
use ferrite_core::touch::{TouchBackend, TouchImpl};
use ferrite_core::types::{COLOR_BLACK, COLOR_RED, COLOR_WHITE};

use crate::lcd::{self, PANEL};

const SYS_REBOOT: u8 = 0x01;

pub struct Exynos4412Platform;

impl Platform for Exynos4412Platform {
    type LcdB = lcd::FimdLcd;
    type FlashB = crate::flash::RamFlash;
    type TouchB = crate::touch::MxtTouch;
    type BacklightB = crate::backlight::GpioBacklight;
    type RtcB = crate::rtc::PmicRtc;
    type UsartB = crate::usart::HwUart;
    type SystickB = crate::systick::MctSystick;
    type GpioB = ferrite_core::gpio::NoGpio;
    type EepromB = ferrite_core::eeprom::NoEeprom;

    // Default layout (config sector 0, FS from 0x1000) inside the RAM region.

    // CAPS: none yet. WM8758 audio (I2S + PL330 DMA in the HAL) → `audio`
    // feature + CAP_AUDIO; JPEG → software decoder behind `jpeg` (no HW codec
    // driver yet). Mirror in tools/devices/exynos4412.json.

    const ANIM_SLOTS: usize = 64;
    // Resources are in RAM: reading a glyph is a memcpy, no cache needed.
    const CACHE_FONT_BITMAPS: bool = false;
    // RAM-backed: a CRC of the image takes a few ms.
    const VERIFY_FS_ON_BOOT: bool = true;
}

static mut KEYS: Option<PowerKeys> = None;

/// Temporary: a status line at the bottom of the screen for the writefs
/// "flash image corrupted" investigation. Remove once resolved.
const DIAG: bool = true;
static mut DIAG_LAST: (u32, u32, u32, usize) = (u32::MAX, 0, 0, 0);

/// `rx` bytes received / dropped / UART error bits, and the CRC-32 of what
/// landed in the RAM flash from FS_BASE to the highest written address
/// (zlib-compatible: compare with `zlib.crc32(open(img,'rb').read())`).
fn draw_diag(ctx: &mut Ctx<Exynos4412Platform>) {
    use core::fmt::Write;
    let (rx, drop, err, end) = unsafe {
        (crate::usart::RX_TOTAL, crate::usart::RX_DROPPED, crate::usart::RX_ERRORS, crate::flash::WRITE_END)
    };
    if unsafe { DIAG_LAST } == (rx, drop, err, end) {
        return;
    }
    unsafe { DIAG_LAST = (rx, drop, err, end) };
    let base = Exynos4412Platform::FS_BASE as usize;
    let len = end.saturating_sub(base);
    let crc = crate::flash::crc(base, end);
    let mut text = alloc::string::String::new();
    let _ = write!(text, "rx={rx} drop={drop} err={err:#x} fs_len={len} crc={crc:08x}");
    let y = PANEL.height as u16 - 24;
    ctx.lcd.fill_rect(0, y, PANEL.width as u16, 24, COLOR_BLACK);
    let font = ctx.fonts.embedded();
    font.draw_str(&ctx.lcd, &ctx.flash, text.as_bytes(), 8, y as i16 + 16, 0xFFE0, Some(COLOR_BLACK));
}

impl PlatformRuntime for Exynos4412Platform {
    const BG_COLOR: u16 = COLOR_BLACK;
    const FG_COLOR: u16 = COLOR_WHITE;
    // Cortex-A9 at 1.4 GHz: long bursts per main-loop pass.
    const VM_STEPS_PER_TICK: u32 = 20_000;
    const MAX_CODE_SIZE: usize = 1024 * 1024;
    type Input = FullInput;

    fn reset() -> ! {
        // A finished writefs (or a config write) reaches the eMMC first.
        crate::flash::save_if_dirty(true);
        exynos_4412_rs::console::flush();
        power::restart(BootMode::Normal)
    }

    fn syscall(id: u8, args: &[i32], _objs: &mut ObjPool) -> Option<i32> {
        let _ = args;
        match id {
            SYS_REBOOT => Self::reset(),
            _ => None,
        }
    }

    fn heap_stats() -> (usize, usize) {
        let s = heap::stats();
        (s.size - s.used, 0)
    }

    fn heap_total() -> usize {
        heap::stats().size
    }

    fn boot(ctx: &mut Ctx<Self>, touch: &mut TouchImpl<Self::TouchB>) -> u8 {
        touch.cal = crate::touch::CAL;
        unsafe { *(&raw mut KEYS) = Some(PowerKeys::new()) };

        ctx.lcd.fill_rect(0, 0, PANEL.width as u16, PANEL.height as u16, COLOR_BLACK);
        lcd::flush();
        ctx.backlight.set_brightness(100);

        // Nothing saved on CACHE yet (or storage off): recovery, so the host
        // can writefs.
        if recovery_requested(ctx) || !fs_present(ctx) {
            recovery_screen(ctx);
            return ERR_EMPTY_RUN;
        }
        0
    }

    fn frame_end(ctx: &mut Ctx<Self>) {
        if DIAG {
            draw_diag(ctx);
        }
        // Show what this pass drew (shadow -> scanned buffer, at a vsync).
        lcd::flush();
        // Config writes (touch calibration…): save once they settle.
        crate::flash::save_if_dirty(false);
        // Power key: hold = off, + vol up = restart, + vol down = fastboot.
        if let Some(keys) = unsafe { (*(&raw mut KEYS)).as_mut() } {
            keys.service(Mct::new());
        }
    }
}

fn fs_present(ctx: &Ctx<Exynos4412Platform>) -> bool {
    let mut magic = [0u8; 4];
    ctx.flash.read(Exynos4412Platform::FS_BASE, &mut magic);
    &magic == b"FERR"
}

/// Top-left 80×80 held for 3 s at boot.
fn recovery_requested(ctx: &mut Ctx<Exynos4412Platform>) -> bool {
    let be = crate::touch::MxtTouch;
    let cal = crate::touch::CAL;
    let held = || be.is_active() && matches!(be.read_screen(&cal), Some((x, y)) if x < 80 && y < 80);
    if !held() {
        return false;
    }
    let start = ctx.systick.millis();
    loop {
        let t = ctx.systick.millis().wrapping_sub(start);
        if t >= 3000 {
            return true;
        }
        ctx.lcd.fill_rect(440, 390, (t * 400 / 3000) as u16, 8, COLOR_RED);
        lcd::flush();
        if !held() {
            ctx.lcd.fill_rect(440, 390, 400, 8, COLOR_BLACK);
            lcd::flush();
            return false;
        }
        ctx.systick.delay_ms(20);
    }
}

fn recovery_screen(ctx: &mut Ctx<Exynos4412Platform>) {
    let (w, h) = (PANEL.width as i16, PANEL.height as i16);
    ctx.lcd.fill_rect(0, 0, w as u16, h as u16, COLOR_BLACK);
    let font = ctx.fonts.embedded();
    let line = |text: &[u8], y: i16, color: u16| {
        let x = w / 2 - font.text_width(text) as i16 / 2;
        font.draw_str(&ctx.lcd, &ctx.flash, text, x, y, color, Some(COLOR_BLACK));
    };
    line(b"RECOVERY MODE", h / 2 - 10, COLOR_RED);
    line(b"Use writefs to flash a new program", h / 2 + 20, 0x8410);
    lcd::flush();
}
