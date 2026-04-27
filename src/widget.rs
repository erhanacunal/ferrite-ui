extern crate alloc;

use alloc::vec::Vec;
use crate::render::SCROLLBAR_W;
use crate::types::{Color, Edges, Offset, Rect, Size};

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

/// Widget slot has been freed via `destroy_subtree` and is eligible for
/// reuse by `alloc`. Treated as not-visible by `is_tree_visible`.
pub const FLAG_DESTROYED: u16 = 1 << 10;


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

// --- Text Alignment ---

pub const ALIGN_LEFT: u8 = 0;
pub const ALIGN_CENTER: u8 = 1;
pub const ALIGN_RIGHT: u8 = 2;

// --- WidgetId ---

/// Widget index. NONE = no link.
///
/// u16 width gives headroom for dialog widgets (allocated/destroyed at runtime
/// without colliding with static UI). Internal cap is `MAX_WIDGETS`.
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

/// Maximum live widgets. Beyond this, alloc() returns None.
/// Was 254 with u8 ids; raised so dialog open/close cycles don't exhaust slots.
pub const MAX_WIDGETS: usize = 1024;

// --- Extension index ---

const EXT_NONE: u16 = 0xFFFF;
/// Maximum live extensions. Same scaling rationale as MAX_WIDGETS.
pub const MAX_EXTENSIONS: usize = 1024;

// --- Widget (base: 20 bytes) ---

/// Base widget node -- tree links, layout, and appearance.
///
/// Optional fields (edges, text, callbacks, etc.) live in WidgetExt,
/// allocated on demand via WidgetTree::ensure_ext(). Widgets that only
/// need position, size, and color pay 20 bytes instead of 50.
pub struct Widget {
    // Tree links (6 bytes — u16 ids)
    pub parent: WidgetId,
    pub first_child: WidgetId,
    pub next_sibling: WidgetId,

    // Type (1 byte) + extension index (2 bytes)
    pub kind: u8,
    ext: u16,

    // State (2 bytes) -- u16 for dual-buffer dirty tracking
    pub flags: u16,

    // Position and size (8 bytes)
    pub location: Offset,
    pub size: Size,

    // Appearance (4 bytes)
    pub background_color: Color,
    pub border_color: Color,
}

impl Widget {
    fn new() -> Self {
        Self {
            parent: WidgetId::NONE,
            first_child: WidgetId::NONE,
            next_sibling: WidgetId::NONE,
            kind: KIND_BASE,
            ext: EXT_NONE,
            flags: FLAG_VISIBLE | FLAG_ENABLED,
            location: Offset { x: 0, y: 0 },
            size: Size { w: 0, h: 0 },
            background_color: 0x0000,
            border_color: 0x0000,
        }
    }

    pub fn has_ext(&self) -> bool {
        self.ext != EXT_NONE
    }

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

// --- WidgetExt (36 bytes) ---

/// Extension data for widgets that need edges, text, callbacks, etc.
/// Allocated on demand -- simple containers skip this entirely.
pub struct WidgetExt {
    // Box model edges (12 bytes)
    pub margin: Edges,
    pub border: Edges,
    pub padding: Edges,

    // Label fields (6 bytes)
    pub text_color: Color,
    pub font_id: u8,
    pub text_align: u8,
    pub text_id: u16,

    // Appearance extras (4 bytes)
    pub border_radius: u16,
    pub press_color: Color,

    // Image + max input length (2 bytes)
    pub image_id: u8,
    pub max_length: u8,

    // Callbacks (6 bytes)
    pub on_click: u16,
    pub on_paint: u16,
    pub on_tap: u16,

    // Value (2 bytes)
    pub value: i16,

    // Gradient (3 bytes)
    pub gradient_color: Color, // End color (background_color is start)
    pub gradient_dir: u8,      // 0=none, 1=horizontal (L→R), 2=vertical (T→B)

    // Rect at which the widget was last drawn to buffers A and B.
    // The renderer compares each against the current absolute_rect and, when
    // they differ, redraws the widgets behind `prev_rect_X` clipped to that
    // area before drawing the widget at its new rect. Per-buffer tracking
    // avoids an ever-growing union under continuous motion.
    pub prev_rect_a: Rect,
    pub prev_rect_b: Rect,
}

impl WidgetExt {
    /// Default extension with all fields at their zero/default values.
    /// Used as fallback when a widget has no extension allocated.
    pub const DEFAULT: Self = Self {
        margin: Edges::ZERO,
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
        on_click: 0,
        on_paint: 0,
        on_tap: 0,
        value: 0,
        gradient_color: 0,
        gradient_dir: 0,
        prev_rect_a: Rect::new(0, 0, 0, 0),
        prev_rect_b: Rect::new(0, 0, 0, 0),
    };

    fn new() -> Self {
        Self::DEFAULT
    }
}

// --- WidgetTree ---

/// Heap-allocated widget tree with on-demand extensions.
///
/// Base widgets (18B each) hold tree links + layout.
/// Extensions (36B each) hold edges, text, callbacks, gradient -- allocated
/// only when a non-default property is set.
///
/// DFS order is cached and rebuilt lazily when the tree structure changes.
pub struct WidgetTree {
    widgets: Vec<Widget>,
    extensions: Vec<WidgetExt>,
    pub root: WidgetId,
    dfs_cache: Vec<WidgetId>,
    dfs_valid: bool,
    /// FIFO queue of widget slots freed by `destroy_subtree`. `alloc()`
    /// drains this before growing the Vec, so re-entrant builders (e.g. modal
    /// dialogs) get back the same slot ids in the same order on each call.
    widget_free: Vec<WidgetId>,
    /// Same idea for ext slots, so destroyed widget extensions don't leak.
    ext_free: Vec<u16>,
}

impl WidgetTree {
    pub fn new() -> Self {
        Self {
            widgets: Vec::new(),
            extensions: Vec::new(),
            root: WidgetId::NONE,
            dfs_cache: Vec::new(),
            dfs_valid: false,
            widget_free: Vec::new(),
            ext_free: Vec::new(),
        }
    }

    /// Pre-allocate capacity for widget_count user widgets and ext_count extensions.
    /// Called once after image header is parsed, before any alloc() calls run.
    /// This prevents heap fragmentation from interleaved Vec doublings.
    pub fn reserve(&mut self, widget_count: usize, ext_count: usize) {
        self.widgets.reserve(widget_count);
        self.extensions.reserve(ext_count);
        self.dfs_cache.reserve(widget_count + 1);
    }

    /// Remove all widgets and reset root. Frees heap memory.
    pub fn clear(&mut self) {
        self.widgets.clear();
        self.extensions.clear();
        self.dfs_cache.clear();
        self.dfs_valid = false;
        self.widget_free.clear();
        self.ext_free.clear();
        self.root = WidgetId::NONE;
    }

    /// Allocate a new widget. Reuses slots freed via `destroy_subtree` (FIFO)
    /// before growing the Vec. Returns None if `MAX_WIDGETS` exhausted.
    pub fn alloc(&mut self) -> Option<WidgetId> {
        if !self.widget_free.is_empty() {
            let id = self.widget_free.remove(0);
            let w = &mut self.widgets[id.index()];
            *w = Widget::new();
            w.flags |= FLAG_DIRTY_AB;
            self.dfs_valid = false;
            return Some(id);
        }
        if self.widgets.len() >= MAX_WIDGETS {
            return None;
        }
        let id = WidgetId(self.widgets.len() as u16);
        let mut w = Widget::new();
        w.flags |= FLAG_DIRTY_AB;
        self.widgets.push(w);
        self.dfs_valid = false;
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

    /// Extension count (for diagnostics).
    pub fn ext_count(&self) -> usize {
        self.extensions.len()
    }

    // --- Extension access ---

    /// Get extension for a widget (read-only). Returns None if not allocated.
    pub fn ext(&self, id: WidgetId) -> Option<&WidgetExt> {
        let idx = self.widgets[id.index()].ext;
        if idx == EXT_NONE { None } else { Some(&self.extensions[idx as usize]) }
    }

    /// Get extension for a widget (mutable). Returns None if not allocated.
    pub fn ext_mut(&mut self, id: WidgetId) -> Option<&mut WidgetExt> {
        let idx = self.widgets[id.index()].ext;
        if idx == EXT_NONE { None } else { Some(&mut self.extensions[idx as usize]) }
    }

    /// Get or allocate extension for a widget. Drains the ext free list
    /// (from destroyed widgets) before growing. Returns None if pool full.
    pub fn ensure_ext(&mut self, id: WidgetId) -> Option<&mut WidgetExt> {
        let idx = self.widgets[id.index()].ext;
        if idx != EXT_NONE {
            return Some(&mut self.extensions[idx as usize]);
        }
        // Reuse a freed slot if available.
        if let Some(reused) = self.ext_free.pop() {
            self.extensions[reused as usize] = WidgetExt::new();
            self.widgets[id.index()].ext = reused;
            return Some(&mut self.extensions[reused as usize]);
        }
        if self.extensions.len() >= MAX_EXTENSIONS {
            return None;
        }
        let new_idx = self.extensions.len() as u16;
        self.extensions.push(WidgetExt::new());
        self.widgets[id.index()].ext = new_idx;
        Some(&mut self.extensions[new_idx as usize])
    }

    // --- Convenience accessors (return defaults when no extension) ---

    pub fn margin(&self, id: WidgetId) -> Edges {
        self.ext(id).map_or(Edges::ZERO, |e| e.margin)
    }

    pub fn border(&self, id: WidgetId) -> Edges {
        self.ext(id).map_or(Edges::ZERO, |e| e.border)
    }

    pub fn padding(&self, id: WidgetId) -> Edges {
        self.ext(id).map_or(Edges::ZERO, |e| e.padding)
    }

    pub fn text_color(&self, id: WidgetId) -> Color {
        self.ext(id).map_or(0xFFFF, |e| e.text_color)
    }

    pub fn font_id(&self, id: WidgetId) -> u8 {
        self.ext(id).map_or(0, |e| e.font_id)
    }

    pub fn text_align(&self, id: WidgetId) -> u8 {
        self.ext(id).map_or(ALIGN_LEFT, |e| e.text_align)
    }

    pub fn text_id(&self, id: WidgetId) -> u16 {
        self.ext(id).map_or(0xFFFF, |e| e.text_id)
    }

    pub fn border_radius(&self, id: WidgetId) -> u16 {
        self.ext(id).map_or(0, |e| e.border_radius)
    }

    pub fn press_color(&self, id: WidgetId) -> Color {
        self.ext(id).map_or(0, |e| e.press_color)
    }

    pub fn image_id(&self, id: WidgetId) -> u8 {
        self.ext(id).map_or(0, |e| e.image_id)
    }

    pub fn max_length(&self, id: WidgetId) -> u8 {
        self.ext(id).map_or(0, |e| e.max_length)
    }

    pub fn on_click(&self, id: WidgetId) -> u16 {
        self.ext(id).map_or(0, |e| e.on_click)
    }

    pub fn on_paint(&self, id: WidgetId) -> u16 {
        self.ext(id).map_or(0, |e| e.on_paint)
    }

    pub fn on_tap(&self, id: WidgetId) -> u16 {
        self.ext(id).map_or(0, |e| e.on_tap)
    }

    pub fn value(&self, id: WidgetId) -> i16 {
        self.ext(id).map_or(0, |e| e.value)
    }

    pub fn gradient_color(&self, id: WidgetId) -> Color {
        self.ext(id).map_or(0, |e| e.gradient_color)
    }

    pub fn gradient_dir(&self, id: WidgetId) -> u8 {
        self.ext(id).map_or(0, |e| e.gradient_dir)
    }

    // --- Tree operations ---

    /// Add child to end of parent's child list (highest z-order).
    pub fn add_child(&mut self, parent: WidgetId, child: WidgetId) {
        if parent == child {
            return;
        }
        self.widgets[child.index()].parent = parent;
        self.widgets[child.index()].next_sibling = WidgetId::NONE;

        let first = self.widgets[parent.index()].first_child;
        if first.is_none() {
            self.widgets[parent.index()].first_child = child;
        } else {
            let max = self.widgets.len();
            let mut last = first;
            let mut guard = 0usize;
            while self.widgets[last.index()].next_sibling.is_some() {
                let next = self.widgets[last.index()].next_sibling;
                if next == child {
                    return;
                }
                last = next;
                guard += 1;
                if guard > max {
                    return;
                }
            }
            self.widgets[last.index()].next_sibling = child;
        }
        self.dfs_valid = false;
    }

    /// Compute widget's absolute border-box rect in screen coordinates.
    /// If an ancestor has FLAG_CLIP_CHILDREN, children are offset by
    /// that ancestor's scroll_y (stored in ext.value).
    pub fn absolute_rect(&self, id: WidgetId) -> Rect {
        let w = &self.widgets[id.index()];
        let m = self.margin(id);
        let mut x = w.location.x + m.left as i16;
        let mut y = w.location.y + m.top as i16;

        let max = self.widgets.len();
        let mut depth = 0usize;
        let mut pid = w.parent;
        while pid.is_some() {
            depth += 1;
            if depth > max {
                break;
            }
            let p = &self.widgets[pid.index()];
            let pm = self.margin(pid);
            if p.kind == KIND_DROPDOWN {
                x += p.location.x + pm.left as i16;
                y += p.location.y + pm.top as i16;
            } else {
                let pb = self.border(pid);
                let pp = self.padding(pid);
                x += p.location.x
                    + pm.left as i16
                    + pb.left as i16
                    + pp.left as i16;
                y += p.location.y
                    + pm.top as i16
                    + pb.top as i16
                    + pp.top as i16;
            }
            // Scroll offset: shift children by -scroll_y
            if p.flags & FLAG_CLIP_CHILDREN != 0 {
                let scroll_y = self.value(pid);
                y -= scroll_y;
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
        let max = self.widgets.len();
        let mut depth = 0usize;
        let mut pid = self.widgets[id.index()].parent;
        while pid.is_some() {
            if self.widgets[pid.index()].flags & FLAG_CLIP_CHILDREN != 0 {
                return pid;
            }
            depth += 1;
            if depth > max {
                break;
            }
            pid = self.widgets[pid.index()].parent;
        }
        WidgetId::NONE
    }

    /// Compute total content height from direct children of a widget.
    /// Used to clamp scroll range: max_scroll = content_height - viewport_height.
    pub fn content_height(&self, id: WidgetId) -> u16 {
        let mut max_bottom: i16 = 0;
        let mut child = self.widgets[id.index()].first_child;
        let guard_max = self.widgets.len();
        let mut guard = 0usize;
        while child.is_some() {
            let c = &self.widgets[child.index()];
            let cm = self.margin(child);
            let bottom = c.location.y + cm.top as i16 + c.size.h as i16 + cm.bottom as i16;
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
        let max = self.widgets.len();
        let mut depth = 0usize;
        let mut current = self.widgets[child.index()].parent;
        while current.is_some() {
            if current == ancestor {
                return true;
            }
            depth += 1;
            if depth > max {
                return false;
            }
            current = self.widgets[current.index()].parent;
        }
        false
    }

    /// Check if this widget and all its ancestors are visible.
    /// Destroyed widgets (FLAG_DESTROYED) are treated as not visible — folding
    /// the destroy check here means render/hit-test skip them automatically.
    pub fn is_tree_visible(&self, id: WidgetId) -> bool {
        let max = self.widgets.len();
        let mut depth = 0usize;
        let mut current = id;
        while current.is_some() {
            let w = &self.widgets[current.index()];
            if w.flags & FLAG_DESTROYED != 0 {
                return false;
            }
            if !w.is_visible() {
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

    /// Is this widget's slot live (not destroyed)?
    pub fn is_live(&self, id: WidgetId) -> bool {
        if id.is_none() || id.index() >= self.widgets.len() {
            return false;
        }
        self.widgets[id.index()].flags & FLAG_DESTROYED == 0
    }

    /// Destroy a widget and all its descendants. Unlinks from parent, marks
    /// FLAG_DESTROYED, frees the WidgetExt slot, and pushes the slot id onto
    /// the reuse queue. Caller is responsible for marking dirty (this fn just
    /// invalidates the DFS cache).
    ///
    /// String fields on the destroyed widgets become unreferenced; the next
    /// `StringPool::smart_clear` will reclaim them.
    pub fn destroy_subtree(&mut self, root: WidgetId) {
        if !self.is_live(root) {
            return;
        }
        // Unlink root from its parent's child list.
        let parent = self.widgets[root.index()].parent;
        if parent.is_some() {
            let next = self.widgets[root.index()].next_sibling;
            let first = self.widgets[parent.index()].first_child;
            if first == root {
                self.widgets[parent.index()].first_child = next;
            } else {
                let mut sib = first;
                let max = self.widgets.len();
                let mut guard = 0usize;
                while sib.is_some() && self.widgets[sib.index()].next_sibling != root {
                    sib = self.widgets[sib.index()].next_sibling;
                    guard += 1;
                    if guard > max { break; }
                }
                if sib.is_some() && self.widgets[sib.index()].next_sibling == root {
                    self.widgets[sib.index()].next_sibling = next;
                }
            }
        }
        // DFS-mark the whole subtree destroyed and free their ext slots.
        let mut stack: Vec<WidgetId> = Vec::new();
        stack.push(root);
        while let Some(id) = stack.pop() {
            // Queue children before mutating.
            let mut c = self.widgets[id.index()].first_child;
            let max = self.widgets.len();
            let mut guard = 0usize;
            while c.is_some() {
                stack.push(c);
                c = self.widgets[c.index()].next_sibling;
                guard += 1;
                if guard > max { break; }
            }
            let w = &mut self.widgets[id.index()];
            // Free ext slot.
            if w.ext != EXT_NONE {
                let ext_idx = w.ext;
                w.ext = EXT_NONE;
                self.ext_free.push(ext_idx);
            }
            // Clear tree links so stale traversal cannot reach live widgets.
            w.first_child = WidgetId::NONE;
            w.next_sibling = WidgetId::NONE;
            w.parent = WidgetId::NONE;
            // Mark destroyed and clear visibility/clickable so any cached
            // pointer treats it as inert.
            w.flags = (w.flags & !(FLAG_VISIBLE | FLAG_CLICKABLE)) | FLAG_DESTROYED;
            self.widget_free.push(id);
        }
        self.dfs_valid = false;
    }

    /// Mark widget and all descendants as dirty in both buffers.
    pub fn mark_dirty(&mut self, id: WidgetId) {
        if id.is_none() || id.index() >= self.widgets.len() {
            return;
        }
        let w = &mut self.widgets[id.index()];
        if w.flags & FLAG_DESTROYED != 0 {
            return;
        }
        w.flags |= FLAG_DIRTY_AB;
        let mut child = self.widgets[id.index()].first_child;
        while child.is_some() {
            self.mark_dirty(child);
            child = self.widgets[child.index()].next_sibling;
        }
    }


    /// Rebuild DFS cache if invalidated.
    fn rebuild_dfs(&mut self) {
        if self.dfs_valid {
            return;
        }

        self.dfs_cache.clear();
        let max = self.widgets.len();

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

            let mut child = self.widgets[id.index()].first_child;
            let start = stack.len();
            let mut child_count = 0usize;
            while child.is_some() {
                stack.push(child);
                child = self.widgets[child.index()].next_sibling;
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
    pub fn dfs_order(&mut self) -> Vec<WidgetId> {
        self.rebuild_dfs();
        self.dfs_cache.clone()
    }
}
