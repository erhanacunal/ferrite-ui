//! `TdoY13Platform` — binds the F1C100s / TL021WVC04 backends to the
//! framework and implements the runtime hooks (`PlatformRuntime`) that
//! `ferrite_core::run` calls.

use ferrite_core::platform::Platform;
use ferrite_core::touch::TouchImpl;

use ferrite_core::ctx::Ctx;
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
}

impl PlatformRuntime for TdoY13Platform {
    const BG_COLOR: u16 = COLOR_BLACK;
    const FG_COLOR: u16 = COLOR_WHITE;
    type Input = FullInput;

    fn reset() -> ! {
        loop {
            f1c100s::thread::thread_sleep(10);
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

    fn boot(ctx: &mut Ctx<Self>, _touch: &mut TouchImpl<Self::TouchB>) -> u8 {
        ctx.systick.delay_ms(500);
        ctx.lcd.fill_rect(0, 0, 480, 480, COLOR_BLACK);
        ctx.backlight.set_brightness(100);
        0
    }

    fn initial_render(_ctx: &mut Ctx<Self>, _vm: &Vm) {
        // Raster panel: first dirty frame handles the initial render.
    }
}
