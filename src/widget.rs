extern crate alloc;

use alloc::vec::Vec;
use crate::types::{Color, Edges, Offset, Rect, Size};

// --- Flags ---

pub const FLAG_VISIBLE: u8 = 1 << 0;
pub const FLAG_ENABLED: u8 = 1 << 1;
pub const FLAG_CLICKABLE: u8 = 1 << 2;
pub const FLAG_DIRTY: u8 = 1 << 3;
pub const FLAG_PRESSED: u8 = 1 << 4;

// --- Widget Kind ---

pub const KIND_BASE: u8 = 0;
pub const KIND_LABEL: u8 = 1;
pub const KIND_BUTTON: u8 = 2;

// --- Text Alignment ---

pub const ALIGN_LEFT: u8 = 0;
pub const ALIGN_CENTER: u8 = 1;
pub const ALIGN_RIGHT: u8 = 2;

// --- WidgetId ---

/// Widget index. NONE = no link.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct WidgetId(pub u8);

impl WidgetId {
    pub const NONE: Self = Self(0xFF);

    pub fn index(self) -> usize {
        self.0 as usize
    }

    pub fn is_none(self) -> bool {
        self.0 == 0xFF
    }

    pub fn is_some(self) -> bool {
        self.0 != 0xFF
    }
}

// --- Widget ---

/// Single widget node.
///
/// Tree structure: parent + first_child + next_sibling (left-child right-sibling).
/// Size = border box (border included, margin excluded).
pub struct Widget {
    // Tree links
    pub parent: WidgetId,
    pub first_child: WidgetId,
    pub next_sibling: WidgetId,

    // State
    pub flags: u8,

    // Widget type
    pub kind: u8,

    // Box model
    pub margin: Edges,
    pub border: Edges,
    pub padding: Edges,

    // Position and size
    pub location: Offset,
    pub size: Size,

    // Appearance
    pub background_color: Color,
    pub border_color: Color,

    // Label fields
    pub text_color: Color,
    pub font_id: u8,
    pub text_align: u8,
    pub text_id: u8, // StringPool str_id (0xFF = no text)

    // Button fields
    pub press_color: Color,

    // Background image (0 = no image, 1-254 = flash image_id)
    pub image_id: u8,

    // Callback: click event (0 = no callback, 1+ = func_id)
    pub on_click: u16,

    // Callback: custom paint event (0 = no callback, 1+ = func_id)
    pub on_paint: u16,

    // Callback: tap with coordinates event (0 = no callback, 1+ = func_id)
    pub on_tap: u16,
}

impl Widget {
    pub fn default() -> Self {
        Self {
            parent: WidgetId::NONE,
            first_child: WidgetId::NONE,
            next_sibling: WidgetId::NONE,
            flags: FLAG_VISIBLE | FLAG_ENABLED,
            kind: KIND_BASE,
            margin: Edges::new(0, 0, 0, 0),
            border: Edges::new(0, 0, 0, 0),
            padding: Edges::new(0, 0, 0, 0),
            location: Offset { x: 0, y: 0 },
            size: Size { w: 0, h: 0 },
            background_color: 0x0000,
            border_color: 0x0000,
            text_color: 0xFFFF,
            font_id: 0xFF,
            text_align: ALIGN_LEFT,
            text_id: 0xFF,
            press_color: 0,
            image_id: 0,
            on_click: 0,
            on_paint: 0,
            on_tap: 0,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.flags & FLAG_VISIBLE != 0
    }

    pub fn is_dirty(&self) -> bool {
        self.flags & FLAG_DIRTY != 0
    }

    pub fn mark_dirty(&mut self) {
        self.flags |= FLAG_DIRTY;
    }

    pub fn clear_dirty(&mut self) {
        self.flags &= !FLAG_DIRTY;
    }
}

// --- WidgetTree ---

/// Heap-allocated widget tree. Grows on demand via Vec.
pub struct WidgetTree {
    widgets: Vec<Widget>,
    pub root: WidgetId,
}

impl WidgetTree {
    pub fn new() -> Self {
        Self {
            widgets: Vec::new(),
            root: WidgetId::NONE,
        }
    }

    /// Allocate a new widget. Returns None if WidgetId overflow (max 254).
    pub fn alloc(&mut self) -> Option<WidgetId> {
        if self.widgets.len() >= 254 {
            return None; // WidgetId is u8, 0xFF is NONE sentinel
        }
        let id = WidgetId(self.widgets.len() as u8);
        self.widgets.push(Widget::default());
        Some(id)
    }

    pub fn get(&self, id: WidgetId) -> &Widget {
        &self.widgets[id.index()]
    }

    pub fn get_mut(&mut self, id: WidgetId) -> &mut Widget {
        &mut self.widgets[id.index()]
    }

    /// Widget count.
    pub fn count(&self) -> usize {
        self.widgets.len()
    }

    // --- Tree operations ---

    /// Add child to end of parent's child list (highest z-order).
    pub fn add_child(&mut self, parent: WidgetId, child: WidgetId) {
        self.widgets[child.index()].parent = parent;

        let first = self.widgets[parent.index()].first_child;
        if first.is_none() {
            self.widgets[parent.index()].first_child = child;
        } else {
            let mut last = first;
            while self.widgets[last.index()].next_sibling.is_some() {
                last = self.widgets[last.index()].next_sibling;
            }
            self.widgets[last.index()].next_sibling = child;
        }
    }

    /// Compute widget's absolute border-box rect in screen coordinates.
    pub fn absolute_rect(&self, id: WidgetId) -> Rect {
        let w = &self.widgets[id.index()];
        let mut x = w.location.x + w.margin.left as i16;
        let mut y = w.location.y + w.margin.top as i16;

        let mut pid = w.parent;
        while pid.is_some() {
            let p = &self.widgets[pid.index()];
            x += p.location.x
                + p.margin.left as i16
                + p.border.left as i16
                + p.padding.left as i16;
            y += p.location.y
                + p.margin.top as i16
                + p.border.top as i16
                + p.padding.top as i16;
            pid = p.parent;
        }

        Rect::new(x, y, w.size.w, w.size.h)
    }

    /// Compute widget's content area (where children are placed).
    pub fn content_rect(&self, id: WidgetId) -> Rect {
        let abs = self.absolute_rect(id);
        let w = &self.widgets[id.index()];
        let inset_l = w.border.left as i16 + w.padding.left as i16;
        let inset_t = w.border.top as i16 + w.padding.top as i16;
        let inset_r = w.border.right as u16 + w.padding.right as u16;
        let inset_b = w.border.bottom as u16 + w.padding.bottom as u16;

        Rect::new(
            abs.x + inset_l,
            abs.y + inset_t,
            abs.w.saturating_sub(inset_l as u16 + inset_r),
            abs.h.saturating_sub(inset_t as u16 + inset_b),
        )
    }

    /// Is child a descendant of ancestor?
    pub fn is_descendant(&self, child: WidgetId, ancestor: WidgetId) -> bool {
        let mut current = self.widgets[child.index()].parent;
        while current.is_some() {
            if current == ancestor {
                return true;
            }
            current = self.widgets[current.index()].parent;
        }
        false
    }

    /// Mark widget and all descendants as dirty.
    pub fn mark_dirty(&mut self, id: WidgetId) {
        self.widgets[id.index()].flags |= FLAG_DIRTY;
        let mut child = self.widgets[id.index()].first_child;
        while child.is_some() {
            self.mark_dirty(child);
            child = self.widgets[child.index()].next_sibling;
        }
    }

    /// Flatten tree to DFS pre-order (z-order).
    pub fn dfs_order(&self) -> Vec<WidgetId> {
        let mut order = Vec::new();

        if self.root.is_none() {
            return order;
        }

        let mut stack = Vec::new();
        stack.push(self.root);

        while let Some(id) = stack.pop() {
            order.push(id);

            // Collect children, push in reverse for correct DFS order
            let mut child = self.widgets[id.index()].first_child;
            let start = stack.len();
            while child.is_some() {
                stack.push(child);
                child = self.widgets[child.index()].next_sibling;
            }
            // Reverse the children we just pushed
            stack[start..].reverse();
        }

        order
    }
}
