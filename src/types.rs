/// RGB565 renk tipi
pub type Color = u16;

pub const COLOR_BLACK: Color = 0x0000;
pub const COLOR_WHITE: Color = 0xFFFF;
pub const COLOR_RED: Color = 0xF800;
pub const COLOR_GREEN: Color = 0x07E0;
pub const COLOR_BLUE: Color = 0x001F;

/// Piksel konumu (parent content area'ya göreceli)
#[derive(Clone, Copy, Default)]
pub struct Offset {
    pub x: i16,
    pub y: i16,
}

/// Boyut (border box — border dahil, margin hariç)
#[derive(Clone, Copy, Default)]
pub struct Size {
    pub w: u16,
    pub h: u16,
}

/// Ekran koordinatlarında dikdörtgen
#[derive(Clone, Copy, Default)]
pub struct Rect {
    pub x: i16,
    pub y: i16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub const fn new(x: i16, y: i16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> i16 {
        self.x + self.w as i16
    }

    pub fn bottom(&self) -> i16 {
        self.y + self.h as i16
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Check if a point (px, py) is inside this rect.
    pub fn contains(&self, px: u16, py: u16) -> bool {
        let px = px as i16;
        let py = py as i16;
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    /// Do two rectangles overlap?
    pub fn intersects(&self, other: &Rect) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.x < other.right()
            && self.right() > other.x
            && self.y < other.bottom()
            && self.bottom() > other.y
    }

    /// Kesişim dikdörtgenini döndür
    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let x1 = if self.x > other.x { self.x } else { other.x };
        let y1 = if self.y > other.y { self.y } else { other.y };
        let x2 = if self.right() < other.right() {
            self.right()
        } else {
            other.right()
        };
        let y2 = if self.bottom() < other.bottom() {
            self.bottom()
        } else {
            other.bottom()
        };

        if x2 > x1 && y2 > y1 {
            Some(Rect::new(x1, y1, (x2 - x1) as u16, (y2 - y1) as u16))
        } else {
            None
        }
    }
}

/// Border, margin, padding için kenar değerleri (piksel)
#[derive(Clone, Copy, Default)]
pub struct Edges {
    pub top: u8,
    pub right: u8,
    pub bottom: u8,
    pub left: u8,
}

impl Edges {
    pub const ZERO: Self = Self { top: 0, right: 0, bottom: 0, left: 0 };

    pub const fn uniform(val: u8) -> Self {
        Self {
            top: val,
            right: val,
            bottom: val,
            left: val,
        }
    }

    pub const fn new(top: u8, right: u8, bottom: u8, left: u8) -> Self {
        Self {
            top,
            right,
            bottom,
            left,
        }
    }

    pub const fn is_zero(&self) -> bool {
        self.top == 0 && self.right == 0 && self.bottom == 0 && self.left == 0
    }
}
