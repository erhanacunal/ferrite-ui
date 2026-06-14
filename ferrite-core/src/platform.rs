/// Platform trait — groups all hardware backend implementations for a BSP.
///
/// Each BSP crate defines a marker struct (e.g. `NextionPlatform`) and
/// implements this trait, mapping associated types to concrete backends.
///
/// The framework (`Ctx`, `Vm`, `render`, etc.) is then generic over
/// `P: Platform`, giving it access to all hardware through a single parameter.
use crate::audio::AudioBackend;
use crate::backlight::BacklightBackend;
use crate::flash::FlashBackend;
use crate::lcd::LcdBackend;
use crate::rtc::RtcBackend;
use crate::sdcard::SdCardBackend;
use crate::systick::SystickBackend;
use crate::touch::TouchBackend;
use crate::usart::UsartBackend;

// --- Capability bits for `Platform::CAPS` ---
//
// Board-level optional features the framework can query at compile time
// (`P::CAPS & CAP_AUDIO != 0` folds to a constant). Alpha blending is NOT
// here — it lives on `LcdBackend::HAS_ALPHA`, next to the blend methods.
// Each bit must be mirrored in the board's `tools/devices/<id>.json`
// (`caps.audio` / `caps.video`) so the designer and build tools agree
// with the firmware.

/// Board has a real audio output backend (`Platform::AudioB` is not `NoAudio`).
pub const CAP_AUDIO: u32 = 1 << 0;
/// Reserved: board can stream video resources.
pub const CAP_VIDEO: u32 = 1 << 1;

pub trait Platform {
    type LcdB: LcdBackend;
    // `Clone` so the VM can take its own flash handle for flash-execution cache
    // refills (backends are stateless MMIO drivers — see `flash::FlashBackend`).
    type FlashB: FlashBackend + Clone;
    type TouchB: TouchBackend;
    type BacklightB: BacklightBackend;
    type RtcB: RtcBackend;
    type SdCardB: SdCardBackend;
    type UsartB: UsartBackend;
    type SystickB: SystickBackend;
    // Boards without audio hardware use `ferrite_core::audio::NoAudio`.
    type AudioB: AudioBackend;

    // --- Flash layout (Rust's stand-in for C-style per-board `#define`s) ---
    //
    // The resource flash is partitioned into a config sector and the resource
    // filesystem. Boards whose firmware lives on a *separate* chip (Nextion,
    // epaper, sim) keep the historical layout: config at sector 0, FS at
    // sector 1. Boards that boot from the *same* SPI NOR as the resources
    // (e.g. tdo_y13, where the F1C100s BootROM loads firmware from offset 0)
    // push both regions past the reserved firmware area by overriding these.

    /// Flash address of the persistent config store (one 4 KB sector).
    const CONFIG_BASE: u32 = crate::config::DEFAULT_CONFIG_BASE;

    /// Flash address where the resource filesystem (TOC + resources) begins —
    /// this is the destination `writefs` flashes to. Must leave a 4 KB sector
    /// below it for [`Platform::CONFIG_BASE`].
    const FS_BASE: u32 = crate::fs::DEFAULT_FS_BASE;

    /// Optional board capabilities — OR of the `CAP_*` bits above. Only set
    /// a bit when the corresponding backend is real (keep CAPS truthful).
    const CAPS: u32 = 0;
}
