/// Platform trait — groups all hardware backend implementations for a BSP.
///
/// Each BSP crate defines a marker struct (e.g. `NextionPlatform`) and
/// implements this trait, mapping associated types to concrete backends.
///
/// The framework (`Ctx`, `Vm`, `render`, etc.) is then generic over
/// `P: Platform`, giving it access to all hardware through a single parameter.
use crate::backlight::BacklightBackend;
use crate::flash::FlashBackend;
use crate::lcd::LcdBackend;
use crate::rtc::RtcBackend;
use crate::sdcard::SdCardBackend;
use crate::systick::SystickBackend;
use crate::touch::TouchBackend;
use crate::usart::UsartBackend;

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
}
