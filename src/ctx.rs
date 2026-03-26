/// Shared application context — core resources passed to VM, render, and page functions.
///
/// Avoids threading 6+ individual parameters through every call chain.
/// Allocated on the heap via Box in main.

use crate::flash::Flash;
use crate::font::FontList;
use crate::fs::Fs;
use crate::image::ImageList;
use crate::lcd::Lcd;
use crate::strpool::StringPool;
use crate::widget::WidgetTree;

pub struct Ctx {
    pub lcd: Lcd,
    pub flash: Flash,
    pub tree: WidgetTree,
    pub fonts: FontList,
    pub images: ImageList,
    pub strpool: StringPool,
    pub fs: Option<Fs>,
}
