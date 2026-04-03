use crate::clip::ClipRegion;
use crate::ctx::Ctx;
use crate::lcd::{self, Lcd};
use crate::types::{Color, Rect};
use crate::widget::{
    WidgetId, WidgetTree, ALIGN_CENTER, ALIGN_RIGHT, FLAG_CHECKED, FLAG_PRESSED,
    KIND_BUTTON, KIND_CHECKBOX, KIND_LABEL, KIND_PROGRESS, KIND_RADIO, KIND_SLIDER,
};

const SCREEN: Rect = Rect::new(0, 0, lcd::WIDTH, lcd::HEIGHT);

// --- Full screen render ---

/// Draw the entire widget tree from scratch (initial draw or full redraw).
pub fn render_all(ctx: &mut Ctx) {
    if ctx.tree.root.is_some() {
        let root = ctx.tree.root;
        render_subtree(ctx, root);
    }
    clear_all_dirty(ctx);
}

/// Draw subtree in DFS pre-order (z-order: parent first, child after).
fn render_subtree(ctx: &Ctx, id: WidgetId) {
    let widget = ctx.tree.get(id);
    if !widget.is_visible() {
        return;
    }

    let abs = ctx.tree.absolute_rect(id);
    draw_widget(ctx, id, &abs);

    let mut child = widget.first_child;
    let max = ctx.tree.count();
    let mut guard = 0usize;
    while child.is_some() {
        render_subtree(ctx, child);
        child = ctx.tree.get(child).next_sibling;
        guard += 1;
        if guard > max {
            break; // sibling cycle
        }
    }
}

// --- Dirty render (painter's algorithm + clip) ---

/// Redraw only dirty widgets.
pub fn render_dirty(ctx: &mut Ctx) {
    let dfs = ctx.tree.dfs_order();

    let mut has_dirty = false;
    for i in 0..dfs.len() {
        if ctx.tree.get(dfs[i]).is_dirty() {
            has_dirty = true;
            break;
        }
    }
    if !has_dirty {
        return;
    }

    for di in 0..dfs.len() {
        let id = dfs[di];

        if !ctx.tree.get(id).is_dirty() {
            continue;
        }

        // Widget just became invisible — erase its area with ancestor background
        if !ctx.tree.is_tree_visible(id) {
            let abs = ctx.tree.absolute_rect(id);
            if !abs.is_empty() {
                let bg = ancestor_bg(&ctx.tree, id);
                fill_rect_screen(&ctx.lcd, abs, bg);
            }
            continue;
        }

        // Find subtree end: contiguous descendants after di
        let mut sub_end = di + 1;
        while sub_end < dfs.len() && ctx.tree.is_descendant(dfs[sub_end], id) {
            sub_end += 1;
        }

        // Collect occluder rects: visible widgets after this subtree in DFS
        let mut occluders = [Rect::new(0, 0, 0, 0); 32];
        let mut occ_count: usize = 0;
        let abs = ctx.tree.absolute_rect(id);

        for j in sub_end..dfs.len() {
            if ctx.tree.is_tree_visible(dfs[j]) {
                let other_abs = ctx.tree.absolute_rect(dfs[j]);
                if abs.intersects(&other_abs) && occ_count < 32 {
                    occluders[occ_count] = other_abs;
                    occ_count += 1;
                }
            }
        }

        // Flat iteration over dirty widget + its descendants (replaces recursion)
        for si in di..sub_end {
            let sid = dfs[si];
            if !ctx.tree.is_tree_visible(sid) {
                continue;
            }

            let sabs = ctx.tree.absolute_rect(sid);

            let mut clip = ClipRegion::from_rect(sabs);
            clip.clip_to_bounds(&SCREEN);

            for oi in 0..occ_count {
                clip.subtract(&occluders[oi]);
            }

            if !clip.is_empty() {
                draw_widget_clipped(ctx, sid, &sabs, &clip);
            }
        }
    }

    clear_all_dirty(ctx);
}

// --- Drawing ---

/// Effective background color for a widget.
/// If the widget is a pressed button, returns press_color.
/// If the widget is a child of a pressed button and shares the same
/// background_color, inherits the parent button's press_color.
#[inline]
/// Find the nearest ancestor with a non-zero background color.
/// Used to erase hidden widgets with the correct background.
fn ancestor_bg(tree: &WidgetTree, id: WidgetId) -> Color {
    let mut pid = tree.get(id).parent;
    let max = tree.count();
    let mut depth = 0usize;
    while pid.is_some() {
        let bg = tree.get(pid).background_color;
        if bg != 0 {
            return bg;
        }
        pid = tree.get(pid).parent;
        depth += 1;
        if depth > max {
            break;
        }
    }
    0 // black fallback
}

fn effective_bg(tree: &WidgetTree, id: WidgetId) -> Color {
    let widget = tree.get(id);

    // This widget is pressed and has a press_color
    if widget.flags & FLAG_PRESSED != 0 && widget.press_color != 0 {
        return widget.press_color;
    }

    // Check ancestor — if a parent is pressed and this widget shares
    // the same background, inherit press_color
    let mut pid = widget.parent;
    let max = tree.count();
    let mut depth = 0usize;
    while pid.is_some() {
        let p = tree.get(pid);
        if p.flags & FLAG_PRESSED != 0 && p.press_color != 0 {
            if widget.background_color == p.background_color {
                return p.press_color;
            }
            break;
        }
        pid = p.parent;
        depth += 1;
        if depth > max {
            break;
        }
    }

    widget.background_color
}

/// Draw widget without clipping (full render path).
fn draw_widget(ctx: &Ctx, id: WidgetId, abs: &Rect) {
    let widget = ctx.tree.get(id);
    let b = &widget.border;
    let bg_color = effective_bg(&ctx.tree, id);
    let r = widget.border_radius;
    // Labels with bg_color 0 are transparent — parent background shows through
    let draw_bg = bg_color != 0 || widget.kind != KIND_LABEL;

    if r > 0 {
        // Rounded mode: draw border as rounded_rect, background as fill_rounded_rect
        let bw = b.top.max(b.bottom).max(b.left).max(b.right);
        if bw > 0 {
            // Draw border outline(s)
            for i in 0..bw {
                rounded_rect_screen(
                    &ctx.lcd,
                    Rect::new(
                        abs.x + i as i16,
                        abs.y + i as i16,
                        abs.w.saturating_sub(i as u16 * 2),
                        abs.h.saturating_sub(i as u16 * 2),
                    ),
                    r.saturating_sub(i as u16),
                    widget.border_color,
                );
            }
        }
        // Background (inside border)
        if draw_bg {
            let bg = inner_rect(abs, b);
            if !bg.is_empty() {
                let inner_r = r.saturating_sub(b.top.max(b.left) as u16);
                fill_rounded_rect_screen(&ctx.lcd, bg, inner_r, bg_color);
            }
        }
    } else {
        // Sharp corners: original rect-based drawing
        if b.top > 0 {
            fill_rect_screen(
                &ctx.lcd,
                Rect::new(abs.x, abs.y, abs.w, b.top as u16),
                widget.border_color,
            );
        }
        if b.bottom > 0 {
            fill_rect_screen(
                &ctx.lcd,
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
                &ctx.lcd,
                Rect::new(abs.x, inner_y, b.left as u16, inner_h),
                widget.border_color,
            );
        }
        if b.right > 0 {
            let inner_y = abs.y + b.top as i16;
            let inner_h = abs.h.saturating_sub(b.top as u16 + b.bottom as u16);
            fill_rect_screen(
                &ctx.lcd,
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
        if draw_bg {
            let bg = inner_rect(abs, b);
            if !bg.is_empty() {
                fill_rect_screen(&ctx.lcd, bg, bg_color);
            }
        }
    }

    // Background image
    if widget.image_id != 0 {
        let bg = inner_rect(abs, b);
        draw_bg_image(ctx, widget.image_id, &bg);
    }

    // Label text
    if widget.kind == KIND_LABEL {
        draw_label_text(ctx, id, abs);
    }

    // Progress bar / slider fill
    if widget.kind == KIND_PROGRESS || widget.kind == KIND_SLIDER {
        let inner = inner_rect(abs, b);
        draw_value_fill(&ctx.lcd, widget, &inner);
    }

    // Checkbox / radio indicator
    if widget.kind == KIND_CHECKBOX || widget.kind == KIND_RADIO {
        let inner = inner_rect(abs, b);
        draw_check_indicator(&ctx.lcd, widget, &inner);
    }
}

/// Draw widget with clip region (dirty render path).
fn draw_widget_clipped(ctx: &Ctx, id: WidgetId, abs: &Rect, clip: &ClipRegion) {
    let widget = ctx.tree.get(id);
    let b = &widget.border;
    let bg_color = effective_bg(&ctx.tree, id);
    let r = widget.border_radius;
    let draw_bg = bg_color != 0 || widget.kind != KIND_LABEL;

    if r > 0 {
        // Rounded mode: fallback to unclipped rounded draw.
        // Rounded rects can't be trivially rect-clipped, so we draw the
        // full rounded shape. The painter's algorithm ensures correctness.
        let bw = b.top.max(b.bottom).max(b.left).max(b.right);
        if bw > 0 {
            for i in 0..bw {
                rounded_rect_screen(
                    &ctx.lcd,
                    Rect::new(
                        abs.x + i as i16,
                        abs.y + i as i16,
                        abs.w.saturating_sub(i as u16 * 2),
                        abs.h.saturating_sub(i as u16 * 2),
                    ),
                    r.saturating_sub(i as u16),
                    widget.border_color,
                );
            }
        }
        if draw_bg {
            let bg = inner_rect(abs, b);
            if !bg.is_empty() {
                let inner_r = r.saturating_sub(b.top.max(b.left) as u16);
                fill_rounded_rect_screen(&ctx.lcd, bg, inner_r, bg_color);
            }
        }
    } else {
        // Sharp corners: rect-clipped drawing
        let border_rects = [
            Rect::new(abs.x, abs.y, abs.w, b.top as u16),
            Rect::new(
                abs.x,
                abs.bottom() - b.bottom as i16,
                abs.w,
                b.bottom as u16,
            ),
            Rect::new(
                abs.x,
                abs.y + b.top as i16,
                b.left as u16,
                abs.h.saturating_sub(b.top as u16 + b.bottom as u16),
            ),
            Rect::new(
                abs.right() - b.right as i16,
                abs.y + b.top as i16,
                b.right as u16,
                abs.h.saturating_sub(b.top as u16 + b.bottom as u16),
            ),
        ];

        for br in &border_rects {
            if !br.is_empty() {
                fill_clipped(&ctx.lcd, br, widget.border_color, clip);
            }
        }

        // Background (inside border)
        if draw_bg {
            let bg = inner_rect(abs, b);
            if !bg.is_empty() {
                fill_clipped(&ctx.lcd, &bg, bg_color, clip);
            }
        }
    }

    // Background image (drawn unclipped — painter's algorithm covers it)
    if widget.image_id != 0 {
        let bg = inner_rect(abs, b);
        draw_bg_image(ctx, widget.image_id, &bg);
    }

    // Label text (drawn unclipped — upper widgets cover it via painter's algorithm)
    if widget.kind == KIND_LABEL {
        draw_label_text(ctx, id, abs);
    }

    // Progress bar / slider fill (drawn unclipped)
    if widget.kind == KIND_PROGRESS || widget.kind == KIND_SLIDER {
        let inner = inner_rect(abs, b);
        draw_value_fill(&ctx.lcd, widget, &inner);
    }

    // Checkbox / radio indicator (drawn unclipped)
    if widget.kind == KIND_CHECKBOX || widget.kind == KIND_RADIO {
        let inner = inner_rect(abs, b);
        draw_check_indicator(&ctx.lcd, widget, &inner);
    }
}

/// Draw background image at the inner rect origin.
fn draw_bg_image(ctx: &Ctx, image_id: u8, inner: &Rect) {
    if inner.is_empty() {
        return;
    }
    if let Some(img) = ctx.images.find(image_id) {
        let x = if inner.x < 0 { 0u16 } else { inner.x as u16 };
        let y = if inner.y < 0 { 0u16 } else { inner.y as u16 };
        img.draw(&ctx.lcd, &ctx.flash, x, y);
    }
}

/// Draw label text in the content area.
fn draw_label_text(ctx: &Ctx, id: WidgetId, abs: &Rect) {
    let widget = ctx.tree.get(id);

    if widget.text_id == 0xFFFF || widget.font_id == 0xFF {
        return;
    }
    let text = ctx.strpool.get(widget.text_id);
    if text.is_empty() {
        return;
    }
    let font = match ctx.fonts.resolve(widget.font_id) {
        Some(f) => f,
        None => return,
    };

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

    let tw = font.text_width(text) as i16;
    let lh = font.line_height() as i16;

    let tx = match widget.text_align {
        ALIGN_CENTER => cx + (cw as i16 - tw) / 2,
        ALIGN_RIGHT => cx + cw as i16 - tw,
        _ => cx,
    };

    let ty = cy + (ch as i16 - lh) / 2 + (lh * 3) / 4;

    let bg_color = effective_bg(&ctx.tree, id);
    let bg = if bg_color == 0 && widget.kind == KIND_LABEL { None } else { Some(bg_color) };
    font.draw_str(&ctx.lcd, &ctx.flash, text, tx, ty, widget.text_color, bg);
}

/// Draw rect clipped against clip region.
fn fill_clipped(lcd: &Lcd, rect: &Rect, color: Color, clip: &ClipRegion) {
    for cr in clip.iter() {
        if let Some(visible) = rect.intersection(cr) {
            fill_rect_screen(lcd, visible, color);
        }
    }
}

/// Draw rect clipped to screen bounds.
fn fill_rect_screen(lcd: &Lcd, rect: Rect, color: Color) {
    if let Some(r) = rect.intersection(&SCREEN) {
        lcd.fill_rect(r.x as u16, r.y as u16, r.w, r.h, color);
    }
}

/// Draw rounded rect outline clipped to screen bounds.
fn rounded_rect_screen(lcd: &Lcd, rect: Rect, radius: u16, color: Color) {
    if let Some(r) = rect.intersection(&SCREEN) {
        lcd.draw_rounded_rect(r.x as u16, r.y as u16, r.w, r.h, radius, color);
    }
}

/// Fill rounded rect clipped to screen bounds.
fn fill_rounded_rect_screen(lcd: &Lcd, rect: Rect, radius: u16, color: Color) {
    if let Some(r) = rect.intersection(&SCREEN) {
        lcd.fill_rounded_rect(r.x as u16, r.y as u16, r.w, r.h, radius, color);
    }
}

/// Compute inner area (background area) from border box.
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

/// Draw progress bar / slider fill inside the inner rect.
/// press_color = fill color, background_color = track color (already drawn).
/// For slider: also draws a thumb indicator using border_color.
fn draw_value_fill(lcd: &Lcd, widget: &crate::widget::Widget, inner: &Rect) {
    if inner.is_empty() || widget.press_color == 0 {
        return;
    }

    let val = widget.value.clamp(0, 100) as u32;
    let fill_w = ((inner.w as u32) * val / 100) as u16;
    let r = widget.border_radius;

    if fill_w > 0 {
        let fill = Rect::new(inner.x, inner.y, fill_w, inner.h);
        if r > 0 {
            // Rounded fill: use fill_rounded_rect, clamp radius to fill width
            let fill_r = r.min(fill_w / 2).min(inner.h / 2);
            fill_rounded_rect_screen(lcd, fill, fill_r, widget.press_color);
        } else {
            fill_rect_screen(lcd, fill, widget.press_color);
        }
    }

    // Slider thumb
    if widget.kind == KIND_SLIDER && inner.w > 0 {
        let thumb_w: u16 = 6;
        let thumb_x = inner.x + fill_w as i16 - (thumb_w as i16 / 2);
        let thumb_x = thumb_x.max(inner.x).min(inner.x + inner.w as i16 - thumb_w as i16);
        let thumb = Rect::new(thumb_x, inner.y, thumb_w, inner.h);
        let thumb_color = if widget.border_color != 0 {
            widget.border_color
        } else {
            0xFFFF
        };
        if r > 0 {
            let thumb_r = r.min(thumb_w / 2).min(inner.h / 2);
            fill_rounded_rect_screen(lcd, thumb, thumb_r, thumb_color);
        } else {
            fill_rect_screen(lcd, thumb, thumb_color);
        }
    }
}

/// Clear dirty flag on all widgets.
/// Draw checkbox or radio indicator inside the inner rect.
///
/// Indicator is a square (checkbox) or circle (radio) on the left side,
/// sized to fit the widget height. Uses text_color for the outline and
/// press_color for the checked fill.
fn draw_check_indicator(lcd: &Lcd, widget: &crate::widget::Widget, inner: &Rect) {
    if inner.is_empty() {
        return;
    }

    let p = &widget.padding;
    let cx = inner.x + p.left as i16;
    let cy = inner.y + p.top as i16;
    let ch = inner.h.saturating_sub(p.top as u16 + p.bottom as u16);

    if ch < 4 {
        return;
    }

    let s = ch;
    let outline_color = widget.text_color;
    let checked = widget.flags & FLAG_CHECKED != 0;

    let fill_color = if widget.press_color != 0 {
        widget.press_color
    } else {
        outline_color
    };

    if widget.kind == KIND_CHECKBOX {
        let r = widget.border_radius;
        if r > 0 {
            let br = r.min(s / 2);
            rounded_rect_screen(lcd, Rect::new(cx, cy, s, s), br, outline_color);
            if checked {
                let inset: i16 = (s as i16) / 4;
                let ir = Rect::new(
                    cx + inset,
                    cy + inset,
                    s.saturating_sub(inset as u16 * 2),
                    s.saturating_sub(inset as u16 * 2),
                );
                let ibr = br.saturating_sub(inset as u16);
                fill_rounded_rect_screen(lcd, ir, ibr, fill_color);
            }
        } else {
            fill_rect_screen(lcd, Rect::new(cx, cy, s, 1), outline_color);
            fill_rect_screen(lcd, Rect::new(cx, cy + s as i16 - 1, s, 1), outline_color);
            fill_rect_screen(lcd, Rect::new(cx, cy, 1, s), outline_color);
            fill_rect_screen(lcd, Rect::new(cx + s as i16 - 1, cy, 1, s), outline_color);
            if checked {
                let x0 = cx + (s as i16) / 4;
                let y0 = cy + (s as i16) / 2;
                let x1 = cx + (s as i16) * 2 / 5;
                let y1 = cy + (s as i16) * 3 / 4;
                let x2 = cx + (s as i16) * 3 / 4;
                let y2 = cy + (s as i16) / 4;
                lcd.draw_line(x0, y0, x1, y1, fill_color);
                lcd.draw_line(x1, y1, x2, y2, fill_color);
                lcd.draw_line(x0 + 1, y0, x1 + 1, y1, fill_color);
                lcd.draw_line(x1 + 1, y1, x2 + 1, y2, fill_color);
            }
        }
    } else {
        // Radio: circle
        let r = (s / 2) as i16;
        let ccx = cx + r;
        let ccy = cy + r;
        lcd.draw_circle(ccx, ccy, r, outline_color);
        if checked {
            let inner_r = r * 2 / 3;
            if inner_r > 0 {
                lcd.fill_circle(ccx, ccy, inner_r, fill_color);
            }
        }
    }
}

fn clear_all_dirty(ctx: &mut Ctx) {
    let dfs = ctx.tree.dfs_order();
    for i in 0..dfs.len() {
        ctx.tree.get_mut(dfs[i]).clear_dirty();
    }
}
