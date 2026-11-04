//! Touch backend — Atmel mXT1664S (T9 multitouch) on I2C5.
//!
//! ferrite-core's input is single-pointer: the first finger down (lowest T9
//! id still down) is the pointer, the rest are ignored. Messages are drained
//! on every `is_active` poll while CHG is low.
//!
//! Positions come in T9's range (`x_range`/`y_range` from the controller) and
//! are scaled to the panel; `CalParams` only contributes swap/flip, so a
//! capacitive panel needs no calibration.
//!
//! TODO: multitouch — a `TouchBackend` extension (points[]) + VM gesture
//! callbacks (pinch/rotate) behind a ferrite-core feature.

use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::mxt::{self, Message, Mxt};
use exynos_4412_rs::drivers::timer::Mct;
use ferrite_core::touch::{CalParams, TouchBackend};

use crate::lcd::PANEL;

struct State {
    mxt: Mxt,
    x_range: u16,
    y_range: u16,
    /// Bit n = T9 touch n is down.
    down: u16,
    /// T9 id of the pointer finger, while one is down.
    pointer: Option<u8>,
    x: u16,
    y: u16,
}

static mut STATE: Option<State> = None;

fn state() -> Option<&'static mut State> {
    unsafe { (*(&raw mut STATE)).as_mut() }
}

impl State {
    fn drain(&mut self) {
        // CHG low = the controller holds messages.
        while !mxt::CHG.read() {
            match self.mxt.read_message() {
                Ok(Some(Message::Touch(t))) => {
                    let bit = 1u16 << (t.id & 15);
                    if t.down() {
                        self.down |= bit;
                        if self.pointer.is_none() {
                            self.pointer = Some(t.id);
                        }
                        if self.pointer == Some(t.id) {
                            self.x = t.x;
                            self.y = t.y;
                        }
                    } else {
                        self.down &= !bit;
                        if self.pointer == Some(t.id) {
                            self.pointer = None;
                        }
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    }
}

pub struct MxtTouch;

fn scale(v: u16, range: u16, size: usize) -> u16 {
    ((v as u32 * (size as u32 - 1)) / range.max(1) as u32).min(size as u32 - 1) as u16
}

impl TouchBackend for MxtTouch {
    fn is_active(&self) -> bool {
        let Some(s) = state() else { return false };
        s.drain();
        s.pointer.is_some()
    }

    fn read_screen(&self, cal: &CalParams) -> Option<(u16, u16)> {
        let s = state()?;
        s.drain();
        s.pointer?;
        let (mut rx, mut ry, mut xr, mut yr) = (s.x, s.y, s.x_range, s.y_range);
        if cal.xy_swap {
            core::mem::swap(&mut rx, &mut ry);
            core::mem::swap(&mut xr, &mut yr);
        }
        let mut x = scale(rx, xr, PANEL.width);
        let mut y = scale(ry, yr, PANEL.height);
        if cal.x_flip {
            x = PANEL.width as u16 - 1 - x;
        }
        if cal.y_flip {
            y = PANEL.height as u16 - 1 - y;
        }
        Some((x, y))
    }

    fn release_debounce(&self) -> u8 {
        // Releases are explicit T9 messages, not missing samples.
        1
    }
}

/// Identity mapping: the controller's T9 ORIENT already matches the panel.
/// Flip here if a board turns out mounted otherwise.
pub const CAL: CalParams = CalParams {
    xy_swap: false,
    x_flip: false,
    y_flip: false,
    x_min: 0,
    x_max: PANEL.width as u16,
    y_min: 0,
    y_max: PANEL.height as u16,
};

/// Powers the controller (PMIC on I2C0), reads its object table and makes it
/// report. On failure touch stays off; the UI still runs.
pub fn init(pmic: Max77686, mct: Mct) -> Result<(), mxt::Error> {
    mxt::power_up(pmic, mct)?;
    let mxt = Mxt::open(I2c::new(Bus::I2c5))?;
    mxt.wake()?;
    let cfg = mxt.touch_config()?;
    unsafe {
        *(&raw mut STATE) = Some(State {
            mxt,
            x_range: cfg.x_range,
            y_range: cfg.y_range,
            down: 0,
            pointer: None,
            x: 0,
            y: 0,
        });
    }
    Ok(())
}
