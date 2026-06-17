//! `TdoY13Platform` — binds the F1C100s / TL021WVC04 backends to the
//! framework and implements the runtime hooks (`PlatformRuntime`) that
//! `ferrite_core::run` calls.

use ferrite_core::config::{self, ConfigStore};
use ferrite_core::platform::Platform;
use ferrite_core::touch::{CalParams, TouchImpl};

use ferrite_core::ctx::Ctx;
use ferrite_core::protocol::{self, RxEvent};
use ferrite_core::render;
use ferrite_core::runtime::{FullInput, PlatformRuntime};
use ferrite_core::strpool::StringPool;
use ferrite_core::types::{COLOR_BLACK, COLOR_WHITE};
use ferrite_core::vm::Vm;

const SYS_REBOOT: u8 = 0x01;

pub struct TdoY13Platform;

impl Platform for TdoY13Platform {
    type LcdB = crate::lcd::DebeLcd;
    type FlashB = crate::flash::HwFlash;
    type TouchB = crate::touch::CstTouch;
    type BacklightB = crate::backlight::PwmBl;
    type RtcB = crate::rtc::StubRtc;
    type SdCardB = crate::sdcard::StubSd;
    type UsartB = crate::usart::HwUart;
    type SystickB = crate::systick::F1cSystick;
    // F1C100s has a built-in audio codec — switch to a real backend (and set
    // CAP_AUDIO + devices.json caps.audio) when the audio milestone lands.
    type AudioB = ferrite_core::audio::NoAudio;

    // The F1C100s BootROM loads firmware from offset 0 of the *same* SPI NOR
    // that holds the resources (BY25Q128AS, 128 Mbit = 16 MB), so the flash is
    // partitioned:
    //   0x000000 - 0x5FFFFF  firmware       6 MB, reserved
    //   0x600000 - 0x600FFF  config store   4 KB, one sector
    //   0x601000 - 0xFFFFFF  resource FS    ~9.99 MB (writefs target)
    const CONFIG_BASE: u32 = 6 * 1024 * 1024; // 0x60_0000
    const FS_BASE: u32 = 6 * 1024 * 1024 + 4 * 1024; // 0x60_1000            
}

impl PlatformRuntime for TdoY13Platform {
    const BG_COLOR: u16 = COLOR_BLACK;
    const FG_COLOR: u16 = COLOR_WHITE;
    const VM_STEPS_PER_TICK: u32 = 1;
    type Input = FullInput;

    fn reset() -> ! {
        // The F1C100s Watchdog can be configured to reset just the CPU core
        // (leaving RAM and peripherals intact) — use that for a quick reboot.
        f1c100s::cpu::cpu_reset();
        loop {
            core::hint::spin_loop();
        }
    }

    fn syscall(id: u8, args: &[i32], _strpool: &StringPool) -> Option<i32> {
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
        0
    }

    fn initial_render(_ctx: &mut Ctx<Self>, _vm: &Vm) {
        // Raster panel: first dirty frame handles the initial render.
    }

    fn on_extra_rx(
        ev: RxEvent,
        ctx: &mut Ctx<Self>,
        vm: &mut Vm,
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
                render::render_all(ctx, vm);
                true
            }
            _ => false,
        }
    }
}
