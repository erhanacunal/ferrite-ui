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

/// Widget arena indeksi. NONE = bağlantı yok.
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

pub const MAX_WIDGETS: usize = 64;
const TEXT_POOL_SIZE: usize = 256;

/// Tek bir widget düğümü.
///
/// Ağaç yapısı: parent + first_child + next_sibling (left-child right-sibling).
/// Size = border box (border dahil, margin hariç).
///
/// kind: KIND_BASE (container), KIND_LABEL (text), KIND_BUTTON (tıklanabilir container).
/// Label/Button base widget özelliklerini (margin, border, padding, renk) miras alır.
pub struct Widget {
    // Ağaç bağlantıları
    pub parent: WidgetId,
    pub first_child: WidgetId,
    pub next_sibling: WidgetId,

    // Durum
    pub flags: u8,

    // Widget tipi
    pub kind: u8,

    // Kutu modeli
    pub margin: Edges,
    pub border: Edges,
    pub padding: Edges,

    // Konum ve boyut
    pub location: Offset,
    pub size: Size,

    // Görünüm
    pub background_color: Color,
    pub border_color: Color,

    // Label alanları
    pub text_color: Color,
    pub font_id: u8,
    pub text_align: u8,
    pub text_offset: u16, // WidgetTree::text_pool'daki offset
    pub text_len: u8,

    // Button fields
    pub press_color: Color,

    // Background image (0 = no image, 1-254 = flash image_id)
    pub image_id: u8,
}

impl Widget {
    pub(crate) const fn default() -> Self {
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
            text_offset: 0,
            text_len: 0,
            press_color: 0,
            image_id: 0,
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

/// Statik arena ile widget ağacı. Heap yok, sabit bellek.
/// Text pool: label metinleri için append-only buffer (256 byte).
pub struct WidgetTree {
    widgets: [Widget; MAX_WIDGETS],
    text_pool: [u8; TEXT_POOL_SIZE],
    text_pool_len: u16,
    count: u8,
    pub root: WidgetId,
}

impl WidgetTree {
    pub const fn new() -> Self {
        Self {
            widgets: [const { Widget::default() }; MAX_WIDGETS],
            text_pool: [0u8; TEXT_POOL_SIZE],
            text_pool_len: 0,
            count: 0,
            root: WidgetId::NONE,
        }
    }

    /// Yeni widget tahsis et. Arena doluysa None döner.
    pub fn alloc(&mut self) -> Option<WidgetId> {
        if (self.count as usize) >= MAX_WIDGETS {
            return None;
        }
        let id = WidgetId(self.count);
        self.widgets[id.index()] = Widget::default();
        self.count += 1;
        Some(id)
    }

    pub fn get(&self, id: WidgetId) -> &Widget {
        &self.widgets[id.index()]
    }

    pub fn get_mut(&mut self, id: WidgetId) -> &mut Widget {
        &mut self.widgets[id.index()]
    }

    // --- Text pool ---

    /// Widget'a metin ata. Text pool'a kopyalar.
    /// Aynı widget'a tekrar set_text çağrılırsa:
    ///   - Yeni metin ≤ eski uzunluk → yerinde üzerine yazar (pool büyümez)
    ///   - Yeni metin > eski uzunluk → pool'a yeni yer tahsis eder (eski alan kayıp)
    pub fn set_text(&mut self, id: WidgetId, text: &[u8]) -> bool {
        let len = text.len();
        if len > 255 {
            return false;
        }

        let w = &self.widgets[id.index()];

        // Yerinde üzerine yazma (eski alan yeterli)
        if w.text_len as usize >= len && w.text_len > 0 {
            let start = w.text_offset as usize;
            self.text_pool[start..start + len].copy_from_slice(text);
            self.widgets[id.index()].text_len = len as u8;
            return true;
        }

        // Pool'a yeni yer tahsis et
        if self.text_pool_len as usize + len > TEXT_POOL_SIZE {
            return false;
        }
        let offset = self.text_pool_len as usize;
        self.text_pool[offset..offset + len].copy_from_slice(text);
        self.text_pool_len += len as u16;
        self.widgets[id.index()].text_offset = offset as u16;
        self.widgets[id.index()].text_len = len as u8;
        true
    }

    /// Widget'ın metnini döndür. Metin yoksa boş slice.
    pub fn get_text(&self, id: WidgetId) -> &[u8] {
        let w = &self.widgets[id.index()];
        if w.text_len == 0 {
            return &[];
        }
        let start = w.text_offset as usize;
        let end = start + w.text_len as usize;
        if end <= TEXT_POOL_SIZE {
            &self.text_pool[start..end]
        } else {
            &[]
        }
    }

    // --- Ağaç işlemleri ---

    /// Alt widget'ı parent'ın çocuk listesinin sonuna ekle (en üst z-order).
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

    /// Widget'ın ekran koordinatlarındaki border box dikdörtgenini hesapla.
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

    /// Widget'ın content area'sını hesapla (children bu alanda konumlanır).
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

    /// child, ancestor'ın soyundan mı?
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

    /// Widget'ı ve tüm alt ağacını dirty işaretle.
    pub fn mark_dirty(&mut self, id: WidgetId) {
        self.widgets[id.index()].flags |= FLAG_DIRTY;
        let mut child = self.widgets[id.index()].first_child;
        while child.is_some() {
            self.mark_dirty(child);
            child = self.widgets[child.index()].next_sibling;
        }
    }

    /// Ağacı DFS pre-order sırasıyla düzleştir (z-order).
    pub fn dfs_order(&self) -> ([WidgetId; MAX_WIDGETS], usize) {
        let mut order = [WidgetId::NONE; MAX_WIDGETS];
        let mut count: usize = 0;

        if self.root.is_none() {
            return (order, 0);
        }

        let mut stack = [WidgetId::NONE; MAX_WIDGETS];
        let mut top: usize = 0;
        stack[0] = self.root;
        top = 1;

        while top > 0 {
            top -= 1;
            let id = stack[top];

            if count < MAX_WIDGETS {
                order[count] = id;
                count += 1;
            }

            let mut children = [WidgetId::NONE; MAX_WIDGETS];
            let mut child_count: usize = 0;
            let mut child = self.widgets[id.index()].first_child;
            while child.is_some() {
                children[child_count] = child;
                child_count += 1;
                child = self.widgets[child.index()].next_sibling;
            }

            let mut i = child_count;
            while i > 0 {
                i -= 1;
                if top < MAX_WIDGETS {
                    stack[top] = children[i];
                    top += 1;
                }
            }
        }

        (order, count)
    }
}
