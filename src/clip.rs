use crate::types::Rect;

/// Statik rect pool boyutu (CLAUDE.md: MAX_CLIP_RECTS = 32)
pub const MAX_CLIP_RECTS: usize = 32;

/// Clip region — bir dizi kesişmeyen dikdörtgen ile temsil edilen görünür alan.
///
/// Painter's algorithm'da bir widget'ın üzerindeki widget'ların rect'leri
/// çıkarılarak sadece görünür kısım hesaplanır.
pub struct ClipRegion {
    rects: [Rect; MAX_CLIP_RECTS],
    count: usize,
}

impl ClipRegion {
    /// Boş clip region
    pub const fn new() -> Self {
        Self {
            rects: [Rect::new(0, 0, 0, 0); MAX_CLIP_RECTS],
            count: 0,
        }
    }

    /// Tek rect'ten clip region oluştur
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

    /// Ekran sınırlarına kırp
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

    /// Region'dan bir dikdörtgeni çıkar (occluder).
    ///
    /// Her mevcut rect'ten `cut` rect'i çıkarılır.
    /// Bir çıkarma max 4 yeni rect üretir (üst, alt, sol, sağ şeritler).
    /// Pool doluysa fallback: orijinal rect korunur (overdraw, tearing yok).
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

        // Mevcut rect'leri geçici buffer'a kopyala
        let mut temp = [Rect::new(0, 0, 0, 0); MAX_CLIP_RECTS];
        let mut new_count: usize = 0;

        for i in 0..self.count {
            let r = self.rects[i];

            if !r.intersects(cut) {
                // Kesişim yok — aynen koru
                if new_count < MAX_CLIP_RECTS {
                    temp[new_count] = r;
                    new_count += 1;
                }
                continue;
            }

            // Kesişim var — max 4 parçaya böl
            // Kaç parça üretileceğini hesapla
            let pieces = Self::count_pieces(&r, cut);

            if new_count + pieces > MAX_CLIP_RECTS {
                // Pool dolacak — fallback: orijinal rect'i koru (overdraw)
                temp[new_count] = r;
                new_count += 1;
                continue;
            }

            // Kesişim alanı
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

            // Üst şerit (tam genişlik)
            if iy1 > r.y {
                temp[new_count] = Rect::new(r.x, r.y, r.w, (iy1 - r.y) as u16);
                new_count += 1;
            }

            // Alt şerit (tam genişlik)
            if iy2 < r.bottom() {
                temp[new_count] =
                    Rect::new(r.x, iy2, r.w, (r.bottom() - iy2) as u16);
                new_count += 1;
            }

            // Sol şerit (kesişim yüksekliğinde)
            if ix1 > r.x {
                temp[new_count] =
                    Rect::new(r.x, iy1, (ix1 - r.x) as u16, (iy2 - iy1) as u16);
                new_count += 1;
            }

            // Sağ şerit (kesişim yüksekliğinde)
            if ix2 < r.right() {
                temp[new_count] = Rect::new(
                    ix2,
                    iy1,
                    (r.right() - ix2) as u16,
                    (iy2 - iy1) as u16,
                );
                new_count += 1;
            }
        }

        // Sonuçları geri kopyala
        self.rects[..new_count].copy_from_slice(&temp[..new_count]);
        self.count = new_count;
    }

    /// Bir rect'ten cut çıkarılınca kaç parça üretileceğini hesapla
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
        } // top
        if iy2 < r.bottom() {
            count += 1;
        } // bottom
        if ix1 > r.x {
            count += 1;
        } // left
        if ix2 < r.right() {
            count += 1;
        } // right
        count
    }
}
