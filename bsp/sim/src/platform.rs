//! `SimPlatform` — binds the host (minifb) backends to the framework and
//! implements the runtime hooks that `ferrite_core::run` calls.
//!
//! minifb's `Window` is `!Send` and the framebuffer / mouse state are shared
//! `Rc`s, so the window loop state lives in a thread-local (`HOST`) that
//! `frame_end` pumps once per iteration: sample the mouse for the next frame and
//! present the framebuffer.

use std::cell::RefCell;

use ferrite_core::platform::{CAP_AUDIO, CAP_JPEG, Platform};

use ferrite_core::ctx::Ctx;
use ferrite_core::runtime::{FullInput, PlatformRuntime};
use ferrite_core::pool::ObjPool;
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
    type UsartB = crate::usart::sim::Stdio;
    type SystickB = crate::systick::SimSystick;
    type AudioB = crate::audio::SimAudio;
    type JpegB = crate::jpeg::SoftJpeg;
    type GpioB = crate::gpio::SimGpio;
    type EepromB = crate::eeprom::SimEeprom;

    // Software JPEG decoder (sim/src/jpeg.rs) + silent real-time audio
    // output (sim/src/audio.rs).
    const CAPS: u32 = CAP_AUDIO | CAP_JPEG;

    const VERIFY_FS_ON_BOOT: bool = true;
}

// --- Host window state (thread-local; minifb Window is !Send) ---

struct Host {
    window: Window,
    fb: Framebuffer,
    mouse: MouseState,
    taps: Vec<ScriptedTap>,
}

/// A scripted press (`--tap X,Y,MS`): the touch reads as held at (x, y) for
/// `TAP_HOLD_MS` starting `at_ms` after launch. minifb samples the real mouse,
/// so synthetic OS input cannot drive the sim — this lets tests click.
#[derive(Clone, Copy)]
pub struct ScriptedTap {
    pub x: u16,
    pub y: u16,
    pub at_ms: u32,
}

/// How long a scripted tap holds the press (well under long-press timing).
const TAP_HOLD_MS: u32 = 150;

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

/// Hand the window, framebuffer and mouse state to the runtime so `frame_end`
/// can pump them. Call once, before `ferrite_core::run`.
pub fn install_host(window: Window, fb: Framebuffer, mouse: MouseState, taps: Vec<ScriptedTap>) {
    HOST.with(|h| *h.borrow_mut() = Some(Host { window, fb, mouse, taps }));
}

impl PlatformRuntime for SimPlatform {
    const BG_COLOR: u16 = COLOR_BLACK;
    const FG_COLOR: u16 = COLOR_WHITE;
    // The window present paces the loop to ~60 Hz; run a hardware-like burst of
    // VM instructions per frame so `millis()`-timed animation advances at the
    // intended rate instead of one instruction per frame. A program that yields
    // each frame stops the burst early, so this only speeds up busy loops.
    const VM_STEPS_PER_TICK: u32 = 50_000;
    // Plenty of RAM: load any program image (u16 VM address space).
    const MAX_CODE_SIZE: usize = 64 * 1024;
    type Input = FullInput;

    fn reset() -> ! {
        std::process::exit(0)
    }

    fn syscall(id: u8, _args: &[i32], _strpool: &ObjPool) -> Option<i32> {
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
            let now = systick::millis();
            match host.taps.iter().find(|t| now >= t.at_ms && now < t.at_ms + TAP_HOLD_MS) {
                Some(t) => host.mouse.update(t.x, t.y, true),
                None => host.mouse.update(mx, my, pressed && pos.is_some()),
            }

            // Present the front (displayed) buffer (blocks to the target FPS
            // for pacing). In buffered mode this is whichever buffer the last
            // end_frame published; in dirty mode it is always buffer 0.
            let buf = host.fb.front_buffer();
            let _ = host
                .window
                .update_with_buffer(&buf, WIDTH as usize, HEIGHT as usize);
        });
    }
}
