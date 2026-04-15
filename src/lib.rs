#![cfg_attr(not(feature = "host"), no_std)]

#[cfg(feature = "host")]
extern crate std;

#[cfg(feature = "host")]
pub mod lcd;

#[cfg(feature = "host")]
pub mod flash;

#[cfg(feature = "host")]
pub mod touch;

#[cfg(feature = "host")]
pub mod backlight;

#[cfg(feature = "host")]
pub mod rtc;

#[cfg(feature = "host")]
pub mod config;

#[cfg(feature = "host")]
pub mod usart;

#[cfg(feature = "host")]
pub mod sdcard;

#[cfg(feature = "host")]
pub mod systick;

// --- Rendering stack (host build) ---

#[cfg(feature = "host")]
pub mod types;

#[cfg(feature = "host")]
pub mod clip;

#[cfg(feature = "host")]
pub mod proto;

#[cfg(feature = "host")]
pub mod widget;

#[cfg(feature = "host")]
pub mod strpool;

#[cfg(feature = "host")]
pub mod fs;

#[cfg(feature = "host")]
pub mod embedded_font;

#[cfg(feature = "host")]
pub mod font;

#[cfg(feature = "host")]
pub mod image;

#[cfg(feature = "host")]
pub mod render;

#[cfg(feature = "host")]
pub mod ctx;

#[cfg(feature = "host")]
pub mod vm;
