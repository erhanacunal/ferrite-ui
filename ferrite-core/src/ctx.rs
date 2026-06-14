/// Shared application context — core resources passed to VM, render, and page functions.
///
/// Generic over `P: Platform` (ferrite-core): the hardware-backed fields use the
/// platform's associated backend types, so the framework is written once and a BSP
/// chooses concrete hardware by supplying `P`. Allocated on the heap via Box in main.
use crate::audio::AudioImpl;
use crate::backlight::BacklightImpl;
use crate::flash::FlashImpl;
use crate::lcd::LcdImpl;
use crate::platform::Platform;
use crate::rtc::RtcImpl;
use crate::systick::SystickImpl;
use crate::usart::UsartImpl;

use crate::font::FontList;
use crate::fs::Fs;
use crate::image::ImageList;
use crate::strpool::StringPool;
use crate::widget::WidgetTree;

pub struct Ctx<P: Platform> {
    pub lcd: LcdImpl<P::LcdB>,
    pub flash: FlashImpl<P::FlashB>,
    pub tree: WidgetTree,
    pub fonts: FontList,
    pub images: ImageList,
    pub strpool: StringPool,
    pub fs: Option<Fs>,
    pub backlight: BacklightImpl<P::BacklightB>,
    pub rtc: RtcImpl<P::RtcB>,
    pub usart: UsartImpl<P::UsartB>,
    pub systick: SystickImpl<P::SystickB>,
    pub audio: AudioImpl<P::AudioB>,
    pub cursor_visible: bool,
}
