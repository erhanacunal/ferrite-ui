extern crate alloc;

use crate::pool::{ObjPool, T_WIDGET};
use crate::render::SCROLLBAR_W;
use crate::types::{Color, Edges, Offset, Rect, Size};
use alloc::vec::Vec;
use core::mem::{align_of, size_of};

// --- Flags (u16) ---

pub const FLAG_VISIBLE: u16 = 1 << 0;
pub const FLAG_ENABLED: u16 = 1 << 1;
pub const FLAG_CLICKABLE: u16 = 1 << 2;
pub const FLAG_DIRTY_A: u16 = 1 << 3;
pub const FLAG_PRESSED: u16 = 1 << 4;
pub const FLAG_CHECKED: u16 = 1 << 5;
pub const FLAG_FOCUSED: u16 = 1 << 6;
pub const FLAG_CLIP_CHILDREN: u16 = 1 << 7;
pub const FLAG_DIRTY_B: u16 = 1 << 8;

/// Combined mask: dirty in both buffers. Used by mark_dirty().
pub const FLAG_DIRTY_AB: u16 = FLAG_DIRTY_A | FLAG_DIRTY_B;

/// Set when the widget has been drawn at least once since its last erase.
/// Used by `render_dirty`'s erase pass to skip widgets that were never
/// rendered — prevents "phantom erase" of animated-but-invisible widgets
/// (e.g. a progress bar on a hidden tab that keeps marking itself dirty).
pub const FLAG_RENDERED: u16 = 1 << 9;

/// Set on the placeholder that `get` / `get_mut` return for an id that names
/// no live widget (destroyed, or another object type). Treated as
/// not-visible by `is_tree_visible`.
pub const FLAG_DESTROYED: u16 = 1 << 10;

/// Label text is rendered as multiple lines split by '\n'.
pub const FLAG_MULTI_LINE: u16 = 1 << 11;

/// Widget has at least one compile-time on_click registration (generated dispatcher).
pub const FLAG_HAS_ON_CLICK: u16 = 1 << 12;
/// Widget has at least one compile-time on_tap / on_change registration.
pub const FLAG_HAS_ON_TAP: u16 = 1 << 13;
/// Widget has at least one compile-time on_paint registration.
pub const FLAG_HAS_ON_PAINT: u16 = 1 << 14;
/// Render-pass scratch: the widget was painted earlier in the current render
/// pass. Set and cleared by the render functions only — always clear outside
/// a pass.
pub const FLAG_PAINTED: u16 = 1 << 15;

// --- Widget Kind ---

pub const KIND_BASE: u8 = 0;
pub const KIND_LABEL: u8 = 1;
pub const KIND_BUTTON: u8 = 2;
pub const KIND_PROGRESS: u8 = 3;
pub const KIND_SLIDER: u8 = 4;
pub const KIND_CHECKBOX: u8 = 5;
pub const KIND_RADIO: u8 = 6;
pub const KIND_INPUT: u8 = 7;
pub const KIND_GAUGE: u8 = 8;
pub const KIND_DROPDOWN: u8 = 9;
pub const KIND_GRAPH: u8 = 10;
pub const KIND_IMAGE: u8 = 11;
// Shape widgets — fill = background_color, stroke = border_color, stroke
// thickness = border width. KIND_POLYGON reads its points from `text`.
pub const KIND_RECTANGLE: u8 = 12;
pub const KIND_CIRCLE: u8 = 13;
pub const KIND_POLYGON: u8 = 14;
pub const KIND_LINE: u8 = 15;
pub const KIND_ELLIPSE: u8 = 16;
/// Toggle button in an exclusive group: widgets of this kind with the same
/// `group_id` form one group, and at most one of them is CHECKED (drawn with
/// `press_color`). See `ObjPool::select_in_group`.
pub const KIND_BUTTON_GROUP: u8 = 17;

// --- Text Alignment ---

pub const ALIGN_LEFT: u8 = 0;
pub const ALIGN_CENTER: u8 = 1;
pub const ALIGN_RIGHT: u8 = 2;

// --- WidgetId ---

/// Widget pointer (an `ObjPtr` naming a `T_WIDGET` object). NONE = no link.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct WidgetId(pub u16);

impl WidgetId {
    pub const NONE: Self = Self(0xFFFF);

    pub fn index(self) -> usize {
        self.0 as usize
    }

    pub fn is_none(self) -> bool {
        self.0 == 0xFFFF
    }

    pub fn is_some(self) -> bool {
        self.0 != 0xFFFF
    }
}

// --- Widget object layout ---
//
// A widget is one pool object: the `Widget` header (6 words) followed by the
// optional sections its `sections` mask names, in bit order. A section is
// added (the object grows) the first time one of its fields gets a non-default
// value, so a plain container is 6 words and a label 8.

/// Section bits (`Widget::sections`) and their sizes in words.
const SEC_BOX: u8 = 1 << 0;
const SEC_TEXT: u8 = 1 << 1;
const SEC_STYLE: u8 = 1 << 2;
const SEC_EXTRA: u8 = 1 << 3;
const SEC_PREV: u8 = 1 << 4;
const SEC_WORDS: [usize; 5] = [2, 2, 2, 2, 4];

/// Header words of every widget object.
const HDR_WORDS: usize = size_of::<Widget>() / 4;

/// Widget header — tree links, layout, appearance and the common `value`.
/// Lives in the pool arena; `get` / `get_mut` return references into it.
#[repr(C)]
pub struct Widget {
    // Tree links (6 bytes)
    pub parent: WidgetId,
    pub first_child: WidgetId,
    pub next_sibling: WidgetId,

    pub kind: u8,
    /// Present sections (`SEC_*`).
    sections: u8,

    // State -- u16 for dual-buffer dirty tracking
    pub flags: u16,

    // Position and size
    pub location: Offset,
    pub size: Size,

    // Appearance
    pub background_color: Color,
    pub border_color: Color,

    /// Progress/slider value, input cursor, scroll offset, graph array id.
    pub value: i16,
}

const _: () = assert!(size_of::<Widget>() == 24 && align_of::<Widget>() <= 4);

impl Widget {
    /// Placeholder for an id that names no live widget.
    pub const DEAD: Self = Self {
        parent: WidgetId::NONE,
        first_child: WidgetId::NONE,
        next_sibling: WidgetId::NONE,
        kind: KIND_BASE,
        sections: 0,
        flags: FLAG_DESTROYED,
        location: Offset { x: 0, y: 0 },
        size: Size { w: 0, h: 0 },
        background_color: 0,
        border_color: 0,
        value: 0,
    };

    const NEW: Self = Self {
        flags: FLAG_VISIBLE | FLAG_ENABLED | FLAG_DIRTY_AB,
        ..Self::DEAD
    };

    pub fn is_visible(&self) -> bool {
        self.flags & FLAG_VISIBLE != 0
    }

    /// Check if dirty in buffer A (used by dirty render mode).
    pub fn is_dirty(&self) -> bool {
        self.flags & FLAG_DIRTY_A != 0
    }

    /// Check if dirty in a specific buffer (0=A, 1=B).
    pub fn is_dirty_buf(&self, buf: u8) -> bool {
        let mask = if buf == 0 { FLAG_DIRTY_A } else { FLAG_DIRTY_B };
        self.flags & mask != 0
    }

    /// Mark dirty in both buffers (used when widget content changes).
    pub fn mark_dirty(&mut self) {
        self.flags |= FLAG_DIRTY_AB;
    }

    /// Clear dirty flag for buffer A only (dirty render mode).
    pub fn clear_dirty(&mut self) {
        self.flags &= !FLAG_DIRTY_A;
    }

    /// Clear dirty flag for a specific buffer (0=A, 1=B).
    pub fn clear_dirty_buf(&mut self, buf: u8) {
        let mask = if buf == 0 { FLAG_DIRTY_A } else { FLAG_DIRTY_B };
        self.flags &= !mask;
    }
}

static DEAD_WIDGET: Widget = Widget::DEAD;

/// A widget section: a fixed-size group of optional fields.
trait Section: Copy + PartialEq {
    const BIT: u8;
    const DEFAULT: Self;
    fn read(e: &WidgetExt) -> Self;
    fn write(&self, e: &mut WidgetExt);
}

#[repr(C, align(4))]
#[derive(Clone, Copy, PartialEq)]
struct SecBox {
    border: Edges,
    padding: Edges,
}

#[repr(C, align(4))]
#[derive(Clone, Copy, PartialEq)]
struct SecText {
    text_id: u16,
    text_color: Color,
    font_id: u8,
    text_align: u8,
    max_length: u8,
    _pad: u8,
}

#[repr(C, align(4))]
#[derive(Clone, Copy, PartialEq)]
struct SecStyle {
    border_radius: u16,
    press_color: Color,
    gradient_color: Color,
    gradient_dir: u8,
    alpha: u8,
}

#[repr(C, align(4))]
#[derive(Clone, Copy, PartialEq)]
struct SecExtra {
    image_id: u8,
    group_id: u8,
    flex: u8,
    flex_gap: u8,
    flex_grow: u8,
    _pad: [u8; 3],
}

#[repr(C, align(4))]
#[derive(Clone, Copy, PartialEq)]
struct SecPrev {
    a: Rect,
    b: Rect,
}

const _: () = assert!(
    size_of::<SecBox>() == 8
        && size_of::<SecText>() == 8
        && size_of::<SecStyle>() == 8
        && size_of::<SecExtra>() == 8
        && size_of::<SecPrev>() == 16
);

impl Section for SecBox {
    const BIT: u8 = SEC_BOX;
    const DEFAULT: Self = Self { border: Edges::ZERO, padding: Edges::ZERO };
    fn read(e: &WidgetExt) -> Self {
        Self { border: e.border, padding: e.padding }
    }
    fn write(&self, e: &mut WidgetExt) {
        e.border = self.border;
        e.padding = self.padding;
    }
}

impl Section for SecText {
    const BIT: u8 = SEC_TEXT;
    const DEFAULT: Self = Self {
        text_id: 0xFFFF,
        text_color: 0xFFFF,
        font_id: 0,
        text_align: ALIGN_LEFT,
        max_length: 0,
        _pad: 0,
    };
    fn read(e: &WidgetExt) -> Self {
        Self {
            text_id: e.text_id,
            text_color: e.text_color,
            font_id: e.font_id,
            text_align: e.text_align,
            max_length: e.max_length,
            _pad: 0,
        }
    }
    fn write(&self, e: &mut WidgetExt) {
        e.text_id = self.text_id;
        e.text_color = self.text_color;
        e.font_id = self.font_id;
        e.text_align = self.text_align;
        e.max_length = self.max_length;
    }
}

impl Section for SecStyle {
    const BIT: u8 = SEC_STYLE;
    const DEFAULT: Self = Self {
        border_radius: 0,
        press_color: 0,
        gradient_color: 0,
        gradient_dir: 0,
        alpha: 255,
    };
    fn read(e: &WidgetExt) -> Self {
        Self {
            border_radius: e.border_radius,
            press_color: e.press_color,
            gradient_color: e.gradient_color,
            gradient_dir: e.gradient_dir,
            alpha: e.alpha,
        }
    }
    fn write(&self, e: &mut WidgetExt) {
        e.border_radius = self.border_radius;
        e.press_color = self.press_color;
        e.gradient_color = self.gradient_color;
        e.gradient_dir = self.gradient_dir;
        e.alpha = self.alpha;
    }
}

impl Section for SecExtra {
    const BIT: u8 = SEC_EXTRA;
    const DEFAULT: Self = Self {
        image_id: 0,
        group_id: 0,
        flex: 0,
        flex_gap: 0,
        flex_grow: 0,
        _pad: [0; 3],
    };
    fn read(e: &WidgetExt) -> Self {
        Self {
            image_id: e.image_id,
            group_id: e.group_id,
            flex: e.flex,
            flex_gap: e.flex_gap,
            flex_grow: e.flex_grow,
            _pad: [0; 3],
        }
    }
    fn write(&self, e: &mut WidgetExt) {
        e.image_id = self.image_id;
        e.group_id = self.group_id;
        e.flex = self.flex;
        e.flex_gap = self.flex_gap;
        e.flex_grow = self.flex_grow;
    }
}

impl Section for SecPrev {
    const BIT: u8 = SEC_PREV;
    const DEFAULT: Self = Self { a: Rect::new(0, 0, 0, 0), b: Rect::new(0, 0, 0, 0) };
    fn read(e: &WidgetExt) -> Self {
        Self { a: e.prev_rect_a, b: e.prev_rect_b }
    }
    fn write(&self, e: &mut WidgetExt) {
        e.prev_rect_a = self.a;
        e.prev_rect_b = self.b;
    }
}

/// Word offset of section `bit` inside a widget payload with mask `mask`.
fn sec_offset(mask: u8, bit: u8) -> usize {
    let mut off = HDR_WORDS;
    for (i, w) in SEC_WORDS.iter().enumerate() {
        if 1 << i == bit {
            break;
        }
        if mask & (1 << i) != 0 {
            off += w;
        }
    }
    off
}

fn sec_words(bit: u8) -> usize {
    SEC_WORDS[bit.trailing_zeros() as usize]
}

/// View `T` at the start of `w`.
#[inline]
fn cast<T>(w: &[u32]) -> &T {
    assert!(w.len() * 4 >= size_of::<T>() && align_of::<T>() <= 4);
    // SAFETY: the slice is word aligned, long enough, and every bit pattern
    // is valid for the plain-integer header and section types.
    unsafe { &*(w.as_ptr() as *const T) }
}

#[inline]
fn cast_mut<T>(w: &mut [u32]) -> &mut T {
    assert!(w.len() * 4 >= size_of::<T>() && align_of::<T>() <= 4);
    // SAFETY: as in `cast`; the borrow is unique.
    unsafe { &mut *(w.as_mut_ptr() as *mut T) }
}

/// The words of a section value.
#[inline]
fn words<T: Section>(t: &T) -> &[u32] {
    const { assert!(size_of::<T>() % 4 == 0 && align_of::<T>() == 4) };
    // SAFETY: sections are padding-free, word-aligned `repr(C)` structs of
    // integers whose size is a multiple of 4; reading them as words is valid.
    unsafe { core::slice::from_raw_parts(t as *const T as *const u32, size_of::<T>() / 4) }
}

// --- WidgetExt ---

/// All optional widget fields, with their defaults. Not stored: it is a value
/// view assembled from the widget's sections by `ObjPool::ext` and written
/// back by `ObjPool::update_ext`, which adds only the sections whose fields
/// differ from the defaults.
#[derive(Clone, Copy)]
pub struct WidgetExt {
    pub group_id: u8, // KIND_BUTTON_GROUP: exclusive selection group
    pub flex: u8,      // lib/flex.fl container mode (0 = not a flex container)
    pub flex_gap: u8,  // lib/flex.fl gap between children, px
    pub flex_grow: u8, // lib/flex.fl grow factor of a child (0 = fixed size)

    // Box model edges
    pub border: Edges,
    pub padding: Edges,

    // Label fields
    pub text_color: Color,
    pub font_id: u8,
    pub text_align: u8,
    pub text_id: u16,

    // Appearance extras
    pub border_radius: u16,
    pub press_color: Color,

    // Image + max input length
    pub image_id: u8,
    pub max_length: u8,

    /// Mirrors `Widget::value` (kept here so drawing code reads one struct).
    pub value: i16,

    // Gradient
    pub gradient_color: Color, // End color (background_color is start)
    pub gradient_dir: u8,      // 0=none, 1=horizontal (L→R), 2=vertical (T→B)

    // Background opacity, 255 = opaque. Only honored on
    // LcdBackend::HAS_ALPHA devices; others draw the background solid.
    pub alpha: u8,

    // Rect at which the widget was last drawn to buffers A and B.
    // The renderer compares each against the current absolute_rect and, when
    // they differ, redraws the widgets behind `prev_rect_X` clipped to that
    // area before drawing the widget at its new rect. Per-buffer tracking
    // avoids an ever-growing union under continuous motion.
    pub prev_rect_a: Rect,
    pub prev_rect_b: Rect,
}

impl WidgetExt {
    /// All fields at their defaults (what a widget without sections reads).
    pub const DEFAULT: Self = Self {
        group_id: 0,
        flex: 0,
        flex_gap: 0,
        flex_grow: 0,
        border: Edges::ZERO,
        padding: Edges::ZERO,
        text_color: 0xFFFF,
        font_id: 0,
        text_align: ALIGN_LEFT,
        text_id: 0xFFFF,
        border_radius: 0,
        press_color: 0,
        image_id: 0,
        max_length: 0,
        value: 0,
        gradient_color: 0,
        gradient_dir: 0,
        alpha: 255,
        prev_rect_a: Rect::new(0, 0, 0, 0),
        prev_rect_b: Rect::new(0, 0, 0, 0),
    };
}

// --- Widget tree (on the object pool) ---

impl ObjPool {
    /// Allocate a new widget (FIFO pointer reuse). Returns None when the pool
    /// is out of pointers or arena space.
    pub fn alloc_widget(&mut self) -> Option<WidgetId> {
        let p = self.alloc_obj(T_WIDGET, HDR_WORDS)?;
        *cast_mut::<Widget>(self.payload_mut(p, T_WIDGET)?) = Widget::NEW;
        self.dfs_valid = false;
        Some(WidgetId(p))
    }

    /// Widget header. An id that names no live widget reads as a destroyed,
    /// invisible placeholder.
    pub fn get(&self, id: WidgetId) -> &Widget {
        match self.payload(id.0, T_WIDGET) {
            Some(w) => cast(w),
            None => &DEAD_WIDGET,
        }
    }

    /// Mutable widget header. Writes through an id that names no live widget
    /// go to a scratch placeholder and are lost.
    pub fn get_mut(&mut self, id: WidgetId) -> &mut Widget {
        if self.block(id.0, T_WIDGET).is_some() {
            return cast_mut(self.payload_mut(id.0, T_WIDGET).unwrap());
        }
        self.dead = Widget::DEAD;
        &mut self.dead
    }

    /// Pointer high-water mark: an upper bound on the widget count, used as a
    /// loop guard for tree walks.
    pub fn count(&self) -> usize {
        self.ptr_count()
    }

    /// Live widgets.
    pub fn widget_count(&self) -> usize {
        self.live_objects(T_WIDGET)
    }

    // --- Sections ---

    // The section logic works on word slices so that it exists once, not once
    // per section type; the typed wrappers only cast.

    /// Words of section `bit` of `id`, if present.
    #[inline(never)]
    fn sec_raw(&self, id: WidgetId, bit: u8) -> Option<&[u32]> {
        let w = self.payload(id.0, T_WIDGET)?;
        let mask = cast::<Widget>(w).sections;
        if mask & bit == 0 {
            return None;
        }
        let off = sec_offset(mask, bit);
        Some(&w[off..off + sec_words(bit)])
    }

    #[inline(never)]
    fn sec_raw_mut(&mut self, id: WidgetId, bit: u8) -> Option<&mut [u32]> {
        let w = self.payload_mut(id.0, T_WIDGET)?;
        let mask = cast::<Widget>(w).sections;
        if mask & bit == 0 {
            return None;
        }
        let off = sec_offset(mask, bit);
        Some(&mut w[off..off + sec_words(bit)])
    }

    /// Section `bit` of `id`, added as `default` if missing (the object grows
    /// and may move). None when `id` is no widget or the pool is full.
    #[inline(never)]
    fn ensure_sec_raw(&mut self, id: WidgetId, bit: u8, default: &[u32]) -> Option<&mut [u32]> {
        let w = self.payload(id.0, T_WIDGET)?;
        let mask = cast::<Widget>(w).sections;
        if mask & bit == 0 {
            let old = w.len();
            let add = sec_words(bit);
            if !self.resize_obj(id.0, T_WIDGET, old + add) {
                return None;
            }
            let w = self.payload_mut(id.0, T_WIDGET)?;
            let off = sec_offset(mask, bit);
            // Open a gap for the section; later sections move up.
            w.copy_within(off..old, off + add);
            w[off..off + add].copy_from_slice(default);
            cast_mut::<Widget>(w).sections = mask | bit;
        }
        self.sec_raw_mut(id, bit)
    }

    /// Write `s` into section `bit` of `id`: in place if present, else add it
    /// unless `s` equals `default`. False when the pool is full.
    #[inline(never)]
    fn put_sec_raw(&mut self, id: WidgetId, bit: u8, s: &[u32], default: &[u32]) -> bool {
        if self.sec_raw(id, bit).is_none() && s == default {
            return true;
        }
        match self.ensure_sec_raw(id, bit, default) {
            Some(cur) => {
                cur.copy_from_slice(s);
                true
            }
            None => false,
        }
    }

    fn sec<T: Section>(&self, id: WidgetId) -> Option<&T> {
        self.sec_raw(id, T::BIT).map(cast)
    }

    fn sec_mut<T: Section>(&mut self, id: WidgetId) -> Option<&mut T> {
        self.sec_raw_mut(id, T::BIT).map(cast_mut)
    }

    fn ensure_sec<T: Section>(&mut self, id: WidgetId) -> Option<&mut T> {
        self.ensure_sec_raw(id, T::BIT, words(&T::DEFAULT)).map(cast_mut)
    }

    fn put_sec<T: Section>(&mut self, id: WidgetId, s: T) -> bool {
        self.put_sec_raw(id, T::BIT, words(&s), words(&T::DEFAULT))
    }

    /// All optional fields of `id` (defaults for missing sections or ids).
    pub fn ext(&self, id: WidgetId) -> WidgetExt {
        let mut e = WidgetExt::DEFAULT;
        if self.block(id.0, T_WIDGET).is_none() {
            return e;
        }
        e.value = self.get(id).value;
        if let Some(s) = self.sec::<SecBox>(id) {
            s.write(&mut e);
        }
        if let Some(s) = self.sec::<SecText>(id) {
            s.write(&mut e);
        }
        if let Some(s) = self.sec::<SecStyle>(id) {
            s.write(&mut e);
        }
        if let Some(s) = self.sec::<SecExtra>(id) {
            s.write(&mut e);
        }
        if let Some(s) = self.sec::<SecPrev>(id) {
            s.write(&mut e);
        }
        e
    }

    /// Store every field of `e` into `id`, adding the sections that now hold
    /// non-default values. False when `id` is no widget or the pool ran out
    /// of space (fields that fit are still written).
    #[inline(never)]
    pub fn put_ext(&mut self, id: WidgetId, e: &WidgetExt) -> bool {
        if self.block(id.0, T_WIDGET).is_none() {
            return false;
        }
        self.get_mut(id).value = e.value;
        let mut ok = self.put_sec(id, SecBox::read(e));
        ok &= self.put_sec(id, SecText::read(e));
        ok &= self.put_sec(id, SecStyle::read(e));
        ok &= self.put_sec(id, SecExtra::read(e));
        ok &= self.put_sec(id, SecPrev::read(e));
        ok
    }

    /// Read-modify-write the optional fields of `id`. False when `id` is no
    /// widget or a needed section did not fit.
    pub fn update_ext(&mut self, id: WidgetId, f: impl FnOnce(&mut WidgetExt)) -> bool {
        let mut e = self.ext(id);
        f(&mut e);
        self.put_ext(id, &e)
    }

    // --- Convenience accessors (defaults when the section is missing) ---

    pub fn border(&self, id: WidgetId) -> Edges {
        self.sec::<SecBox>(id).map_or(Edges::ZERO, |s| s.border)
    }

    pub fn padding(&self, id: WidgetId) -> Edges {
        self.sec::<SecBox>(id).map_or(Edges::ZERO, |s| s.padding)
    }

    pub fn text_color(&self, id: WidgetId) -> Color {
        self.sec::<SecText>(id).map_or(0xFFFF, |s| s.text_color)
    }

    pub fn font_id(&self, id: WidgetId) -> u8 {
        self.sec::<SecText>(id).map_or(0, |s| s.font_id)
    }

    pub fn text_align(&self, id: WidgetId) -> u8 {
        self.sec::<SecText>(id).map_or(ALIGN_LEFT, |s| s.text_align)
    }

    pub fn text_id(&self, id: WidgetId) -> u16 {
        self.sec::<SecText>(id).map_or(0xFFFF, |s| s.text_id)
    }

    /// Set the widget's text string (no-op when `id` is no widget).
    pub fn set_text_id(&mut self, id: WidgetId, text: u16) -> bool {
        match self.sec_mut::<SecText>(id) {
            Some(s) => {
                s.text_id = text;
                true
            }
            None => self.update_ext(id, |e| e.text_id = text),
        }
    }

    pub fn border_radius(&self, id: WidgetId) -> u16 {
        self.sec::<SecStyle>(id).map_or(0, |s| s.border_radius)
    }

    pub fn press_color(&self, id: WidgetId) -> Color {
        self.sec::<SecStyle>(id).map_or(0, |s| s.press_color)
    }

    pub fn image_id(&self, id: WidgetId) -> u8 {
        self.sec::<SecExtra>(id).map_or(0, |s| s.image_id)
    }

    pub fn max_length(&self, id: WidgetId) -> u8 {
        self.sec::<SecText>(id).map_or(0, |s| s.max_length)
    }

    pub fn value(&self, id: WidgetId) -> i16 {
        self.get(id).value
    }

    pub fn gradient_color(&self, id: WidgetId) -> Color {
        self.sec::<SecStyle>(id).map_or(0, |s| s.gradient_color)
    }

    pub fn gradient_dir(&self, id: WidgetId) -> u8 {
        self.sec::<SecStyle>(id).map_or(0, |s| s.gradient_dir)
    }

    pub fn alpha(&self, id: WidgetId) -> u8 {
        self.sec::<SecStyle>(id).map_or(255, |s| s.alpha)
    }

    /// Selection group of a KIND_BUTTON_GROUP / KIND_RADIO widget.
    pub fn group_id(&self, id: WidgetId) -> u8 {
        self.sec::<SecExtra>(id).map_or(0, |s| s.group_id)
    }

    /// Rect at which `id` was last drawn to buffer `buf` (0=A, 1=B).
    pub fn prev_rect(&self, id: WidgetId, buf: u8) -> Rect {
        match self.sec::<SecPrev>(id) {
            Some(s) if buf == 0 => s.a,
            Some(s) => s.b,
            None => Rect::new(0, 0, 0, 0),
        }
    }

    /// Store the drawn rect for buffer `buf`. Adds the section on demand; an
    /// empty rect on a widget without one is not stored.
    pub fn set_prev_rect(&mut self, id: WidgetId, buf: u8, rect: Rect) {
        if rect.is_empty() && self.sec::<SecPrev>(id).is_none() {
            return;
        }
        if let Some(s) = self.ensure_sec::<SecPrev>(id) {
            if buf == 0 {
                s.a = rect;
            } else {
                s.b = rect;
            }
        }
    }

    // --- Tree operations ---

    /// Add child to end of parent's child list (highest z-order).
    pub fn add_child(&mut self, parent: WidgetId, child: WidgetId) {
        if parent == child || !self.is_live(parent) || !self.is_live(child) {
            return;
        }
        self.get_mut(child).parent = parent;
        self.get_mut(child).next_sibling = WidgetId::NONE;

        let first = self.get(parent).first_child;
        if first.is_none() {
            self.get_mut(parent).first_child = child;
        } else {
            let max = self.count();
            let mut last = first;
            let mut guard = 0usize;
            while self.get(last).next_sibling.is_some() {
                let next = self.get(last).next_sibling;
                if next == child {
                    return;
                }
                last = next;
                guard += 1;
                if guard > max {
                    return;
                }
            }
            self.get_mut(last).next_sibling = child;
        }
        self.dfs_valid = false;
    }

    /// Compute widget's absolute border-box rect in screen coordinates.
    /// If an ancestor has FLAG_CLIP_CHILDREN, children are offset by
    /// that ancestor's scroll_y (stored in `value`).
    pub fn absolute_rect(&self, id: WidgetId) -> Rect {
        let w = self.get(id);
        let mut x = w.location.x;
        let mut y = w.location.y;

        let max = self.count();
        let mut depth = 0usize;
        let mut pid = w.parent;
        while pid.is_some() {
            depth += 1;
            if depth > max {
                break;
            }
            let p = self.get(pid);
            if p.kind == KIND_DROPDOWN {
                x += p.location.x;
                y += p.location.y;
            } else {
                let pb = self.border(pid);
                let pp = self.padding(pid);
                x += p.location.x + pb.left as i16 + pp.left as i16;
                y += p.location.y + pb.top as i16 + pp.top as i16;
            }
            // Scroll offset: shift children by -scroll_y
            if p.flags & FLAG_CLIP_CHILDREN != 0 {
                y -= p.value;
            }
            pid = p.parent;
        }

        Rect::new(x, y, w.size.w, w.size.h)
    }

    /// Compute widget's content area (where children are placed).
    pub fn content_rect(&self, id: WidgetId) -> Rect {
        let abs = self.absolute_rect(id);
        let b = self.border(id);
        let p = self.padding(id);
        let inset_l = b.left as i16 + p.left as i16;
        let inset_t = b.top as i16 + p.top as i16;
        let inset_r = b.right as u16 + p.right as u16;
        let inset_b = b.bottom as u16 + p.bottom as u16;

        Rect::new(
            abs.x + inset_l,
            abs.y + inset_t,
            abs.w.saturating_sub(inset_l as u16 + inset_r),
            abs.h.saturating_sub(inset_t as u16 + inset_b),
        )
    }

    /// Scroll viewport: content_rect minus scrollbar width when scrollbar is needed.
    /// Used for viewport checks (render, hit_test). If no scrollbar needed, returns content_rect.
    pub fn scroll_viewport(&self, id: WidgetId) -> Rect {
        let cr = self.content_rect(id);
        let ch = self.content_height(id);
        if ch > cr.h {
            // Scrollbar present — narrow the viewport
            Rect::new(cr.x, cr.y, cr.w.saturating_sub(SCROLLBAR_W), cr.h)
        } else {
            cr
        }
    }

    /// Find nearest ancestor with FLAG_CLIP_CHILDREN. Returns NONE if none found.
    pub fn scroll_parent(&self, id: WidgetId) -> WidgetId {
        let max = self.count();
        let mut depth = 0usize;
        let mut pid = self.get(id).parent;
        while pid.is_some() {
            if self.get(pid).flags & FLAG_CLIP_CHILDREN != 0 {
                return pid;
            }
            depth += 1;
            if depth > max {
                break;
            }
            pid = self.get(pid).parent;
        }
        WidgetId::NONE
    }

    /// Compute total content height from direct children of a widget.
    /// Used to clamp scroll range: max_scroll = content_height - viewport_height.
    pub fn content_height(&self, id: WidgetId) -> u16 {
        let mut max_bottom: i16 = 0;
        let mut child = self.get(id).first_child;
        let guard_max = self.count();
        let mut guard = 0usize;
        while child.is_some() {
            let c = self.get(child);
            let bottom = c.location.y + c.size.h as i16;
            if bottom > max_bottom {
                max_bottom = bottom;
            }
            child = c.next_sibling;
            guard += 1;
            if guard > guard_max {
                break;
            }
        }
        if max_bottom > 0 { max_bottom as u16 } else { 0 }
    }

    /// Is child a descendant of ancestor?
    pub fn is_descendant(&self, child: WidgetId, ancestor: WidgetId) -> bool {
        let max = self.count();
        let mut depth = 0usize;
        let mut current = self.get(child).parent;
        while current.is_some() {
            if current == ancestor {
                return true;
            }
            depth += 1;
            if depth > max {
                return false;
            }
            current = self.get(current).parent;
        }
        false
    }

    /// Check if this widget and all its ancestors are visible. An id that
    /// names no live widget is not visible.
    pub fn is_tree_visible(&self, id: WidgetId) -> bool {
        let max = self.count();
        let mut depth = 0usize;
        let mut current = id;
        while current.is_some() {
            let w = self.get(current);
            if w.flags & FLAG_DESTROYED != 0 || !w.is_visible() {
                return false;
            }
            depth += 1;
            if depth > max {
                return false;
            }
            current = w.parent;
        }
        true
    }

    /// Does `id` name a live widget?
    pub fn is_live(&self, id: WidgetId) -> bool {
        self.block(id.0, T_WIDGET).is_some()
    }

    /// Destroy a widget and all its descendants: unlink it from its parent and
    /// free the objects (their pointers go back on the reuse queue). Caller
    /// is responsible for marking dirty.
    ///
    /// String fields on the destroyed widgets become unreferenced; the next
    /// string sweep (`strClear()`) frees them.
    pub fn destroy_subtree(&mut self, root: WidgetId) {
        if !self.is_live(root) {
            return;
        }
        // Unlink root from its parent's child list.
        let parent = self.get(root).parent;
        if parent.is_some() {
            let next = self.get(root).next_sibling;
            let first = self.get(parent).first_child;
            if first == root {
                self.get_mut(parent).first_child = next;
            } else {
                let mut sib = first;
                let max = self.count();
                let mut guard = 0usize;
                while sib.is_some() && self.get(sib).next_sibling != root {
                    sib = self.get(sib).next_sibling;
                    guard += 1;
                    if guard > max {
                        break;
                    }
                }
                if sib.is_some() && self.get(sib).next_sibling == root {
                    self.get_mut(sib).next_sibling = next;
                }
            }
        }
        // Free the whole subtree.
        let mut stack: Vec<WidgetId> = Vec::new();
        stack.push(root);
        while let Some(id) = stack.pop() {
            let mut c = self.get(id).first_child;
            let max = self.count();
            let mut guard = 0usize;
            while c.is_some() {
                stack.push(c);
                c = self.get(c).next_sibling;
                guard += 1;
                if guard > max {
                    break;
                }
            }
            self.free_obj(id.0, T_WIDGET);
        }
        self.dfs_valid = false;
    }

    /// Check `id` (a KIND_BUTTON_GROUP or KIND_RADIO widget) and uncheck every
    /// other live widget of the same kind in its group, so at most one per
    /// group is selected. A group is all widgets of that kind with the same
    /// `group_id`, anywhere in the tree — except radios with `group_id` 0,
    /// whose group is their sibling radios with `group_id` 0 (the original
    /// radio behaviour). Button groups and radios never share a group.
    /// Changed widgets are marked dirty; re-selecting keeps `id` checked.
    pub fn select_in_group(&mut self, id: WidgetId) {
        let kind = self.get(id).kind;
        let parent = self.get(id).parent;
        let group = self.group_id(id);
        let siblings_only = kind == KIND_RADIO && group == 0;
        for p in 0..self.count() as u16 {
            let other = WidgetId(p);
            if other == id || !self.is_live(other) {
                continue;
            }
            let w = self.get(other);
            if w.kind != kind
                || w.flags & FLAG_CHECKED == 0
                || (siblings_only && w.parent != parent)
                || self.group_id(other) != group
            {
                continue;
            }
            self.get_mut(other).flags &= !FLAG_CHECKED;
            self.mark_dirty(other);
        }
        if self.get(id).flags & FLAG_CHECKED == 0 {
            self.get_mut(id).flags |= FLAG_CHECKED;
            self.mark_dirty(id);
        }
    }

    /// Mark widget and all descendants as dirty in both buffers.
    pub fn mark_dirty(&mut self, id: WidgetId) {
        if !self.is_live(id) {
            return;
        }
        self.get_mut(id).flags |= FLAG_DIRTY_AB;
        let mut child = self.get(id).first_child;
        while child.is_some() {
            self.mark_dirty(child);
            child = self.get(child).next_sibling;
        }
    }

    /// Rebuild DFS cache if invalidated.
    fn rebuild_dfs(&mut self) {
        if self.dfs_valid {
            return;
        }

        self.dfs_cache.clear();
        let max = self.count();

        if self.root.is_none() {
            self.dfs_valid = true;
            return;
        }

        let mut stack = Vec::new();
        stack.push(self.root);

        while let Some(id) = stack.pop() {
            if self.dfs_cache.len() >= max {
                break;
            }
            self.dfs_cache.push(id);

            let mut child = self.get(id).first_child;
            let start = stack.len();
            let mut child_count = 0usize;
            while child.is_some() {
                stack.push(child);
                child = self.get(child).next_sibling;
                child_count += 1;
                if child_count > max {
                    break;
                }
            }
            stack[start..].reverse();
        }

        self.dfs_valid = true;
    }

    /// Get DFS pre-order (z-order). Rebuilds cache if tree changed,
    /// then returns a copy.
    ///
    /// Use this when the caller may structurally mutate the tree during
    /// iteration (e.g. event dispatch that can destroy/alloc widgets).
    /// For read-only render loops, prefer ensure_dfs + dfs_len/dfs_at.
    pub fn dfs_order(&mut self) -> Vec<WidgetId> {
        self.rebuild_dfs();
        self.dfs_cache.clone()
    }

    /// Ensure the DFS cache is up to date. Call once before dfs_len/dfs_at.
    /// The cache stays valid until the tree is structurally mutated
    /// (alloc / add_child / destroy_subtree / clear).
    pub fn ensure_dfs(&mut self) {
        self.rebuild_dfs();
    }

    /// Number of widgets in DFS pre-order.
    /// Only valid after ensure_dfs() and until the next structural mutation.
    pub fn dfs_len(&self) -> usize {
        self.dfs_cache.len()
    }

    /// Widget id at DFS pre-order position i.
    /// Only valid after ensure_dfs() and until the next structural mutation.
    pub fn dfs_at(&self, i: usize) -> WidgetId {
        self.dfs_cache[i]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group_widget(tree: &mut ObjPool, kind: u8, group: u8) -> WidgetId {
        let id = tree.alloc_widget().unwrap();
        tree.get_mut(id).kind = kind;
        assert!(tree.update_ext(id, |e| e.group_id = group));
        id
    }

    #[test]
    fn sections_grow_on_demand_and_keep_fields() {
        let mut tree = ObjPool::new();
        let w = tree.alloc_widget().unwrap();
        let other = tree.alloc_widget().unwrap();
        let plain = tree.bytes_of(T_WIDGET) / 2;
        assert_eq!(plain, 4 + 24, "header only");

        // Sections out of bit order: STYLE first, then BOX (inserted before it).
        assert!(tree.update_ext(w, |e| e.border_radius = 7));
        assert!(tree.update_ext(w, |e| e.padding = Edges::uniform(3)));
        tree.set_text_id(w, 42);
        tree.get_mut(w).value = -5;
        let e = tree.ext(w);
        assert_eq!(e.border_radius, 7);
        assert!(e.padding == Edges::uniform(3));
        assert_eq!(e.text_id, 42);
        assert_eq!(e.value, -5);
        assert_eq!(e.alpha, 255, "untouched fields keep defaults");
        assert_eq!(tree.bytes_of(T_WIDGET), plain + 28 + 3 * 8);

        // Writing defaults adds nothing.
        assert!(tree.update_ext(other, |e| e.alpha = 255));
        assert_eq!(tree.ext(other).text_id, 0xFFFF);

        // Moving / compacting keeps everything.
        tree.compact();
        assert!(tree.ext(w).padding == Edges::uniform(3));
        assert_eq!(tree.text_id(w), 42);
    }

    #[test]
    fn stale_ids_read_as_dead_widgets() {
        let mut tree = ObjPool::new();
        let root = tree.alloc_widget().unwrap();
        let child = tree.alloc_widget().unwrap();
        tree.add_child(root, child);
        tree.destroy_subtree(child);
        assert!(!tree.is_live(child));
        assert!(!tree.is_tree_visible(child));
        assert!(tree.get(root).first_child.is_none());
        tree.get_mut(child).location.x = 9; // lost, no panic
        assert_eq!(tree.get(child).location.x, 0);
        // FIFO reuse hands the pointer back.
        assert_eq!(tree.alloc_widget().map(|w| w.0), Some(child.0));
    }

    #[test]
    fn select_in_group_keeps_one_checked_per_group() {
        let mut tree = ObjPool::new();
        let a = group_widget(&mut tree, KIND_BUTTON_GROUP, 1);
        let b = group_widget(&mut tree, KIND_BUTTON_GROUP, 1);
        let other = group_widget(&mut tree, KIND_BUTTON_GROUP, 2);
        let checked = |tree: &ObjPool, id: WidgetId| tree.get(id).flags & FLAG_CHECKED != 0;

        tree.select_in_group(other);
        tree.select_in_group(a);
        assert!(checked(&tree, a) && !checked(&tree, b) && checked(&tree, other));

        tree.select_in_group(b);
        assert!(!checked(&tree, a) && checked(&tree, b) && checked(&tree, other));

        // Re-selecting the selected button keeps it selected.
        tree.select_in_group(b);
        assert!(checked(&tree, b));
    }

    #[test]
    fn radio_group_zero_is_siblings_nonzero_is_tree_wide() {
        let mut tree = ObjPool::new();
        let p1 = tree.alloc_widget().unwrap();
        let p2 = tree.alloc_widget().unwrap();
        let radio = |tree: &mut ObjPool, parent: WidgetId, group: u8| {
            let id = group_widget(tree, KIND_RADIO, group);
            tree.add_child(parent, id);
            id
        };
        let checked = |tree: &ObjPool, id: WidgetId| tree.get(id).flags & FLAG_CHECKED != 0;

        // group 0: per parent, as before.
        let a1 = radio(&mut tree, p1, 0);
        let a2 = radio(&mut tree, p1, 0);
        let b1 = radio(&mut tree, p2, 0);
        tree.select_in_group(b1);
        tree.select_in_group(a1);
        tree.select_in_group(a2);
        assert!(!checked(&tree, a1) && checked(&tree, a2) && checked(&tree, b1));

        // group 5: across parents, independent of the group-0 radios.
        let g1 = radio(&mut tree, p1, 5);
        let g2 = radio(&mut tree, p2, 5);
        tree.select_in_group(g1);
        tree.select_in_group(g2);
        assert!(!checked(&tree, g1) && checked(&tree, g2));
        assert!(checked(&tree, a2) && checked(&tree, b1));

        // A button group with the same id is a separate group.
        let btn = group_widget(&mut tree, KIND_BUTTON_GROUP, 5);
        tree.select_in_group(btn);
        assert!(checked(&tree, btn) && checked(&tree, g2));
    }
}
