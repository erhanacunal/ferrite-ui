use crate::types::Rect;

/// Static rect pool size (MAX_CLIP_RECTS = 32)
pub const MAX_CLIP_RECTS: usize = 32;

/// Clip region — visible area represented as a set of non-overlapping rectangles.
///
/// In the painter's algorithm, occluder rects are subtracted from each widget's
/// clip region, leaving only the actually visible portions.
pub struct ClipRegion {
    rects: [Rect; MAX_CLIP_RECTS],
    count: usize,
}

impl ClipRegion {
    /// Empty clip region
    pub const fn new() -> Self {
        Self {
            rects: [Rect::new(0, 0, 0, 0); MAX_CLIP_RECTS],
            count: 0,
        }
    }

    /// Create clip region from a single rect
    pub fn from_rect(r: Rect) -> Self {
        let mut region = Self::new();
        if !r.is_empty() {
            region.rects[0] = r;
            region.count = 1;
        }
        region
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn iter(&self) -> &[Rect] {
        &self.rects[..self.count]
    }

    /// Clip all rects to screen bounds
    pub fn clip_to_bounds(&mut self, bounds: &Rect) {
        let mut new_count = 0;
        for i in 0..self.count {
            if let Some(clipped) = self.rects[i].intersection(bounds) {
                self.rects[new_count] = clipped;
                new_count += 1;
            }
        }
        self.count = new_count;
    }

    /// Restrict the region to the union of `keep`, which must not overlap
    /// (so the result doesn't either). If the pieces don't fit the rect pool,
    /// the region is left as it was (overdraw, never underdraw).
    pub fn restrict(&mut self, keep: &[Rect]) {
        let mut out = Self::new();
        for r in self.iter() {
            for k in keep {
                if let Some(i) = r.intersection(k) {
                    if out.count == MAX_CLIP_RECTS {
                        return;
                    }
                    out.rects[out.count] = i;
                    out.count += 1;
                }
            }
        }
        *self = out;
    }

    /// Subtract an occluder rect from the region.
    ///
    /// Each subtraction can produce up to 4 new rects (top, bottom, left, right strips).
    /// When the pool is full, the original rect is preserved (overdraw, no tearing).
    ///
    /// ```text
    /// +------------------+
    /// |     top strip    |
    /// +---+---------+----+
    /// | L |   cut   |  R |
    /// +---+---------+----+
    /// |   bottom strip   |
    /// +------------------+
    /// ```
    pub fn subtract(&mut self, cut: &Rect) {
        if cut.is_empty() {
            return;
        }

        let mut temp = [Rect::new(0, 0, 0, 0); MAX_CLIP_RECTS];
        let mut new_count: usize = 0;

        for i in 0..self.count {
            let r = self.rects[i];

            if !r.intersects(cut) {
                if new_count < MAX_CLIP_RECTS {
                    temp[new_count] = r;
                    new_count += 1;
                }
                continue;
            }

            let pieces = Self::count_pieces(&r, cut);

            if new_count + pieces > MAX_CLIP_RECTS {
                temp[new_count] = r;
                new_count += 1;
                continue;
            }

            let ix1 = if r.x > cut.x { r.x } else { cut.x };
            let iy1 = if r.y > cut.y { r.y } else { cut.y };
            let ix2 = if r.right() < cut.right() {
                r.right()
            } else {
                cut.right()
            };
            let iy2 = if r.bottom() < cut.bottom() {
                r.bottom()
            } else {
                cut.bottom()
            };

            // Top strip
            if iy1 > r.y {
                temp[new_count] = Rect::new(r.x, r.y, r.w, (iy1 - r.y) as u16);
                new_count += 1;
            }

            // Bottom strip
            if iy2 < r.bottom() {
                temp[new_count] = Rect::new(r.x, iy2, r.w, (r.bottom() - iy2) as u16);
                new_count += 1;
            }

            // Left strip
            if ix1 > r.x {
                temp[new_count] = Rect::new(r.x, iy1, (ix1 - r.x) as u16, (iy2 - iy1) as u16);
                new_count += 1;
            }

            // Right strip
            if ix2 < r.right() {
                temp[new_count] = Rect::new(ix2, iy1, (r.right() - ix2) as u16, (iy2 - iy1) as u16);
                new_count += 1;
            }
        }

        self.rects[..new_count].copy_from_slice(&temp[..new_count]);
        self.count = new_count;
    }

    fn count_pieces(r: &Rect, cut: &Rect) -> usize {
        let ix1 = if r.x > cut.x { r.x } else { cut.x };
        let iy1 = if r.y > cut.y { r.y } else { cut.y };
        let ix2 = if r.right() < cut.right() {
            r.right()
        } else {
            cut.right()
        };
        let iy2 = if r.bottom() < cut.bottom() {
            r.bottom()
        } else {
            cut.bottom()
        };

        let mut count = 0;
        if iy1 > r.y {
            count += 1;
        }
        if iy2 < r.bottom() {
            count += 1;
        }
        if ix1 > r.x {
            count += 1;
        }
        if ix2 < r.right() {
            count += 1;
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(r: &ClipRegion) -> u32 {
        r.iter().iter().map(|r| r.w as u32 * r.h as u32).sum()
    }

    #[test]
    fn restrict_keeps_disjoint_union() {
        let mut clip = ClipRegion::from_rect(Rect::new(0, 0, 100, 100));
        clip.subtract(&Rect::new(40, 40, 20, 20));
        // Two keep rects, one crossing the hole, one past the right edge.
        clip.restrict(&[Rect::new(30, 30, 40, 20), Rect::new(70, 40, 60, 10)]);
        // 40×20 minus the hole's share (40..60 × 40..50 = 200), plus 30×10.
        assert_eq!(area(&clip), 800 - 200 + 300);
        let rects = clip.iter();
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert!(!a.intersects(b), "overlapping pieces");
            }
            assert!(!a.intersects(&Rect::new(40, 40, 20, 20)), "piece inside the hole");
        }
    }

    #[test]
    fn restrict_to_nothing_empties() {
        let mut clip = ClipRegion::from_rect(Rect::new(0, 0, 10, 10));
        clip.restrict(&[]);
        assert!(clip.is_empty());
    }
}
