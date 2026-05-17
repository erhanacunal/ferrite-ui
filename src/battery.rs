//! Battery voltage reader for the LilyGo T5-ePaper-S3.
//!
//! Pin assignment:
//!   GPIO14 = Battery ADC input (divided by 2 on-board resistor divider)
//!
//! Source: adapted from lilygo-epd47-rs/src/battery.rs

#![cfg(feature = "epaper")]

use core::cell::RefCell;

use critical_section::Mutex;
use esp_hal::{
    Blocking,
    analog::adc::{Adc, AdcCalCurve, AdcConfig, AdcPin, Attenuation},
    peripherals::{ADC2, GPIO14},
};

// --- Battery driver ---

pub struct Battery {
    adc: Adc<'static, ADC2<'static>, Blocking>,
    adc_pin: AdcPin<GPIO14<'static>, ADC2<'static>, AdcCalCurve<ADC2<'static>>>,
}

impl Battery {
    fn new(pin: GPIO14<'static>, adc: ADC2<'static>) -> Self {
        let mut config = AdcConfig::new();
        let adc_pin = config.enable_pin_with_cal(pin, Attenuation::_11dB);
        Self {
            adc: Adc::new(adc, config),
            adc_pin,
        }
    }

    /// Read battery voltage in millivolts.
    /// The on-board resistor divider halves the battery voltage before the ADC pin,
    /// so the raw ADC reading (in mV) is multiplied by 2.
    fn read_mv(&mut self) -> i32 {
        let raw = nb::block!(self.adc.read_oneshot(&mut self.adc_pin)).unwrap_or(0);
        (raw as i32) * 2
    }
}

// Battery is used only from a single thread (no RTOS), safe to mark Send.
unsafe impl Send for Battery {}

// --- Global instance ---

static BATTERY: Mutex<RefCell<Option<Battery>>> = Mutex::new(RefCell::new(None));

/// Install the battery ADC. Call once at startup before any `read_mv()` calls.
pub fn init(pin: GPIO14<'static>, adc: ADC2<'static>) {
    critical_section::with(|cs| {
        BATTERY.borrow_ref_mut(cs).replace(Battery::new(pin, adc));
    });
}

/// Read battery voltage in millivolts. Returns `None` if not initialized.
pub fn read_mv() -> Option<i32> {
    critical_section::with(|cs| {
        BATTERY.borrow_ref_mut(cs).as_mut().map(|b| b.read_mv())
    })
}
