//! `TdoY13Platform` — binds the F1C100s / TL021WVC04 backends to the
//! framework and implements the runtime hooks (`PlatformRuntime`) that
//! `ferrite_core::run` calls.

use ferrite_core::config::{self, ConfigStore};
use ferrite_core::platform::Platform;
use ferrite_core::touch::{CalParams, TouchBackend, TouchImpl};

use ferrite_core::ctx::Ctx;
use ferrite_core::protocol::{self, RxEvent};
use ferrite_core::render;
use ferrite_core::runtime::{ERR_EMPTY_RUN, FullInput, PlatformRuntime};
use ferrite_core::pool::ObjPool;
use ferrite_core::types::{COLOR_BLACK, COLOR_RED, COLOR_WHITE};
use ferrite_core::vm::Vm;

const SYS_REBOOT: u8 = 0x01;

pub struct TdoY13Platform;

impl Platform for TdoY13Platform {
    type LcdB = crate::lcd::DebeLcd;
    type FlashB = crate::flash::HwFlash;
    type TouchB = crate::touch::CstTouch;
    type BacklightB = crate::backlight::PwmBl;
    type RtcB = crate::rtc::SoftRtc;
    type UsartB = crate::usart::HwUart;
    type SystickB = crate::systick::F1cSystick;
    // F1C100s has a built-in audio codec — enable ferrite-core's `audio`
    // feature with a real `AudioB` (and set CAP_AUDIO + devices.json
    // caps.audio) when the audio milestone lands.
    type GpioB = ferrite_core::gpio::NoGpio;
    type EepromB = ferrite_core::eeprom::NoEeprom;

    // The F1C100s BootROM loads firmware from offset 0 of the *same* SPI NOR
    // that holds the resources (BY25Q128AS, 128 Mbit = 16 MB), so the flash is
    // partitioned:
    //   0x000000 - 0x5FFFFF  firmware       6 MB, reserved
    //   0x600000 - 0x600FFF  config store   4 KB, one sector
    //   0x601000 - 0xFFFFFF  resource FS    ~9.99 MB (writefs target)
    const CONFIG_BASE: u32 = 6 * 1024 * 1024; // 0x60_0000
    const FS_BASE: u32 = 6 * 1024 * 1024 + 4 * 1024; // 0x60_1000

    // F1C100s has ample SDRAM — allow richer multi-widget animated scenes.
    const ANIM_SLOTS: usize = 32;

    // Resources live on SPI NOR (one SPI transaction per read): keep glyph
    // bitmaps in SDRAM instead of reading them for every glyph drawn.
    const CACHE_FONT_BITMAPS: bool = true;

    // CRC the FS image at boot: SPI0 runs at 50 MHz, so ~0.2 s per MB.
    const VERIFY_FS_ON_BOOT: bool = true;
}

impl PlatformRuntime for TdoY13Platform {
    const BG_COLOR: u16 = COLOR_BLACK;
    const FG_COLOR: u16 = COLOR_WHITE;
    // A main-loop pass also polls touch (costly while pressed) and checks
    // render state, so 10 steps a pass starved the VM: a ~3k-instruction game
    // frame took hundreds of passes and slowed down whenever a finger was on
    // the panel. The burst still ends at the program's yield/delay.
    const VM_STEPS_PER_TICK: u32 = 10_000;
    // Plenty of RAM: load any program image (u16 VM address space).
    const MAX_CODE_SIZE: usize = 64 * 1024;
    type Input = FullInput;

    fn reset() -> ! {
        // The F1C100s Watchdog can be configured to reset just the CPU core
        // (leaving RAM and peripherals intact) — use that for a quick reboot.
        f1c100s::cpu::cpu_reset();
        loop {
            core::hint::spin_loop();
        }
    }

    fn syscall(id: u8, args: &[i32], _strpool: &mut ObjPool) -> Option<i32> {
        match id {
            SYS_REBOOT => {
                let _ = args;
                Self::reset();
            }
            _ => None,
        }
    }

    fn stack_info() -> (u32, u32) {
        (0, 32 * 1024 * 1024)
    }

    fn heap_stats() -> (usize, usize) {
        // Report through the f1c100s allocator we registered as global; the
        // lib's static heap is disabled under the `external_alloc` feature.
        (f1c100s::allocator::free_bytes(), 0)
    }

    fn heap_total() -> usize {
        // Same arena main.rs hands to `init_heap`: `_end` up to DRAM_END.
        unsafe extern "C" {
            static _end: u8;
        }
        crate::DRAM_END - core::ptr::addr_of!(_end) as usize
    }

    fn boot(ctx: &mut Ctx<Self>, touch: &mut TouchImpl<Self::TouchB>) -> u8 {
        // CST8xx capacitive panel defaults. The controller already reports
        // coordinates in screen orientation (top=0, left=0), so no axis flip
        // is needed — confirmed against corner touches.
        touch.cal = CalParams {
            xy_swap: false,
            x_flip: false,
            y_flip: false,
            x_min: 0,
            x_max: 480,
            y_min: 0,
            y_max: 480,
        };

        // Load saved calibration from config (overrides defaults if present).
        let cfg = ConfigStore::mount_or_format(&ctx.flash, Self::CONFIG_BASE);
        let mut cal_buf = [0u8; 9];
        if let Some(len) = cfg.read(&ctx.flash, config::KEY_TOUCH_CAL, &mut cal_buf) {
            if len >= 9 {
                if let Some(cal) = CalParams::from_bytes(&cal_buf) {
                    touch.cal = cal;
                }
            }
        }

        ctx.systick.delay_ms(500);
        ctx.lcd.fill_rect(0, 0, 480, 480, COLOR_BLACK);
        ctx.backlight.set_brightness(100);

        // Recovery mode: hold the screen centre for 3 seconds at boot (the
        // round panel has no reachable corners).
        if recovery_touch(&touch.cal, ctx, 200) && recovery_hold(ctx, &touch.cal) {
            return ERR_EMPTY_RUN;
        }
        0
    }

    fn initial_render(_ctx: &mut Ctx<Self>, _vm: &Vm) {
        // Raster panel: first dirty frame handles the initial render.
    }

    fn present_buffered(ctx: &mut Ctx<Self>) {
        // DIAGNOSTIC (bisection): present SYNCHRONOUSLY on the UI thread (the
        // 2-thread arrangement the tear-free build used), instead of handing off
        // to the present thread via `submit_present`. This isolates whether the
        // idle hard-freeze is caused by the present thread / 3-thread scheduler
        // change: if it still freezes here, the cause is pre-existing (scheduler
        // / INTC / touch), not the threading. Restore `submit_present()` once
        // decided. `end_frame` does the D-cache clean + DEBE buffer flip.
        ctx.lcd.end_frame();
        ctx.lcd.flush_dirty();
    }

    fn frame_end(_ctx: &mut Ctx<Self>) {
        // The framebuffers are write-back cacheable. Dirty mode draws straight
        // into the visible buffer, so write those pixels back for the DEBE.
        // Cheap when nothing was drawn (cost = dirty cache lines).
        crate::lcd::flush_cache();
    }

    fn on_extra_rx(
        ev: RxEvent,
        ctx: &mut Ctx<Self>,
        _vm: &mut Vm,
        touch: &mut TouchImpl<Self::TouchB>,
    ) -> bool {
        match ev {
            RxEvent::TouchCalibrate => {
                // Capacitive panel: calibration is identity — store and report.
                let cal = CalParams {
                    xy_swap: false,
                    x_flip: false,
                    y_flip: false,
                    x_min: 0,
                    x_max: 480,
                    y_min: 0,
                    y_max: 480,
                };
                touch.cal = cal;
                let mut cfg = ConfigStore::mount_or_format(&ctx.flash, Self::CONFIG_BASE);
                cfg.write(&ctx.flash, config::KEY_TOUCH_CAL, &cal.to_bytes());
                protocol::send_touch_cal(&ctx.usart, &cal);
                render::render_all(ctx);
                true
            }
            _ => false,
        }
    }
}

/// Side of the square at the screen centre that starts recovery mode.
const RECOVERY_BOX: u16 = 120;

/// True if the panel is held inside the centre box for `hold_ms`.
fn recovery_touch(cal: &CalParams, ctx: &Ctx<TdoY13Platform>, hold_ms: u32) -> bool {
    let lo = (480 - RECOVERY_BOX) / 2;
    let hi = lo + RECOVERY_BOX;
    let touch = crate::touch::new();
    let start = ctx.systick.millis();
    while ctx.systick.millis().wrapping_sub(start) < hold_ms {
        if !touch.is_active() {
            return false;
        }
        match touch.read_screen(cal) {
            Some((x, y)) if (lo..hi).contains(&x) && (lo..hi).contains(&y) => {}
            Some(_) => return false,
            None => {} // between samples
        }
        ctx.systick.delay_ms(10);
    }
    true
}

/// Animate the recovery progress bar; return `true` if held the full 3 seconds.
fn recovery_hold(ctx: &mut Ctx<TdoY13Platform>, cal: &CalParams) -> bool {
    // A bar under the centre box, inside the round visible area.
    const BAR_X: u16 = 140;
    const BAR_Y: u16 = 320;
    const BAR_W: u16 = 200;
    const BAR_H: u16 = 6;
    let hold_ms: u32 = 3000;
    let start = ctx.systick.millis();
    let mut entered = false;

    loop {
        let elapsed = ctx.systick.millis().wrapping_sub(start);
        if elapsed >= hold_ms {
            entered = true;
            break;
        }
        let fill = ((elapsed * BAR_W as u32) / hold_ms) as u16;
        ctx.lcd.fill_rect(BAR_X, BAR_Y, fill, BAR_H, COLOR_RED);
        crate::lcd::flush_cache();
        if !recovery_touch(cal, ctx, 50) {
            break; // released or moved out
        }
    }

    ctx.lcd.fill_rect(0, 0, 480, 480, COLOR_BLACK);
    if entered {
        let font = ctx.fonts.embedded();
        let line = |text: &[u8], y: i16, color: u16| {
            let x = 240 - font.text_width(text) as i16 / 2;
            font.draw_str(&ctx.lcd, &ctx.flash, text, x, y, color, Some(COLOR_BLACK));
        };
        line(b"RECOVERY MODE", 230, COLOR_RED);
        line(b"Use writefs to flash", 260, 0x4208);
        line(b"a new program", 282, 0x4208);
    }
    crate::lcd::flush_cache();
    entered
}
