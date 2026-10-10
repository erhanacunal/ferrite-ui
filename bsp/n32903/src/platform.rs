//! `N32903Platform` — binds the N32903 / 800×480 RGB LCD backends to the
//! framework and implements the runtime hooks (`PlatformRuntime`) that
//! `ferrite_core::run` calls.

use ferrite_core::config::{self, ConfigStore};
use ferrite_core::ctx::Ctx;
use ferrite_core::platform::Platform;
use ferrite_core::protocol::{self, RxEvent};
use ferrite_core::render;
use ferrite_core::runtime::{ERR_EMPTY_RUN, FullInput, PlatformRuntime};
use ferrite_core::strpool::StringPool;
use ferrite_core::touch::{CalParams, TouchImpl};
use ferrite_core::types::{COLOR_BLACK, COLOR_RED, COLOR_WHITE};
use ferrite_core::vm::Vm;

use n32903_rs::timer::{self, WdtInterval};

use crate::{flash, lcd, systick, touch, usart};

/// Top of the SVC stack and the bottom of its 256 KB area (startup.s).
const SVC_STACK_TOP: u32 = 0x0080_0000 - 1024;
const SVC_STACK_BOTTOM: u32 = SVC_STACK_TOP - 256 * 1024;

pub struct N32903Platform;

impl Platform for N32903Platform {
    type LcdB = crate::lcd::VpostLcd;
    type FlashB = crate::flash::NandFlash;
    type TouchB = crate::touch::AdcTouch;
    type BacklightB = crate::backlight::PwmBl;
    type RtcB = crate::rtc::At8563;
    type SdCardB = crate::sdcard::StubSd;
    type UsartB = crate::usart::HwUart;
    type SystickB = crate::systick::N32903Systick;
    type AudioB = ferrite_core::audio::NoAudio;

    // The "flash" is the SDRAM shadow of a NAND region (see flash/mod.rs), so
    // the default layout applies: config at 0x0000, resource FS from 0x1000.

    // 8 MB SDRAM — allow richer multi-widget animated scenes.
    const ANIM_SLOTS: usize = 32;
}

impl PlatformRuntime for N32903Platform {
    const BG_COLOR: u16 = COLOR_BLACK;
    const FG_COLOR: u16 = COLOR_WHITE;
    const VM_STEPS_PER_TICK: u32 = 10;
    type Input = FullInput;

    fn reset() -> ! {
        // Pending shadow writes (e.g. a just-finished writefs) go to NAND first.
        flash::flush();
        usart::log("reset\r\n");
        unsafe {
            timer::wdt_enable(WdtInterval::Ms500, true);
        }
        loop {
            core::hint::spin_loop();
        }
    }

    fn syscall(_id: u8, _args: &[i32], _strpool: &StringPool) -> Option<i32> {
        None
    }

    fn stack_info() -> (u32, u32) {
        let sp: u32;
        unsafe { core::arch::asm!("mov {}, sp", out(reg) sp) };
        (SVC_STACK_TOP.saturating_sub(sp), sp.saturating_sub(SVC_STACK_BOTTOM))
    }

    fn heap_stats() -> (usize, usize) {
        // The HAL heap is the global allocator (ferrite-core `external_alloc`).
        let s = n32903_rs::heap::stats();
        (s.free, s.largest_free)
    }

    fn boot(ctx: &mut Ctx<Self>, touch: &mut TouchImpl<Self::TouchB>) -> u8 {
        touch.cal = touch::DEFAULT_CAL;

        // Saved touch calibration overrides the defaults.
        let cfg = ConfigStore::mount_or_format(&ctx.flash, Self::CONFIG_BASE);
        let mut cal_buf = [0u8; 9];
        if let Some(len) = cfg.read(&ctx.flash, config::KEY_TOUCH_CAL, &mut cal_buf) {
            if len >= 9 {
                if let Some(cal) = CalParams::from_bytes(&cal_buf) {
                    touch.cal = cal;
                }
            }
        }

        // Both buffers black, then the backlight on.
        ctx.lcd.fill_rect(0, 0, 800, 480, COLOR_BLACK);
        ctx.lcd.begin_frame();
        ctx.lcd.fill_rect(0, 0, 800, 480, COLOR_BLACK);
        ctx.lcd.end_frame();
        ctx.backlight.set_brightness(100);

        // Recovery mode: hold the top-left corner for 3 seconds at boot.
        if touch::is_pressed() && touch::check_recovery_touch(&touch.cal, 200) {
            if recovery_hold(ctx, &touch.cal) {
                return ERR_EMPTY_RUN;
            }
        }
        0
    }

    fn initial_render(_ctx: &mut Ctx<Self>, _vm: &Vm) {
        // Frame-buffer panel: the first dirty frame does the initial render.
    }

    fn on_extra_rx(
        ev: RxEvent,
        ctx: &mut Ctx<Self>,
        vm: &mut Vm,
        touch: &mut TouchImpl<Self::TouchB>,
    ) -> bool {
        match ev {
            RxEvent::TouchCalibrate => {
                let cal = touch::run_calibration(touch, &ctx.lcd);
                let mut cfg = ConfigStore::mount_or_format(&ctx.flash, Self::CONFIG_BASE);
                cfg.write(&ctx.flash, config::KEY_TOUCH_CAL, &cal.to_bytes());
                protocol::send_touch_cal(&ctx.usart, &cal);
                render::render_all(ctx, vm);
                true
            }
            _ => false,
        }
    }

    fn frame_end(_ctx: &mut Ctx<Self>) {
        // Dirty mode draws into the visible (cacheable) buffer: write it back
        // so the VPOST sees it. Cheap when nothing was drawn.
        lcd::flush_cache();
        flash::flush_if_idle();
    }
}

/// Animate the recovery progress bar; return `true` if held the full 3 seconds.
fn recovery_hold(ctx: &mut Ctx<N32903Platform>, cal: &CalParams) -> bool {
    let start = systick::millis();
    let hold_ms: u32 = 3000;
    let bar_h: u16 = 6;
    let mut entered = false;

    loop {
        let elapsed = systick::millis().wrapping_sub(start);
        if elapsed >= hold_ms {
            entered = true;
            break;
        }
        let fill = ((elapsed * 800) / hold_ms) as u16;
        ctx.lcd.fill_rect(0, 0, fill, bar_h, COLOR_RED);
        lcd::flush_cache();
        if !touch::check_recovery_touch(cal, 50) {
            break; // released or moved out
        }
    }

    if entered {
        ctx.lcd.fill_rect(0, 0, 800, bar_h, COLOR_RED);
        ctx.lcd.fill_rect(0, bar_h, 800, 480 - bar_h, COLOR_BLACK);
        let font = ctx.fonts.embedded();
        font.draw_str(&ctx.lcd, &ctx.flash, b"RECOVERY MODE", 290, 230, COLOR_RED, Some(COLOR_BLACK));
        font.draw_str(
            &ctx.lcd,
            &ctx.flash,
            b"Use writefs to flash new program",
            210,
            260,
            0x4208,
            Some(COLOR_BLACK),
        );
    } else {
        ctx.lcd.fill_rect(0, 0, 800, bar_h, COLOR_BLACK);
    }
    lcd::flush_cache();
    entered
}
