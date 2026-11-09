//! FIMD window 1 board test (HTTP `POST /api/overlay`), step 1 of hardware layers: a
//! premultiplied ARGB8888 test picture shown by the display controller above window 0 (the
//! UI), placed and faded by register writes only (HAL `Fimd::set_overlay`). Nothing of the UI
//! is redrawn for it.
//!
//! Modes (`mode=`):
//! - `off`: window 1 hidden.
//! - `still` (`x`, `y`, `alpha`): the 256x256 picture at a fixed place; `x`/`y` may be
//!   negative or past the screen edge (cut off by the HAL).
//! - `move`: bounces across the screen and 128 px past each edge, at 960 px/s.
//! - `fade`: centred, the plane alpha cycling 0 -> 255 -> 0 over 2 s.
//! - `full` (`alpha`): a full-screen 1280x800 scrim (50% black, white frame, alpha ramp), for
//!   scan-out bandwidth with presents running (look for flicker or shifted lines).
//!
//! The 256x256 picture, inside a 4 px opaque white frame: the top-left quarter fully
//! transparent; the rest 50% green; an opaque disc in the centre, red to blue; at the bottom a
//! white bar with alpha 0 (left) to 255 (right). Premultiplied, so a correct blend shows the
//! UI through the transparent quarter unchanged and through the green at half strength, and
//! the ramp goes smoothly from invisible to white with no bright fringe.
//!
//! The buffers are allocated on the first use and kept.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, AtomicU8, Ordering};

use exynos_4412_rs::cpu;
use exynos_4412_rs::drivers::display::{Fimd, Overlay};
use exynos_4412_rs::mmu;

use crate::systick;

const WINDOW: usize = 1;
const SIDE: usize = 256;
const SCREEN_W: usize = 1280;
const SCREEN_H: usize = 800;

pub const OFF: u8 = 0;
pub const STILL: u8 = 1;
pub const MOVE: u8 = 2;
pub const FADE: u8 = 3;
pub const FULL: u8 = 4;

/// Set by HTTP (CPU3), read by [`service`] (CPU0).
pub static MODE: AtomicU8 = AtomicU8::new(OFF);
pub static X: AtomicI32 = AtomicI32::new(0);
pub static Y: AtomicI32 = AtomicI32::new(0);
pub static ALPHA: AtomicU8 = AtomicU8::new(255);

/// CPU0 only.
struct State {
    small: Option<Vec<u32>>,
    full: Option<Vec<u32>>,
    shown: Option<Overlay>,
    /// When the current mode started, for the animations.
    since: u32,
    mode: u8,
}

static mut STATE: State = State { small: None, full: None, shown: None, since: 0, mode: OFF };

/// Premultiplies a straight colour.
fn premul(a: u32, r: u32, g: u32, b: u32) -> u32 {
    a << 24 | (r * a / 255) << 16 | (g * a / 255) << 8 | b * a / 255
}

fn small_picture() -> Option<Vec<u32>> {
    let mut px = Vec::new();
    px.try_reserve_exact(SIDE * SIDE).ok()?;
    let c = SIDE as i32 / 2;
    for y in 0..SIDE {
        for x in 0..SIDE {
            let frame = x < 4 || y < 4 || x >= SIDE - 4 || y >= SIDE - 4;
            let (dx, dy) = (x as i32 - c, y as i32 - c);
            let v = if frame {
                0xFFFF_FFFF
            } else if y >= SIDE - 36 && y < SIDE - 12 && x >= 12 && x < SIDE - 12 {
                let a = ((x - 12) * 255 / (SIDE - 25)) as u32;
                premul(a, 255, 255, 255)
            } else if dx * dx + dy * dy < 56 * 56 {
                let t = (x as u32 - 72) * 255 / 112;
                premul(255, 255 - t.min(255), 40, t.min(255))
            } else if x < SIDE / 2 && y < SIDE / 2 {
                0
            } else {
                premul(128, 0, 255, 0)
            };
            px.push(v);
        }
    }
    Some(px)
}

fn full_picture() -> Option<Vec<u32>> {
    let mut px = Vec::new();
    px.try_reserve_exact(SCREEN_W * SCREEN_H).ok()?;
    for y in 0..SCREEN_H {
        for x in 0..SCREEN_W {
            let frame = x < 8 || y < 8 || x >= SCREEN_W - 8 || y >= SCREEN_H - 8;
            let v = if frame {
                0xFFFF_FFFF
            } else if y >= SCREEN_H - 80 && y < SCREEN_H - 40 {
                premul((x * 255 / (SCREEN_W - 1)) as u32, 255, 255, 255)
            } else {
                premul(128, 0, 0, 0)
            };
            px.push(v);
        }
    }
    Some(px)
}

/// Writes a picture's lines out of the cache: the FIMD reads DRAM.
fn clean(px: &[u32]) {
    mmu::clean_dcache_range(px.as_ptr() as usize, px.len() * 4);
}

/// CPU0, once per pass (`frame_end`): places window 1 for the current mode.
pub fn service() {
    // SAFETY: CPU0 only, not reentrant.
    let st = unsafe { &mut *(&raw mut STATE) };
    let mode = MODE.load(Ordering::Acquire);
    // The windows belong to the renderer's layers while it uses them.
    if crate::lcd::layers_in_use() {
        if mode != OFF {
            exynos_4412_rs::println!("ferrite: overlay test off: layers in use");
            MODE.store(OFF, Ordering::Release);
        }
        st.mode = OFF;
        st.shown = None;
        return;
    }
    let now = systick::millis();
    if mode != st.mode {
        st.mode = mode;
        st.since = now;
    }
    let t = now.wrapping_sub(st.since);
    let wanted = match mode {
        STILL | MOVE | FADE => {
            if st.small.is_none() {
                st.small = small_picture();
                if let Some(px) = &st.small {
                    clean(px);
                }
            }
            st.small.as_ref().map(|px| {
                let (x, y, alpha) = match mode {
                    STILL => (X.load(Ordering::Relaxed), Y.load(Ordering::Relaxed), ALPHA.load(Ordering::Relaxed)),
                    MOVE => {
                        // 960 px/s along both axes, bouncing between -128 and the far edge + 128.
                        let bounce = |span: i32, ms: u32| {
                            let d = (ms as u64 * 960 / 1000) as i32 % (2 * span);
                            (if d < span { d } else { 2 * span - d }) - 128
                        };
                        let sx = SCREEN_W as i32 - SIDE as i32 + 256;
                        let sy = SCREEN_H as i32 - SIDE as i32 + 256;
                        (bounce(sx, t), bounce(sy, t * 3 / 4), 255)
                    }
                    _ => {
                        let p = t % 2000;
                        let a = if p < 1000 { p * 255 / 1000 } else { (2000 - p) * 255 / 1000 };
                        (((SCREEN_W - SIDE) / 2) as i32, ((SCREEN_H - SIDE) / 2) as i32, a as u8)
                    }
                };
                Overlay { address: px.as_ptr() as usize, stride: SIDE * 4, width: SIDE, height: SIDE, x, y, alpha }
            })
        }
        FULL => {
            if st.full.is_none() {
                st.full = full_picture();
                if let Some(px) = &st.full {
                    clean(px);
                }
            }
            st.full.as_ref().map(|px| Overlay {
                address: px.as_ptr() as usize,
                stride: SCREEN_W * 4,
                width: SCREEN_W,
                height: SCREEN_H,
                x: 0,
                y: 0,
                alpha: ALPHA.load(Ordering::Relaxed),
            })
        }
        _ => None,
    };
    if wanted == st.shown {
        return;
    }
    // The present (G2D interrupt, CPU0) writes SHADOWCON for window 0 too: no interrupt in
    // between this read-modify-write.
    let masked = cpu::mask_irq();
    let result = Fimd::new().set_overlay(WINDOW, wanted.as_ref());
    cpu::restore_irq(masked);
    match result {
        Ok(()) => st.shown = wanted,
        Err(e) => {
            exynos_4412_rs::println!("ferrite: overlay test: {:?}", e);
            MODE.store(OFF, Ordering::Release);
            st.shown = None;
        }
    }
}

/// For `/api/status`: the mode and where window 1 scans from now (0 = hidden or not applied).
pub fn status() -> (u8, u32) {
    (MODE.load(Ordering::Relaxed), Fimd::new().overlay_scan_address(WINDOW))
}
