//! `SimPlatform` — binds the host (minifb) backends to the framework and
//! implements the runtime hooks that `ferrite_core::run` calls.
//!
//! minifb's `Window` is `!Send` and the framebuffer / mouse state are shared
//! `Rc`s, so the window loop state lives in a thread-local (`HOST`) that
//! `frame_end` pumps once per iteration: sample the mouse for the next frame and
//! present the framebuffer.

use std::cell::RefCell;

use ferrite_core::platform::Platform;

use ferrite_core::ctx::Ctx;
use ferrite_core::runtime::{FullInput, PlatformRuntime};
use ferrite_core::strpool::StringPool;
use ferrite_core::types::{COLOR_BLACK, COLOR_WHITE};

use minifb::{Key, MouseButton, MouseMode, Window};

use crate::lcd::sim::Framebuffer;
use crate::lcd::{HEIGHT, WIDTH};
use crate::systick;
use crate::touch::sim::MouseState;

/// Marker type implementing `Platform` + `PlatformRuntime` for the host sim.
pub struct SimPlatform;

impl Platform for SimPlatform {
    type LcdB = crate::lcd::sim::SimLcd;
    type FlashB = crate::flash::sim::FileFlash;
    type TouchB = crate::touch::sim::MouseTouch;
    type BacklightB = crate::backlight::sim::StubBacklight;
    type RtcB = crate::rtc::sim::SystemRtc;
    type SdCardB = crate::sdcard::sim::FileSd;
    type UsartB = crate::usart::sim::Stdio;
    type SystickB = crate::systick::SimSystick;
    type AudioB = ferrite_core::audio::NoAudio;
}

// --- Host window state (thread-local; minifb Window is !Send) ---

struct Host {
    window: Window,
    fb: Framebuffer,
    mouse: MouseState,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

/// Hand the window, framebuffer and mouse state to the runtime so `frame_end`
/// can pump them. Call once, before `ferrite_core::run`.
pub fn install_host(window: Window, fb: Framebuffer, mouse: MouseState) {
    HOST.with(|h| *h.borrow_mut() = Some(Host { window, fb, mouse }));
}

impl PlatformRuntime for SimPlatform {
    const BG_COLOR: u16 = COLOR_BLACK;
    const FG_COLOR: u16 = COLOR_WHITE;
    type Input = FullInput;

    fn reset() -> ! {
        std::process::exit(0)
    }

    fn syscall(id: u8, _args: &[i32], _strpool: &StringPool) -> Option<i32> {
        match id {
            0x00 => Some(systick::millis() as i32), // SYS_UPTIME
            _ => None,
        }
    }

    fn frame_end(_ctx: &mut Ctx<Self>) {
        HOST.with(|h| {
            let mut guard = h.borrow_mut();
            let host = match guard.as_mut() {
                Some(h) => h,
                None => return,
            };

            if !host.window.is_open() || host.window.is_key_down(Key::Escape) {
                std::process::exit(0);
            }

            // Sample the mouse for the next frame's input dispatch.
            let pressed = host.window.get_mouse_down(MouseButton::Left);
            let pos = host.window.get_mouse_pos(MouseMode::Discard);
            let (mx, my) = match pos {
                Some((x, y)) => (
                    x.clamp(0.0, (WIDTH - 1) as f32) as u16,
                    y.clamp(0.0, (HEIGHT - 1) as f32) as u16,
                ),
                None => (0, 0),
            };
            host.mouse.update(mx, my, pressed && pos.is_some());

            // Present the framebuffer (blocks to the target FPS for pacing).
            let buf = host.fb.borrow();
            let _ = host
                .window
                .update_with_buffer(&buf, WIDTH as usize, HEIGHT as usize);
        });
    }
}
