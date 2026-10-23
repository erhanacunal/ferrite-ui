use crate::clip::ClipRegion;
use crate::ctx::Ctx;
use crate::lcd::{self};
use crate::types::{Color, Edges, Offset, Rect};
use crate::vm::Vm;
use crate::lcd::{LcdBackend, LcdImpl, Underlay};
use crate::platform::Platform;
use crate::widget::{
    ALIGN_CENTER, ALIGN_RIGHT, FLAG_CHECKED, FLAG_CLIP_CHILDREN, FLAG_FOCUSED, FLAG_MULTI_LINE,
    FLAG_HAS_ON_PAINT, FLAG_PAINTED, FLAG_PRESSED, FLAG_RENDERED, KIND_CHECKBOX, KIND_CIRCLE, KIND_DROPDOWN, KIND_ELLIPSE, KIND_GAUGE,
    KIND_GRAPH, KIND_IMAGE, KIND_INPUT, KIND_LABEL, KIND_LINE, KIND_POLYGON, KIND_PROGRESS,
    KIND_BUTTON_GROUP, KIND_RADIO, KIND_SLIDER, Widget, WidgetExt, WidgetId, WidgetTree,
};

/// Screen-bounds rect: the panel size from `LcdBackend::WIDTH`/`HEIGHT`,
/// swapped in portrait mode (`LcdImpl::width`/`height`).
#[inline]
fn screen_rect<B: LcdBackend>(lcd: &LcdImpl<B>) -> Rect {
    Rect::new(0, 0, lcd.width(), lcd.height())
}

// --- Buffered render (double-buffered partial redraw) ---

/// Check if the back buffer has any dirty widgets.
pub fn buffered_has_dirty<P: Platform>(ctx: &mut Ctx<P>) -> bool {
    let buf = ctx.lcd.back_buf();
    ctx.tree.ensure_dfs();
    for i in 0..ctx.tree.dfs_len() {
        if ctx.tree.get(ctx.tree.dfs_at(i)).is_dirty_buf(buf) {
            return true;
        }
    }
    false
}

/// Widgets whose `on_paint` must run right after the render pass that drew
/// them (fixed capacity — no heap per frame). See `Vm::run_paint`.
pub struct PaintList {
    ids: [WidgetId; PAINT_MAX],
    len: usize,
}

/// Capacity of a `PaintList` (on_paint widgets per frame).
pub const PAINT_MAX: usize = 64;

impl PaintList {
    pub fn new() -> Self {
        Self {
            ids: [WidgetId::NONE; PAINT_MAX],
            len: 0,
        }
    }

    pub fn push(&mut self, id: WidgetId) {
        if self.len < PAINT_MAX {
            self.ids[self.len] = id;
            self.len += 1;
        }
    }

    pub fn ids(&self) -> &[WidgetId] {
        &self.ids[..self.len]
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Dirty-mode counterpart of the list `render_buffered_content` returns: the
/// dirty on_paint widgets, collected *before* `render_dirty` clears the flags.
pub fn collect_dirty_paint<P: Platform>(ctx: &mut Ctx<P>, out: &mut PaintList) {
    ctx.tree.ensure_dfs();
    for i in 0..ctx.tree.dfs_len() {
        let id = ctx.tree.dfs_at(i);
        let w = ctx.tree.get(id);
        if w.is_dirty() && w.flags & FLAG_HAS_ON_PAINT != 0 {
            out.push(id);
        }
    }
}

/// Draw dirty widgets to the current back buffer. For each dirty widget, if
/// its previously-drawn rect in this buffer differs from the current rect,
/// the widgets behind it are redrawn clipped to the stale rect first — so
/// moving widgets don't leave a trail. Per-buffer `prev_rect` tracking keeps
/// the erase area bounded even under continuous motion.
/// Caller must bracket with lcd.begin_frame() / lcd.end_frame(), and run
/// `on_paint` for the returned widgets in between (`Vm::run_paint`): their
/// custom drawing belongs to this back buffer, on top of the background just
/// drawn — painting after the swap would reach only one of the two buffers.
pub fn render_buffered_content<P: Platform>(ctx: &mut Ctx<P>, vm: &Vm) -> PaintList {
    let mut paint = PaintList::new();
    let buf = ctx.lcd.back_buf();
    ctx.tree.ensure_dfs();
    // Whole-screen clip for the unclipped Pass-2 draws (built once).
    let full = ClipRegion::from_rect(screen_rect(&ctx.lcd));

    // Pass 1: for each dirty widget whose rect in THIS buffer is stale,
    // redraw everything behind it clipped to the stale rect.
    for i in 0..ctx.tree.dfs_len() {
        let id = ctx.tree.dfs_at(i);
        let w = ctx.tree.get(id);
        if !w.is_dirty_buf(buf) {
            continue;
        }
        let stale = buf_prev_rect(&ctx.tree, id, buf);
        if stale.is_empty() {
            continue;
        }
        let current = if w.is_visible() {
            ctx.tree.absolute_rect(id)
        } else {
            Rect::new(0, 0, 0, 0)
        };
        if stale == current {
            continue;
        } // rect unchanged — no trail to clean
        redraw_behind(ctx, vm, i, stale);
    }

    // Pass 2: draw dirty visible widgets at their current rect
    clear_painted(&mut ctx.tree);
    for i in 0..ctx.tree.dfs_len() {
        let id = ctx.tree.dfs_at(i);
        let w = ctx.tree.get(id);
        if !w.is_dirty_buf(buf) || !ctx.tree.is_tree_visible(id) {
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

        // Translucent widgets and transparent labels must not blend over
        // their own previous output in this buffer — restore what is behind
        // them first, unless this pass already repainted all of it.
        if ctx.lcd.has_alpha() && shows_behind(&ctx.tree, id) && !behind_is_fresh(&ctx.tree, 0, i, &abs)
        {
            redraw_behind(ctx, vm, i, abs);
        }
        draw_widget(ctx, vm, id, &abs, &full);
        ctx.tree.get_mut(id).flags |= FLAG_RENDERED | FLAG_PAINTED;
        if ctx.tree.get(id).flags & FLAG_HAS_ON_PAINT != 0 {
            paint.push(id);
        }
    }

    // Draw scrollbars on top
    for i in 0..ctx.tree.dfs_len() {
        let id = ctx.tree.dfs_at(i);
        if ctx.tree.get(id).flags & FLAG_CLIP_CHILDREN != 0
            && ctx.tree.get(id).is_dirty_buf(buf)
            && ctx.tree.is_tree_visible(id)
        {
            let abs = ctx.tree.absolute_rect(id);
            draw_scrollbar(ctx, id, &abs);
        }
    }

    clear_painted(&mut ctx.tree);

    // Record this buffer's state and clear its dirty flag for each widget.
    for i in 0..ctx.tree.dfs_len() {
        let id = ctx.tree.dfs_at(i);
        if ctx.tree.get(id).is_dirty_buf(buf) {
            let drawn_rect = if ctx.tree.is_tree_visible(id) {
                ctx.tree.absolute_rect(id)
            } else {
                Rect::new(0, 0, 0, 0)
            };
            set_buf_prev_rect(&mut ctx.tree, id, buf, drawn_rect);
        }
        ctx.tree.get_mut(id).clear_dirty_buf(buf);
    }
    paint
}

/// Read this widget's prev_rect for the given buffer index (0=A, 1=B).
fn buf_prev_rect(tree: &WidgetTree, id: WidgetId, buf: u8) -> Rect {
    match tree.ext(id) {
        Some(ext) if buf == 0 => ext.prev_rect_a,
        Some(ext) => ext.prev_rect_b,
        None => Rect::new(0, 0, 0, 0),
    }
}

/// Store this widget's drawn rect for the given buffer. Allocates ext on
/// demand — returns silently if the widget isn't visible AND the stored
/// rect would be empty (nothing to track).
fn set_buf_prev_rect(tree: &mut WidgetTree, id: WidgetId, buf: u8, rect: Rect) {
    // Skip if both old and new are empty — nothing to remember.
    if rect.is_empty() {
        if tree
            .ext(id)
            .map(|e| {
                if buf == 0 {
                    e.prev_rect_a.is_empty()
                } else {
                    e.prev_rect_b.is_empty()
                }
            })
            .unwrap_or(true)
        {
            return;
        }
    }
    if let Some(ext) = tree.ensure_ext(id) {
        if buf == 0 {
            ext.prev_rect_a = rect;
        } else {
            ext.prev_rect_b = rect;
        }
    }
}

/// Redraw widgets that sit behind widget `target_idx` (earlier in DFS order),
/// clipped to `stale`. Used to restore pixels under a moved/hidden widget.
/// Reads the DFS order from the tree cache — ensure_dfs must have been called
/// before entering the enclosing render loop.
fn redraw_behind<P: Platform>(ctx: &mut Ctx<P>, vm: &Vm, target_idx: usize, stale: Rect) {
    let mut clip = ClipRegion::from_rect(stale);
    clip.clip_to_bounds(&screen_rect(&ctx.lcd));
    if clip.is_empty() {
        return;
    }
    redraw_behind_region(ctx, vm, target_idx, &clip, &stale);
}

/// `redraw_behind` over a prebuilt clip region (`bounds` is its enclosing
/// rect, used for the cheap intersection pre-test). Also used to restore the
/// pixels under a translucent widget before it is re-blended — without this,
/// a widget with `alpha < 255` redrawn in place blends over its own previous
/// output and accumulates toward solid a little more every frame.
fn redraw_behind_region<P: Platform>(
    ctx: &mut Ctx<P>,
    vm: &Vm,
    target_idx: usize,
    clip: &ClipRegion,
    bounds: &Rect,
) {
    for i in 0..target_idx {
        let uid = ctx.tree.dfs_at(i);
        if !ctx.tree.is_tree_visible(uid) {
            continue;
        }
        let u_abs = ctx.tree.absolute_rect(uid);
        if !u_abs.intersects(bounds) {
            continue;
        }
        draw_widget(ctx, vm, uid, &u_abs, clip);
    }
}

// --- Full screen render ---

/// Draw the entire widget tree from scratch (initial draw or full redraw).
/// Iterative DFS using cached order — no recursion, no stack growth.
pub fn render_all<P: Platform>(ctx: &mut Ctx<P>, vm: &Vm) {
    render_all_iterative(ctx, vm);
    clear_all_dirty(ctx);
}

/// Iterative full redraw: walk DFS order, skip invisible subtrees.
/// Children of scroll containers are clipped to the viewport.
fn render_all_iterative<P: Platform>(ctx: &mut Ctx<P>, vm: &Vm) {
    ctx.tree.ensure_dfs();
    // Whole-screen clip for the full redraw — built once, reused for every
    // widget so we don't pay region setup in the per-widget loop.
    let full = ClipRegion::from_rect(screen_rect(&ctx.lcd));
    let mut i = 0;
    while i < ctx.tree.dfs_len() {
        let id = ctx.tree.dfs_at(i);
        if !ctx.tree.is_tree_visible(id) {
            // Skip entire subtree of this invisible widget
            let mut end = i + 1;
            while end < ctx.tree.dfs_len() && ctx.tree.is_descendant(ctx.tree.dfs_at(end), id) {
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
                draw_widget(ctx, vm, id, &abs, &full);
                ctx.tree.get_mut(id).flags |= FLAG_RENDERED;
            } else {
                // Outside or partially visible — skip entire subtree
                let mut end = i + 1;
                while end < ctx.tree.dfs_len() && ctx.tree.is_descendant(ctx.tree.dfs_at(end), id) {
                    end += 1;
                }
                i = end;
                continue;
            }
        } else {
            draw_widget(ctx, vm, id, &abs, &full);
            ctx.tree.get_mut(id).flags |= FLAG_RENDERED;
        }
        i += 1;
    }

    // Draw scrollbars on top of all children
    for i in 0..ctx.tree.dfs_len() {
        let id = ctx.tree.dfs_at(i);
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
pub fn render_dirty<P: Platform>(ctx: &mut Ctx<P>, vm: &Vm) {
    ctx.tree.ensure_dfs();

    let mut has_dirty = false;
    for i in 0..ctx.tree.dfs_len() {
        if ctx.tree.get(ctx.tree.dfs_at(i)).is_dirty() {
            has_dirty = true;
            break;
        }
    }
    if !has_dirty {
        return;
    }

    // Pass 1: erase dirty widgets that became invisible.
    // Only erase widgets that were actually drawn before (FLAG_RENDERED).
    // Without this gate, an animated widget on a hidden page (e.g. a progress
    // bar still being ticked by loop()) would repaint its stale rect every
    // frame with the ancestor bg, overwriting whatever sibling page is shown.
    for di in 0..ctx.tree.dfs_len() {
        let id = ctx.tree.dfs_at(di);
        if !ctx.tree.get(id).is_dirty() {
            continue;
        }
        if !ctx.tree.is_tree_visible(id) {
            if ctx.tree.get(id).flags & FLAG_RENDERED != 0 {
                let abs = ctx.tree.absolute_rect(id);
                if !abs.is_empty() {
                    erase_widget_area(&ctx.tree, &ctx.lcd, id, abs);
                }
                ctx.tree.get_mut(id).flags &= !FLAG_RENDERED;
            }
        }
    }

    // Pass 1b: for each dirty visible widget whose rect in buffer A is stale,
    // redraw everything behind it clipped to the stale rect (restores pixels
    // hidden by the widget's previous position).
    for di in 0..ctx.tree.dfs_len() {
        let id = ctx.tree.dfs_at(di);
        let w = ctx.tree.get(id);
        if !w.is_dirty() {
            continue;
        }
        let stale = buf_prev_rect(&ctx.tree, id, 0);
        if stale.is_empty() {
            continue;
        }
        let current = if w.is_visible() {
            ctx.tree.absolute_rect(id)
        } else {
            Rect::new(0, 0, 0, 0)
        };
        if stale == current {
            continue;
        }
        redraw_behind(ctx, vm, di, stale);
    }

    // Pass 2: draw dirty visible widgets with clipping. A dirty widget is
    // drawn together with its whole subtree, after which the loop resumes past
    // that subtree: `mark_dirty` marks every descendant too, so visiting them
    // again would redraw each nested level once more (a label three levels
    // deep was drawn four times).
    clear_painted(&mut ctx.tree);
    let mut di = 0;
    while di < ctx.tree.dfs_len() {
        let id = ctx.tree.dfs_at(di);

        if !ctx.tree.get(id).is_dirty() || !ctx.tree.is_tree_visible(id) {
            di += 1;
            continue;
        }

        // Find subtree end: contiguous descendants after di
        let mut sub_end = di + 1;
        while sub_end < ctx.tree.dfs_len() && ctx.tree.is_descendant(ctx.tree.dfs_at(sub_end), id) {
            sub_end += 1;
        }

        // Collect occluder rects: visible widgets after this subtree in DFS
        let mut occluders = [Rect::new(0, 0, 0, 0); 32];
        let mut occ_count: usize = 0;
        let abs = ctx.tree.absolute_rect(id);

        for j in sub_end..ctx.tree.dfs_len() {
            if ctx.tree.is_tree_visible(ctx.tree.dfs_at(j)) {
                let other_abs = ctx.tree.absolute_rect(ctx.tree.dfs_at(j));
                if abs.intersects(&other_abs) && occ_count < 32 {
                    occluders[occ_count] = other_abs;
                    occ_count += 1;
                }
            }
        }

        // Flat iteration over dirty widget + its descendants
        for si in di..sub_end {
            let sid = ctx.tree.dfs_at(si);
            if !ctx.tree.is_tree_visible(sid) {
                continue;
            }

            let sabs = ctx.tree.absolute_rect(sid);

            let mut clip = ClipRegion::from_rect(sabs);
            clip.clip_to_bounds(&screen_rect(&ctx.lcd));

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
                // Translucent widgets and transparent labels must not blend
                // over their own previous output — restore what is behind
                // them (same clip) first, unless this subtree's pass already
                // repainted all of it. Only widgets of this subtree count:
                // they share its occluder list, so they were painted
                // everywhere this widget's clip reaches.
                if ctx.lcd.has_alpha()
                    && shows_behind(&ctx.tree, sid)
                    && !behind_is_fresh(&ctx.tree, di, si, &sabs)
                {
                    redraw_behind_region(ctx, vm, si, &clip, &sabs);
                }
                draw_widget(ctx, vm, sid, &sabs, &clip);
                ctx.tree.get_mut(sid).flags |= FLAG_RENDERED | FLAG_PAINTED;
            }
        }
        di = sub_end;
    }
    clear_painted(&mut ctx.tree);

    // Draw scrollbars on top of all children (only for dirty scroll containers)
    for di in 0..ctx.tree.dfs_len() {
        let id = ctx.tree.dfs_at(di);
        let w = ctx.tree.get(id);
        if w.flags & FLAG_CLIP_CHILDREN != 0 && w.is_dirty() && ctx.tree.is_tree_visible(id) {
            let abs = ctx.tree.absolute_rect(id);
            draw_scrollbar(ctx, id, &abs);
        }
    }

    clear_all_dirty(ctx);
}

/// Clear the per-pass `FLAG_PAINTED` marks.
fn clear_painted(tree: &mut WidgetTree) {
    for i in 0..tree.dfs_len() {
        let id = tree.dfs_at(i);
        tree.get_mut(id).flags &= !FLAG_PAINTED;
    }
}

/// True when the pixels behind `rect` (what DFS positions before `idx`
/// show there) were all repainted earlier in this pass, so restoring them
/// for a see-through widget at `idx` would only draw the same pixels again.
///
/// Walks back from `idx` (down to `lo`) over the visible widgets that touch
/// `rect`: every one must be `FLAG_PAINTED`, until one is found that paints
/// all of `rect` opaquely — nothing behind that one can show through.
fn behind_is_fresh(tree: &WidgetTree, lo: usize, idx: usize, rect: &Rect) -> bool {
    for k in (lo..idx).rev() {
        let id = tree.dfs_at(k);
        if !tree.is_tree_visible(id) {
            continue;
        }
        let abs = tree.absolute_rect(id);
        if !abs.intersects(rect) {
            continue;
        }
        if tree.get(id).flags & FLAG_PAINTED == 0 {
            return false;
        }
        if covers_opaquely(tree, id, &abs, rect) {
            return true;
        }
    }
    false
}

/// Does `draw_widget` paint every pixel of `rect` opaquely for `id` (whose
/// absolute rect is `abs`)? True for a solid, square-cornered widget with a
/// background: its border strips plus background fill tile its whole rect.
fn covers_opaquely(tree: &WidgetTree, id: WidgetId, abs: &Rect, rect: &Rect) -> bool {
    let w = tree.get(id);
    let ext = tree.ext(id).unwrap_or(&WidgetExt::DEFAULT);
    if is_freeform_shape(w.kind) || ext.border_radius != 0 || ext.alpha < 255 {
        return false;
    }
    // Same condition as `draw_bg` in `draw_widget`.
    let bg = effective_bg(tree, w, ext);
    if bg == 0 && (w.kind == KIND_LABEL || w.kind == KIND_IMAGE) {
        return false;
    }
    rect.x >= abs.x && rect.y >= abs.y && rect.right() <= abs.right() && rect.bottom() <= abs.bottom()
}

/// True when what is behind `id` shows through it: a translucent
/// background, a label without a background of its own (its text is
/// drawn straight over the content behind it on readback backends), or an
/// anti-aliased outline — rounded corners and freeform shapes blend their
/// edge pixels over what is behind (only asked on readback backends).
fn shows_behind(tree: &WidgetTree, id: WidgetId) -> bool {
    if tree.alpha(id) < 255 {
        return true;
    }
    let w = tree.get(id);
    if is_freeform_shape(w.kind) || tree.ext(id).is_some_and(|e| e.border_radius != 0) {
        return true;
    }
    w.kind == KIND_LABEL
        && effective_bg(tree, w, tree.ext(id).unwrap_or(&WidgetExt::DEFAULT)) == 0
}

// --- Drawing ---

/// Effective background color for a widget.
/// If the widget is pressed and has press_color, returns press_color.
/// If a parent is pressed and this widget shares the same background,
/// inherits the parent's press_color — unless it is transparent (a label or
/// image without a background paints none): the pressed parent's fill, with
/// its rounded corners, already shows through, and a filled copy would be a
/// square box over it.
fn effective_bg(tree: &WidgetTree, widget: &Widget, ext: &WidgetExt) -> Color {
    // This widget is pressed (or a selected group button) and has a press_color
    if shows_press(widget) && ext.press_color != 0 {
        return ext.press_color;
    }
    if widget.background_color == 0 && (widget.kind == KIND_LABEL || widget.kind == KIND_IMAGE) {
        return 0;
    }

    // Check ancestor
    let mut pid = widget.parent;
    let max = tree.count();
    let mut depth = 0usize;
    while pid.is_some() {
        let p = tree.get(pid);
        let p_ext = tree.ext(pid).unwrap_or(&WidgetExt::DEFAULT);
        if shows_press(p) && p_ext.press_color != 0 {
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

/// Pressed, or a selected (CHECKED) KIND_BUTTON_GROUP widget: both are
/// drawn with `press_color`.
fn shows_press(w: &Widget) -> bool {
    w.flags & FLAG_PRESSED != 0
        || (w.kind == KIND_BUTTON_GROUP && w.flags & FLAG_CHECKED != 0)
}

/// Walk up the widget tree to find the actual visible background color for
/// text rendering.  When a label has `background_color = 0` it means
/// "transparent" — the text sits on its parent's filled background.  This
/// function resolves that parent colour so anti-aliased glyphs blend
/// correctly instead of blending against black.
fn resolved_text_bg(tree: &WidgetTree, widget: &Widget, ext: &WidgetExt, fallback: Color) -> Color {
    // If this widget has a non-zero background, use it directly (after
    // factoring in press state).
    let bg = effective_bg(tree, widget, ext);
    if bg != 0 {
        return bg;
    }

    // Walk up the tree to find the first ancestor with a non-zero
    // background_color, respecting press state along the way.
    let mut pid = widget.parent;
    let max = tree.count();
    let mut depth = 0usize;
    while pid.is_some() {
        let p = tree.get(pid);
        let p_ext = tree.ext(pid).unwrap_or(&WidgetExt::DEFAULT);
        let ancestor_bg = effective_bg(tree, p, p_ext);
        if ancestor_bg != 0 {
            return ancestor_bg;
        }
        pid = p.parent;
        depth += 1;
        if depth > max {
            break;
        }
    }

    // No ancestor has a background — use the platform's default background.
    fallback
}

/// Draw one widget, clipped to `clip`.
///
/// This is the single widget-draw path for both full and dirty rendering. The
/// full-redraw callers pass a whole-screen `ClipRegion`, for which the clipped
/// fills reduce to plain screen-bounded fills (one rect intersection each, the
/// same clamp `fill_rect_screen` already does) — so the full path pays nothing
/// for going through the clip.
fn draw_widget<P: Platform>(
    ctx: &Ctx<P>,
    vm: &Vm,
    id: WidgetId,
    abs: &Rect,
    clip: &ClipRegion,
) {
    let widget = ctx.tree.get(id);
    let ext = ctx.tree.ext(id).unwrap_or(&WidgetExt::DEFAULT);
    let b = ext.border;
    let r = ext.border_radius;
    let bg_color = effective_bg(&ctx.tree, widget, ext);
    // Freeform shapes (circle/polygon/line) paint their own fill + stroke
    // below, so neutralize the default rectangular background/border here:
    // zero the border edges and skip the bg fill. The shape draws read the
    // real thickness from `ext.border`, not this shadowed `b`.
    let freeform = is_freeform_shape(widget.kind);
    let b = if freeform { Edges::ZERO } else { b };
    let draw_bg =
        !freeform && (bg_color != 0 || (widget.kind != KIND_LABEL && widget.kind != KIND_IMAGE));
    // See draw_widget — background opacity, blending-capable backends only.
    let alpha = if ctx.lcd.has_alpha() { ext.alpha } else { 255 };
    let gdir = if bg_color == widget.background_color {
        ctx.tree.gradient_dir(id)
    } else {
        0
    };
    let gcol = if gdir != 0 {
        ctx.tree.gradient_color(id)
    } else {
        0
    };
    // Write-only backends anti-alias against known colours: what is behind
    // this widget, and (for its contents) its own background.
    let back = backdrop(&ctx.tree, widget, P::DEFAULT_BG_COLOR);
    let content_bg = if draw_bg { bg_color } else { back };
    if ctx.lcd.wants_underlay() {
        ctx.lcd.set_underlay(Some(Underlay::solid(back)));
    }

    if r > 0 {
        // The rounded primitives take no clip, so confine them to the clip
        // region via the LCD scissor, once per (disjoint) clip rect. Without
        // this, a clipped redraw — e.g. restoring what is behind a transparent
        // label — repaints the whole widget over content drawn after it.
        let bw = b.top.max(b.bottom).max(b.left).max(b.right);
        for cr in clip.iter() {
            ctx.lcd.set_scissor(Some(*cr));
            if ctx.lcd.smooth() {
                // Anti-aliased: background first, then one border ring over
                // it. The fill reaches halfway under the ring, so the ring's
                // soft inner edge blends over the background rather than over
                // what is behind the widget (without readback: mixes with the
                // fill colour, its outer edge with the backdrop).
                let Some(outer) = abs.intersection(&screen_rect(&ctx.lcd)) else {
                    continue;
                };
                if draw_bg {
                    let inset = (bw / 2) as u16;
                    let bg = Rect::new(
                        outer.x + inset as i16,
                        outer.y + inset as i16,
                        outer.w.saturating_sub(inset * 2),
                        outer.h.saturating_sub(inset * 2),
                    );
                    if !bg.is_empty() {
                        let bg_r = r.saturating_sub(inset);
                        if gdir != 0 {
                            fill_gradient_rounded_rect_screen(&ctx.lcd, bg, bg_r, bg_color, gcol, gdir);
                        } else if alpha < 255 {
                            fill_rounded_rect_blend_screen(&ctx.lcd, bg, bg_r, bg_color, alpha);
                        } else {
                            fill_rounded_rect_screen(&ctx.lcd, bg, bg_r, bg_color);
                        }
                    }
                }
                ctx.lcd.update_underlay(|u| Underlay { inner: content_bg, ..u });
                ctx.lcd.stroke_rounded_rect(
                    outer.x,
                    outer.y,
                    outer.w,
                    outer.h,
                    r,
                    bw as u16,
                    widget.border_color,
                );
                ctx.lcd.update_underlay(|_| Underlay::solid(back));
                continue;
            }
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
            if draw_bg {
                let bg = inner_rect(abs, &b);
                if !bg.is_empty() {
                    let inner_r = r.saturating_sub(b.top.max(b.left) as u16);
                    if gdir != 0 {
                        fill_gradient_rounded_rect_screen(&ctx.lcd, bg, inner_r, bg_color, gcol, gdir);
                    } else if alpha < 255 {
                        fill_rounded_rect_blend_screen(&ctx.lcd, bg, inner_r, bg_color, alpha);
                    } else {
                        fill_rounded_rect_screen(&ctx.lcd, bg, inner_r, bg_color);
                    }
                }
            }
        }
        ctx.lcd.set_scissor(None);
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
                if gdir != 0 {
                    fill_gradient_clipped(&ctx.lcd, &bg, bg_color, gcol, gdir, clip);
                } else if alpha < 255 {
                    blend_clipped(&ctx.lcd, &bg, bg_color, alpha, clip);
                } else {
                    fill_clipped(&ctx.lcd, &bg, bg_color, clip);
                }
            }
        }
    }

    // Contents (indicators, arcs, curves) sit on the widget's own background.
    ctx.lcd.update_underlay(|_| Underlay::solid(content_bg));

    // Background image. KIND_GRAPH repurposes ext.image_id as graph flags,
    // and KIND_IMAGE draws its image centered below, so skip both here.
    // Image draws honor the scissor (see `Image::draw`): set it per clip
    // rect so a partial redraw decodes only the exposed part instead of the
    // whole image, and does not paint over widgets in front of it.
    if ext.image_id != 0 && widget.kind != KIND_GRAPH && widget.kind != KIND_IMAGE {
        let bg = inner_rect(abs, &b);
        for cr in clip.iter() {
            ctx.lcd.set_scissor(Some(*cr));
            draw_bg_image(ctx, ext.image_id, &bg);
        }
        ctx.lcd.set_scissor(None);
    }

    // Label text. Readback backends draw it over whatever is already on
    // screen (the label's own background, or what shows through a
    // transparent one), confined to the clip: blending is not idempotent, so
    // a pixel outside the clip must not be blended twice. Without readback,
    // glyph cells are filled opaquely with the nearest solid background.
    if widget.kind == KIND_LABEL {
        if ctx.lcd.has_alpha() {
            for cr in clip.iter() {
                ctx.lcd.set_scissor(Some(*cr));
                draw_label_text(ctx, widget, abs, ext, None);
            }
            ctx.lcd.set_scissor(None);
        } else {
            let bg = resolved_text_bg(&ctx.tree, widget, ext, P::DEFAULT_BG_COLOR);
            draw_label_text(ctx, widget, abs, ext, Some(bg));
        }
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

    // Dropdown selected text + arrow
    if widget.kind == KIND_DROPDOWN {
        draw_dropdown(ctx, widget, abs, ext);
    }

    // Gauge arc
    if widget.kind == KIND_GAUGE {
        let inner = inner_rect(abs, &ext.border);
        draw_gauge(&ctx.lcd, widget, &inner, ext);
    }

    // Spline graph (clip region already applied via screen-clipped fill calls
    // below; pixel writes are cheap so we just bound to inner).
    if widget.kind == KIND_GRAPH {
        let inner = inner_rect(abs, &b);
        draw_graph(&ctx.lcd, vm, widget, &inner, ext);
    }

    // Image widget — draw image centered in content area
    if widget.kind == KIND_IMAGE {
        for cr in clip.iter() {
            ctx.lcd.set_scissor(Some(*cr));
            draw_widget_image(ctx, abs, ext);
        }
        ctx.lcd.set_scissor(None);
    }

    // Shape widgets — fill = bg_color, stroke = border_color (thickness from
    // the border width). These skipped the default bg/border above. The shape
    // primitives draw raw geometry and take no clip, so confine them to the
    // clip region via the LCD scissor — drawing once per clip rect. Without
    // this, a clipped redraw (e.g. restoring the background behind a moved
    // widget in buffered mode) would repaint the entire shape and overwrite
    // unrelated content.
    if is_freeform_shape(widget.kind) {
        for cr in clip.iter() {
            ctx.lcd.set_scissor(Some(*cr));
            match widget.kind {
                KIND_CIRCLE => draw_circle_shape(&ctx.lcd, widget, abs, ext),
                KIND_ELLIPSE => draw_ellipse_shape(&ctx.lcd, widget, abs, ext),
                KIND_POLYGON => draw_polygon_shape(ctx, widget, abs, ext),
                KIND_LINE => draw_line_shape(&ctx.lcd, widget, abs, ext),
                _ => {}
            }
        }
        ctx.lcd.set_scissor(None);
    }
    // Unknown again: VM drawing (on_paint) stays aliased on write-only
    // backends rather than mixing against a stale colour.
    ctx.lcd.set_underlay(None);
}

/// Colour painted behind widget `w`: its nearest ancestor that paints a
/// background (the `draw_bg` rule in `draw_widget`), else `fallback`. What
/// anti-aliased edges mix against on write-only backends.
fn backdrop(tree: &WidgetTree, w: &Widget, fallback: Color) -> Color {
    let mut pid = w.parent;
    let mut depth = 0usize;
    while pid.is_some() {
        let p = tree.get(pid);
        let bg = effective_bg(tree, p, tree.ext(pid).unwrap_or(&WidgetExt::DEFAULT));
        if !is_freeform_shape(p.kind) && (bg != 0 || (p.kind != KIND_LABEL && p.kind != KIND_IMAGE)) {
            return bg;
        }
        pid = p.parent;
        depth += 1;
        if depth > tree.count() {
            break;
        }
    }
    fallback
}

/// Circle/Polygon/Line draw their own fill + stroke and skip the default
/// rectangular background/border. Rectangle keeps the default rect rendering.
fn is_freeform_shape(kind: u8) -> bool {
    matches!(
        kind,
        KIND_CIRCLE | KIND_ELLIPSE | KIND_POLYGON | KIND_LINE
    )
}

/// Stroke / line thickness for shape widgets: the border width (max edge),
/// clamped to a 1px minimum.
fn shape_stroke(ext: &WidgetExt) -> u16 {
    let t = ext
        .border
        .top
        .max(ext.border.right)
        .max(ext.border.bottom)
        .max(ext.border.left);
    (t as u16).max(1)
}

/// Circle shape: fill with `bg_color` (if set), stroke with `border_color`
/// drawn as `thickness` concentric rings. Centered in the widget's box.
fn draw_circle_shape<B: LcdBackend>(lcd: &LcdImpl<B>, widget: &Widget, abs: &Rect, ext: &WidgetExt) {
    if lcd.smooth() {
        // Anti-aliased: inscribed in the centred square so the soft edge
        // stays inside the widget box (and its clip).
        let d = abs.w.min(abs.h);
        let x = abs.x + ((abs.w - d) / 2) as i16;
        let y = abs.y + ((abs.h - d) / 2) as i16;
        draw_smooth_oval_shape(lcd, widget, ext, Rect::new(x, y, d, d));
        return;
    }
    let r = (abs.w.min(abs.h) / 2) as i16;
    if r <= 0 {
        return;
    }
    let cx = abs.x + abs.w as i16 / 2;
    let cy = abs.y + abs.h as i16 / 2;
    if widget.background_color != 0 {
        lcd.fill_circle(cx, cy, r, widget.background_color);
    }
    if widget.border_color != 0 {
        let t = shape_stroke(ext) as i16;
        lcd.stroke_ellipse(cx, cy, r, r, t, widget.border_color);
    }
}

/// Ellipse shape: fill with `bg_color` (if set), stroke with `border_color`
/// drawn as `thickness` concentric outlines. Fills the widget's w×h box.
fn draw_ellipse_shape<B: LcdBackend>(lcd: &LcdImpl<B>, widget: &Widget, abs: &Rect, ext: &WidgetExt) {
    if lcd.has_alpha() {
        draw_smooth_oval_shape(lcd, widget, ext, *abs);
        return;
    }
    let rx = (abs.w / 2) as i16;
    let ry = (abs.h / 2) as i16;
    if rx <= 0 || ry <= 0 {
        return;
    }
    let cx = abs.x + rx;
    let cy = abs.y + ry;
    if widget.background_color != 0 {
        lcd.fill_ellipse(cx, cy, rx, ry, widget.background_color);
    }
    if widget.border_color != 0 {
        let t = shape_stroke(ext) as i16;
        lcd.stroke_ellipse(cx, cy, rx, ry, t, widget.border_color);
    }
}

/// Anti-aliased circle/ellipse shape inscribed in `bx`: fill, then the
/// stroke ring over it. The fill stops halfway under the ring, so neither
/// soft edge blends over the other's fringe.
fn draw_smooth_oval_shape<B: LcdBackend>(lcd: &LcdImpl<B>, widget: &Widget, ext: &WidgetExt, bx: Rect) {
    let stroke = widget.border_color != 0;
    let t = shape_stroke(ext);
    if widget.background_color != 0 {
        let inset = if stroke { t / 2 } else { 0 };
        lcd.draw_oval(
            bx.x + inset as i16,
            bx.y + inset as i16,
            bx.w.saturating_sub(inset * 2),
            bx.h.saturating_sub(inset * 2),
            0,
            widget.background_color,
        );
    }
    if stroke {
        // Without readback the ring's inner edge mixes with the fill.
        let base = lcd.underlay();
        if widget.background_color != 0 {
            lcd.update_underlay(|u| Underlay { inner: widget.background_color, ..u });
        }
        lcd.draw_oval(bx.x, bx.y, bx.w, bx.h, t, widget.border_color);
        lcd.set_underlay(base);
    }
}

/// Line shape: the widget-box diagonal `(x,y) → (x+w-1, y+h-1)`, drawn in
/// `border_color` (falling back to `bg_color`) at the border thickness.
fn draw_line_shape<B: LcdBackend>(lcd: &LcdImpl<B>, widget: &Widget, abs: &Rect, ext: &WidgetExt) {
    let color = if widget.border_color != 0 {
        widget.border_color
    } else {
        widget.background_color
    };
    if color == 0 || abs.w == 0 || abs.h == 0 {
        return;
    }
    lcd.draw_line_thick(
        abs.x,
        abs.y,
        abs.right() - 1,
        abs.bottom() - 1,
        color,
        shape_stroke(ext),
    );
}

/// Polygon shape: points parsed from `text` ("x,y x,y …", relative to the
/// widget's top-left), filled with `bg_color` and outlined in `border_color`.
fn draw_polygon_shape<P: Platform>(ctx: &Ctx<P>, widget: &Widget, abs: &Rect, ext: &WidgetExt) {
    if ext.text_id == 0xFFFF {
        return;
    }
    let text = ctx.strpool.get(ext.text_id);
    let mut pts = [Offset { x: 0, y: 0 }; lcd::MAX_POLY_POINTS];
    let n = parse_points(text, &mut pts);
    if n < 2 {
        return;
    }
    // Points are relative to the widget's top-left (border-box origin).
    for p in pts[..n].iter_mut() {
        p.x += abs.x;
        p.y += abs.y;
    }
    let poly = &pts[..n];
    if widget.background_color != 0 && n >= 3 {
        ctx.lcd.fill_polygon(poly, widget.background_color);
    }
    if widget.border_color != 0 {
        let t = shape_stroke(ext);
        if widget.background_color != 0 && n >= 3 && !ctx.lcd.has_alpha() {
            // The fill is aliased here; a soft outline would mix its inner
            // edge with the backdrop instead of the fill.
            ctx.lcd.draw_polyline_hard(poly, widget.border_color, true, t);
        } else {
            ctx.lcd.draw_polyline(poly, widget.border_color, true, t);
        }
    }
}

/// Parse `"x,y x,y …"` (signed ints, any non-digit/`-` separators) into `out`
/// as alternating x/y. Returns the number of complete points written.
fn parse_points(bytes: &[u8], out: &mut [Offset]) -> usize {
    let mut pending_x: i16 = 0;
    let mut have_x = false;
    let mut count = 0usize;
    let mut i = 0usize;
    while i < bytes.len() && count < out.len() {
        // Skip separators up to the next number.
        while i < bytes.len() && !(bytes[i].is_ascii_digit() || bytes[i] == b'-') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let neg = bytes[i] == b'-';
        if neg {
            i += 1;
        }
        let mut v: i32 = 0;
        let mut digits = 0;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            v = v * 10 + (bytes[i] - b'0') as i32;
            i += 1;
            digits += 1;
        }
        if digits == 0 {
            continue; // lone '-' with no digits
        }
        let val = if neg { -v } else { v } as i16;
        if !have_x {
            pending_x = val;
            have_x = true;
        } else {
            out[count] = Offset {
                x: pending_x,
                y: val,
            };
            count += 1;
            have_x = false;
        }
    }
    count
}

/// Draw background image at the inner rect origin.
fn draw_bg_image<P: Platform>(ctx: &Ctx<P>, image_id: u8, inner: &Rect) {
    if inner.is_empty() {
        return;
    }
    if let Some(img) = ctx.images.find(image_id) {
        let x = if inner.x < 0 { 0u16 } else { inner.x as u16 };
        let y = if inner.y < 0 { 0u16 } else { inner.y as u16 };
        img.draw(&ctx.lcd, &ctx.flash, x, y);
    }
}

/// Draw image widget content — image centered in the content area.
fn draw_widget_image<P: Platform>(ctx: &Ctx<P>, abs: &Rect, ext: &WidgetExt) {
    if ext.image_id == 0 {
        return;
    }
    let inner = inner_rect(abs, &ext.border);
    if inner.is_empty() {
        return;
    }
    if let Some(img) = ctx.images.find(ext.image_id) {
        // Center the image horizontally and vertically within the inner rect
        let dx = if inner.w > img.width {
            inner.x + ((inner.w - img.width) / 2) as i16
        } else {
            inner.x
        };
        let dy = if inner.h > img.height {
            inner.y + ((inner.h - img.height) / 2) as i16
        } else {
            inner.y
        };
        let x = if dx < 0 { 0u16 } else { dx as u16 };
        let y = if dy < 0 { 0u16 } else { dy as u16 };
        img.draw(&ctx.lcd, &ctx.flash, x, y);
    }
}

/// Draw label text in the content area. `bg = None` draws only the glyph
/// pixels (blended on anti-aliased fonts — readback backends only).
fn draw_label_text<P: Platform>(
    ctx: &Ctx<P>,
    widget: &Widget,
    abs: &Rect,
    ext: &WidgetExt,
    bg: Option<Color>,
) {
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

    let lh = font.line_height() as i16;

    // Multi-line mode: split on '\n', center block vertically
    if widget.flags & FLAG_MULTI_LINE != 0 {
        // Count lines (first pass) — max 16 lines
        let mut line_count: usize = 0;
        let mut has_content = false;
        let bytes = text;
        let mut i: usize = 0;
        while i < bytes.len() {
            let end = memchr_byte(b'\n', bytes, i);
            if !bytes[i..end].is_empty() {
                has_content = true;
            }
            line_count += 1;
            if end >= bytes.len() {
                break;
            }
            i = end + 1;
        }
        if !has_content || line_count == 0 {
            return;
        }

        let block_h = (line_count as i16).saturating_mul(lh);
        let base_y = cy + (ch as i16 - block_h) / 2;

        // Second pass: draw each line
        let mut li: usize = 0;
        let mut i: usize = 0;
        while i < bytes.len() {
            let end = memchr_byte(b'\n', bytes, i);
            let line = &bytes[i..end];
            if !line.is_empty() {
                let lw = font.text_width(line) as i16;
                let lx = match ext.text_align {
                    ALIGN_CENTER => cx + (cw as i16 - lw) / 2,
                    ALIGN_RIGHT => cx + cw as i16 - lw,
                    _ => cx,
                };
                let ly = base_y + li as i16 * lh + (lh * 3) / 4;
                font.draw_str(&ctx.lcd, &ctx.flash, line, lx, ly, ext.text_color, bg);
            }
            li += 1;
            if end >= bytes.len() {
                break;
            }
            i = end + 1;
        }
    } else {
        // Single-line mode (original behavior)
        let tw = font.text_width(text) as i16;
        let tx = match ext.text_align {
            ALIGN_CENTER => cx + (cw as i16 - tw) / 2,
            ALIGN_RIGHT => cx + cw as i16 - tw,
            _ => cx,
        };
        let ty = cy + (ch as i16 - lh) / 2 + (lh * 3) / 4;
        font.draw_str(&ctx.lcd, &ctx.flash, text, tx, ty, ext.text_color, bg);
    }
}

/// Scan for `byte` in `data[start..]`, returning the index of the first
/// match or `data.len()` if not found (analogous to memchr).
fn memchr_byte(byte: u8, data: &[u8], start: usize) -> usize {
    for i in start..data.len() {
        if data[i] == byte {
            return i;
        }
    }
    data.len()
}

/// Draw input text with cursor in the content area.
fn draw_input_text<P: Platform>(ctx: &Ctx<P>, widget: &Widget, abs: &Rect, ext: &WidgetExt) {
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
            let mut byte_pos = 0;
            while byte_pos < end {
                if let Some(cp) = crate::font::utf8_next(text, &mut byte_pos) {
                    if cp != 0xFFFF {
                        cursor_x += font.char_width_cp(cp) as i16;
                    }
                } else {
                    break;
                }
            }
        }
        // Draw 2px wide cursor line
        let cursor_y = cy + (ch as i16 - lh) / 2;
        let cursor_h = lh as u16;
        if cursor_x >= cx && cursor_x < cx + cw as i16 {
            fill_rect_screen(
                &ctx.lcd,
                Rect::new(cursor_x, cursor_y, 2, cursor_h),
                ext.text_color,
            );
        }
    }
}

/// Draw dropdown selected text and the disclosure arrow in the content area.
fn draw_dropdown<P: Platform>(ctx: &Ctx<P>, widget: &Widget, abs: &Rect, ext: &WidgetExt) {
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

    let arrow_w: u16 = 18;
    let text_w = cw.saturating_sub(arrow_w + 4);
    let lh = font.line_height() as i16;
    let ty = cy + (ch as i16 - lh) / 2 + (lh * 3) / 4;
    let bg_color = effective_bg(&ctx.tree, widget, ext);
    let bg = if bg_color == 0 { None } else { Some(bg_color) };

    if ext.text_id != 0xFFFF && text_w > 0 {
        let text = ctx.strpool.get(ext.text_id);
        if !text.is_empty() {
            font.draw_str(&ctx.lcd, &ctx.flash, text, cx, ty, ext.text_color, bg);
        }
    }

    if cw >= arrow_w && ch >= 8 {
        let ax = cx + cw as i16 - arrow_w as i16 + 4;
        let mid_y = cy + ch as i16 / 2;
        let color = if ext.text_color != 0 {
            ext.text_color
        } else {
            0xFFFF
        };
        let d = if widget.flags & FLAG_CHECKED != 0 { -3 } else { 3 };
        if ctx.lcd.smooth() {
            let chevron = [
                Offset { x: ax, y: mid_y - d },
                Offset { x: ax + 5, y: mid_y + d },
                Offset { x: ax + 10, y: mid_y - d },
            ];
            ctx.lcd.draw_polyline(&chevron, color, false, 1);
        } else {
            ctx.lcd.draw_line(ax, mid_y - d, ax + 5, mid_y + d, color);
            ctx.lcd.draw_line(ax + 5, mid_y + d, ax + 10, mid_y - d, color);
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
fn draw_gauge<B: LcdBackend>(lcd: &LcdImpl<B>, widget: &Widget, inner: &Rect, ext: &WidgetExt) {
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
    lcd.fill_arc(cx, cy, outer_r, inner_r, ARC_START, ARC_END, track_color);

    // Draw value arc (filled portion)
    if val > 0 && ext.press_color != 0 {
        let end = if val >= 100 {
            ARC_END
        } else {
            value_angle as i16
        };
        // Without readback the value arc's leading end mixes with the track.
        let base = lcd.underlay();
        lcd.update_underlay(|u| Underlay { sweep: track_color, ..u });
        lcd.fill_arc(cx, cy, outer_r, inner_r, ARC_START, end, ext.press_color);
        lcd.set_underlay(base);
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
        if lcd.smooth() {
            // One anti-aliased tapered stroke whose round base is the hub
            // (separate strokes would double-blend or, without readback,
            // overwrite each other's soft edges).
            lcd.draw_wedge(cx, cy, nx, ny, 3 * 256 + 128, 256, needle_color);
        } else {
            lcd.draw_line(cx, cy, nx, ny, needle_color);
            // Draw thicker needle (2 extra lines offset by 1px)
            lcd.draw_line(cx + 1, cy, nx + 1, ny, needle_color);
            lcd.draw_line(cx, cy + 1, nx, ny + 1, needle_color);
        }
    }

    // Center dot (part of the wedge when smooth)
    if !lcd.smooth() || needle_r <= 4 {
        lcd.fill_circle(cx, cy, 3, needle_color);
    }

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

// --- Spline graph ---
//
// Reads samples from a VM array (id stored in `ext.value`) and draws them
// across the inner rect. Catmull-Rom interpolation between samples gives a
// smooth curve without storing per-pixel splines on the device. Sample-space
// → pixel-space mapping happens before interpolation, so the cubic terms stay
// in i32 range even for full-display widgets.
//
// Field layout for KIND_GRAPH (aliased onto WidgetExt):
//   ext.value     -> graph_arr_id (u16)
//   ext.max_length-> sample-count cap (u8; 0 means use full array length)
//   ext.image_id  -> flags: bit 0 linear (else spline), bit 1 fill area
//   ext.text_color-> line color
//   ext.gradient_color (when fill flag set) -> fill color under the curve

const GRAPH_FLAG_LINEAR: u8 = 1 << 0;
const GRAPH_FLAG_FILL: u8 = 1 << 1;

fn draw_graph<B: LcdBackend>(
    lcd: &LcdImpl<B>,
    vm: &Vm,
    _widget: &Widget,
    inner: &Rect,
    ext: &WidgetExt,
) {
    if inner.is_empty() || inner.w < 2 || inner.h < 2 {
        return;
    }

    let arr_id = ext.value as u16;
    let samples = match vm.array_slice(arr_id) {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };

    let cap = ext.max_length as usize;
    let count = if cap == 0 {
        samples.len()
    } else {
        cap.min(samples.len())
    };
    if count < 2 {
        return;
    }

    // Auto-range from data so the curve always fills the widget vertically.
    // A flat series (min == max) gets a unit span so the line lands mid-rect.
    let mut y_min = samples[0];
    let mut y_max = samples[0];
    for &v in &samples[..count] {
        if v < y_min {
            y_min = v;
        }
        if v > y_max {
            y_max = v;
        }
    }
    let mut y_span = (y_max - y_min) as i64;
    if y_span <= 0 {
        y_span = 1;
    }

    let inner_x0 = inner.x as i32;
    let inner_y0 = inner.y as i32;
    let inner_w = inner.w as i32 - 1;
    let inner_h = inner.h as i32 - 1;
    let denom = (count as i32 - 1).max(1);

    // No pre-mapped pixel buffers: the device only has 20KB RAM and most of
    // it is heap, so a [i16; 256] pair (1KB on stack) corrupts globals. Each
    // (px, py) is recomputed from `samples` on demand instead.
    let plot_x = |i: usize| -> i32 { inner_x0 + (i as i32 * inner_w) / denom };
    let plot_y = |sample: i32| -> i32 {
        let v = sample as i64 - y_min as i64;
        inner_y0 + inner_h - ((v * inner_h as i64) / y_span) as i32
    };
    // Index clamp for the 4-point Catmull-Rom window at the ends.
    let sample_at = |i: isize| -> i32 {
        let idx = if i < 0 {
            0
        } else if (i as usize) >= count {
            count - 1
        } else {
            i as usize
        };
        samples[idx]
    };

    let line_color = if ext.text_color != 0 && ext.text_color != 0xFFFF {
        ext.text_color
    } else {
        0xFFFF
    };
    let flags = ext.image_id;
    let linear = flags & GRAPH_FLAG_LINEAR != 0;
    let do_fill = flags & GRAPH_FLAG_FILL != 0 && ext.gradient_color != 0;
    let baseline_y = (inner_y0 + inner_h) as i16;

    let mut prev_x = plot_x(0) as i16;
    let mut prev_y = plot_y(samples[0]) as i16;
    // One path for the whole curve: anti-aliased segments share pixels
    // instead of drawing them twice.
    let mut pen = lcd.pen(line_color, 1);
    pen.to(prev_x, prev_y);

    if linear {
        for i in 1..count {
            let cx = plot_x(i) as i16;
            let cy = plot_y(samples[i]) as i16;
            if do_fill {
                fill_under_segment(lcd, prev_x, prev_y, cx, cy, baseline_y, ext.gradient_color);
            }
            pen.to(cx, cy);
            prev_x = cx;
            prev_y = cy;
        }
        return;
    }

    // Catmull-Rom: for each segment p1→p2, evaluate the cubic at every pixel
    // step in x. Endpoints duplicate (p0=p1 at start, p3=p2 at end) to keep
    // the curve from over-shooting at the borders.
    for i in 0..count - 1 {
        let p0 = plot_y(sample_at(i as isize - 1));
        let p1 = plot_y(samples[i]);
        let p2 = plot_y(samples[i + 1]);
        let p3 = plot_y(sample_at(i as isize + 2));

        let x1 = plot_x(i);
        let x2 = plot_x(i + 1);
        let steps = (x2 - x1).max(1);

        for s in 1..=steps {
            // t in Q8 fixed-point (0..=256 over the segment).
            let t: i32 = (s * 256) / steps;
            let t2 = (t * t) >> 8;
            let t3 = (t2 * t) >> 8;
            // 2 * P(t) in Q8 — divide by 2 at the end keeps an even integer.
            let two_pt = (2 * p1) * 256
                + (-p0 + p2) * t
                + (2 * p0 - 5 * p1 + 4 * p2 - p3) * t2
                + (-p0 + 3 * p1 - 3 * p2 + p3) * t3;
            let cy = ((two_pt / 2) >> 8) as i16;
            let cx = (x1 + s) as i16;
            if do_fill {
                fill_under_segment(lcd, prev_x, prev_y, cx, cy, baseline_y, ext.gradient_color);
            }
            pen.to(cx, cy);
            prev_x = cx;
            prev_y = cy;
        }
    }
}

/// Fill a 1-pixel-wide vertical column at each x along the line a→b down to
/// the baseline. Used when GRAPH_FLAG_FILL is set: gives a slim shaded region
/// under the curve without a separate polygon rasterizer.
fn fill_under_segment<B: LcdBackend>(
    lcd: &LcdImpl<B>,
    x0: i16,
    y0: i16,
    x1: i16,
    y1: i16,
    baseline_y: i16,
    color: Color,
) {
    if x1 == x0 {
        let top = y0.min(baseline_y);
        let bot = y0.max(baseline_y);
        if bot > top {
            fill_rect_screen(lcd, Rect::new(x0, top, 1, (bot - top) as u16), color);
        }
        return;
    }
    let dx = (x1 - x0) as i32;
    let dy = (y1 - y0) as i32;
    let step: i32 = if dx > 0 { 1 } else { -1 };
    let mut x = x0 as i32;
    while x != x1 as i32 {
        let t_num = x - x0 as i32;
        let y = y0 as i32 + (dy * t_num) / dx;
        let top = (y as i16).min(baseline_y);
        let bot = (y as i16).max(baseline_y);
        if bot > top {
            fill_rect_screen(lcd, Rect::new(x as i16, top, 1, (bot - top) as u16), color);
        }
        x += step;
    }
}

/// Scrollbar constants
pub const SCROLLBAR_W: u16 = 10;
const SCROLLBAR_TRACK: Color = 0x18C3; // dark gray
const SCROLLBAR_THUMB: Color = 0x6B4D; // medium gray
const SCROLLBAR_MIN_THUMB: u16 = 16; // minimum thumb height

/// Draw scrollbar on the right side of a scroll container.
fn draw_scrollbar<P: Platform>(ctx: &Ctx<P>, id: WidgetId, _abs: &Rect) {
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
    fill_rect_screen(
        &ctx.lcd,
        Rect::new(track_x, track_y, SCROLLBAR_W, track_h),
        SCROLLBAR_TRACK,
    );

    // Thumb: proportional size and position
    let thumb_h = ((vh * track_h as u32) / ch)
        .max(SCROLLBAR_MIN_THUMB as u32)
        .min(track_h as u32) as u16;
    let max_scroll = ch - vh;
    let scroll_range = track_h.saturating_sub(thumb_h) as u32;
    let thumb_y = if max_scroll > 0 {
        track_y + ((scroll_y * scroll_range) / max_scroll) as i16
    } else {
        track_y
    };
    fill_rect_screen(
        &ctx.lcd,
        Rect::new(track_x, thumb_y, SCROLLBAR_W, thumb_h),
        SCROLLBAR_THUMB,
    );
}

/// Draw rect clipped against clip region.
fn fill_clipped<B: LcdBackend>(lcd: &LcdImpl<B>, rect: &Rect, color: Color, clip: &ClipRegion) {
    for cr in clip.iter() {
        if let Some(visible) = rect.intersection(cr) {
            fill_rect_screen(lcd, visible, color);
        }
    }
}

/// Draw gradient rect clipped against clip region.
/// `full_rect` is the logical gradient area; each visible sub-rect is filled
/// with the correct gradient slice relative to that area.
fn fill_gradient_clipped<B: LcdBackend>(
    lcd: &LcdImpl<B>,
    full_rect: &Rect,
    c1: Color,
    c2: Color,
    dir: u8,
    clip: &ClipRegion,
) {
    for cr in clip.iter() {
        if let Some(visible) = full_rect.intersection(cr) {
            fill_gradient_rect_screen(lcd, visible, *full_rect, c1, c2, dir);
        }
    }
}

/// Blend rect clipped against clip region. Clip rects are disjoint (region
/// subtraction yields non-overlapping strips), so no pixel blends twice.
fn blend_clipped<B: LcdBackend>(
    lcd: &LcdImpl<B>,
    rect: &Rect,
    color: Color,
    alpha: u8,
    clip: &ClipRegion,
) {
    for cr in clip.iter() {
        if let Some(visible) = rect.intersection(cr) {
            blend_rect_screen(lcd, visible, color, alpha);
        }
    }
}

/// Draw rect clipped to screen bounds.
fn fill_rect_screen<B: LcdBackend>(lcd: &LcdImpl<B>, rect: Rect, color: Color) {
    if let Some(r) = rect.intersection(&screen_rect(lcd)) {
        lcd.fill_rect(r.x as u16, r.y as u16, r.w, r.h, color);
    }
}

/// Blend rect clipped to screen bounds.
fn blend_rect_screen<B: LcdBackend>(lcd: &LcdImpl<B>, rect: Rect, color: Color, alpha: u8) {
    if let Some(r) = rect.intersection(&screen_rect(lcd)) {
        lcd.blend_rect(r.x as u16, r.y as u16, r.w, r.h, color, alpha);
    }
}

/// Fill gradient rect clipped to screen bounds.
/// `full_rect` is the logical gradient area so the slice is correctly computed
/// even when `rect` is a clipped sub-region.
fn fill_gradient_rect_screen<B: LcdBackend>(
    lcd: &LcdImpl<B>,
    rect: Rect,
    full_rect: Rect,
    c1: Color,
    c2: Color,
    dir: u8,
) {
    if let Some(r) = rect.intersection(&screen_rect(lcd)) {
        let x_off = (r.x as i32 - full_rect.x as i32).max(0) as u16;
        let y_off = (r.y as i32 - full_rect.y as i32).max(0) as u16;
        lcd.fill_gradient_rect(
            r.x as u16,
            r.y as u16,
            r.w,
            r.h,
            c1,
            c2,
            dir,
            x_off,
            y_off,
            full_rect.w,
            full_rect.h,
        );
    }
}

/// Fill gradient rounded rect clipped to screen bounds.
fn fill_gradient_rounded_rect_screen<B: LcdBackend>(
    lcd: &LcdImpl<B>,
    rect: Rect,
    radius: u16,
    c1: Color,
    c2: Color,
    dir: u8,
) {
    if let Some(r) = rect.intersection(&screen_rect(lcd)) {
        lcd.fill_gradient_rounded_rect(r.x as u16, r.y as u16, r.w, r.h, radius, c1, c2, dir);
    }
}

/// Draw rounded rect outline clipped to screen bounds.
fn rounded_rect_screen<B: LcdBackend>(lcd: &LcdImpl<B>, rect: Rect, radius: u16, color: Color) {
    if let Some(r) = rect.intersection(&screen_rect(lcd)) {
        lcd.draw_rounded_rect(r.x as u16, r.y as u16, r.w, r.h, radius, color);
    }
}

/// Fill rounded rect clipped to screen bounds.
fn fill_rounded_rect_screen<B: LcdBackend>(lcd: &LcdImpl<B>, rect: Rect, radius: u16, color: Color) {
    if let Some(r) = rect.intersection(&screen_rect(lcd)) {
        lcd.fill_rounded_rect(r.x as u16, r.y as u16, r.w, r.h, radius, color);
    }
}

/// Blend rounded rect clipped to screen bounds.
fn fill_rounded_rect_blend_screen<B: LcdBackend>(
    lcd: &LcdImpl<B>,
    rect: Rect,
    radius: u16,
    color: Color,
    alpha: u8,
) {
    if let Some(r) = rect.intersection(&screen_rect(lcd)) {
        lcd.fill_rounded_rect_blend(r.x as u16, r.y as u16, r.w, r.h, radius, color, alpha);
    }
}

/// Erase the area of a widget that is becoming invisible by repainting the
/// nearest ancestor background.  Handles both solid and gradient ancestors so
/// a gradient parent is not corrupted by a solid-color erase rectangle.
fn erase_widget_area<B: LcdBackend>(tree: &WidgetTree, lcd: &LcdImpl<B>, id: WidgetId, abs: Rect) {
    let mut pid = tree.get(id).parent;
    let max = tree.count();
    let mut depth = 0usize;
    while pid.is_some() {
        let bg = tree.get(pid).background_color;
        if bg != 0 {
            let gdir = tree.gradient_dir(pid);
            if gdir != 0 {
                let gcol = tree.gradient_color(pid);
                let parent_abs = tree.absolute_rect(pid);
                let parent_b = tree.border(pid);
                let parent_bg_rect = inner_rect(&parent_abs, &parent_b);
                fill_gradient_rect_screen(lcd, abs, parent_bg_rect, bg, gcol, gdir);
            } else {
                fill_rect_screen(lcd, abs, bg);
            }
            return;
        }
        pid = tree.get(pid).parent;
        depth += 1;
        if depth > max {
            break;
        }
    }
    fill_rect_screen(lcd, abs, 0);
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
fn draw_value_fill<B: LcdBackend>(lcd: &LcdImpl<B>, widget: &Widget, inner: &Rect, ext: &WidgetExt) {
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
fn draw_check_indicator<B: LcdBackend>(
    lcd: &LcdImpl<B>,
    widget: &Widget,
    inner: &Rect,
    ext: &WidgetExt,
) {
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
                if lcd.smooth() {
                    let tick = [Offset { x: x0, y: y0 }, Offset { x: x1, y: y1 }, Offset { x: x2, y: y2 }];
                    lcd.draw_polyline(&tick, fill_color, false, 2);
                } else {
                    lcd.draw_line(x0, y0, x1, y1, fill_color);
                    lcd.draw_line(x1, y1, x2, y2, fill_color);
                    lcd.draw_line(x0 + 1, y0, x1 + 1, y1, fill_color);
                    lcd.draw_line(x1 + 1, y1, x2 + 1, y2, fill_color);
                }
            }
        }
    } else {
        // Radio: circle
        if lcd.smooth() {
            // Inscribed in the s×s box; the dot keeps s's parity so it is
            // centred exactly.
            lcd.draw_oval(cx, cy, s, s, 1, outline_color);
            if checked {
                let mut d = s * 2 / 3;
                if (s - d) % 2 != 0 {
                    d -= 1;
                }
                let o = ((s - d) / 2) as i16;
                lcd.draw_oval(cx + o, cy + o, d, d, 0, fill_color);
            }
            return;
        }
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

fn clear_all_dirty<P: Platform>(ctx: &mut Ctx<P>) {
    ctx.tree.ensure_dfs();
    for i in 0..ctx.tree.dfs_len() {
        let id = ctx.tree.dfs_at(i);
        ctx.tree.get_mut(id).clear_dirty();
    }
}

#[cfg(all(test, feature = "mock"))]
mod tests {
    use super::*;
    use crate::widget::KIND_BASE;

    /// Pressed, background-less rounded button ("Ghost") with a full-size
    /// transparent label child.
    fn ghost(tree: &mut WidgetTree) -> (WidgetId, WidgetId) {
        let button = tree.alloc().unwrap();
        tree.get_mut(button).kind = KIND_BASE;
        tree.get_mut(button).flags |= FLAG_PRESSED;
        tree.ensure_ext(button).unwrap().press_color = 0x2C9F;
        let label = tree.alloc().unwrap();
        tree.get_mut(label).kind = KIND_LABEL;
        tree.add_child(button, label);
        (button, label)
    }

    #[test]
    fn transparent_label_does_not_inherit_press_color() {
        let mut tree = WidgetTree::new();
        let (button, label) = ghost(&mut tree);
        let ext = tree.ext(label).unwrap_or(&WidgetExt::DEFAULT);
        // No fill of its own: the button's rounded press fill shows through.
        assert_eq!(effective_bg(&tree, tree.get(label), ext), 0);
        // Text without readback still blends against the press color.
        assert_eq!(resolved_text_bg(&tree, tree.get(label), ext, 0), 0x2C9F);
        let bext = tree.ext(button).unwrap();
        assert_eq!(effective_bg(&tree, tree.get(button), bext), 0x2C9F);
    }

    #[test]
    fn same_bg_container_still_inherits_press_color() {
        let mut tree = WidgetTree::new();
        let (button, _) = ghost(&mut tree);
        let inner = tree.alloc().unwrap();
        tree.get_mut(inner).kind = KIND_BASE;
        tree.add_child(button, inner);
        let ext = tree.ext(inner).unwrap_or(&WidgetExt::DEFAULT);
        assert_eq!(effective_bg(&tree, tree.get(inner), ext), 0x2C9F);
    }
}
