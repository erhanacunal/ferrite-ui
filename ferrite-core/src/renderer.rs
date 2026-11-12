//! Per-board widget drawing.
//!
//! The render pipeline in [`crate::render`] (DFS order, clip regions,
//! occluders, restores) is shared by every board. Only
//! the last step, drawing one widget into its clip region, can be taken over
//! by a board: `Platform::Renderer` names a [`WidgetRenderer`], and
//! `render::draw_widget` asks it first. [`DefaultRenderer`] takes nothing
//! over, so a board that uses it draws exactly as before.
//!
//! Dispatch is static (`P::Renderer` is known at compile time): no vtable,
//! no state, and with `DefaultRenderer` the hook folds away.
//!
//! # Contract for an override
//!
//! The dirty renderer relies on how widgets draw. A `draw` that returns
//! `true` must:
//!
//! 1. Write only inside `clip` (a set of disjoint screen rects, already
//!    reduced by the occluders) and inside the widget's box `abs`. Restores,
//!    occlusion and the moved-subtree reuse depend on it. Confine primitives
//!    that take no clip with `ctx.lcd.set_scissor`, once per clip rect, and
//!    reset it to `None` afterwards.
//! 2. Paint every pixel of `abs` opaquely when the default drawing does: a
//!    widget with a background, square corners and full alpha
//!    (`render::covers_opaquely`) hides everything behind it, so nothing
//!    behind it is redrawn.
//! 3. Report through [`WidgetRenderer::shows_behind`] when its drawing lets
//!    what is behind show through (translucency, soft edges) and the default
//!    one does not, or the other way round. On readback backends what is
//!    behind is restored first only for those widgets; otherwise edge pixels
//!    blend over their own previous output.
//! 4. Be the same whatever the clip: a widget redrawn in pieces must end up
//!    as if drawn whole. Gauges redraw only the boxes a value change touches
//!    (`GaugeGeom::change_boxes`); an override whose gauge changes other
//!    pixels returns `false` from [`WidgetRenderer::partial_redraw`].
//!
//! [`crate::render::draw_default`] draws a widget the built-in way, for an
//! override that only adds to it or handles some cases of a kind.

use crate::clip::ClipRegion;
use crate::ctx::Ctx;
use crate::platform::Platform;
use crate::pool::ObjPool;
use crate::types::Rect;
use crate::widget::WidgetId;

/// How a board draws widgets. Every method has a default: an impl overrides
/// only what it changes. Methods take no `self` (renderers are zero-sized;
/// hardware state lives in the board's backends or statics).
pub trait WidgetRenderer<P: Platform> {
    /// Draw widget `id` (absolute box `abs`) into `clip`. Return `false` to
    /// leave it to the built-in drawing. See the module's contract.
    #[inline(always)]
    fn draw(_ctx: &Ctx<P>, _id: WidgetId, _abs: &Rect, _clip: &ClipRegion) -> bool {
        false
    }

    /// Whether what is behind `id` shows through it, if this renderer's
    /// drawing differs from the built-in one there (`None` = as built in).
    #[inline(always)]
    fn shows_behind(_objs: &ObjPool, _id: WidgetId) -> Option<bool> {
        None
    }

    /// Whether a gauge may be redrawn only where its value change touches.
    #[inline(always)]
    fn partial_redraw(_objs: &ObjPool, _id: WidgetId) -> bool {
        true
    }

}

/// The built-in drawing for every widget.
pub struct DefaultRenderer;

impl<P: Platform> WidgetRenderer<P> for DefaultRenderer {}

#[cfg(all(test, feature = "mock"))]
mod tests {
    use super::*;
    use crate::backlight::BacklightImpl;
    use crate::flash::FlashImpl;
    use crate::font::FontList;
    use crate::gpio::GpioImpl;
    use crate::image::ImageList;
    use crate::lcd::LcdImpl;
    use crate::mock::*;
    use crate::rtc::RtcImpl;
    use crate::systick::SystickImpl;
    use crate::types::{Offset, Size};
    use crate::usart::UsartImpl;
    use crate::widget::{KIND_GAUGE, KIND_LABEL};
    use alloc::vec::Vec;
    use core::cell::RefCell;

    std::thread_local! {
        /// (kind, area of the clip inside the widget's box) per `draw` call.
        static CALLS: RefCell<Vec<(u8, u32)>> = const { RefCell::new(Vec::new()) };
    }

    /// Takes gauges over (always redrawn whole), leaves the rest built in.
    struct GaugeRenderer;
    impl WidgetRenderer<TestPlatform> for GaugeRenderer {
        fn draw(ctx: &Ctx<TestPlatform>, id: WidgetId, abs: &Rect, clip: &ClipRegion) -> bool {
            let kind = ctx.objs.get(id).kind;
            let area = clip
                .iter()
                .iter()
                .filter_map(|r| r.intersection(abs))
                .map(|r| r.w as u32 * r.h as u32)
                .sum();
            CALLS.with(|c| c.borrow_mut().push((kind, area)));
            kind == KIND_GAUGE
        }
        fn partial_redraw(_: &ObjPool, _: WidgetId) -> bool {
            false
        }
    }

    struct TestPlatform;
    impl Platform for TestPlatform {
        type LcdB = MockLcd;
        type FlashB = MockFlash;
        type TouchB = MockTouch;
        type BacklightB = MockBacklight;
        type RtcB = MockRtc;
        type UsartB = MockUsart;
        type SystickB = MockSystick;
        #[cfg(feature = "audio")]
        type AudioB = crate::audio::NoAudio;
        #[cfg(feature = "jpeg")]
        type JpegB = crate::jpeg::NoJpeg;
        type GpioB = MockGpio;
        type EepromB = MockEeprom;
        type Renderer = GaugeRenderer;
    }

    fn ctx() -> Ctx<TestPlatform> {
        Ctx {
            lcd: LcdImpl::with_backend(MockLcd),
            flash: FlashImpl::with_backend(MockFlash),
            fonts: FontList::new(),
            images: ImageList::new(),
            objs: ObjPool::new(),
            fs: None,
            backlight: BacklightImpl::with_backend(MockBacklight),
            rtc: RtcImpl::with_backend(MockRtc),
            usart: UsartImpl::with_backend(MockUsart),
            systick: SystickImpl::init(),
            #[cfg(feature = "audio")]
            audio: crate::audio::AudioImpl::none(),
            #[cfg(feature = "jpeg")]
            jpeg: crate::jpeg::JpegImpl::none(),
            gpio: GpioImpl::with_backend(MockGpio::default()),
            eeprom: crate::eeprom::EepromImpl::with_backend(MockEeprom::default()),
            cursor_visible: false,
        }
    }

    fn take() -> Vec<(u8, u32)> {
        CALLS.with(|c| core::mem::take(&mut *c.borrow_mut()))
    }

    #[test]
    fn board_renderer_is_asked_for_every_widget_drawn() {
        let mut ctx = ctx();
        let root = ctx.objs.alloc_widget().unwrap();
        ctx.objs.root = root;
        ctx.objs.get_mut(root).size = Size { w: 800, h: 480 };
        let gauge = ctx.objs.alloc_widget().unwrap();
        ctx.objs.add_child(root, gauge);
        let g = ctx.objs.get_mut(gauge);
        g.kind = KIND_GAUGE;
        g.location = Offset { x: 10, y: 10 };
        g.size = Size { w: 100, h: 100 };
        let label = ctx.objs.alloc_widget().unwrap();
        ctx.objs.add_child(root, label);
        let l = ctx.objs.get_mut(label);
        l.kind = KIND_LABEL;
        l.location = Offset { x: 200, y: 10 };
        l.size = Size { w: 50, h: 20 };
        l.background_color = 0x1234;

        crate::render::render_dirty(&mut ctx);
        let first = take();
        assert_eq!(first.len(), 3, "root, gauge and label: {first:?}");
        assert!(first.contains(&(KIND_GAUGE, 100 * 100)));

        // A value change redraws the whole gauge: `partial_redraw` is false.
        ctx.objs.get_mut(gauge).value = 60;
        ctx.objs.mark_dirty(gauge);
        crate::render::render_dirty(&mut ctx);
        assert_eq!(take(), [(KIND_GAUGE, 100 * 100)]);
    }
}
