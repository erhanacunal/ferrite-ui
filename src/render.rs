use crate::clip::ClipRegion;
use crate::flash::Flash;
use crate::font::FontList;
use crate::image::ImageList;
use crate::lcd::{self, Lcd};
use crate::strpool;
use crate::types::{Color, Rect};
use crate::widget::{
    WidgetId, WidgetTree, ALIGN_CENTER, ALIGN_RIGHT, FLAG_PRESSED, KIND_BUTTON, KIND_LABEL,
};

const SCREEN: Rect = Rect::new(0, 0, lcd::WIDTH, lcd::HEIGHT);

// --- Tam ekran render ---

/// Tüm widget ağacını sıfırdan çiz (ilk açılış veya tam yeniden çizim).
pub fn render_all(tree: &mut WidgetTree, lcd: &Lcd, flash: &Flash, fonts: &FontList, images: &ImageList) {
    if tree.root.is_some() {
        render_subtree(tree, lcd, flash, fonts, images, tree.root);
    }

    // Tüm dirty flag'leri temizle
    clear_all_dirty(tree);
}

/// Alt ağacı DFS pre-order sırasıyla çiz (z-order: parent önce, child sonra).
fn render_subtree(
    tree: &WidgetTree,
    lcd: &Lcd,
    flash: &Flash,
    fonts: &FontList,
    images: &ImageList,
    id: WidgetId,
) {
    let widget = tree.get(id);
    if !widget.is_visible() {
        return;
    }

    let abs = tree.absolute_rect(id);
    draw_widget(lcd, flash, fonts, images, tree, id, &abs);

    let mut child = widget.first_child;
    while child.is_some() {
        render_subtree(tree, lcd, flash, fonts, images, child);
        child = tree.get(child).next_sibling;
    }
}

// --- Dirty render (painter's algorithm + clip) ---

/// Sadece dirty widget'ları yeniden çiz.
///
/// Algoritma:
/// 1. DFS order hesapla (z-order)
/// 2. Her dirty widget için:
///    a. Clip region = widget'ın abs_rect'i
///    b. Üzerindeki (DFS'te sonraki, soyundan olmayan) widget'ları çıkar
///    c. Kalan clip rect'ler üzerinden widget'ı çiz
///    d. Çocukları aynı occluder listesiyle recursive çiz
pub fn render_dirty(tree: &mut WidgetTree, lcd: &Lcd, flash: &Flash, fonts: &FontList, images: &ImageList) {
    let (dfs, dfs_count) = tree.dfs_order();

    // Dirty widget var mı?
    let mut has_dirty = false;
    for i in 0..dfs_count {
        if tree.get(dfs[i]).is_dirty() {
            has_dirty = true;
            break;
        }
    }
    if !has_dirty {
        return;
    }

    for di in 0..dfs_count {
        let id = dfs[di];

        if !tree.get(id).is_dirty() || !tree.get(id).is_visible() {
            continue;
        }

        let abs = tree.absolute_rect(id);

        // Occluder rect'lerini topla:
        // DFS'te bu widget'ın subtree'sinden SONRA gelen,
        // soyundan olmayan, görünür widget'lar
        let mut occluders = [Rect::new(0, 0, 0, 0); 32];
        let mut occ_count: usize = 0;
        let mut after_subtree = false;

        for j in (di + 1)..dfs_count {
            if !after_subtree {
                if !tree.is_descendant(dfs[j], id) {
                    after_subtree = true;
                }
            }
            if after_subtree {
                let other = tree.get(dfs[j]);
                if other.is_visible() {
                    let other_abs = tree.absolute_rect(dfs[j]);
                    if abs.intersects(&other_abs) && occ_count < 32 {
                        occluders[occ_count] = other_abs;
                        occ_count += 1;
                    }
                }
            }
        }

        // Widget'ı ve alt ağacını clip'li çiz
        render_subtree_clipped(tree, lcd, flash, fonts, images, id, &occluders[..occ_count]);
    }

    clear_all_dirty(tree);
}

/// Alt ağacı occluder'lara göre clip'leyerek çiz.
fn render_subtree_clipped(
    tree: &WidgetTree,
    lcd: &Lcd,
    flash: &Flash,
    fonts: &FontList,
    images: &ImageList,
    id: WidgetId,
    occluders: &[Rect],
) {
    let widget = tree.get(id);
    if !widget.is_visible() {
        return;
    }

    let abs = tree.absolute_rect(id);

    // Clip region: widget'ın tam rect'i - occluder'lar
    let mut clip = ClipRegion::from_rect(abs);
    clip.clip_to_bounds(&SCREEN);

    for occ in occluders {
        clip.subtract(occ);
    }

    if !clip.is_empty() {
        draw_widget_clipped(lcd, flash, fonts, images, tree, id, &abs, &clip);
    }

    // Çocukları aynı occluder listesiyle çiz
    let mut child = widget.first_child;
    while child.is_some() {
        render_subtree_clipped(tree, lcd, flash, fonts, images, child, occluders);
        child = tree.get(child).next_sibling;
    }
}

// --- Çizim ---

/// Aktif background rengi: button pressed ise press_color, yoksa background_color.
#[inline]
fn effective_bg(widget: &crate::widget::Widget) -> Color {
    if widget.kind == KIND_BUTTON
        && widget.flags & FLAG_PRESSED != 0
        && widget.press_color != 0
    {
        widget.press_color
    } else {
        widget.background_color
    }
}

/// Widget'ı clip olmadan çiz (tam render için).
fn draw_widget(
    lcd: &Lcd,
    flash: &Flash,
    fonts: &FontList,
    images: &ImageList,
    tree: &WidgetTree,
    id: WidgetId,
    abs: &Rect,
) {
    let widget = tree.get(id);
    let b = &widget.border;
    let bg_color = effective_bg(widget);

    // Border çiz
    if b.top > 0 {
        fill_rect_screen(
            lcd,
            Rect::new(abs.x, abs.y, abs.w, b.top as u16),
            widget.border_color,
        );
    }
    if b.bottom > 0 {
        fill_rect_screen(
            lcd,
            Rect::new(
                abs.x,
                abs.bottom() - b.bottom as i16,
                abs.w,
                b.bottom as u16,
            ),
            widget.border_color,
        );
    }
    if b.left > 0 {
        let inner_y = abs.y + b.top as i16;
        let inner_h = abs.h.saturating_sub(b.top as u16 + b.bottom as u16);
        fill_rect_screen(
            lcd,
            Rect::new(abs.x, inner_y, b.left as u16, inner_h),
            widget.border_color,
        );
    }
    if b.right > 0 {
        let inner_y = abs.y + b.top as i16;
        let inner_h = abs.h.saturating_sub(b.top as u16 + b.bottom as u16);
        fill_rect_screen(
            lcd,
            Rect::new(
                abs.right() - b.right as i16,
                inner_y,
                b.right as u16,
                inner_h,
            ),
            widget.border_color,
        );
    }

    // Background (inside border)
    let bg = inner_rect(abs, b);
    if !bg.is_empty() {
        fill_rect_screen(lcd, bg, bg_color);
    }

    // Background image (drawn on top of bg_color, inside border)
    if widget.image_id != 0 {
        draw_bg_image(lcd, flash, images, widget.image_id, &bg);
    }

    // Label text
    if widget.kind == KIND_LABEL {
        draw_label_text(lcd, flash, fonts, tree, id, abs);
    }
}

/// Draw widget with clip region (dirty render path).
fn draw_widget_clipped(
    lcd: &Lcd,
    flash: &Flash,
    fonts: &FontList,
    images: &ImageList,
    tree: &WidgetTree,
    id: WidgetId,
    abs: &Rect,
    clip: &ClipRegion,
) {
    let widget = tree.get(id);
    let b = &widget.border;
    let bg_color = effective_bg(widget);

    // Border rect'leri
    let border_rects = [
        // Top
        Rect::new(abs.x, abs.y, abs.w, b.top as u16),
        // Bottom
        Rect::new(
            abs.x,
            abs.bottom() - b.bottom as i16,
            abs.w,
            b.bottom as u16,
        ),
        // Left
        Rect::new(
            abs.x,
            abs.y + b.top as i16,
            b.left as u16,
            abs.h.saturating_sub(b.top as u16 + b.bottom as u16),
        ),
        // Right
        Rect::new(
            abs.right() - b.right as i16,
            abs.y + b.top as i16,
            b.right as u16,
            abs.h.saturating_sub(b.top as u16 + b.bottom as u16),
        ),
    ];

    // Border kenarlarını clip'li çiz
    for br in &border_rects {
        if !br.is_empty() {
            fill_clipped(lcd, br, widget.border_color, clip);
        }
    }

    // Background (inside border)
    let bg = inner_rect(abs, b);
    if !bg.is_empty() {
        fill_clipped(lcd, &bg, bg_color, clip);
    }

    // Background image (drawn unclipped — painter's algorithm covers it)
    if widget.image_id != 0 {
        draw_bg_image(lcd, flash, images, widget.image_id, &bg);
    }

    // Label text (drawn unclipped — upper widgets cover it via painter's algorithm)
    if widget.kind == KIND_LABEL {
        draw_label_text(lcd, flash, fonts, tree, id, abs);
    }
}

/// Draw background image at the inner rect origin (after border).
/// Looks up image by image_id in the ImageList.
fn draw_bg_image(lcd: &Lcd, flash: &Flash, images: &ImageList, image_id: u8, inner: &Rect) {
    if inner.is_empty() {
        return;
    }
    if let Some(img) = images.find(image_id) {
        let x = if inner.x < 0 { 0u16 } else { inner.x as u16 };
        let y = if inner.y < 0 { 0u16 } else { inner.y as u16 };
        img.draw(lcd, flash, x, y);
    }
}

/// Draw label text in the content area.
fn draw_label_text(
    lcd: &Lcd,
    flash: &Flash,
    fonts: &FontList,
    tree: &WidgetTree,
    id: WidgetId,
    abs: &Rect,
) {
    let widget = tree.get(id);

    // No text or no font assigned
    if widget.text_id == 0xFF || widget.font_id == 0xFF {
        return;
    }
    let text = strpool::pool().get(widget.text_id);
    if text.is_empty() {
        return;
    }
    // Resolve font by ID, fall back to embedded font
    let font = match fonts.resolve(widget.font_id) {
        Some(f) => f,
        None => return,
    };

    // Content area (border + padding içerisi)
    let b = &widget.border;
    let p = &widget.padding;
    let cx = abs.x + b.left as i16 + p.left as i16;
    let cy = abs.y + b.top as i16 + p.top as i16;
    let cw = abs.w.saturating_sub(
        b.left as u16 + b.right as u16 + p.left as u16 + p.right as u16,
    );
    let ch = abs.h.saturating_sub(
        b.top as u16 + b.bottom as u16 + p.top as u16 + p.bottom as u16,
    );

    if cw == 0 || ch == 0 {
        return;
    }

    // Text boyutu
    let tw = font.text_width(text) as i16;
    let lh = font.line_height() as i16;

    // Horizontal alignment
    let tx = match widget.text_align {
        ALIGN_CENTER => cx + (cw as i16 - tw) / 2,
        ALIGN_RIGHT => cx + cw as i16 - tw,
        _ => cx, // LEFT
    };

    // Vertical center — baseline pozisyonu
    // Baseline ≈ content üst + dikey ortalama + ascent tahmini (lh * 3/4)
    let ty = cy + (ch as i16 - lh) / 2 + (lh * 3) / 4;

    // Opaque mod: text_color fg, widget bg üzerine
    let bg_color = effective_bg(widget);
    font.draw_str(lcd, flash, text, tx, ty, widget.text_color, Some(bg_color));
}

/// Rect'i clip region ile kesişimleyerek çiz.
fn fill_clipped(lcd: &Lcd, rect: &Rect, color: Color, clip: &ClipRegion) {
    for cr in clip.iter() {
        if let Some(visible) = rect.intersection(cr) {
            fill_rect_screen(lcd, visible, color);
        }
    }
}

/// Rect'i ekran sınırlarına kırparak LCD'ye çiz.
fn fill_rect_screen(lcd: &Lcd, rect: Rect, color: Color) {
    if let Some(r) = rect.intersection(&SCREEN) {
        lcd.fill_rect(r.x as u16, r.y as u16, r.w, r.h, color);
    }
}

/// Border box'tan iç alanı (background alanı) hesapla.
fn inner_rect(abs: &Rect, border: &crate::types::Edges) -> Rect {
    Rect::new(
        abs.x + border.left as i16,
        abs.y + border.top as i16,
        abs.w
            .saturating_sub(border.left as u16 + border.right as u16),
        abs.h
            .saturating_sub(border.top as u16 + border.bottom as u16),
    )
}

/// Tüm widget'ların dirty flag'ini temizle.
fn clear_all_dirty(tree: &mut WidgetTree) {
    let (dfs, count) = tree.dfs_order();
    for i in 0..count {
        tree.get_mut(dfs[i]).clear_dirty();
    }
}
