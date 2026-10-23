//! Simulator BSP — host-side desktop simulator.
//!
//! Builds the application context against the host (minifb) backends and hands
//! it to the shared `ferrite_core::run` loop. The window event loop is pumped from
//! `SimPlatform::frame_end` once per iteration.
//!
//! Build:  cargo build -p bsp-sim --release
//! Run:    cargo run -p bsp-sim -- --fsimage examples/widget_demo/fs.bin

use std::env;
use std::fs;

use minifb::{Window, WindowOptions};

use ferrite_core::ctx::Ctx;
use ferrite_core::font::FontList;
use ferrite_core::fs::{self as ferrite_fs, Fs};
use ferrite_core::image::ImageList;
use ferrite_core::strpool::StringPool;
use ferrite_core::widget::WidgetTree;

// --- Concrete host backends (type aliases + free-fn constructors) ---
mod backlight;
mod eeprom;
mod flash;
mod gpio;
mod lcd;
mod rtc;
mod sdcard;
mod systick;
mod touch;
mod usart;

mod platform;

use lcd::sim::new_framebuffer;
use lcd::{HEIGHT, WIDTH};
use platform::SimPlatform;
use touch::sim::MouseState;
use touch::CalParams;

fn main() {
    let (fsimage_path, taps, eeprom_path) = parse_args();

    systick::init();

    let flash = match load_flash(fsimage_path.as_deref()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("failed to load fsimage: {e}");
            std::process::exit(1);
        }
    };

    // Report what the filesystem contains (informational; `run` mounts it again).
    match Fs::mount(&flash, ferrite_fs::DEFAULT_FS_BASE) {
        Ok(f) => {
            let progs = f.count_by_kind(&flash, ferrite_fs::RES_PROGRAM);
            let fonts = f.count_by_kind(&flash, ferrite_fs::RES_FONT);
            let imgs = f.count_by_kind(&flash, ferrite_fs::RES_IMAGE);
            println!("fs mounted: programs={progs} fonts={fonts} images={imgs}");
        }
        Err(_) => println!("fs mount failed; no program will be loaded"),
    }

    let fb = new_framebuffer();
    let mouse = MouseState::new();
    let mut touch = touch::new(mouse.clone());
    touch.cal = identity_cal();

    let window = match Window::new(
        "ferrite-ui sim",
        WIDTH as usize,
        HEIGHT as usize,
        WindowOptions::default(),
    ) {
        Ok(mut w) => {
            w.set_target_fps(60);
            w
        }
        Err(e) => {
            eprintln!("window open failed: {e}");
            std::process::exit(1);
        }
    };

    let ctx = Box::new(Ctx::<SimPlatform> {
        lcd: lcd::new(fb.clone()),
        flash,
        tree: WidgetTree::new(),
        fonts: FontList::new(),
        images: ImageList::new(),
        strpool: StringPool::new(),
        fs: None,
        backlight: backlight::new(),
        rtc: rtc::init(),
        usart: usart::init(),
        systick: systick::Systick::handle(),
        audio: ferrite_core::audio::AudioImpl::none(),
        gpio: ferrite_core::gpio::GpioImpl::with_backend(gpio::SimGpio::default()),
        eeprom: ferrite_core::eeprom::EepromImpl::with_backend(eeprom::SimEeprom::new(eeprom_path)),
        cursor_visible: false,
    });

    platform::install_host(window, fb.clone(), mouse.clone(), taps);

    ferrite_core::runtime::run::<SimPlatform>(ctx, touch)
}

/// `--fsimage <path>`, any number of `--tap X,Y,MS` (scripted presses) and
/// `--eeprom <path>` (file that keeps the virtual EEPROM across runs).
fn parse_args() -> (Option<String>, Vec<platform::ScriptedTap>, Option<String>) {
    let mut fsimage = None;
    let mut eeprom = None;
    let mut taps = Vec::new();
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fsimage" => fsimage = args.next(),
            "--eeprom" => eeprom = args.next(),
            "--tap" => {
                let spec = args.next().unwrap_or_default();
                let parts: Vec<u32> = spec.split(',').filter_map(|p| p.trim().parse().ok()).collect();
                match parts[..] {
                    [x, y, at_ms] => taps.push(platform::ScriptedTap { x: x as u16, y: y as u16, at_ms }),
                    _ => eprintln!("ignoring --tap {spec:?}: expected X,Y,MS"),
                }
            }
            _ => {}
        }
    }
    (fsimage, taps, eeprom)
}

fn load_flash(path: Option<&str>) -> Result<flash::Flash, String> {
    match path {
        Some(p) => {
            let mut bytes = fs::read(p).map_err(|e| format!("{p}: {e}"))?;
            let raw_len = bytes.len();

            // An fs-only image (starts with "FERR" at offset 0) is padded with
            // 0x1000 reserved bytes so the header lands at FS_BASE.
            if bytes.len() >= 4 && &bytes[..4] == b"FERR" {
                let mut padded = std::vec![0xFFu8; ferrite_fs::DEFAULT_FS_BASE as usize];
                padded.extend_from_slice(&bytes);
                bytes = padded;
                println!(
                    "loaded {raw_len} bytes from {p} (fs-only image — padded with {}B prefix)",
                    ferrite_fs::DEFAULT_FS_BASE
                );
            } else {
                println!("loaded {raw_len} bytes from {p}");
            }
            Ok(flash::from_image(bytes))
        }
        None => {
            println!("no --fsimage specified; starting with blank 32MB flash");
            Ok(flash::new())
        }
    }
}

fn identity_cal() -> CalParams {
    CalParams {
        xy_swap: false,
        x_flip: false,
        y_flip: true,
        x_min: 0,
        x_max: WIDTH - 1,
        y_min: 0,
        y_max: HEIGHT - 1,
    }
}
