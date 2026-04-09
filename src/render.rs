use crate::clip::ClipRegion;
use crate::ctx::Ctx;
use crate::lcd::{self, Lcd};
use crate::types::{Color, Edges, Rect};
use crate::widget::{
    ALIGN_CENTER, ALIGN_RIGHT, FLAG_CHECKED, FLAG_CLIP_CHILDREN, FLAG_FOCUSED, FLAG_PRESSED,
    KIND_CHECKBOX, KIND_GAUGE, KIND_INPUT, KIND_LABEL, KIND_PROGRESS, KIND_RADIO, KIND_SLIDER,
    Widget, WidgetExt, WidgetId, WidgetTree,
};

const SCREEN: Rect = Rect::new(0, 0, lcd::WIDTH, lcd::HEIGHT);

// --- Buffered render (double-buffered partial redraw) ---

/// Check if the back buffer has any dirty widgets.
pub fn buffered_has_dirty(ctx: &mut Ctx) -> bool {
    let buf = ctx.lcd.back_buf();
    let dfs = ctx.tree.dfs_order();
    for i in 0..dfs.len() {
        if ctx.tree.get(dfs[i]).is_dirty_buf(buf) {
            return true;
        }
    }
    false
}

/// Draw only dirty widgets to the current back buffer.
/// Caller must call lcd.begin_frame() before and lcd.end_frame() after.
/// This allows the caller to draw overlays (keyboard) between widgets and swap.
pub fn render_buffered_content(ctx: &mut Ctx) {
    let buf = ctx.lcd.back_buf();
    let dfs = ctx.tree.dfs_order();

    // Partial redraw: only widgets dirty in this buffer
    for i in 0..dfs.len() {
        let id = dfs[i];
        if !ctx.tree.get(id).is_dirty_buf(buf) || !ctx.tree.is_tree_visible(id) {
            continue;
        }

        let abs = ctx.tree.absolute_rect(id);

        // Viewport clipping for children of scroll containers
        let sp = ctx.tree.scroll_parent(id);
        if sp.is_some() {
            let viewport = ctx.tree.scroll_viewport(sp);
            if abs.x < viewport.x
                || abs.y < viewport.y
                || abs.right() > viewport.right()
                || abs.bottom() > viewport.bottom()
            {
                continue; // outside or partially visible
            }
        }

        draw_widget(ctx, id, &abs);
    }

    // Draw scrollbars on top
    for i in 0..dfs.len() {
        let id = dfs[i];
        if ctx.tree.get(id).flags & FLAG_CLIP_CHILDREN != 0
            && ctx.tree.get(id).is_dirty_buf(buf)
            && ctx.tree.is_tree_visible(id)
        {
            let abs = ctx.tree.absolute_rect(id);
            draw_scrollbar(ctx, id, &abs);
        }
    }

    // Clear only this buffer's dirty flags
    for i in 0..dfs.len() {
        ctx.tree.get_mut(dfs[i]).clear_dirty_buf(buf);
    }
}

// --- Full screen render ---

/// Draw the entire widget tree from scratch (initial draw or full redraw).
/// Iterative DFS using cached order — no recursion, no stack growth.
pub fn render_all(ctx: &mut Ctx) {
    render_all_iterative(ctx);
    clear_all_dirty(ctx);
}

/// Iterative full redraw: walk DFS order, skip invisible subtrees.
/// Children of scroll containers are clipped to the viewport.
fn render_all_iterative(ctx: &mut Ctx) {
    let dfs = ctx.tree.dfs_order();
    let mut i = 0;
    while i < dfs.len() {
        let id = dfs[i];
        if !ctx.tree.is_tree_visible(id) {
            // Skip entire subtree of this invisible widget
            let mut end = i + 1;
            while end < dfs.len() && ctx.tree.is_descendant(dfs[end], id) {
                end += 1;
            }
            i = end;
            continue;
        }
        let abs = ctx.tree.absolute_rect(id);

        // Viewport clipping for children of scroll containers
        let sp = ctx.tree.scroll_parent(id);
        if sp.is_some() {
            let viewport = ctx.tree.scroll_viewport(sp);
            // Only draw if FULLY inside viewport (text can't be partially clipped)
            if abs.x >= viewport.x
                && abs.y >= viewport.y
                && abs.right() <= viewport.right()
                && abs.bottom() <= viewport.bottom()
            {
                draw_widget(ctx, id, &abs);
            } else {
                // Outside or partially visible — skip entire subtree
                let mut end = i + 1;
                while end < dfs.len() && ctx.tree.is_descendant(dfs[end], id) {
                    end += 1;
                }
                i = end;
                continue;
            }
        } else {
            draw_widget(ctx, id, &abs);
        }
        i += 1;
    }

    // Draw scrollbars on top of all children
    for i in 0..dfs.len() {
        let id = dfs[i];
        if ctx.tree.get(id).flags & FLAG_CLIP_CHILDREN != 0 && ctx.tree.is_tree_visible(id) {
            let abs = ctx.tree.absolute_rect(id);
            draw_scrollbar(ctx, id, &abs);
        }
    }
}

// --- Dirty render (painter's algorithm + clip) ---

/// Redraw only dirty widgets.
///
/// Two-pass approach: erase invisible widgets first, then draw visible ones.
/// This prevents hidden panels from overwriting visible siblings that share
/// the same screen area (e.g. tab panels stacked at the same position).
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

    // Pass 1: erase dirty widgets that became invisible
    for di in 0..dfs.len() {
        let id = dfs[di];
        if !ctx.tree.get(id).is_dirty() {
            continue;
        }
        if !ctx.tree.is_tree_visible(id) {
            let abs = ctx.tree.absolute_rect(id);
            if !abs.is_empty() {
                let bg = ancestor_bg(&ctx.tree, id);
                fill_rect_screen(&ctx.lcd, abs, bg);
            }
        }
    }

    // Pass 2: draw dirty visible widgets with clipping
    for di in 0..dfs.len() {
        let id = dfs[di];

        if !ctx.tree.get(id).is_dirty() {
            continue;
        }
        if !ctx.tree.is_tree_visible(id) {
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

        // Flat iteration over dirty widget + its descendants
        for si in di..sub_end {
            let sid = dfs[si];
            if !ctx.tree.is_tree_visible(sid) {
                continue;
            }

            let sabs = ctx.tree.absolute_rect(sid);

            let mut clip = ClipRegion::from_rect(sabs);
            clip.clip_to_bounds(&SCREEN);

            // Viewport clipping for children of scroll containers
            let sp = ctx.tree.scroll_parent(sid);
            if sp.is_some() {
                let viewport = ctx.tree.scroll_viewport(sp);
                // Only draw if fully inside viewport (text can't be partially clipped)
                if sabs.x < viewport.x
                    || sabs.y < viewport.y
                    || sabs.right() > viewport.right()
                    || sabs.bottom() > viewport.bottom()
                {
                    continue;
                }
            }

            for oi in 0..occ_count {
                clip.subtract(&occluders[oi]);
            }

            if !clip.is_empty() {
                draw_widget_clipped(ctx, sid, &sabs, &clip);
            }
        }
    }

    // Draw scrollbars on top of all children (only for dirty scroll containers)
    for di in 0..dfs.len() {
        let id = dfs[di];
        let w = ctx.tree.get(id);
        if w.flags & FLAG_CLIP_CHILDREN != 0 && w.is_dirty() && ctx.tree.is_tree_visible(id) {
            let abs = ctx.tree.absolute_rect(id);
            draw_scrollbar(ctx, id, &abs);
        }
    }

    clear_all_dirty(ctx);
}

// --- Drawing ---

/// Find the nearest ancestor with a non-zero background color.
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
    0
}

/// Effective background color for a widget.
/// If the widget is pressed and has press_color, returns press_color.
/// If a parent is pressed and this widget shares the same background,
/// inherits the parent's press_color.
fn effective_bg(tree: &WidgetTree, widget: &Widget, ext: &WidgetExt) -> Color {
    // This widget is pressed and has a press_color
    if widget.flags & FLAG_PRESSED != 0 && ext.press_color != 0 {
        return ext.press_color;
    }

    // Check ancestor
    let mut pid = widget.parent;
    let max = tree.count();
    let mut depth = 0usize;
    while pid.is_some() {
        let p = tree.get(pid);
        let p_ext = tree.ext(pid).unwrap_or(&WidgetExt::DEFAULT);
        if p.flags & FLAG_PRESSED != 0 && p_ext.press_color != 0 {
            if widget.background_color == p.background_color {
                return p_ext.press_color;
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
    let ext = ctx.tree.ext(id).unwrap_or(&WidgetExt::DEFAULT);
    let b = ext.border;
    let r = ext.border_radius;
    let bg_color = effective_bg(&ctx.tree, widget, ext);
    let draw_bg = bg_color != 0 || widget.kind != KIND_LABEL;

    if r > 0 {
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
            let bg = inner_rect(abs, &b);
            if !bg.is_empty() {
                let inner_r = r.saturating_sub(b.top.max(b.left) as u16);
                fill_rounded_rect_screen(&ctx.lcd, bg, inner_r, bg_color);
            }
        }
    } else {
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
        if draw_bg {
            let bg = inner_rect(abs, &b);
            if !bg.is_empty() {
                fill_rect_screen(&ctx.lcd, bg, bg_color);
            }
        }
    }

    // Background image
    if ext.image_id != 0 {
        let bg = inner_rect(abs, &b);
        draw_bg_image(ctx, ext.image_id, &bg);
    }

    // Label text
    if widget.kind == KIND_LABEL {
        draw_label_text(ctx, widget, abs, ext);
    }

    // Progress bar / slider fill
    if widget.kind == KIND_PROGRESS || widget.kind == KIND_SLIDER {
        let inner = inner_rect(abs, &b);
        draw_value_fill(&ctx.lcd, widget, &inner, ext);
    }

    // Checkbox / radio indicator
    if widget.kind == KIND_CHECKBOX || widget.kind == KIND_RADIO {
        let inner = inner_rect(abs, &b);
        draw_check_indicator(&ctx.lcd, widget, &inner, ext);
    }

    // Input text + cursor
    if widget.kind == KIND_INPUT {
        draw_input_text(ctx, widget, abs, ext);
    }

    // Gauge arc
    if widget.kind == KIND_GAUGE {
        let inner = inner_rect(abs, &ext.border);
        draw_gauge(&ctx.lcd, widget, &inner, ext);
    }
}

/// Draw widget with clip region (dirty render path).
fn draw_widget_clipped(ctx: &Ctx, id: WidgetId, abs: &Rect, clip: &ClipRegion) {
    let widget = ctx.tree.get(id);
    let ext = ctx.tree.ext(id).unwrap_or(&WidgetExt::DEFAULT);
    let b = ext.border;
    let r = ext.border_radius;
    let bg_color = effective_bg(&ctx.tree, widget, ext);
    let draw_bg = bg_color != 0 || widget.kind != KIND_LABEL;

    if r > 0 {
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
            let bg = inner_rect(abs, &b);
            if !bg.is_empty() {
                let inner_r = r.saturating_sub(b.top.max(b.left) as u16);
                fill_rounded_rect_screen(&ctx.lcd, bg, inner_r, bg_color);
            }
        }
    } else {
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

        if draw_bg {
            let bg = inner_rect(abs, &b);
            if !bg.is_empty() {
                fill_clipped(&ctx.lcd, &bg, bg_color, clip);
            }
        }
    }

    // Background image
    if ext.image_id != 0 {
        let bg = inner_rect(abs, &b);
        draw_bg_image(ctx, ext.image_id, &bg);
    }

    // Label text
    if widget.kind == KIND_LABEL {
        draw_label_text(ctx, widget, abs, ext);
    }

    // Progress bar / slider fill
    if widget.kind == KIND_PROGRESS || widget.kind == KIND_SLIDER {
        let inner = inner_rect(abs, &b);
        draw_value_fill(&ctx.lcd, widget, &inner, ext);
    }

    // Checkbox / radio indicator
    if widget.kind == KIND_CHECKBOX || widget.kind == KIND_RADIO {
        let inner = inner_rect(abs, &b);
        draw_check_indicator(&ctx.lcd, widget, &inner, ext);
    }

    // Input text + cursor
    if widget.kind == KIND_INPUT {
        draw_input_text(ctx, widget, abs, ext);
    }

    // Gauge arc
    if widget.kind == KIND_GAUGE {
        let inner = inner_rect(abs, &ext.border);
        draw_gauge(&ctx.lcd, widget, &inner, ext);
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
fn draw_label_text(ctx: &Ctx, widget: &Widget, abs: &Rect, ext: &WidgetExt) {
    if ext.text_id == 0xFFFF || ext.font_id == 0xFF {
        return;
    }
    let text = ctx.strpool.get(ext.text_id);
    if text.is_empty() {
        return;
    }
    let font = match ctx.fonts.resolve(ext.font_id) {
        Some(f) => f,
        None => return,
    };

    let b = ext.border;
    let p = ext.padding;
    let cx = abs.x + b.left as i16 + p.left as i16;
    let cy = abs.y + b.top as i16 + p.top as i16;
    let cw = abs
        .w
        .saturating_sub(b.left as u16 + b.right as u16 + p.left as u16 + p.right as u16);
    let ch = abs
        .h
        .saturating_sub(b.top as u16 + b.bottom as u16 + p.top as u16 + p.bottom as u16);

    if cw == 0 || ch == 0 {
        return;
    }

    let tw = font.text_width(text) as i16;
    let lh = font.line_height() as i16;

    let tx = match ext.text_align {
        ALIGN_CENTER => cx + (cw as i16 - tw) / 2,
        ALIGN_RIGHT => cx + cw as i16 - tw,
        _ => cx,
    };

    let ty = cy + (ch as i16 - lh) / 2 + (lh * 3) / 4;

    let bg_color = effective_bg(&ctx.tree, widget, ext);
    let bg = if bg_color == 0 && widget.kind == KIND_LABEL {
        None
    } else {
        Some(bg_color)
    };
    font.draw_str(&ctx.lcd, &ctx.flash, text, tx, ty, ext.text_color, bg);
}

/// Draw input text with cursor in the content area.
fn draw_input_text(ctx: &Ctx, widget: &Widget, abs: &Rect, ext: &WidgetExt) {
    let font = match ctx.fonts.resolve(ext.font_id) {
        Some(f) => f,
        None => return,
    };

    let b = ext.border;
    let p = ext.padding;
    let cx = abs.x + b.left as i16 + p.left as i16;
    let cy = abs.y + b.top as i16 + p.top as i16;
    let cw = abs
        .w
        .saturating_sub(b.left as u16 + b.right as u16 + p.left as u16 + p.right as u16);
    let ch = abs
        .h
        .saturating_sub(b.top as u16 + b.bottom as u16 + p.top as u16 + p.bottom as u16);

    if cw == 0 || ch == 0 {
        return;
    }

    let lh = font.line_height() as i16;
    let ty = cy + (ch as i16 - lh) / 2 + (lh * 3) / 4;

    let bg_color = effective_bg(&ctx.tree, widget, ext);
    let bg = if bg_color == 0 { None } else { Some(bg_color) };

    // Draw text if present
    let has_text = ext.text_id != 0xFFFF && ext.font_id != 0xFF;
    if has_text {
        let text = ctx.strpool.get(ext.text_id);
        if !text.is_empty() {
            font.draw_str(&ctx.lcd, &ctx.flash, text, cx, ty, ext.text_color, bg);
        }
    }

    // Draw cursor when focused
    if widget.flags & FLAG_FOCUSED != 0 && ctx.cursor_visible {
        let cursor_pos = ext.value.max(0) as usize;
        let mut cursor_x = cx;
        if has_text {
            let text = ctx.strpool.get(ext.text_id);
            let end = cursor_pos.min(text.len());
            for i in 0..end {
                cursor_x += font.char_width(text[i] as char) as i16;
            }
        }
        // Draw 2px wide cursor line
        let cursor_y = cy + (ch as i16 - lh) / 2;
        let cursor_h = lh as u16;
        if cursor_x >= cx && cursor_x < cx + cw as i16 {
            fill_rect_screen(&ctx.lcd, Rect::new(cursor_x, cursor_y, 2, cursor_h), ext.text_color);
        }
    }
}

/// Draw gauge arc/dial inside the inner rect.
///
/// 270° arc from 135° (bottom-left) to 45° (bottom-right), clockwise through top.
/// - border_color: track arc color
/// - press_color: filled arc color (value portion)
/// - text_color: needle color
/// - value: 0-100 gauge position
fn draw_gauge(lcd: &Lcd, widget: &Widget, inner: &Rect, ext: &WidgetExt) {
    if inner.is_empty() {
        return;
    }

    // Gauge geometry
    let cx = inner.x + inner.w as i16 / 2;
    let cy = inner.y + inner.h as i16 / 2;
    let r = (inner.w.min(inner.h) / 2).saturating_sub(2) as i16;
    if r < 8 {
        return;
    }

    // Arc angles: 135° (start) → 45° (end), 270° sweep
    const ARC_START: i16 = 135;
    const ARC_END: i16 = 45;
    const ARC_SWEEP: i32 = 270;

    let val = ext.value.clamp(0, 100) as i32;
    let value_angle = (ARC_START as i32 + val * ARC_SWEEP / 100) % 360;

    // Arc thickness: ~8% of radius, min 3px
    let thickness = (r / 12).max(3) as i16;
    let outer_r = r;
    let inner_r = r - thickness;

    // Draw track arc (full 270°)
    let track_color = if widget.border_color != 0 {
        widget.border_color
    } else {
        0x3186 // default dark gray
    };
    for dr in inner_r..=outer_r {
        lcd.draw_arc(cx, cy, dr, ARC_START, ARC_END, track_color);
    }

    // Draw value arc (filled portion)
    if val > 0 && ext.press_color != 0 {
        let end = if val >= 100 { ARC_END } else { value_angle as i16 };
        for dr in inner_r..=outer_r {
            lcd.draw_arc(cx, cy, dr, ARC_START, end, ext.press_color);
        }
    }

    // Draw needle
    let needle_color = if ext.text_color != 0 && ext.text_color != 0xFFFF {
        ext.text_color
    } else {
        0xFFFF // white default
    };
    let needle_r = inner_r - 4;
    if needle_r > 4 {
        let (sin_v, cos_v) = lcd::sin_cos_deg(value_angle as i16);
        let nx = cx + ((needle_r as i32 * cos_v as i32) >> 8) as i16;
        let ny = cy + ((needle_r as i32 * sin_v as i32) >> 8) as i16;
        lcd.draw_line(cx, cy, nx, ny, needle_color);
        // Draw thicker needle (2 extra lines offset by 1px)
        lcd.draw_line(cx + 1, cy, nx + 1, ny, needle_color);
        lcd.draw_line(cx, cy + 1, nx, ny + 1, needle_color);
    }

    // Center dot
    lcd.fill_circle(cx, cy, 3, needle_color);

    // Draw tick marks at 0%, 25%, 50%, 75%, 100%
    let tick_color = track_color;
    for i in 0..=4u32 {
        let tick_angle = (ARC_START as i32 + i as i32 * ARC_SWEEP / 4) % 360;
        let (sin_v, cos_v) = lcd::sin_cos_deg(tick_angle as i16);
        let tx0 = cx + (((outer_r + 2) as i32 * cos_v as i32) >> 8) as i16;
        let ty0 = cy + (((outer_r + 2) as i32 * sin_v as i32) >> 8) as i16;
        let tx1 = cx + (((outer_r + 6) as i32 * cos_v as i32) >> 8) as i16;
        let ty1 = cy + (((outer_r + 6) as i32 * sin_v as i32) >> 8) as i16;
        lcd.draw_line(tx0, ty0, tx1, ty1, tick_color);
    }
}

/// Scrollbar constants
pub const SCROLLBAR_W: u16 = 10;
const SCROLLBAR_TRACK: Color = 0x18C3;  // dark gray
const SCROLLBAR_THUMB: Color = 0x6B4D;  // medium gray
const SCROLLBAR_MIN_THUMB: u16 = 16;    // minimum thumb height

/// Draw scrollbar on the right side of a scroll container.
fn draw_scrollbar(ctx: &Ctx, id: WidgetId, abs: &Rect) {
    let content_h = ctx.tree.content_height(id);
    let cr = ctx.tree.content_rect(id);
    if cr.h == 0 || content_h <= cr.h {
        return; // no scrollbar needed — all content fits
    }

    let scroll_y = ctx.tree.value(id).max(0) as u32;
    let ch = content_h as u32;
    let vh = cr.h as u32;

    // Track: right edge of content area
    let track_x = cr.x + cr.w as i16 - SCROLLBAR_W as i16;
    let track_y = cr.y;
    let track_h = cr.h;
    fill_rect_screen(&ctx.lcd, Rect::new(track_x, track_y, SCROLLBAR_W, track_h), SCROLLBAR_TRACK);

    // Thumb: proportional size and position
    let thumb_h = ((vh * track_h as u32) / ch).max(SCROLLBAR_MIN_THUMB as u32).min(track_h as u32) as u16;
    let max_scroll = ch - vh;
    let scroll_range = track_h.saturating_sub(thumb_h) as u32;
    let thumb_y = if max_scroll > 0 {
        track_y + ((scroll_y * scroll_range) / max_scroll) as i16
    } else {
        track_y
    };
    fill_rect_screen(&ctx.lcd, Rect::new(track_x, thumb_y, SCROLLBAR_W, thumb_h), SCROLLBAR_THUMB);
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
fn inner_rect(abs: &Rect, border: &Edges) -> Rect {
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
fn draw_value_fill(lcd: &Lcd, widget: &Widget, inner: &Rect, ext: &WidgetExt) {
    if inner.is_empty() || ext.press_color == 0 {
        return;
    }

    let r = ext.border_radius;
    let val = ext.value.clamp(0, 100) as u32;
    let fill_w = ((inner.w as u32) * val / 100) as u16;

    if fill_w > 0 {
        let fill = Rect::new(inner.x, inner.y, fill_w, inner.h);
        if r > 0 {
            let fill_r = r.min(fill_w / 2).min(inner.h / 2);
            fill_rounded_rect_screen(lcd, fill, fill_r, ext.press_color);
        } else {
            fill_rect_screen(lcd, fill, ext.press_color);
        }
    }

    // Slider thumb
    if widget.kind == KIND_SLIDER && inner.w > 0 {
        let thumb_w: u16 = 6;
        let thumb_x = inner.x + fill_w as i16 - (thumb_w as i16 / 2);
        let thumb_x = thumb_x
            .max(inner.x)
            .min(inner.x + inner.w as i16 - thumb_w as i16);
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

/// Draw checkbox or radio indicator inside the inner rect.
fn draw_check_indicator(lcd: &Lcd, widget: &Widget, inner: &Rect, ext: &WidgetExt) {
    if inner.is_empty() {
        return;
    }

    let p = ext.padding;
    let r = ext.border_radius;
    let cx = inner.x + p.left as i16;
    let cy = inner.y + p.top as i16;
    let ch = inner.h.saturating_sub(p.top as u16 + p.bottom as u16);

    if ch < 4 {
        return;
    }

    let s = ch;
    let outline_color = ext.text_color;
    let checked = widget.flags & FLAG_CHECKED != 0;

    let fill_color = if ext.press_color != 0 {
        ext.press_color
    } else {
        outline_color
    };

    if widget.kind == KIND_CHECKBOX {
        if r > 0 {
            let br = r.min(s / 2);
            rounded_rect_screen(&lcd, Rect::new(cx, cy, s, s), br, outline_color);
            if checked {
                let inset: i16 = (s as i16) / 4;
                let ir = Rect::new(
                    cx + inset,
                    cy + inset,
                    s.saturating_sub(inset as u16 * 2),
                    s.saturating_sub(inset as u16 * 2),
                );
                let ibr = br.saturating_sub(inset as u16);
                fill_rounded_rect_screen(&lcd, ir, ibr, fill_color);
            }
        } else {
            fill_rect_screen(&lcd, Rect::new(cx, cy, s, 1), outline_color);
            fill_rect_screen(&lcd, Rect::new(cx, cy + s as i16 - 1, s, 1), outline_color);
            fill_rect_screen(&lcd, Rect::new(cx, cy, 1, s), outline_color);
            fill_rect_screen(&lcd, Rect::new(cx + s as i16 - 1, cy, 1, s), outline_color);
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
        let rc = (s / 2) as i16;
        let ccx = cx + rc;
        let ccy = cy + rc;
        lcd.draw_circle(ccx, ccy, rc, outline_color);
        if checked {
            let inner_r = rc * 2 / 3;
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
