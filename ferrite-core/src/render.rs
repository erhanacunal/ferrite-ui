use crate::clip::ClipRegion;
use crate::ctx::Ctx;
use crate::lcd::{self};
use crate::types::{Color, Edges, Offset, Rect};
use crate::pool::ObjPool;
use crate::lcd::{LcdBackend, LcdImpl, Underlay};
use crate::platform::Platform;
use crate::renderer::WidgetRenderer;
use crate::widget::{
    ALIGN_CENTER, ALIGN_RIGHT, FLAG_CHECKED, FLAG_CLIP_CHILDREN, FLAG_FOCUSED, FLAG_MULTI_LINE,
    FLAG_DIRTY_AB, FLAG_HAS_ON_PAINT, FLAG_PAINTED, FLAG_PRESSED, FLAG_RENDERED, KIND_CHECKBOX, KIND_CIRCLE, KIND_DROPDOWN, KIND_ELLIPSE, KIND_GAUGE,
    KIND_GRAPH, KIND_IMAGE, KIND_INPUT, KIND_LABEL, KIND_LINE, KIND_POLYGON, KIND_PROGRESS,
    KIND_BUTTON_GROUP, KIND_RADIO, KIND_SLIDER, Widget, WidgetExt, WidgetId,
};

/// Screen-bounds rect: the panel size from `LcdBackend::WIDTH`/`HEIGHT`,
/// swapped in portrait mode (`LcdImpl::width`/`height`).
#[inline]
pub fn screen_rect<B: LcdBackend>(lcd: &LcdImpl<B>) -> Rect {
    Rect::new(0, 0, lcd.width(), lcd.height())
}

// --- Buffered render (double-buffered partial redraw) ---

/// Check if the back buffer has any dirty widgets.
pub fn buffered_has_dirty<P: Platform>(ctx: &mut Ctx<P>) -> bool {
    if !ctx.objs.render.maybe_dirty {
        return false;
    }
    let buf = ctx.lcd.back_buf();
    ctx.objs.ensure_dfs();
    let mut other = false;
    for i in 0..ctx.objs.dfs_len() {
        let w = ctx.objs.get(ctx.objs.dfs_at(i));
        if w.is_dirty_buf(buf) {
            return true;
        }
        other |= w.flags & FLAG_DIRTY_AB != 0;
    }
    // Nothing dirty in either buffer: idle passes can skip the scan.
    if !other {
        ctx.objs.render.maybe_dirty = false;
    }
    false
}

/// Widgets whose `on_paint` must run right after the render pass that drew
/// them (fixed capacity — no heap per frame). See `Vm::run_paint`.
#[derive(Clone, Copy)]
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
    if !ctx.objs.render.maybe_dirty {
        return;
    }
    ctx.objs.ensure_dfs();
    for i in 0..ctx.objs.dfs_len() {
        let id = ctx.objs.dfs_at(i);
        let w = ctx.objs.get(id);
        if w.is_dirty() && w.flags & FLAG_HAS_ON_PAINT != 0 {
            out.push(id);
        }
    }
}

/// Dirty mode, before `render_dirty`: offers each visible widget in `paint` to the board
/// renderer's capture (`WidgetRenderer::capture_begin`); `run` paints the ones it takes
/// (one-entry lists) into their captures, with the scissor on the widget's box. Returns
/// the rest, whose `on_paint` runs on the screen after the render as before. With the
/// default renderer nothing is taken: `paint` comes back as it was.
pub fn capture_paint<P: Platform>(
    ctx: &mut Ctx<P>,
    paint: &PaintList,
    mut run: impl FnMut(&mut Ctx<P>, &PaintList),
) -> PaintList {
    if !<P::Renderer as WidgetRenderer<P>>::CAPTURES || paint.is_empty() {
        return *paint;
    }
    let mut rest = PaintList::new();
    for &id in paint.ids() {
        if !ctx.objs.is_tree_visible(id) {
            rest.push(id);
            continue;
        }
        let abs = ctx.objs.absolute_rect(id);
        let Some(boxed) = abs.intersection(&screen_rect(&ctx.lcd)) else {
            rest.push(id);
            continue;
        };
        if !<P::Renderer as WidgetRenderer<P>>::capture_begin(ctx, id, &abs) {
            rest.push(id);
            continue;
        }
        let scissor = ctx.lcd.scissor();
        ctx.lcd.set_scissor(Some(boxed));
        let mut one = PaintList::new();
        one.push(id);
        run(ctx, &one);
        ctx.lcd.set_scissor(scissor);
        <P::Renderer as WidgetRenderer<P>>::capture_end(ctx, id);
    }
    rest
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
pub fn render_buffered_content<P: Platform>(ctx: &mut Ctx<P>) -> PaintList {
    let mut paint = PaintList::new();
    let buf = ctx.lcd.back_buf();
    // Layers are a dirty-mode feature.
    #[cfg(feature = "layers")]
    if ctx.lcd.layers_allocated() != 0 {
        layers_off(ctx);
    }
    ctx.objs.ensure_dfs();
    ctx.objs.geo_begin();
    // The two buffers hold different frames: no incremental redraws.
    ctx.objs.screen_touched();
    // Whole-screen clip for the unclipped Pass-2 draws (built once).
    let full = ClipRegion::from_rect(screen_rect(&ctx.lcd));

    // Pass 1: for each dirty widget whose rect in THIS buffer is stale,
    // redraw everything behind it clipped to the stale rect.
    for i in 0..ctx.objs.dfs_len() {
        let id = ctx.objs.dfs_at(i);
        let w = ctx.objs.get(id);
        if !w.is_dirty_buf(buf) {
            continue;
        }
        let stale = buf_prev_rect(&ctx.objs, id, buf);
        if stale.is_empty() {
            continue;
        }
        let current = if w.is_visible() {
            ctx.objs.abs_at(i)
        } else {
            Rect::new(0, 0, 0, 0)
        };
        if stale == current {
            continue;
        } // rect unchanged — no trail to clean
        redraw_behind(ctx, i, stale);
    }

    // Pass 2: draw dirty visible widgets at their current rect
    clear_painted(&mut ctx.objs);
    let mut next = 0;
    while next < ctx.objs.dfs_len() {
        let i = next;
        next += 1;
        let id = ctx.objs.dfs_at(i);
        let w = ctx.objs.get(id);
        if !w.is_dirty_buf(buf) || !ctx.objs.vis_at(i) {
            continue;
        }

        let abs = ctx.objs.abs_at(i);

        // Children of scroll containers: outside or partly visible — skip the subtree.
        if scroll_hidden(&ctx.objs, i, &abs) {
            next = ctx.objs.sub_end_at(i);
            continue;
        }

        // Translucent widgets and transparent labels must not blend over
        // their own previous output in this buffer — restore what is behind
        // them first, unless this pass already repainted all of it.
        if ctx.lcd.has_alpha() && shows_behind_on::<P>(&ctx.objs, id) && !behind_is_fresh(&ctx.objs, 0, i, &abs, false)
        {
            redraw_behind(ctx, i, abs);
        }
        draw_widget(ctx, id, &abs, &full);
        ctx.objs.get_mut(id).flags |= FLAG_RENDERED | FLAG_PAINTED;
        if ctx.objs.get(id).flags & FLAG_HAS_ON_PAINT != 0 {
            paint.push(id);
        }
    }

    // Draw scrollbars on top
    for i in 0..ctx.objs.dfs_len() {
        let id = ctx.objs.dfs_at(i);
        if ctx.objs.get(id).flags & FLAG_CLIP_CHILDREN != 0
            && ctx.objs.get(id).is_dirty_buf(buf)
            && ctx.objs.vis_at(i)
        {
            let abs = ctx.objs.abs_at(i);
            draw_scrollbar(ctx, id, &abs);
        }
    }

    clear_painted(&mut ctx.objs);

    // Record this buffer's state and clear its dirty flag for each widget.
    for i in 0..ctx.objs.dfs_len() {
        let id = ctx.objs.dfs_at(i);
        if ctx.objs.get(id).is_dirty_buf(buf) {
            let drawn_rect = if ctx.objs.vis_at(i) {
                ctx.objs.abs_at(i)
            } else {
                Rect::new(0, 0, 0, 0)
            };
            set_buf_prev_rect(&mut ctx.objs, id, buf, drawn_rect);
        }
        ctx.objs.get_mut(id).clear_dirty_buf(buf);
    }
    ctx.objs.geo_end();
    paint
}

/// Read this widget's prev_rect for the given buffer index (0=A, 1=B).
fn buf_prev_rect(tree: &ObjPool, id: WidgetId, buf: u8) -> Rect {
    tree.prev_rect(id, buf)
}

/// Store this widget's drawn rect for the given buffer. Adds the widget's
/// prev-rect section on demand; an empty rect with nothing stored yet is
/// skipped (nothing to track).
fn set_buf_prev_rect(tree: &mut ObjPool, id: WidgetId, buf: u8, rect: Rect) {
    tree.set_prev_rect(id, buf, rect);
}

/// Whether DFS entry `i` (at `abs`) is left out as a scroll container's child: not wholly
/// inside its nearest scroll container's viewport (text can't be partially clipped). Draw
/// loops skip its whole subtree with it (`render_all` always did), so a partly visible row
/// doesn't leave its children drawn without it; occluder and restore loops ignore it.
/// Board compositors apply the same rule.
pub fn scroll_hidden(objs: &ObjPool, i: usize, abs: &Rect) -> bool {
    let sp = objs.scroll_parent_at(i);
    if sp.is_none() {
        return false;
    }
    let v = objs.scroll_viewport(sp);
    abs.x < v.x || abs.y < v.y || abs.right() > v.right() || abs.bottom() > v.bottom()
}

/// Redraw widgets that sit behind widget `target_idx` (earlier in DFS order),
/// clipped to `stale`. Used to restore pixels under a moved/hidden widget.
/// Reads the DFS order from the tree cache — ensure_dfs must have been called
/// before entering the enclosing render loop.
fn redraw_behind<P: Platform>(ctx: &mut Ctx<P>, target_idx: usize, stale: Rect) {
    // Ancestors repainted here may cover widgets in front that are not
    // redrawn: their pixels are no longer what they last drew.
    ctx.objs.screen_touched();
    let mut clip = ClipRegion::from_rect(stale);
    clip.clip_to_bounds(&screen_rect(&ctx.lcd));
    if clip.is_empty() {
        return;
    }
    redraw_behind_region(ctx, target_idx, &clip, &stale);
}

/// Dirty mode: put back what widget `idx` covered at `area` (it moved away
/// or was hidden). The widgets behind are redrawn there; that overwrites
/// visible widgets in front of it which are not dirty, so mark those for
/// pass 2 (they are later in DFS order, so pass 2 still reaches them).
fn restore_behind<P: Platform>(ctx: &mut Ctx<P>, idx: usize, area: Rect) {
    redraw_behind(ctx, idx, area);
    let end = ctx.objs.sub_end_at(idx);
    for j in end..ctx.objs.dfs_len() {
        let jabs = ctx.objs.abs_at(j);
        if ctx.objs.vis_at(j) && jabs.intersects(&area) && !scroll_hidden(&ctx.objs, j, &jabs) {
            let jid = ctx.objs.dfs_at(j);
            if !ctx.objs.get(jid).is_dirty() {
                ctx.objs.mark_dirty(jid);
            }
        }
    }
}

/// `redraw_behind` over a prebuilt clip region (`bounds` is its enclosing
/// rect, used for the cheap intersection pre-test). Also used to restore the
/// pixels under a translucent widget before it is re-blended — without this,
/// a widget with `alpha < 255` redrawn in place blends over its own previous
/// output and accumulates toward solid a little more every frame.
fn redraw_behind_region<P: Platform>(
    ctx: &mut Ctx<P>,
    target_idx: usize,
    clip: &ClipRegion,
    bounds: &Rect,
) {
    // Behind a layer's root there is nothing: transparent.
    #[cfg(feature = "layers")]
    if ctx.objs.in_layer_pass() {
        for r in clip.iter() {
            ctx.lcd.layer_clear(r);
        }
    }
    let mut i = 0;
    while i < target_idx {
        if !ctx.objs.vis_at(i) {
            i += 1;
            continue;
        }
        let uid = ctx.objs.dfs_at(i);
        let u_abs = ctx.objs.abs_at(i);
        if scroll_hidden(&ctx.objs, i, &u_abs) {
            // Scrolled out of its container's view: not on the screen, nor its subtree.
            i = ctx.objs.sub_end_at(i);
            continue;
        }
        i += 1;
        if !u_abs.intersects(bounds) {
            continue;
        }
        draw_widget(ctx, uid, &u_abs, clip);
        // Partly redrawn (possibly with a newer value than recorded).
        ctx.objs.forget_drawn(uid);
    }
    // The scrollbars of containers repainted here go over their children again.
    for i in 0..target_idx {
        let id = ctx.objs.dfs_at(i);
        if ctx.objs.get(id).flags & FLAG_CLIP_CHILDREN != 0
            && ctx.objs.vis_at(i)
            && ctx.objs.abs_at(i).intersects(bounds)
            && let Some(parts) = scrollbar_parts(&ctx.objs, id)
        {
            for (rect, color) in parts {
                fill_clipped(&ctx.lcd, &rect, color, clip);
            }
        }
    }
}

// --- Full screen render ---

/// Draw the entire widget tree from scratch (initial draw or full redraw).
/// Iterative DFS using cached order — no recursion, no stack growth.
pub fn render_all<P: Platform>(ctx: &mut Ctx<P>) {
    ctx.objs.ensure_dfs();
    ctx.objs.geo_begin();
    #[cfg(feature = "layers")]
    let active = {
        let active = layers_sync(ctx);
        for slot in ctx.objs.layers.slots.iter_mut() {
            // The whole screen is drawn: nothing of a layer to restore on it.
            slot.restore = Rect::new(0, 0, 0, 0);
            slot.recompose = false;
        }
        // Composed layers' surfaces first: the screen's pass blends them.
        for s in 0..crate::layer::MAX_LAYERS {
            if ctx.objs.layers.slots[s].composed {
                layer_pass(ctx, s, true);
            }
        }
        if active != 0 {
            ctx.objs.layers.scope = base_scope(&ctx.objs);
        }
        active
    };
    let n = ctx.objs.dfs_len();
    render_all_iterative(ctx, 0, n);
    #[cfg(feature = "layers")]
    {
        ctx.objs.layers.scope = crate::layer::Scope::All;
        if active != 0 {
            for s in 0..crate::layer::MAX_LAYERS {
                let slot = ctx.objs.layers.slots[s];
                if slot.active && !slot.composed {
                    layer_pass(ctx, s, true);
                }
            }
        }
        place_layers(ctx);
    }
    ctx.objs.geo_end();
    clear_all_dirty(ctx);
    // Everything shows what it last drew (see `render_dirty`'s `settled`).
    ctx.objs.render.settled_epoch = ctx.objs.render.draw_epoch;
}

/// Iterative full redraw of DFS entries `lo..hi` (`ensure_dfs` +
/// `geo_begin` first): walk DFS order, skip invisible subtrees.
/// Children of scroll containers are clipped to the viewport.
fn render_all_iterative<P: Platform>(ctx: &mut Ctx<P>, lo: usize, hi: usize) {
    // Whole-screen clip for the full redraw — built once, reused for every
    // widget so we don't pay region setup in the per-widget loop.
    let full = ClipRegion::from_rect(screen_rect(&ctx.lcd));
    let mut i = lo;
    while i < hi {
        let id = ctx.objs.dfs_at(i);
        if !ctx.objs.vis_at(i) {
            // Skip entire subtree of this invisible widget
            i = ctx.objs.sub_end_at(i);
            continue;
        }
        let abs = ctx.objs.abs_at(i);

        // Children of scroll containers: outside or partly visible — skip the subtree.
        if scroll_hidden(&ctx.objs, i, &abs) {
            i = ctx.objs.sub_end_at(i);
            continue;
        }
        draw_widget(ctx, id, &abs, &full);
        ctx.objs.get_mut(id).flags |= FLAG_RENDERED;
        record_drawn(ctx, id, &abs);
        i += 1;
    }

    // Draw scrollbars on top of all children
    for i in lo..hi {
        let id = ctx.objs.dfs_at(i);
        if ctx.objs.get(id).flags & FLAG_CLIP_CHILDREN != 0 && ctx.objs.vis_at(i) {
            let abs = ctx.objs.abs_at(i);
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
pub fn render_dirty<P: Platform>(ctx: &mut Ctx<P>) {
    if !ctx.objs.render.maybe_dirty {
        return;
    }
    // Every widget still shows what it last drew: nothing drew over the
    // screen since the last render, and the tree kept its shape.
    let settled = ctx.objs.render.draw_epoch == ctx.objs.render.settled_epoch && ctx.objs.dfs_valid;
    ctx.objs.ensure_dfs();

    let mut has_dirty = false;
    for i in 0..ctx.objs.dfs_len() {
        if ctx.objs.get(ctx.objs.dfs_at(i)).is_dirty() {
            has_dirty = true;
            break;
        }
    }
    // A layer moved, faded, was shown/hidden or (un)marked: placements and
    // eligibility change even with nothing dirty.
    #[cfg(feature = "layers")]
    let placed = core::mem::take(&mut ctx.objs.layers.placed);
    #[cfg(not(feature = "layers"))]
    let placed = false;
    if !has_dirty && !placed {
        // Dirty mode uses only the A bits: nothing left to draw until a
        // widget is marked again.
        ctx.objs.render.maybe_dirty = false;
        ctx.objs.clear_changed();
        return;
    }
    ctx.objs.geo_begin();

    #[cfg(feature = "layers")]
    let active = layers_sync(ctx);
    #[cfg(not(feature = "layers"))]
    let active = 0usize;

    // A board that repaints whole areas (a GPU) takes the render over.
    if active == 0 && <P::Renderer as WidgetRenderer<P>>::COMPOSES && compose_dirty(ctx, false) {
        ctx.objs.geo_end();
        return;
    }
    #[cfg(feature = "layers")]
    if active != 0 {
        // Composed layers' surfaces first: what changed in them is blended
        // into the screen (by the compositor or the passes), their root
        // redrawn there.
        for s in 0..crate::layer::MAX_LAYERS {
            if !ctx.objs.layers.slots[s].composed {
                continue;
            }
            if layer_pass(ctx, s, false) {
                ctx.objs.layers.slots[s].recompose = true;
            }
            let slot = &mut ctx.objs.layers.slots[s];
            if core::mem::take(&mut slot.recompose) && !slot.shown.is_empty() {
                let root = slot.root;
                // The root alone: its subtree is not the screen's. (Pass 2
                // redraws the see-through widgets in front of it too.)
                ctx.objs.get_mut(root).flags |= crate::widget::FLAG_DIRTY_A;
            }
        }
    }
    // With layers: the screen's part (their subtrees left out; a composed
    // layer's root is its surface). The windowed layers' surfaces are drawn
    // by the passes after.
    #[cfg(feature = "layers")]
    if active != 0 && <P::Renderer as WidgetRenderer<P>>::COMPOSES {
        ctx.objs.layers.scope = base_scope(&ctx.objs);
        let taken = compose_dirty(ctx, true);
        ctx.objs.layers.scope = crate::layer::Scope::All;
        if taken {
            for s in 0..crate::layer::MAX_LAYERS {
                let slot = ctx.objs.layers.slots[s];
                if slot.active && !slot.composed {
                    layer_pass(ctx, s, false);
                }
            }
            place_layers(ctx);
            ctx.objs.geo_end();
            clear_all_dirty(ctx);
            ctx.objs.render.shifted.clear();
            ctx.objs.render.settled_epoch = ctx.objs.render.draw_epoch;
            return;
        }
    }

    #[cfg(feature = "layers")]
    if active != 0 {
        ctx.objs.layers.scope = base_scope(&ctx.objs);
    }
    dirty_passes(ctx, settled, true);
    #[cfg(feature = "layers")]
    if active != 0 {
        ctx.objs.layers.scope = crate::layer::Scope::All;
        for s in 0..crate::layer::MAX_LAYERS {
            let slot = ctx.objs.layers.slots[s];
            if slot.active && !slot.composed {
                layer_pass(ctx, s, false);
            }
        }
    }
    #[cfg(feature = "layers")]
    place_layers(ctx);
    ctx.objs.geo_end();

    clear_all_dirty(ctx);
    ctx.objs.render.shifted.clear();
    ctx.objs.render.settled_epoch = ctx.objs.render.draw_epoch;
}

/// The passes of `render_dirty` over the running scope: the screen (`base`,
/// with pixel shifts) or a layer's surface (`ctx.objs.layers`).
fn dirty_passes<P: Platform>(ctx: &mut Ctx<P>, settled: bool, base: bool) {
    let mut restores = Restores::new();
    // Pass 0: opaque panels that only moved get their pixels shifted.
    if base && <P::LcdB as LcdBackend>::CAN_MOVE {
        shift_moved(ctx, settled, &mut restores);
    }
    #[cfg(not(feature = "layers"))]
    let _ = base;
    // Layers that just moved into their surface: put back what the screen
    // drew of them.
    #[cfg(feature = "layers")]
    if base {
        for s in 0..crate::layer::MAX_LAYERS {
            let slot = ctx.objs.layers.slots[s];
            if !slot.active || slot.restore.is_empty() {
                continue;
            }
            ctx.objs.layers.slots[s].restore = Rect::new(0, 0, 0, 0);
            if let Some(ri) = ctx.objs.dfs_index(slot.root) {
                restore_behind(ctx, ri, slot.restore);
                restores.push(ri, slot.restore);
            }
        }
    }
    // Pass 1: erase dirty widgets that became invisible.
    // Only erase widgets that were actually drawn before (FLAG_RENDERED).
    // Without this gate, an animated widget on a hidden page (e.g. a progress
    // bar still being ticked by loop()) would repaint its stale rect every
    // frame with the ancestor bg, overwriting whatever sibling page is shown.
    for di in 0..ctx.objs.dfs_len() {
        let id = ctx.objs.dfs_at(di);
        if !ctx.objs.get(id).is_dirty() {
            continue;
        }
        #[cfg(feature = "layers")]
        if !ctx.objs.in_scope(di) {
            continue;
        }
        if !ctx.objs.vis_at(di) {
            if ctx.objs.get(id).flags & FLAG_RENDERED != 0 {
                let abs = ctx.objs.abs_at(di);
                if !abs.is_empty() {
                    // Restore what is behind it (ancestors and earlier
                    // siblings, not just the nearest ancestor's bg: a hidden
                    // popup would otherwise leave a blank hole). In a layer
                    // the restore starts from transparent instead.
                    #[cfg(feature = "layers")]
                    let erase = !ctx.objs.in_layer_pass();
                    #[cfg(not(feature = "layers"))]
                    let erase = true;
                    if erase {
                        erase_widget_area(&ctx.objs, &ctx.lcd, id, abs);
                    }
                    restore_behind(ctx, di, abs);
                    restores.push(di, abs);
                }
                ctx.objs.get_mut(id).flags &= !FLAG_RENDERED;
            }
        }
    }

    // Pass 1b: for each dirty visible widget whose rect in buffer A is stale,
    // redraw everything behind it clipped to the stale rect (restores pixels
    // hidden by the widget's previous position).
    for di in 0..ctx.objs.dfs_len() {
        let id = ctx.objs.dfs_at(di);
        let w = ctx.objs.get(id);
        if !w.is_dirty() {
            continue;
        }
        #[cfg(feature = "layers")]
        if !ctx.objs.in_scope(di) {
            continue;
        }
        let stale = buf_prev_rect(&ctx.objs, id, 0);
        if stale.is_empty() {
            continue;
        }
        // Recorded on screen; a layer pass draws in its surface's coordinates.
        #[cfg(feature = "layers")]
        let stale = {
            let o = ctx.objs.layers.origin;
            Rect::new(stale.x.wrapping_sub(o.x), stale.y.wrapping_sub(o.y), stale.w, stale.h)
        };
        let current = if ctx.objs.vis_at(di) {
            ctx.objs.abs_at(di)
        } else {
            Rect::new(0, 0, 0, 0)
        };
        // Recorded by `will_move`: used once.
        ctx.objs.set_prev_rect(id, 0, Rect::new(0, 0, 0, 0));
        if stale == current {
            continue;
        }
        restore_behind(ctx, di, stale);
        restores.push(di, stale);
    }

    // A restore after the shift that drew over shifted pixels (a widget
    // behind the panel restored there): the panel is drawn there again.
    if !ctx.objs.render.shifted.is_empty() {
        let shifted = &mut ctx.objs.render.shifted;
        shifted.retain(|&(sdi, r)| {
            !restores.full && !restores.list[..restores.n].iter().any(|&(i, a)| i != sdi && a.intersects(&r))
        });
    }

    // Pass 2: draw dirty visible widgets with clipping. A dirty widget is
    // drawn together with its whole subtree, after which the loop resumes past
    // that subtree: `mark_dirty` marks every descendant too, so visiting them
    // again would redraw each nested level once more (a label three levels
    // deep was drawn four times).
    clear_painted(&mut ctx.objs);
    let mut di = 0;
    while di < ctx.objs.dfs_len() {
        let id = ctx.objs.dfs_at(di);

        if !ctx.objs.get(id).is_dirty() || !ctx.objs.vis_at(di) {
            di += 1;
            continue;
        }

        // Find subtree end: contiguous descendants after di
        let sub_end = ctx.objs.sub_end_at(di);

        // Collect occluder rects: visible widgets after this subtree in DFS
        let mut occluders = [Rect::new(0, 0, 0, 0); 32];
        let mut occ_count: usize = 0;
        let abs = ctx.objs.abs_at(di);

        let alpha = ctx.lcd.has_alpha();
        for j in sub_end..ctx.objs.dfs_len() {
            if ctx.objs.vis_at(j) {
                let other_abs = ctx.objs.abs_at(j);
                if !abs.intersects(&other_abs) || scroll_hidden(&ctx.objs, j, &other_abs) {
                    continue;
                }
                if occ_count < 32 {
                    occluders[occ_count] = other_abs;
                    occ_count += 1;
                }
                // Left out of this subtree's clip, a see-through widget in
                // front would keep showing what was behind it before: it is
                // drawn again too (it restores behind itself first, this
                // subtree included). Later in DFS order, so this pass still
                // reaches it.
                let jid = ctx.objs.dfs_at(j);
                if alpha && !ctx.objs.get(jid).is_dirty() && shows_behind_on::<P>(&ctx.objs, jid) {
                    ctx.objs.mark_dirty(jid);
                }
            }
        }

        // Pass 1b put back what is behind this subtree (it moved): its
        // see-through widgets inside the root's box need not restore it again.
        // Not if a widget before it was drawn in this pass over its box: that
        // one left the box out of its clip (every later widget is an
        // occluder), counting on this subtree to restore behind itself.
        let moved_fresh = restores.fresh_behind(di, abs) && !painted_before(&ctx.objs, di, &abs);

        // Its pixels were shifted (pass 0): draw only what came into view
        // (`shift_keep`, kept in `ctx.objs.render.shift_keep`).
        let keep = !ctx.objs.render.shifted.is_empty() && shift_keep(ctx, di, &abs);

        // Flat iteration over dirty widget + its descendants
        let mut next = di;
        while next < sub_end {
            let si = next;
            next += 1;
            let sid = ctx.objs.dfs_at(si);
            if !ctx.objs.vis_at(si) {
                continue;
            }

            let sabs = ctx.objs.abs_at(si);
            // Children of scroll containers: outside or partly visible — skip the subtree.
            if scroll_hidden(&ctx.objs, si, &sabs) {
                next = ctx.objs.sub_end_at(si);
                continue;
            }

            let mut clip = ClipRegion::from_rect(sabs);
            clip.clip_to_bounds(&screen_rect(&ctx.lcd));
            if keep {
                // One rect cut by at most MAX_CLIP_RECTS disjoint ones: fits.
                clip.restrict(&ctx.objs.render.shift_keep);
            }

            for oi in 0..occ_count {
                clip.subtract(&occluders[oi]);
            }

            // A gauge whose own value changed (it is the dirty root, so no
            // ancestor repainted over it) redraws only the boxes the change
            // touches: the same drawing, clipped smaller.
            let gauge = ctx.objs.get(sid).kind == KIND_GAUGE;
            if gauge && si == di {
                if let Some((rects, n)) = gauge_changed_rects::<P>(&ctx.objs, sid, &sabs) {
                    clip.restrict(&rects[..n]);
                }
            }

            if !clip.is_empty() {
                // An empty group (a label without text, bg or border) inside
                // the subtree root's box: nothing to draw, and nothing of it
                // to erase, as the root repaints or restores its whole box
                // first. (The root itself still restores: it may just have
                // lost its text.)
                if si != di && paints_nothing(&ctx.objs, sid) && contains(&abs, &sabs) {
                    ctx.objs.get_mut(sid).flags |= FLAG_RENDERED | FLAG_PAINTED;
                    continue;
                }
                // Translucent widgets and transparent labels must not blend
                // over their own previous output — restore what is behind
                // them (same clip) first, unless this subtree's pass already
                // repainted all of it. Only widgets of this subtree count:
                // they share its occluder list, so they were painted
                // everywhere this widget's clip reaches.
                if ctx.lcd.has_alpha()
                    && shows_behind_on::<P>(&ctx.objs, sid)
                    && !behind_is_fresh(&ctx.objs, di, si, &sabs, moved_fresh && contains(&abs, &sabs))
                {
                    redraw_behind_region(ctx, si, &clip, &sabs);
                }
                draw_widget(ctx, sid, &sabs, &clip);
                ctx.objs.get_mut(sid).flags |= FLAG_RENDERED | FLAG_PAINTED;
            }
            if gauge {
                // All of its visible part now shows the current value.
                record_drawn(ctx, sid, &sabs);
            }
        }
        di = sub_end;
    }
    clear_painted(&mut ctx.objs);

    // Draw scrollbars on top of all children (only for dirty scroll containers)
    for di in 0..ctx.objs.dfs_len() {
        let id = ctx.objs.dfs_at(di);
        let w = ctx.objs.get(id);
        if w.flags & FLAG_CLIP_CHILDREN != 0 && w.is_dirty() && ctx.objs.vis_at(di) {
            let abs = ctx.objs.abs_at(di);
            draw_scrollbar(ctx, id, &abs);
        }
    }
}

// --- Layers (`layer.rs`) ---

/// Decides how each layer is drawn this render (`ObjPool::layer_fit`):
/// (re)allocates the surfaces of active ones, hands the ones that stop back
/// to the tree, and for composed ones notes where the screen must show them
/// (`Slot::shown`; the old place goes to `Slot::restore`). Scope must be
/// `All`, origin 0. Returns how many are active.
#[cfg(feature = "layers")]
fn layers_sync<P: Platform>(ctx: &mut Ctx<P>) -> usize {
    use crate::layer::{Fit, MAX_LAYERS, Slot};
    let none = Rect::new(0, 0, 0, 0);
    let merge = |a: Rect, b: Rect| {
        if a.is_empty() {
            b
        } else if b.is_empty() {
            a
        } else {
            a.union(&b)
        }
    };
    let count = ctx.lcd.layer_count();
    let can_compose = ctx.lcd.can_compose();
    let mut active = 0;
    let mut composed = 0u8;
    for s in 0..MAX_LAYERS {
        let mut slot: Slot = ctx.objs.layers.slots[s];
        let live = slot.root.is_some() && ctx.objs.is_live(slot.root);
        let range = if live { ctx.objs.dfs_index(slot.root).map(|ri| (ri, ctx.objs.sub_end_at(ri))) } else { None };
        let mut fit = match range {
            Some((ri, end)) if s < count => ctx.objs.layer_fit(s, ri, end),
            _ => Fit::Tree,
        };
        if fit == Fit::Compose && !can_compose {
            fit = Fit::Tree;
        }
        let (ri, end) = range.unwrap_or((0, 0));
        if fit != Fit::Tree {
            let size = ctx.objs.get(slot.root).size;
            if !slot.active || (slot.w, slot.h) != (size.w, size.h) {
                if ctx.lcd.layer_alloc(s, size.w, size.h) {
                    if !slot.active {
                        slot.restore = ctx.objs.layer_drawn_bounds(ri, end);
                    }
                    slot.active = true;
                    slot.full = true;
                    slot.w = size.w;
                    slot.h = size.h;
                } else {
                    fit = Fit::Tree;
                }
            }
        }
        if fit == Fit::Tree && slot.active {
            // Drawn in the tree again.
            if slot.composed && live && !slot.shown.is_empty() {
                // Where the screen shows its surface (the root may have moved
                // since) is put back as the root's previous place.
                ctx.objs.set_prev_rect(slot.root, 0, slot.shown);
                ctx.objs.get_mut(slot.root).flags |= FLAG_RENDERED;
            }
            slot.active = false;
            slot.composed = false;
            slot.shown = none;
            slot.recompose = false;
            slot.w = 0;
            slot.h = 0;
            if live {
                ctx.objs.mark_dirty(slot.root);
            }
        }
        if slot.active {
            let compose = fit == Fit::Compose;
            if compose != slot.composed {
                // Into the screen: drawn there below. Out of it: what the
                // screen shows of it is put back (its window shows it).
                slot.restore = merge(slot.restore, slot.shown);
                slot.shown = none;
                slot.composed = compose;
                slot.recompose = compose;
            }
            if slot.composed {
                let at = if ctx.objs.vis_at(ri) && slot.alpha != 0 { ctx.objs.abs_at(ri) } else { none };
                if at != slot.shown || slot.alpha != slot.shown_alpha {
                    slot.restore = merge(slot.restore, slot.shown);
                    slot.shown = at;
                    slot.shown_alpha = slot.alpha;
                    slot.recompose = true;
                }
                composed |= 1 << s;
            }
        }
        if !slot.active {
            // Also surfaces left over from a cleared pool.
            ctx.lcd.layer_free(s);
        }
        if !live {
            slot = Slot { root: WidgetId::NONE, alpha: 255, active: false, ..slot };
        }
        active += slot.active as usize;
        ctx.objs.layers.slots[s] = slot;
    }
    ctx.objs.layers.composed = composed;
    active
}

/// The screen's scope: every DFS entry but the active layers' subtrees.
#[cfg(feature = "layers")]
fn base_scope(objs: &ObjPool) -> crate::layer::Scope {
    let mut excl = [(0u32, 0u32); crate::layer::MAX_LAYERS];
    let mut n = 0;
    for slot in objs.layers.slots.iter().filter(|s| s.active) {
        if let Some(ri) = objs.dfs_index(slot.root) {
            // A composed layer's root is drawn on the screen (its surface).
            excl[n] = (ri as u32 + slot.composed as u32, objs.sub_end_at(ri) as u32);
            n += 1;
        }
    }
    crate::layer::Scope::Base { excl, n: n as u8 }
}

/// Draws layer `s`'s dirty widgets into its surface (`all`: every widget,
/// from a cleared surface). Scope must be `All` on entry and is again after.
/// True if anything was drawn.
#[cfg(feature = "layers")]
fn layer_pass<P: Platform>(ctx: &mut Ctx<P>, s: usize, all: bool) -> bool {
    let slot = ctx.objs.layers.slots[s];
    let Some(ri) = ctx.objs.dfs_index(slot.root) else { return false };
    let end = ctx.objs.sub_end_at(ri);
    let abs = ctx.objs.abs_at(ri);
    let mut full = slot.full || all;
    if !ctx.objs.vis_at(ri) {
        // Hidden: its surface keeps what it showed; anything that changed
        // meanwhile is redrawn when it shows again.
        if ctx.objs.layer_has_dirty(ri, end, false) {
            ctx.objs.layers.slots[s].full = true;
        }
        return false;
    }
    if !full && !ctx.objs.layer_has_dirty(ri, end, false) {
        ctx.objs.layers.slots[s].at = Offset { x: abs.x, y: abs.y };
        return false;
    }
    let at = Offset { x: abs.x, y: abs.y };
    // Prev rects recorded since the root last moved are relative to a
    // position the surface no longer has: no partial restore can be trusted.
    if (at.x, at.y) != (slot.at.x, slot.at.y) && ctx.objs.layer_has_dirty(ri, end, true) {
        full = true;
    }
    ctx.objs.layers.scope = crate::layer::Scope::Layer(ri as u32, end as u32);
    ctx.objs.layers.origin = at;
    ctx.lcd.layer_target(Some((s, slot.w, slot.h)));
    if full {
        ctx.lcd.layer_clear(&Rect::new(0, 0, slot.w, slot.h));
        ctx.objs.layer_redraw_all(ri, end);
    }
    if all {
        render_all_iterative(ctx, ri, end);
    } else {
        dirty_passes(ctx, false, false);
    }
    ctx.lcd.layer_target(None);
    ctx.objs.layers.origin = Offset { x: 0, y: 0 };
    ctx.objs.layers.scope = crate::layer::Scope::All;
    let slot = &mut ctx.objs.layers.slots[s];
    slot.at = at;
    slot.full = false;
    true
}

/// Shows every active layer where its root is (scope `All`, origin 0).
#[cfg(feature = "layers")]
fn place_layers<P: Platform>(ctx: &mut Ctx<P>) {
    for s in 0..crate::layer::MAX_LAYERS {
        let slot = ctx.objs.layers.slots[s];
        if !slot.active {
            continue;
        }
        if let Some(ri) = ctx.objs.dfs_index(slot.root) {
            let abs = ctx.objs.abs_at(ri);
            let vis = ctx.objs.vis_at(ri) && !slot.composed;
            ctx.lcd.layer_place(s, abs.x as i32, abs.y as i32, slot.alpha, vis);
        }
    }
}

/// Hands every active layer back to the tree and frees the surfaces
/// (buffered mode, which doesn't use layers).
#[cfg(feature = "layers")]
fn layers_off<P: Platform>(ctx: &mut Ctx<P>) {
    for s in 0..crate::layer::MAX_LAYERS {
        let slot = ctx.objs.layers.slots[s];
        if slot.active {
            let slot = &mut ctx.objs.layers.slots[s];
            slot.active = false;
            slot.composed = false;
            slot.shown = Rect::new(0, 0, 0, 0);
            slot.recompose = false;
            slot.w = 0;
            slot.h = 0;
            let root = slot.root;
            ctx.objs.mark_dirty(root);
        }
    }
    ctx.objs.layers.composed = 0;
    ctx.lcd.layers_off();
}

/// Most areas one `WidgetRenderer::compose` call gets: more are merged.
pub const COMPOSE_AREAS: usize = 8;
/// `WidgetRenderer::compose` areas start and end on multiples of this (or
/// the screen's edge): the GPU's tiles.
pub const COMPOSE_GRID: i16 = 16;

/// The areas a dirty render changes, rounded out to [`COMPOSE_GRID`],
/// merged until disjoint and at most [`COMPOSE_AREAS`].
struct Areas {
    list: [Rect; COMPOSE_AREAS],
    n: usize,
}

impl Areas {
    fn new() -> Self {
        Self { list: [Rect::new(0, 0, 0, 0); COMPOSE_AREAS], n: 0 }
    }

    fn add(&mut self, r: Rect, screen: &Rect) {
        if r.is_empty() {
            return;
        }
        let g = COMPOSE_GRID as i32;
        let x0 = (r.x as i32).div_euclid(g) * g;
        let y0 = (r.y as i32).div_euclid(g) * g;
        let x1 = (r.x as i32 + r.w as i32 + g - 1).div_euclid(g) * g;
        let y1 = (r.y as i32 + r.h as i32 + g - 1).div_euclid(g) * g;
        let Some(mut r) = rect_from(x0, y0, x1, y1).and_then(|r| r.intersection(screen)) else { return };
        loop {
            if let Some(i) = self.list[..self.n].iter().position(|a| a.intersects(&r)) {
                r = bounds(&r, &self.list[i]);
                self.remove(i);
                continue;
            }
            if self.n < COMPOSE_AREAS {
                break;
            }
            // Full: joined with the one it adds the least to.
            let area = |r: &Rect| r.w as u32 * r.h as u32;
            let i = (0..self.n)
                .min_by_key(|&i| area(&bounds(&r, &self.list[i])) - area(&self.list[i]))
                .unwrap_or(0);
            r = bounds(&r, &self.list[i]);
            self.remove(i);
        }
        self.list[self.n] = r;
        self.n += 1;
    }

    fn remove(&mut self, i: usize) {
        self.n -= 1;
        self.list[i] = self.list[self.n];
    }
}

fn rect_from(x0: i32, y0: i32, x1: i32, y1: i32) -> Option<Rect> {
    let x0 = x0.clamp(i16::MIN as i32, i16::MAX as i32);
    let y0 = y0.clamp(i16::MIN as i32, i16::MAX as i32);
    let (w, h) = ((x1 - x0).clamp(0, u16::MAX as i32), (y1 - y0).clamp(0, u16::MAX as i32));
    (w > 0 && h > 0).then(|| Rect::new(x0 as i16, y0 as i16, w as u16, h as u16))
}

/// The smallest rect holding both.
fn bounds(a: &Rect, b: &Rect) -> Rect {
    let x0 = (a.x as i32).min(b.x as i32);
    let y0 = (a.y as i32).min(b.y as i32);
    let x1 = (a.x as i32 + a.w as i32).max(b.x as i32 + b.w as i32);
    let y1 = (a.y as i32 + a.h as i32).max(b.y as i32 + b.h as i32);
    rect_from(x0, y0, x1, y1).unwrap_or(*a)
}

/// Dirty mode with a board that repaints whole areas
/// (`WidgetRenderer::COMPOSES`): gathers what the render changes (each
/// dirty widget's box where it is visible or was drawn, and where a moved
/// one was drawn), offers it to `WidgetRenderer::compose`, and if taken does
/// the bookkeeping the passes would. False = declined, nothing changed.
///
/// `layered`: active layers, the scope is the screen's (`base_scope`): only
/// its widgets count, plus what the screen drew of layers that just took
/// over their surface (`Slot::restore`, repainted without them). The caller
/// then draws the layers and settles; their widgets' flags are left alone.
#[inline(never)]
fn compose_dirty<P: Platform>(ctx: &mut Ctx<P>, layered: bool) -> bool {
    #[cfg(feature = "layers")]
    let mine = |objs: &ObjPool, di: usize| objs.in_scope(di);
    #[cfg(not(feature = "layers"))]
    let mine = |_: &ObjPool, _: usize| true;
    let screen = screen_rect(&ctx.lcd);
    let mut areas = Areas::new();
    for di in 0..ctx.objs.dfs_len() {
        let id = ctx.objs.dfs_at(di);
        let w = ctx.objs.get(id);
        if !w.is_dirty() || !mine(&ctx.objs, di) {
            continue;
        }
        if ctx.objs.vis_at(di) || w.flags & FLAG_RENDERED != 0 {
            areas.add(ctx.objs.abs_at(di), &screen);
        }
        areas.add(buf_prev_rect(&ctx.objs, id, 0), &screen);
    }
    #[cfg(feature = "layers")]
    if layered {
        for slot in ctx.objs.layers.slots.iter().filter(|s| s.active) {
            areas.add(slot.restore, &screen);
        }
    }
    if areas.n > 0 && !<P::Renderer as WidgetRenderer<P>>::compose(ctx, &areas.list[..areas.n]) {
        return false;
    }
    #[cfg(feature = "layers")]
    if layered {
        for slot in ctx.objs.layers.slots.iter_mut() {
            slot.restore = Rect::new(0, 0, 0, 0);
        }
    }
    for di in 0..ctx.objs.dfs_len() {
        let id = ctx.objs.dfs_at(di);
        if !ctx.objs.get(id).is_dirty() || !mine(&ctx.objs, di) {
            continue;
        }
        // Where a moved widget was drawn: repainted.
        ctx.objs.set_prev_rect(id, 0, Rect::new(0, 0, 0, 0));
        let vis = ctx.objs.vis_at(di);
        let w = ctx.objs.get_mut(id);
        if vis {
            w.flags |= FLAG_RENDERED;
        } else {
            w.flags &= !FLAG_RENDERED;
        }
    }
    // What gauges recorded of their last drawing is stale: the areas were
    // drawn another way.
    ctx.objs.render.draw_epoch = ctx.objs.render.draw_epoch.wrapping_add(1);
    if !layered {
        clear_all_dirty(ctx);
        ctx.objs.render.shifted.clear();
        ctx.objs.render.settled_epoch = ctx.objs.render.draw_epoch;
    }
    true
}

/// Most rects one shift moves (they come from two clip regions).
const SHIFT_MAX: usize = crate::clip::MAX_CLIP_RECTS;

/// Dirty mode, pass 0 (before the restores): a widget that only moved since
/// the last render, and paints its box opaquely, still shows its last
/// drawing at the old place. Instead of putting back what is behind the old
/// box and drawing the whole subtree at the new one, its pixels are moved
/// there (`LcdImpl::move_rects`); only the part of the old box it left is
/// restored, and pass 2 draws only the part of the new box that came into
/// view (`ObjPool::shifted`). A sliding drawer then costs two strips a frame.
///
/// Taken only when the result is the same as the full redraw:
/// - `settled`: nothing drew over the screen since the last render, and the
///   tree kept its shape (every widget shows what it last drew);
/// - the root moved (`will_move` recorded where it was drawn), kept its
///   size, its parent is not dirty, and nothing in its subtree changed
///   (`ObjPool::changed_within`; location writes use `mark_moved`);
/// - it paints its box opaquely (`opaque_radius`) apart from its rounded
///   corners, which are left out (what is behind them shows through), and
///   it is not inside a scroll container (viewport clipping);
/// - pixels of widgets in front of it (where they are or were, drawn or
///   hidden) are neither moved nor overwritten;
/// - every clip region involved is exact (no overflow).
///
/// Several shifts in one render must not touch each other's boxes (one's
/// restore would overwrite the other's source).
#[inline(never)]
fn shift_moved<P: Platform>(ctx: &mut Ctx<P>, settled: bool, restores: &mut Restores) {
    ctx.objs.render.shifted.clear();
    if !settled || ctx.lcd.is_portrait() {
        return;
    }
    let screen = screen_rect(&ctx.lcd);
    let n = ctx.objs.dfs_len();
    let mut touched: Option<Rect> = None;
    for di in 0..n {
        let id = ctx.objs.dfs_at(di);
        let w = ctx.objs.get(id);
        if !w.is_dirty() || w.flags & FLAG_RENDERED == 0 || !ctx.objs.vis_at(di) {
            continue;
        }
        let old = ctx.objs.prev_rect(id, 0);
        let new = ctx.objs.abs_at(di);
        if old.is_empty() || old == new || old.w != new.w || old.h != new.h {
            continue;
        }
        let parent = w.parent;
        if parent.is_none() || ctx.objs.get(parent).is_dirty() || ctx.objs.scroll_parent_at(di).is_some() {
            continue;
        }
        let span = old.union(&new);
        if touched.is_some_and(|t| t.intersects(&span)) || ctx.objs.changed_within(id) {
            continue;
        }
        let Some(radius) = opaque_radius(&ctx.objs, id, &new) else { continue };
        let sub_end = ctx.objs.sub_end_at(di);
        // A descendant that moved on its own (pending prev rect) would be
        // carried along at its old place: redraw instead.
        if (di + 1..sub_end).any(|j| !ctx.objs.prev_rect(ctx.objs.dfs_at(j), 0).is_empty()) {
            continue;
        }
        // A small box is cheaper to redraw than to move (a move costs a
        // fixed setup per rect on the exynos G2D: the Monitor stress test's
        // 51 tiles a frame were slower moved than redrawn).
        if (old.w as u32) * (old.h as u32) < <P::LcdB as LcdBackend>::MOVE_MIN_PX {
            continue;
        }
        let Some(moves) = plan_moves(&ctx.objs, &old, &new, radius, sub_end, &screen) else { continue };
        let (dx, dy) = (new.x - old.x, new.y - old.y);
        let dest = |m: &Rect| Rect::new(m.x + dx, m.y + dy, m.w, m.h);
        // What it leaves behind: the old box but where it moves to.
        let mut behind = ClipRegion::from_rect(old);
        behind.clip_to_bounds(&screen);
        let mut exact = true;
        for m in &moves {
            exact &= behind.subtract(&dest(m));
        }
        if !exact || moves.is_empty() || !ctx.lcd.move_rects(&moves, dx, dy) {
            continue;
        }
        #[cfg(feature = "perf")]
        {
            use crate::perf::{add_count, count};
            add_count(count::SHIFTS, 1);
            let px: u32 = moves.iter().map(|m| m.w as u32 * m.h as u32).sum();
            add_count(count::SHIFT_KPX, (px + 512) >> 10);
        }
        // From here on as `restore_behind`, over `behind` only.
        ctx.objs.screen_touched();
        if !behind.is_empty() {
            redraw_behind_region(ctx, di, &behind, &old);
            for j in sub_end..n {
                if !ctx.objs.vis_at(j) {
                    continue;
                }
                let a = ctx.objs.abs_at(j);
                if behind.iter().iter().any(|r| r.intersects(&a)) {
                    let jid = ctx.objs.dfs_at(j);
                    if !ctx.objs.get(jid).is_dirty() {
                        ctx.objs.mark_dirty(jid);
                    }
                }
            }
        }
        // Pass 1b must not restore the old box again.
        ctx.objs.set_prev_rect(id, 0, Rect::new(0, 0, 0, 0));
        restores.push(di, old);
        for m in &moves {
            ctx.objs.render.shifted.push((di, dest(m)));
        }
        touched = Some(touched.map_or(span, |t| t.union(&span)));
    }
}

/// The corner radius of widget `id` (at `abs`) if it paints its whole box
/// opaquely except, when rounded, the four `radius` x `radius` corners;
/// None if anything behind it may show through elsewhere (or it paints by
/// script, `on_paint`).
fn opaque_radius(tree: &ObjPool, id: WidgetId, abs: &Rect) -> Option<u16> {
    #[cfg(feature = "layers")]
    if tree.composed_root(id).is_some() {
        return None;
    }
    let w = tree.get(id);
    let ext = &tree.ext(id);
    if is_freeform_shape(w.kind) || ext.alpha < 255 || w.flags & FLAG_HAS_ON_PAINT != 0 {
        return None;
    }
    // Same condition as `draw_bg` in `draw_widget`.
    let bg = effective_bg(tree, w, ext);
    if bg == 0 && (w.kind == KIND_LABEL || w.kind == KIND_IMAGE) {
        return None;
    }
    Some(ext.border_radius.min(abs.w / 2).min(abs.h / 2))
}

/// The rects (at the old place) whose pixels move from `old` to `new`: the
/// box at both places on screen, without the rounded corners (`radius`) and
/// without what any widget from DFS index `front` on covers or covered
/// there. None if a region overflowed (it would be too large) or there are
/// too many pieces.
fn plan_moves(tree: &ObjPool, old: &Rect, new: &Rect, radius: u16, front: usize, screen: &Rect) -> Option<alloc::vec::Vec<Rect>> {
    let (dx, dy) = (new.x - old.x, new.y - old.y);
    let mut src = ClipRegion::from_rect(*old);
    src.clip_to_bounds(screen);
    let mut dst = ClipRegion::from_rect(*new);
    dst.clip_to_bounds(screen);
    let mut exact = true;
    if radius > 0 {
        for (region, b) in [(&mut src, old), (&mut dst, new)] {
            let r = radius;
            let (x1, y1) = (b.right() - r as i16, b.bottom() - r as i16);
            for (x, y) in [(b.x, b.y), (x1, b.y), (b.x, y1), (x1, y1)] {
                exact &= region.subtract(&Rect::new(x, y, r, r));
            }
        }
    }
    let span = old.union(new);
    for j in front..tree.dfs_len() {
        let jid = tree.dfs_at(j);
        // Drawn now or before (a hidden one still shows until pass 1).
        if !tree.vis_at(j) && tree.get(jid).flags & FLAG_RENDERED == 0 {
            continue;
        }
        for r in [tree.abs_at(j), tree.prev_rect(jid, 0)] {
            if r.intersects(&span) {
                exact &= src.subtract(&r);
                exact &= dst.subtract(&r);
            }
        }
    }
    if !exact {
        return None;
    }
    let mut out = alloc::vec::Vec::new();
    for s in src.iter() {
        for d in dst.iter() {
            if let Some(i) = s.intersection(&Rect::new(d.x - dx, d.y - dy, d.w, d.h)) {
                if out.len() == SHIFT_MAX {
                    return None;
                }
                out.push(i);
            }
        }
    }
    Some(out)
}

/// For the dirty subtree at DFS index `di` (root at `abs`): if pass 0 moved
/// its pixels, put in `ctx.objs.render.shift_keep` the part of its box still to be
/// drawn (on screen, minus the moved rects) and return true. False = draw
/// it whole (nothing moved, or the region overflowed).
#[inline(never)]
fn shift_keep<P: Platform>(ctx: &mut Ctx<P>, di: usize, abs: &Rect) -> bool {
    if !ctx.objs.render.shifted.iter().any(|&(i, _)| i == di) {
        return false;
    }
    let mut keep = ClipRegion::from_rect(*abs);
    keep.clip_to_bounds(&screen_rect(&ctx.lcd));
    let mut exact = true;
    for &(i, r) in &ctx.objs.render.shifted {
        if i == di {
            exact &= keep.subtract(&r);
        }
    }
    if !exact {
        return false;
    }
    ctx.objs.render.shift_keep.clear();
    ctx.objs.render.shift_keep.extend_from_slice(keep.iter());
    true
}

/// After a complete drawing of widget `id` at `abs`: remember what an
/// incrementally redrawn kind (gauge) now shows.
fn record_drawn<P: Platform>(ctx: &mut Ctx<P>, id: WidgetId, abs: &Rect) {
    if ctx.objs.get(id).kind == KIND_GAUGE {
        let key = gauge_key::<P>(&ctx.objs, id, abs);
        let value = ctx.objs.get(id).value;
        ctx.objs.set_drawn(id, key, value);
    }
}

/// The restores of one `render_dirty` (pass 1 and 1b), in order: the DFS
/// index of the widget whose behind was redrawn, and where.
struct Restores {
    list: [(usize, Rect); 16],
    n: usize,
    /// More than fit: no freshness is claimed.
    full: bool,
}

impl Restores {
    fn new() -> Self {
        Self { list: [(0, Rect::new(0, 0, 0, 0)); 16], n: 0, full: false }
    }

    fn push(&mut self, idx: usize, rect: Rect) {
        if self.n < self.list.len() {
            self.list[self.n] = (idx, rect);
            self.n += 1;
        } else {
            self.full = true;
        }
    }

    /// Whether the screen holds nothing the dirty subtree rooted at `di`
    /// (now at `current`) drew before, inside its root's box: true when its
    /// own restore put back what was behind its old box (it moved) — outside
    /// that box it never drew. Unless a later restore for a widget past `di`
    /// touched the old or new box: that one redraws everything before its
    /// widget, so the subtree too.
    fn fresh_behind(&self, di: usize, current: Rect) -> bool {
        if self.full {
            return false;
        }
        let Some(at) = self.list[..self.n].iter().position(|&(i, _)| i == di) else {
            return false;
        };
        let old = self.list[at].1;
        !self.list[at + 1..self.n]
            .iter()
            .any(|&(i, r)| i > di && (r.intersects(&old) || r.intersects(&current)))
    }
}

/// A visible widget before DFS position `idx` was drawn in this pass
/// (`FLAG_PAINTED`) and touches `rect`.
fn painted_before(tree: &ObjPool, idx: usize, rect: &Rect) -> bool {
    (0..idx).any(|k| {
        tree.vis_at(k) && tree.get(tree.dfs_at(k)).flags & FLAG_PAINTED != 0 && tree.abs_at(k).intersects(rect)
    })
}

/// A widget that draws no pixel: a label without text, background, border
/// or background image (tablet demo groups).
fn paints_nothing(tree: &ObjPool, id: WidgetId) -> bool {
    #[cfg(feature = "layers")]
    if tree.composed_root(id).is_some() {
        return false;
    }
    let w = tree.get(id);
    if w.kind != KIND_LABEL || w.flags & FLAG_HAS_ON_PAINT != 0 {
        return false;
    }
    let ext = tree.ext(id);
    let b = ext.border;
    let no_text = ext.text_id == crate::pool::PTR_NONE || tree.str_get(ext.text_id).is_empty();
    no_text
        && ext.image_id == 0
        && (b.top | b.bottom | b.left | b.right) == 0
        && effective_bg(tree, w, &ext) == 0
}

/// Clear the per-pass `FLAG_PAINTED` marks.
fn clear_painted(tree: &mut ObjPool) {
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
/// all of `rect` opaquely — nothing behind that one can show through — or,
/// past the subtree root `lo`, `fresh`: nothing the subtree drew before is
/// left behind `rect` (see `Restores::fresh_behind`).
fn behind_is_fresh(tree: &ObjPool, lo: usize, idx: usize, rect: &Rect, fresh: bool) -> bool {
    for k in (lo..idx).rev() {
        if !tree.vis_at(k) {
            continue;
        }
        let id = tree.dfs_at(k);
        let abs = tree.abs_at(k);
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
    fresh
}

/// `inner` lies entirely within `outer`.
fn contains(outer: &Rect, inner: &Rect) -> bool {
    inner.x >= outer.x && inner.y >= outer.y && inner.right() <= outer.right() && inner.bottom() <= outer.bottom()
}

/// Does `draw_widget` paint every pixel of `rect` opaquely for `id` (whose
/// absolute rect is `abs`)? True for a solid, square-cornered widget with a
/// background: its border strips plus background fill tile its whole rect.
pub fn covers_opaquely(tree: &ObjPool, id: WidgetId, abs: &Rect, rect: &Rect) -> bool {
    #[cfg(feature = "layers")]
    if tree.composed_root(id).is_some() {
        return false;
    }
    let w = tree.get(id);
    let ext = &tree.ext(id);
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
/// As the board's renderer reports it (`WidgetRenderer::shows_behind`), else
/// the built-in rule ([`shows_behind`]).
fn shows_behind_on<P: Platform>(tree: &ObjPool, id: WidgetId) -> bool {
    #[cfg(feature = "layers")]
    if tree.composed_root(id).is_some() {
        return true;
    }
    P::Renderer::shows_behind(tree, id).unwrap_or_else(|| shows_behind(tree, id))
}

/// [`shows_behind_on`] for the built-in drawing.
pub fn shows_behind(tree: &ObjPool, id: WidgetId) -> bool {
    #[cfg(feature = "layers")]
    if tree.composed_root(id).is_some() {
        return true;
    }
    if tree.alpha(id) < 255 {
        return true;
    }
    let w = tree.get(id);
    if is_freeform_shape(w.kind) || tree.border_radius(id) != 0 {
        return true;
    }
    // Without a background a label shows only its text, an image only its
    // opaque pixels (an alpha icon swapped for another must not keep the
    // old one's pixels).
    (w.kind == KIND_LABEL || w.kind == KIND_IMAGE)
        && effective_bg(tree, w, &tree.ext(id)) == 0
}

// --- Drawing ---

/// Effective background color for a widget.
/// If the widget is pressed and has press_color, returns press_color.
/// If a parent is pressed and this widget shares the same background,
/// inherits the parent's press_color — unless it is transparent (a label or
/// image without a background paints none): the pressed parent's fill, with
/// its rounded corners, already shows through, and a filled copy would be a
/// square box over it.
pub fn effective_bg(tree: &ObjPool, widget: &Widget, ext: &WidgetExt) -> Color {
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
        let p_ext = &tree.ext(pid);
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
fn resolved_text_bg(tree: &ObjPool, widget: &Widget, ext: &WidgetExt, fallback: Color) -> Color {
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
        let p_ext = &tree.ext(pid);
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
fn draw_widget<P: Platform>(ctx: &Ctx<P>, id: WidgetId, abs: &Rect, clip: &ClipRegion) {
    #[cfg(feature = "perf")]
    let t0 = crate::perf::now();
    #[cfg(feature = "layers")]
    if let Some(s) = ctx.objs.composed_root(id) {
        // A composed layer's root on the screen: its surface.
        let slot = &ctx.objs.layers.slots[s];
        let (at, size) = (Offset { x: abs.x, y: abs.y }, crate::types::Size { w: slot.w, h: slot.h });
        for r in clip.iter() {
            ctx.lcd.layer_compose(s, at, size, r, slot.alpha);
        }
        #[cfg(feature = "perf")]
        crate::perf::add_sub(crate::perf::phase::DRAW, crate::perf::now().wrapping_sub(t0));
        return;
    }
    if !P::Renderer::draw(ctx, id, abs, clip) {
        draw_widget_body(ctx, id, abs, clip);
    }
    #[cfg(feature = "perf")]
    crate::perf::add_sub(crate::perf::phase::DRAW, crate::perf::now().wrapping_sub(t0));
}

/// Draw widget `id` the built-in way (what [`DefaultRenderer`] does), for a
/// board renderer that adds to it or handles only some cases of a kind.
///
/// [`DefaultRenderer`]: crate::renderer::DefaultRenderer
pub fn draw_default<P: Platform>(ctx: &Ctx<P>, id: WidgetId, abs: &Rect, clip: &ClipRegion) {
    draw_widget_body(ctx, id, abs, clip);
}

fn draw_widget_body<P: Platform>(
    ctx: &Ctx<P>,
    id: WidgetId,
    abs: &Rect,
    clip: &ClipRegion,
) {
    let widget = ctx.objs.get(id);
    let ext = &ctx.objs.ext(id);
    let b = ext.border;
    let r = ext.border_radius;
    let bg_color = effective_bg(&ctx.objs, widget, ext);
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
        ctx.objs.gradient_dir(id)
    } else {
        0
    };
    let gcol = if gdir != 0 {
        ctx.objs.gradient_color(id)
    } else {
        0
    };
    // Write-only backends anti-alias against known colours: what is behind
    // this widget, and (for its contents) its own background.
    let back = backdrop(&ctx.objs, widget, P::DEFAULT_BG_COLOR);
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
                //
                // Readback backends draw the whole box, partly off the screen
                // or not (see `fill_rounded_rect_screen`); write-only backends
                // keep the box cut to the screen.
                let outer = if <P::LcdB as LcdBackend>::HAS_ALPHA {
                    *abs
                } else {
                    let Some(outer) = abs.intersection(&screen_rect(&ctx.lcd)) else {
                        continue;
                    };
                    outer
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
    // The text is also confined to the label's box: glyphs taller
    // than the box were drawn past it by a full redraw (`render_all` clips to
    // the screen only), and no later redraw or restore, both clipped to the
    // box, erased those pixels again.
    if widget.kind == KIND_LABEL {
        if ctx.lcd.has_alpha() {
            for cr in clip.iter() {
                if let Some(r) = cr.intersection(abs) {
                    ctx.lcd.set_scissor(Some(r));
                    draw_label_text(ctx, widget, abs, ext, None);
                }
            }
        } else {
            let bg = resolved_text_bg(&ctx.objs, widget, ext, P::DEFAULT_BG_COLOR);
            ctx.lcd.set_scissor(Some(*abs));
            draw_label_text(ctx, widget, abs, ext, Some(bg));
        }
        ctx.lcd.set_scissor(None);
    }

    // Value fill (progress / slider), check indicator, input and dropdown
    // contents. Their primitives take no clip: confine them to the clip
    // region and the box via the scissor. Without it a clipped redraw drew
    // them over widgets in front, and over pixels it was not to touch (a
    // shifted subtree's: the AA ends of a fill were blended twice).
    if matches!(
        widget.kind,
        KIND_PROGRESS | KIND_SLIDER | KIND_CHECKBOX | KIND_RADIO | KIND_INPUT | KIND_DROPDOWN
    ) {
        let inner = inner_rect(abs, &b);
        for cr in clip.iter() {
            let Some(r) = cr.intersection(abs) else { continue };
            ctx.lcd.set_scissor(Some(r));
            match widget.kind {
                KIND_PROGRESS | KIND_SLIDER => draw_value_fill(&ctx.lcd, widget, &inner, ext),
                KIND_CHECKBOX | KIND_RADIO => draw_check_indicator(&ctx.lcd, widget, &inner, ext),
                _ => draw_field(ctx, widget, abs, ext),
            }
        }
        ctx.lcd.set_scissor(None);
    }

    // Gauge arc. Like the shapes below, its primitives take no clip: confine
    // them to the clip region via the scissor.
    if widget.kind == KIND_GAUGE {
        if let Some(g) = GaugeGeom::new(&inner_rect(abs, &ext.border)) {
            for cr in clip.iter() {
                ctx.lcd.set_scissor(Some(*cr));
                draw_gauge(&ctx.lcd, widget, &g, ext);
            }
            ctx.lcd.set_scissor(None);
        }
    }

    // Spline graph. Its curve and fill primitives take no clip: confine them
    // to the clip region and the widget's box via the scissor. Without it a
    // graph updated under a widget in front (a pull-down shade) drew over it,
    // and the spline's overshoot past the top or bottom left lines outside
    // the box, where no restore reaches.
    if widget.kind == KIND_GRAPH {
        let inner = inner_rect(abs, &b);
        for cr in clip.iter() {
            if let Some(r) = cr.intersection(abs) {
                ctx.lcd.set_scissor(Some(r));
                draw_graph(&ctx.lcd, &ctx.objs, widget, &inner, ext);
            }
        }
        ctx.lcd.set_scissor(None);
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
pub fn backdrop(tree: &ObjPool, w: &Widget, fallback: Color) -> Color {
    let mut pid = w.parent;
    let mut depth = 0usize;
    while pid.is_some() {
        let p = tree.get(pid);
        let bg = effective_bg(tree, p, &tree.ext(pid));
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

/// One anti-aliased primitive of a round widget, as its drawing on a readback backend issues
/// it ([`round_parts`]): for renderers that draw them another way (a GPU's triangles).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundPart {
    /// `LcdImpl::draw_oval(x, y, w, h, t, color)`.
    Oval { x: i16, y: i16, w: u16, h: u16, t: u16, color: Color },
    /// `LcdImpl::fill_arc(cx, cy, r, ir, start, end, color)`.
    Arc { cx: i16, cy: i16, r: i16, ir: i16, start: i16, end: i16, color: Color },
    /// `LcdImpl::draw_wedge(ax, ay, bx, by, ra_q8, rb_q8, color)` (`draw_line_thick(t)` =
    /// both radii `t · 128`).
    Wedge { ax: i16, ay: i16, bx: i16, by: i16, ra_q8: i32, rb_q8: i32, color: Color },
    /// A plain rect, cut to the screen (`fill_rect_screen`).
    Rect { x: i16, y: i16, w: u16, h: u16, color: Color },
    /// `LcdImpl::fill_rounded_rect_at(x, y, w, h, r, color, 255)` when `t` is 0, else
    /// `LcdImpl::stroke_rounded_rect(x, y, w, h, r, t, color)`; `r` > 0.
    RoundRect { x: i16, y: i16, w: u16, h: u16, r: u16, t: u16, color: Color },
    /// `LcdImpl::draw_polyline(&pts[..n], color, false, t)` (a check mark's tick).
    Path { pts: [(i16, i16); 3], n: u8, t: u16, color: Color },
}

/// A fill of `rect` with corner radius `r` as `fill_rounded_rect_screen` draws it: a plain
/// rect when `r` is 0.
fn round_rect_part(rect: Rect, r: u16, color: Color) -> RoundPart {
    if r > 0 {
        RoundPart::RoundRect { x: rect.x, y: rect.y, w: rect.w, h: rect.h, r, t: 0, color }
    } else {
        RoundPart::Rect { x: rect.x, y: rect.y, w: rect.w, h: rect.h, color }
    }
}

/// `draw_value_fill`'s parts (readback backends).
fn value_fill_parts(widget: &Widget, inner: &Rect, ext: &WidgetExt, f: &mut impl FnMut(RoundPart)) {
    if inner.is_empty() || ext.press_color == 0 {
        return;
    }
    let r = ext.border_radius;
    let val = ext.value.clamp(0, 100) as u32;
    let fill_w = ((inner.w as u32) * val / 100) as u16;
    if fill_w > 0 {
        let fill = Rect::new(inner.x, inner.y, fill_w, inner.h);
        let fill_r = if r > 0 { r.min(fill_w / 2).min(inner.h / 2) } else { 0 };
        f(round_rect_part(fill, fill_r, ext.press_color));
    }
    if widget.kind == KIND_SLIDER {
        let thumb_w: u16 = 6;
        let thumb_x = (inner.x + fill_w as i16 - (thumb_w as i16 / 2)).max(inner.x).min(inner.x + inner.w as i16 - thumb_w as i16);
        let thumb = Rect::new(thumb_x, inner.y, thumb_w, inner.h);
        let color = if widget.border_color != 0 { widget.border_color } else { 0xFFFF };
        let thumb_r = if r > 0 { r.min(thumb_w / 2).min(inner.h / 2) } else { 0 };
        f(round_rect_part(thumb, thumb_r, color));
    }
}

/// `draw_check_indicator`'s parts (readback backends: smooth).
fn check_parts(widget: &Widget, inner: &Rect, ext: &WidgetExt, f: &mut impl FnMut(RoundPart)) {
    if inner.is_empty() {
        return;
    }
    let p = ext.padding;
    let r = ext.border_radius;
    let (cx, cy) = (inner.x + p.left as i16, inner.y + p.top as i16);
    let s = inner.h.saturating_sub(p.top as u16 + p.bottom as u16);
    if s < 4 {
        return;
    }
    let outline = ext.text_color;
    let checked = widget.flags & FLAG_CHECKED != 0;
    let fill = if ext.press_color != 0 { ext.press_color } else { outline };
    if widget.kind == KIND_CHECKBOX {
        if r > 0 {
            let br = r.min(s / 2);
            f(RoundPart::RoundRect { x: cx, y: cy, w: s, h: s, r: br, t: 1, color: outline });
            if checked {
                let inset = (s as i16) / 4;
                let side = s.saturating_sub(inset as u16 * 2);
                f(round_rect_part(Rect::new(cx + inset, cy + inset, side, side), br.saturating_sub(inset as u16), fill));
            }
        } else {
            let line = |x, y, w, h| RoundPart::Rect { x, y, w, h, color: outline };
            f(line(cx, cy, s, 1));
            f(line(cx, cy + s as i16 - 1, s, 1));
            f(line(cx, cy, 1, s));
            f(line(cx + s as i16 - 1, cy, 1, s));
            if checked {
                let s = s as i16;
                let pts = [(cx + s / 4, cy + s / 2), (cx + s * 2 / 5, cy + s * 3 / 4), (cx + s * 3 / 4, cy + s / 4)];
                f(RoundPart::Path { pts, n: 3, t: 2, color: fill });
            }
        }
    } else {
        f(RoundPart::Oval { x: cx, y: cy, w: s, h: s, t: 1, color: outline });
        if checked {
            let mut d = s * 2 / 3;
            if (s - d) % 2 != 0 {
                d -= 1;
            }
            let o = ((s - d) / 2) as i16;
            f(RoundPart::Oval { x: cx + o, y: cy + o, w: d, h: d, t: 0, color: fill });
        }
    }
}

/// The parts a circle / ellipse shape or a gauge (its dial, over the box `draw_widget` paints
/// first) draws on a readback backend, in drawing order, for a widget at `abs`. False for
/// other kinds. Mirrors `draw_circle_shape`/`draw_ellipse_shape` (smooth) and `draw_gauge`
/// (test `round_parts_draw_what_the_widget_draws`).
pub fn round_parts(widget: &Widget, abs: &Rect, ext: &WidgetExt, mut f: impl FnMut(RoundPart)) -> bool {
    match widget.kind {
        KIND_CIRCLE | KIND_ELLIPSE => {
            let bx = if widget.kind == KIND_CIRCLE {
                let d = abs.w.min(abs.h);
                Rect::new(abs.x + ((abs.w - d) / 2) as i16, abs.y + ((abs.h - d) / 2) as i16, d, d)
            } else {
                *abs
            };
            let stroke = widget.border_color != 0;
            let t = shape_stroke(ext);
            if widget.background_color != 0 {
                let inset = if stroke { t / 2 } else { 0 };
                f(RoundPart::Oval {
                    x: bx.x + inset as i16,
                    y: bx.y + inset as i16,
                    w: bx.w.saturating_sub(inset * 2),
                    h: bx.h.saturating_sub(inset * 2),
                    t: 0,
                    color: widget.background_color,
                });
            }
            if stroke {
                f(RoundPart::Oval { x: bx.x, y: bx.y, w: bx.w, h: bx.h, t, color: widget.border_color });
            }
            true
        }
        KIND_GAUGE => {
            let Some(g) = GaugeGeom::new(&inner_rect(abs, &ext.border)) else { return true };
            let (cx, cy) = (g.cx, g.cy);
            let val = ext.value.clamp(0, 100) as i32;
            let value_angle = GaugeGeom::angle(val);
            let track = if widget.border_color != 0 { widget.border_color } else { 0x3186 };
            let arc = |start, end, color| RoundPart::Arc { cx, cy, r: g.outer_r, ir: g.inner_r, start, end, color };
            f(arc(GAUGE_ARC_START, GAUGE_ARC_END, track));
            if val > 0 && ext.press_color != 0 {
                let end = if val >= 100 { GAUGE_ARC_END } else { (value_angle % 360) as i16 };
                f(arc(GAUGE_ARC_START, end, ext.press_color));
            }
            for i in 0..=4 {
                let angle = GAUGE_ARC_START as i32 + i * GAUGE_ARC_SWEEP / 4;
                let (ax, ay) = g.point(angle, g.tick_r0);
                let (bx, by) = g.point(angle, g.tick_r1);
                f(RoundPart::Wedge { ax, ay, bx, by, ra_q8: 256, rb_q8: 256, color: track });
            }
            let needle = if ext.text_color != 0 && ext.text_color != 0xFFFF { ext.text_color } else { 0xFFFF };
            if g.needle_r > 4 {
                let (bx, by) = g.point(value_angle, g.needle_r);
                f(RoundPart::Wedge {
                    ax: cx,
                    ay: cy,
                    bx,
                    by,
                    ra_q8: GAUGE_NEEDLE_BASE_Q8,
                    rb_q8: GAUGE_NEEDLE_TIP_Q8,
                    color: needle,
                });
            } else {
                // `fill_circle(cx, cy, 3)`: the oval inscribed in the 7×7 box around it.
                f(RoundPart::Oval { x: cx - 3, y: cy - 3, w: 7, h: 7, t: 0, color: needle });
            }
            true
        }
        KIND_PROGRESS | KIND_SLIDER => {
            value_fill_parts(widget, &inner_rect(abs, &ext.border), ext, &mut f);
            true
        }
        KIND_CHECKBOX | KIND_RADIO => {
            check_parts(widget, &inner_rect(abs, &ext.border), ext, &mut f);
            true
        }
        KIND_LINE => {
            let color = if widget.border_color != 0 { widget.border_color } else { widget.background_color };
            if color != 0 && abs.w != 0 && abs.h != 0 {
                let hw = shape_stroke(ext).max(1) as i32 * 128;
                let (bx, by) = (abs.right() - 1, abs.bottom() - 1);
                f(RoundPart::Wedge { ax: abs.x, ay: abs.y, bx, by, ra_q8: hw, rb_q8: hw, color });
            }
            true
        }
        _ => false,
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
/// What a polygon shape at `abs` draws: its points in screen coordinates (in `out`), how
/// many, its fill colour (0 = none; only with 3 points or more) and its outline's colour (0 =
/// none) and thickness. None when it has under 2 points. Shared by `draw_polygon_shape` and
/// renderers that draw polygons another way.
pub fn polygon_shape(
    objs: &ObjPool,
    widget: &Widget,
    abs: &Rect,
    ext: &WidgetExt,
    out: &mut [Offset; lcd::MAX_POLY_POINTS],
) -> Option<(usize, Color, Color, u16)> {
    if ext.text_id == crate::pool::PTR_NONE {
        return None;
    }
    let n = parse_points(objs.str_get(ext.text_id), out);
    if n < 2 {
        return None;
    }
    // Points are relative to the widget's top-left (border-box origin).
    for p in out[..n].iter_mut() {
        p.x += abs.x;
        p.y += abs.y;
    }
    let fill = if n >= 3 { widget.background_color } else { 0 };
    Some((n, fill, widget.border_color, shape_stroke(ext)))
}

fn draw_polygon_shape<P: Platform>(ctx: &Ctx<P>, widget: &Widget, abs: &Rect, ext: &WidgetExt) {
    let mut pts = [Offset { x: 0, y: 0 }; lcd::MAX_POLY_POINTS];
    let Some((n, fill, stroke, t)) = polygon_shape(&ctx.objs, widget, abs, ext, &mut pts) else { return };
    let poly = &pts[..n];
    if fill != 0 {
        ctx.lcd.fill_polygon(poly, fill);
    }
    if stroke != 0 {
        if fill != 0 && !ctx.lcd.has_alpha() {
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
        img.draw(&ctx.lcd, &ctx.flash, inner.x, inner.y);
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
        // May be negative: a widget partly off the top/left edge (a page
        // sliding out) shows the part of its image that is on screen.
        img.draw(&ctx.lcd, &ctx.flash, dx, dy);
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
    label_lines(ctx, widget, abs, ext, |font, line, x, y| {
        font.draw_str(&ctx.lcd, &ctx.flash, line, x, y, ext.text_color, bg);
    });
}

/// A label's text as its drawing lays it out: `f(font, line, pen x, baseline y)` for each
/// line drawn (one unless multi-line). The glyphs are drawn clipped to the label's box.
/// Public for board renderers that draw text their own way (exynos4412's GPU compositor).
pub fn label_lines<P: Platform>(
    ctx: &Ctx<P>,
    widget: &Widget,
    abs: &Rect,
    ext: &WidgetExt,
    mut f: impl FnMut(&crate::font::Font, &[u8], i16, i16),
) {
    if ext.text_id == crate::pool::PTR_NONE || ext.font_id == 0xFF {
        return;
    }
    let text = ctx.objs.str_get(ext.text_id);
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

    // Multi-line mode: newlines and word wrap, block centred vertically.
    if widget.flags & FLAG_MULTI_LINE != 0 {
        let bytes = text;
        let mut line_count: i16 = 0;
        let mut i = 0;
        while i <= bytes.len() {
            let (_, next) = font.wrap_line(bytes, i, cw);
            line_count += 1;
            i = next;
        }
        let block_h = line_count.saturating_mul(lh);
        // Text taller than the box starts at its top (the first lines show).
        let base_y = cy + ((ch as i16 - block_h) / 2).max(0);

        let mut li: i16 = 0;
        let mut i = 0;
        while i <= bytes.len() {
            let (end, next) = font.wrap_line(bytes, i, cw);
            let line = &bytes[i..end];
            let ly = base_y + li * lh;
            // Only lines that fit completely.
            if !line.is_empty() && ly >= cy && ly + lh <= cy + ch as i16 {
                let lw = font.text_width(line) as i16;
                let lx = match ext.text_align {
                    ALIGN_CENTER => cx + (cw as i16 - lw) / 2,
                    ALIGN_RIGHT => cx + cw as i16 - lw,
                    _ => cx,
                };
                f(font, line, lx, ly + (lh * 3) / 4);
            }
            li += 1;
            i = next;
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
        f(font, text, tx, ty);
    }
}

/// Multi-line input: the first line to show (word-wrapped to `cw`), so the
/// line holding the byte offset `cursor` stays in view in `ch` pixels.
/// Shared with the touch-to-cursor code in `runtime`.
pub(crate) fn input_first_line(font: &crate::font::Font, text: &[u8], cw: u16, ch: u16, cursor: usize) -> usize {
    let lh = (font.line_height() as usize).max(1);
    let rows = (ch as usize / lh).max(1);
    let mut li: usize = 0;
    let mut cur_line = None;
    let mut i = 0;
    while i <= text.len() {
        let (end, next) = font.wrap_line(text, i, cw);
        if cur_line.is_none() && cursor >= i && cursor <= end {
            cur_line = Some(li);
        }
        li += 1;
        i = next;
    }
    cur_line.unwrap_or(li.saturating_sub(1)).saturating_sub(rows - 1)
}

/// What an input or dropdown draws over its box, in drawing order ([`field_contents`]).
pub enum FieldItem<'a> {
    /// `font.draw_str(line, x, y, text_color, bg)`: with `bg`, each glyph's cell is painted
    /// opaquely (the coverage mixed with `bg`); without, the glyphs blend over what is there.
    Text { font: &'a crate::font::Font, line: &'a [u8], x: i16, y: i16, bg: Option<Color> },
    /// The cursor bar (`Rect`) or the dropdown's chevron (`Path`, 1 px).
    Part(RoundPart),
}

/// Draws one [`RoundPart`] as its drawing issues it (any backend).
fn draw_round_part<B: LcdBackend>(lcd: &LcdImpl<B>, p: RoundPart) {
    match p {
        RoundPart::Oval { x, y, w, h, t, color } => lcd.draw_oval(x, y, w, h, t, color),
        RoundPart::Arc { cx, cy, r, ir, start, end, color } => lcd.fill_arc(cx, cy, r, ir, start, end, color),
        RoundPart::Wedge { ax, ay, bx, by, ra_q8, rb_q8, color } => lcd.draw_wedge(ax, ay, bx, by, ra_q8, rb_q8, color),
        RoundPart::Rect { x, y, w, h, color } => fill_rect_screen(lcd, Rect::new(x, y, w, h), color),
        RoundPart::RoundRect { x, y, w, h, r, t: 0, color } => lcd.fill_rounded_rect_at(x, y, w, h, r, color, 255),
        RoundPart::RoundRect { x, y, w, h, r, t, color } => lcd.stroke_rounded_rect(x, y, w, h, r, t, color),
        RoundPart::Path { pts, n, t, color } => {
            let pts = pts.map(|(x, y)| Offset { x, y });
            lcd.draw_polyline(&pts[..n as usize], color, false, t);
        }
    }
}

/// Draw input text with cursor, or a dropdown's text and arrow, in the content area.
fn draw_field<P: Platform>(ctx: &Ctx<P>, widget: &Widget, abs: &Rect, ext: &WidgetExt) {
    field_contents(ctx, widget, abs, ext, |item| match item {
        FieldItem::Text { font, line, x, y, bg } => {
            font.draw_str(&ctx.lcd, &ctx.flash, line, x, y, ext.text_color, bg);
        }
        FieldItem::Part(p) => draw_round_part(&ctx.lcd, p),
    });
}

/// What an input (`KIND_INPUT`: its text, single or multi-line, and the cursor while focused
/// and blinking on) or a dropdown (its text and disclosure arrow) draws over its box, in
/// order. The drawing confines it to the box (scissor). Shared by `draw_widget` and renderers
/// that draw it another way (exynos4412's GPU compositor).
pub fn field_contents<'a, P: Platform>(
    ctx: &'a Ctx<P>,
    widget: &Widget,
    abs: &Rect,
    ext: &WidgetExt,
    mut f: impl FnMut(FieldItem<'a>),
) {
    let Some(font) = ctx.fonts.resolve(ext.font_id) else { return };
    let b = ext.border;
    let p = ext.padding;
    let cx = abs.x + b.left as i16 + p.left as i16;
    let cy = abs.y + b.top as i16 + p.top as i16;
    let cw = abs.w.saturating_sub(b.left as u16 + b.right as u16 + p.left as u16 + p.right as u16);
    let ch = abs.h.saturating_sub(b.top as u16 + b.bottom as u16 + p.top as u16 + p.bottom as u16);
    if cw == 0 || ch == 0 {
        return;
    }
    let lh = font.line_height() as i16;
    let ty = cy + (ch as i16 - lh) / 2 + (lh * 3) / 4;
    let bg_color = effective_bg(&ctx.objs, widget, ext);
    let bg = if bg_color == 0 { None } else { Some(bg_color) };
    let has_text = ext.text_id != crate::pool::PTR_NONE && ext.font_id != 0xFF;
    let text: &'a [u8] = if has_text { ctx.objs.str_get(ext.text_id) } else { &[] };

    if widget.kind == KIND_DROPDOWN {
        let arrow_w: u16 = 18;
        let text_w = cw.saturating_sub(arrow_w + 4);
        // The selected text whatever `font_id` says (a missing font was handled above).
        let text: &'a [u8] = if ext.text_id != crate::pool::PTR_NONE { ctx.objs.str_get(ext.text_id) } else { &[] };
        if text_w > 0 && !text.is_empty() {
            f(FieldItem::Text { font, line: text, x: cx, y: ty, bg });
        }
        if cw >= arrow_w && ch >= 8 {
            let ax = cx + cw as i16 - arrow_w as i16 + 4;
            let mid_y = cy + ch as i16 / 2;
            let color = if ext.text_color != 0 { ext.text_color } else { 0xFFFF };
            let d = if widget.flags & FLAG_CHECKED != 0 { -3 } else { 3 };
            let pts = [(ax, mid_y - d), (ax + 5, mid_y + d), (ax + 10, mid_y - d)];
            f(FieldItem::Part(RoundPart::Path { pts, n: 3, t: 1, color }));
        }
        return;
    }

    let focused = widget.flags & FLAG_FOCUSED != 0 && ctx.cursor_visible;
    if widget.flags & FLAG_MULTI_LINE != 0 {
        // Word-wrapped lines from the top of the content box, scrolled to keep the cursor
        // line visible, and the cursor.
        let cursor = (ext.value.max(0) as usize).min(text.len());
        let first = input_first_line(font, text, cw, ch, cursor);
        let rows = (ch as i16 / lh.max(1)).max(1);
        let mut li = 0usize;
        let mut i = 0;
        let mut cursor_done = false;
        while i <= text.len() {
            let (end, next) = font.wrap_line(text, i, cw);
            if li >= first {
                let row = (li - first) as i16;
                if row >= rows {
                    break;
                }
                let ly = cy + row * lh;
                let line = &text[i..end];
                if !line.is_empty() {
                    f(FieldItem::Text { font, line, x: cx, y: ly + (lh * 3) / 4, bg });
                }
                if focused && !cursor_done && cursor >= i && cursor <= end {
                    cursor_done = true;
                    let x = cx + font.text_width(&text[i..cursor]) as i16;
                    if x < cx + cw as i16 {
                        let color = ext.text_color;
                        f(FieldItem::Part(RoundPart::Rect { x, y: ly, w: 2, h: lh as u16, color }));
                    }
                }
            }
            li += 1;
            i = next;
        }
        return;
    }

    if !text.is_empty() {
        f(FieldItem::Text { font, line: text, x: cx, y: ty, bg });
    }
    if focused {
        let end = (ext.value.max(0) as usize).min(text.len());
        let mut cursor_x = cx;
        let mut byte_pos = 0;
        while byte_pos < end {
            match crate::font::utf8_next(text, &mut byte_pos) {
                Some(cp) if cp != 0xFFFF => cursor_x += font.char_width_cp(cp) as i16,
                Some(_) => {}
                None => break,
            }
        }
        // A 2 px wide bar.
        if cursor_x >= cx && cursor_x < cx + cw as i16 {
            let y = cy + (ch as i16 - lh) / 2;
            f(FieldItem::Part(RoundPart::Rect { x: cursor_x, y, w: 2, h: lh as u16, color: ext.text_color }));
        }
    }
}
/// Gauge layout inside the widget's inner rect: a 270° arc from 135°
/// (bottom-left) to 45° (bottom-right), clockwise through the top, tick marks
/// in a band just outside it, the needle inside it. Everything stays inside
/// the rect, anti-aliased fringes included.
#[derive(Clone, Copy)]
struct GaugeGeom {
    cx: i16,
    cy: i16,
    outer_r: i16,
    inner_r: i16,
    needle_r: i16,
    tick_r0: i16,
    tick_r1: i16,
}

const GAUGE_ARC_START: i16 = 135;
const GAUGE_ARC_END: i16 = 45;
const GAUGE_ARC_SWEEP: i32 = 270;
/// Needle half-widths (Q8 px) at the hub and at the tip.
const GAUGE_NEEDLE_BASE_Q8: i32 = 3 * 256 + 128;
const GAUGE_NEEDLE_TIP_Q8: i32 = 256;

impl GaugeGeom {
    fn new(inner: &Rect) -> Option<Self> {
        if inner.is_empty() {
            return None;
        }
        let r = (inner.w.min(inner.h) / 2) as i16 - 1;
        // 2 px wide ticks with round caps: keep the cap off the edge.
        let tick_r1 = r - 2;
        let tick_r0 = tick_r1 - 4;
        let outer_r = tick_r0 - 2;
        if outer_r < 8 {
            return None;
        }
        // Arc thickness: ~8% of radius, min 3px
        let inner_r = outer_r - (outer_r / 12).max(3);
        Some(Self {
            cx: inner.x + inner.w as i16 / 2,
            cy: inner.y + inner.h as i16 / 2,
            outer_r,
            inner_r,
            // A gap between the needle tip and the arc.
            needle_r: inner_r - 6,
            tick_r0,
            tick_r1,
        })
    }

    /// Angle (unwrapped, 135..=405) of a 0..100 value.
    fn angle(value: i32) -> i32 {
        GAUGE_ARC_START as i32 + value.clamp(0, 100) * GAUGE_ARC_SWEEP / 100
    }

    fn point(&self, angle: i32, radius: i16) -> (i16, i16) {
        let (sin_v, cos_v) = lcd::sin_cos_deg((angle % 360) as i16);
        (
            self.cx + ((radius as i32 * cos_v as i32) >> 8) as i16,
            self.cy + ((radius as i32 * sin_v as i32) >> 8) as i16,
        )
    }

    /// Box around the needle (and hub) at `value`.
    fn needle_box(&self, value: i32) -> Rect {
        let (nx, ny) = self.point(Self::angle(value), self.needle_r);
        // Hub/base radius + anti-aliasing (and the 1 px offset lines of the
        // aliased needle).
        const M: i16 = 6;
        let (x0, x1) = (self.cx.min(nx) - M, self.cx.max(nx) + M);
        let (y0, y1) = (self.cy.min(ny) - M, self.cy.max(ny) + M);
        Rect::new(x0, y0, (x1 - x0 + 1) as u16, (y1 - y0 + 1) as u16)
    }

    /// Box around the arc between two values (either order), wide enough for
    /// the value end's anti-aliased edge.
    fn arc_box(&self, a: i32, b: i32) -> Rect {
        let lo = Self::angle(a.min(b)) - 3;
        let hi = Self::angle(a.max(b)) + 3;
        let (mut x0, mut y0, mut x1, mut y1) = (i16::MAX, i16::MAX, i16::MIN, i16::MIN);
        let mut ang = lo;
        loop {
            for r in [self.inner_r - 1, self.outer_r + 1] {
                let (x, y) = self.point(ang, r);
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
            if ang == hi {
                break;
            }
            // Sampling every 10° leaves under 1 px of sagitta at these radii.
            ang = (ang + 10).min(hi);
        }
        const M: i16 = 2;
        Rect::new(x0 - M, y0 - M, (x1 - x0 + 2 * M + 1) as u16, (y1 - y0 + 2 * M + 1) as u16)
    }

    /// Non-overlapping boxes holding every pixel that differs between the
    /// drawings at `old` and `new`: both needles (with the hub) and the arc
    /// in between. Returns the boxes and how many are used.
    fn change_boxes(&self, old: i32, new: i32) -> ([Rect; 2], usize) {
        // Both needle boxes contain the hub, so one box around both wastes little.
        let (a, b) = (self.needle_box(old), self.needle_box(new));
        let (x0, y0) = (a.x.min(b.x), a.y.min(b.y));
        let needles = Rect::new(
            x0,
            y0,
            (a.right().max(b.right()) - x0) as u16,
            (a.bottom().max(b.bottom()) - y0) as u16,
        );
        let arc = self.arc_box(old, new);
        if !needles.intersects(&arc) {
            return ([needles, arc], 2);
        }
        // Overlapping boxes would draw (and on readback backends blend) twice.
        let (x0, y0) = (needles.x.min(arc.x), needles.y.min(arc.y));
        let both = Rect::new(
            x0,
            y0,
            (needles.right().max(arc.right()) - x0) as u16,
            (needles.bottom().max(arc.bottom()) - y0) as u16,
        );
        ([both, both], 1)
    }
}

/// Draw gauge arc/dial inside the inner rect (see `GaugeGeom`).
/// - border_color: track arc and tick color
/// - press_color: filled arc color (value portion)
/// - text_color: needle color
/// - value: 0-100 gauge position
fn draw_gauge<B: LcdBackend>(lcd: &LcdImpl<B>, widget: &Widget, g: &GaugeGeom, ext: &WidgetExt) {
    let (cx, cy) = (g.cx, g.cy);
    let val = ext.value.clamp(0, 100) as i32;
    let value_angle = GaugeGeom::angle(val);

    // Draw track arc (full 270°)
    let track_color = if widget.border_color != 0 {
        widget.border_color
    } else {
        0x3186 // default dark gray
    };
    lcd.fill_arc(cx, cy, g.outer_r, g.inner_r, GAUGE_ARC_START, GAUGE_ARC_END, track_color);

    // Draw value arc (filled portion)
    if val > 0 && ext.press_color != 0 {
        let end = if val >= 100 {
            GAUGE_ARC_END
        } else {
            (value_angle % 360) as i16
        };
        // Without readback the value arc's leading end mixes with the track.
        let base = lcd.underlay();
        lcd.update_underlay(|u| Underlay { sweep: track_color, ..u });
        lcd.fill_arc(cx, cy, g.outer_r, g.inner_r, GAUGE_ARC_START, end, ext.press_color);
        lcd.set_underlay(base);
    }

    // Tick marks at 0%, 25%, 50%, 75%, 100%, outside the arc
    for i in 0..=4 {
        let angle = GAUGE_ARC_START as i32 + i * GAUGE_ARC_SWEEP / 4;
        let (tx0, ty0) = g.point(angle, g.tick_r0);
        let (tx1, ty1) = g.point(angle, g.tick_r1);
        lcd.draw_line_thick(tx0, ty0, tx1, ty1, track_color, 2);
    }

    // Draw needle
    let needle_color = if ext.text_color != 0 && ext.text_color != 0xFFFF {
        ext.text_color
    } else {
        0xFFFF // white default
    };
    if g.needle_r > 4 {
        let (nx, ny) = g.point(value_angle, g.needle_r);
        if lcd.smooth() {
            // One anti-aliased tapered stroke whose round base is the hub
            // (separate strokes would double-blend or, without readback,
            // overwrite each other's soft edges).
            lcd.draw_wedge(cx, cy, nx, ny, GAUGE_NEEDLE_BASE_Q8, GAUGE_NEEDLE_TIP_Q8, needle_color);
        } else {
            lcd.draw_line(cx, cy, nx, ny, needle_color);
            // Draw thicker needle (2 extra lines offset by 1px)
            lcd.draw_line(cx + 1, cy, nx + 1, ny, needle_color);
            lcd.draw_line(cx, cy + 1, nx, ny + 1, needle_color);
        }
    }

    // Center dot (part of the wedge when smooth)
    if !lcd.smooth() || g.needle_r <= 4 {
        lcd.fill_circle(cx, cy, 3, needle_color);
    }
}

/// Key of everything that shapes a gauge's pixels except its value, for
/// `ObjPool::drawn_value`. Never 0.
fn gauge_key<P: Platform>(objs: &ObjPool, id: WidgetId, abs: &Rect) -> u32 {
    let w = objs.get(id);
    let e = objs.ext(id);
    let pair = |a: u16, b: u16| a as u32 | (b as u32) << 16;
    let edges = |x: Edges| u32::from_le_bytes([x.top, x.right, x.bottom, x.left]);
    let words = [
        pair(abs.x as u16, abs.y as u16),
        pair(abs.w, abs.h),
        pair(w.kind as u16, w.flags & !(FLAG_DIRTY_AB | FLAG_RENDERED | FLAG_PAINTED)),
        pair(w.background_color, w.border_color),
        pair(effective_bg(objs, w, &e), backdrop(objs, w, P::DEFAULT_BG_COLOR)),
        pair(e.text_color, e.press_color),
        pair(e.gradient_color, e.border_radius),
        u32::from_le_bytes([e.gradient_dir, e.alpha, e.image_id, 0]),
        edges(e.border),
        edges(e.padding),
    ];
    // FNV-1a over the words.
    let mut h: u32 = 0x811C_9DC5;
    for v in words {
        for b in v.to_le_bytes() {
            h = (h ^ b as u32).wrapping_mul(0x0100_0193);
        }
    }
    h | 1
}

/// The part of gauge `id` a value change touches, if its pixels still show
/// its last full drawing (else None: redraw it whole). No rects when the
/// value is unchanged.
fn gauge_changed_rects<P: Platform>(objs: &ObjPool, id: WidgetId, abs: &Rect) -> Option<([Rect; 2], usize)> {
    let w = objs.get(id);
    if w.flags & FLAG_RENDERED == 0 || !P::Renderer::partial_redraw(objs, id) {
        return None;
    }
    let old = objs.drawn_value(id, gauge_key::<P>(objs, id, abs))?.clamp(0, 100) as i32;
    let new = w.value.clamp(0, 100) as i32;
    let none = [Rect::new(0, 0, 0, 0); 2];
    if old == new {
        return Some((none, 0));
    }
    let g = GaugeGeom::new(&inner_rect(abs, &objs.border(id)))?;
    Some(g.change_boxes(old, new))
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
    objs: &ObjPool,
    _widget: &Widget,
    inner: &Rect,
    ext: &WidgetExt,
) {
    let style = graph_style(inner, ext);
    // One path for the whole curve: anti-aliased segments share pixels
    // instead of drawing them twice.
    let mut pen = lcd.pen(style.line, 1);
    let mut prev: Option<(i16, i16)> = None;
    graph_points(objs, inner, ext, |x, y| {
        if let (Some((px, py)), Some(fill)) = (prev, style.fill) {
            fill_under_segment(lcd, px, py, x, y, style.baseline, fill);
        }
        pen.to(x, y);
        prev = Some((x, y));
    });
}

/// How a graph draws its curve: line colour, the fill under it (if any), down to `baseline`
/// (the inner rect's last row).
pub struct GraphStyle {
    pub line: Color,
    pub fill: Option<Color>,
    pub baseline: i16,
}

/// The style of graph `ext` in `inner` (see [`graph_points`]).
pub fn graph_style(inner: &Rect, ext: &WidgetExt) -> GraphStyle {
    let line = if ext.text_color != 0 && ext.text_color != 0xFFFF { ext.text_color } else { 0xFFFF };
    let fill = (ext.image_id & GRAPH_FLAG_FILL != 0 && ext.gradient_color != 0).then_some(ext.gradient_color);
    GraphStyle { line, fill, baseline: inner.y + inner.h as i16 - 1 }
}

/// The points of graph `ext`'s curve in `inner`, in order: `f(x, y)` for each, as the
/// drawing strokes them (a 1 px anti-aliased line through them, and with a fill each segment
/// `(x0, y0) → (x1, y1)` filled as `fill_under_segment` does). Nothing if there are fewer than
/// two samples or the rect is too small. Public for board renderers that draw graphs their
/// own way (exynos4412's GPU compositor).
pub fn graph_points(objs: &ObjPool, inner: &Rect, ext: &WidgetExt, mut f: impl FnMut(i16, i16)) {
    if inner.is_empty() || inner.w < 2 || inner.h < 2 {
        return;
    }

    let arr_id = ext.value as crate::pool::ObjPtr;
    let samples = match objs.arr(arr_id) {
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
        y_min = y_min.saturating_sub(1);
        y_span = 2;
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

    let linear = ext.image_id & GRAPH_FLAG_LINEAR != 0;
    f(plot_x(0) as i16, plot_y(samples[0]) as i16);

    if linear {
        for i in 1..count {
            f(plot_x(i) as i16, plot_y(samples[i]) as i16);
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
            f((x1 + s) as i16, cy);
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

/// What scroll container `id`'s scrollbar draws, in screen coordinates: the track, then the
/// thumb over it (each a solid rect in its colour). None when all content fits. Shared by the
/// passes ([`draw_scrollbar`]) and board compositors.
pub fn scrollbar_parts(objs: &ObjPool, id: WidgetId) -> Option<[(Rect, Color); 2]> {
    let content_h = objs.content_height(id);
    let cr = objs.content_rect(id);
    if cr.h == 0 || content_h <= cr.h {
        return None; // no scrollbar needed — all content fits
    }

    let scroll_y = objs.value(id).max(0) as u32;
    let ch = content_h as u32;
    let vh = cr.h as u32;

    // Track: right edge of content area
    let track_x = cr.x + cr.w as i16 - SCROLLBAR_W as i16;
    let track_y = cr.y;
    let track_h = cr.h;

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
    Some([
        (Rect::new(track_x, track_y, SCROLLBAR_W, track_h), SCROLLBAR_TRACK),
        (Rect::new(track_x, thumb_y, SCROLLBAR_W, thumb_h), SCROLLBAR_THUMB),
    ])
}

/// Draw scrollbar on the right side of a scroll container.
fn draw_scrollbar<P: Platform>(ctx: &Ctx<P>, id: WidgetId, _abs: &Rect) {
    if let Some(parts) = scrollbar_parts(&ctx.objs, id) {
        for (rect, color) in parts {
            fill_rect_screen(&ctx.lcd, rect, color);
        }
    }
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

// The rounded helpers below draw the whole `rect` on readback backends,
// partly off the screen or not, and let the scissor / screen cut it: the
// corners stay where they are. Write-only backends cut the rect to the screen
// first and round the cut rect (a corner appears at the screen edge), as
// they always did. A widget sliding onto the screen is drawn partly off it,
// and the renderer may later move those pixels on screen (`shift_moved`).

/// Fill gradient rounded rect, clipped to the screen.
fn fill_gradient_rounded_rect_screen<B: LcdBackend>(
    lcd: &LcdImpl<B>,
    rect: Rect,
    radius: u16,
    c1: Color,
    c2: Color,
    dir: u8,
) {
    lcd.fill_gradient_rounded_rect_at(rect.x, rect.y, rect.w, rect.h, radius, c1, c2, dir);
}

/// Draw rounded rect outline, clipped to the screen.
fn rounded_rect_screen<B: LcdBackend>(lcd: &LcdImpl<B>, rect: Rect, radius: u16, color: Color) {
    if B::HAS_ALPHA && radius > 0 {
        // The same anti-aliased 1 px ring as `draw_rounded_rect`.
        lcd.stroke_rounded_rect(rect.x, rect.y, rect.w, rect.h, radius, 1, color);
    } else if let Some(r) = rect.intersection(&screen_rect(lcd)) {
        lcd.draw_rounded_rect(r.x as u16, r.y as u16, r.w, r.h, radius, color);
    }
}

/// Fill rounded rect, clipped to the screen.
fn fill_rounded_rect_screen<B: LcdBackend>(lcd: &LcdImpl<B>, rect: Rect, radius: u16, color: Color) {
    lcd.fill_rounded_rect_at(rect.x, rect.y, rect.w, rect.h, radius, color, 255);
}

/// Blend rounded rect, clipped to the screen.
fn fill_rounded_rect_blend_screen<B: LcdBackend>(
    lcd: &LcdImpl<B>,
    rect: Rect,
    radius: u16,
    color: Color,
    alpha: u8,
) {
    lcd.fill_rounded_rect_at(rect.x, rect.y, rect.w, rect.h, radius, color, alpha);
}

/// Erase the area of a widget that is becoming invisible by repainting the
/// nearest ancestor background.  Handles both solid and gradient ancestors so
/// a gradient parent is not corrupted by a solid-color erase rectangle.
fn erase_widget_area<B: LcdBackend>(tree: &ObjPool, lcd: &LcdImpl<B>, id: WidgetId, abs: Rect) {
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
pub fn inner_rect(abs: &Rect, border: &Edges) -> Rect {
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
    ctx.objs.ensure_dfs();
    for i in 0..ctx.objs.dfs_len() {
        let id = ctx.objs.dfs_at(i);
        ctx.objs.get_mut(id).clear_dirty();
    }
    ctx.objs.clear_changed();
}

#[cfg(all(test, feature = "mock"))]
mod tests {
    use super::*;
    use crate::widget::KIND_BASE;

    /// Pressed, background-less rounded button ("Ghost") with a full-size
    /// transparent label child.
    fn ghost(tree: &mut ObjPool) -> (WidgetId, WidgetId) {
        let button = tree.alloc_widget().unwrap();
        tree.get_mut(button).kind = KIND_BASE;
        tree.get_mut(button).flags |= FLAG_PRESSED;
        tree.update_ext(button, |e| e.press_color = 0x2C9F);
        let label = tree.alloc_widget().unwrap();
        tree.get_mut(label).kind = KIND_LABEL;
        tree.add_child(button, label);
        (button, label)
    }

    /// 128×128 frame buffer; `ALPHA` = readback backend (blends), else
    /// write-only with underlays (Nextion).
    struct Fb<const ALPHA: bool> {
        px: core::cell::RefCell<alloc::vec::Vec<u16>>,
    }
    const FB: usize = 128;
    impl<const ALPHA: bool> LcdBackend for Fb<ALPHA> {
        const WIDTH: u16 = FB as u16;
        const HEIGHT: u16 = FB as u16;
        const HAS_ALPHA: bool = ALPHA;
        const AA_UNDERLAY: bool = !ALPHA;
        fn begin_frame(&mut self) {}
        fn end_frame(&mut self) {}
        fn back_buf(&self) -> u8 {
            0
        }
        fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, c: u16) {
            self.blend_rect(x, y, w, h, c, 255);
        }
        fn blend_rect(&self, x: u16, y: u16, w: u16, h: u16, c: u16, a: u8) {
            let mut px = self.px.borrow_mut();
            for yy in y as usize..(y as usize + h as usize).min(FB) {
                for xx in x as usize..(x as usize + w as usize).min(FB) {
                    let p = &mut px[yy * FB + xx];
                    *p = lcd::lerp_rgb565(*p, c, a as u16, 255);
                }
            }
        }
        fn blend_pixel(&self, x: u16, y: u16, c: u16, a: u8) {
            self.blend_rect(x, y, 1, 1, c, a);
        }
        fn begin_pixels(&self, _: u16, _: u16, _: u16, _: u16) {}
        fn write_pixel(&self, _: u16) {}
        fn send_command(&self, _: u16) {}
        fn send_data(&self, _: u16) {}
    }

    const GAUGE_BG: u16 = 0x18E3;
    const GAUGE_BOX: Rect = Rect::new(4, 4, 120, 120);

    /// The gauge part of `draw_widget` (background + `draw_gauge`), clipped
    /// to `rects`.
    fn paint_gauge<const ALPHA: bool>(lcd: &LcdImpl<Fb<ALPHA>>, value: i16, rects: &[Rect]) {
        let mut w = Widget::DEAD;
        w.kind = KIND_GAUGE;
        w.border_color = 0x3186;
        let mut e = WidgetExt::DEFAULT;
        e.value = value as crate::widget::WValue;
        e.press_color = 0x05FF;
        e.text_color = 0xF800;
        let g = GaugeGeom::new(&GAUGE_BOX).unwrap();
        if lcd.wants_underlay() {
            lcd.set_underlay(Some(Underlay::solid(GAUGE_BG)));
        }
        for r in rects {
            lcd.set_scissor(Some(*r));
            lcd.fill_rect(GAUGE_BOX.x as u16, GAUGE_BOX.y as u16, GAUGE_BOX.w, GAUGE_BOX.h, GAUGE_BG);
            draw_gauge(lcd, &w, &g, &e);
        }
        lcd.set_scissor(None);
        lcd.set_underlay(None);
    }

    fn fresh<const ALPHA: bool>() -> LcdImpl<Fb<ALPHA>> {
        LcdImpl::with_backend(Fb { px: core::cell::RefCell::new(alloc::vec![0; FB * FB]) })
    }

    /// Redrawing only `change_boxes` turns the drawing at `old` into exactly
    /// the drawing at `new`.
    fn gauge_update_matches_full_draw<const ALPHA: bool>() {
        let g = GaugeGeom::new(&GAUGE_BOX).unwrap();
        let values = [0, 1, 7, 25, 33, 50, 51, 66, 75, 99, 100];
        for &old in &values {
            for &new in &values {
                let inc = fresh::<ALPHA>();
                paint_gauge(&inc, old, &[GAUGE_BOX]);
                let mut clip = ClipRegion::from_rect(GAUGE_BOX);
                let (boxes, n) = g.change_boxes(old as i32, new as i32);
                clip.restrict(&boxes[..n]);
                paint_gauge(&inc, new, clip.iter());

                let full = fresh::<ALPHA>();
                paint_gauge(&full, new, &[GAUGE_BOX]);
                let (a, b) = (inc.backend().px.borrow(), full.backend().px.borrow());
                for i in 0..FB * FB {
                    assert!(a[i] == b[i], "alpha={ALPHA} {old}->{new}: pixel ({}, {})", i % FB, i / FB);
                }
            }
        }
    }

    /// Draws `parts` with the primitives they name.
    fn draw_parts(lcd: &LcdImpl<Fb<true>>, parts: &[RoundPart]) {
        for p in parts {
            draw_round_part(lcd, *p);
        }
    }

    #[test]
    fn round_parts_draw_what_the_widget_draws() {
        let same = |a: &LcdImpl<Fb<true>>, b: &LcdImpl<Fb<true>>, what: &str| {
            let (a, b) = (a.backend().px.borrow(), b.backend().px.borrow());
            for i in 0..FB * FB {
                assert!(a[i] == b[i], "{what}: pixel ({}, {})", i % FB, i / FB);
            }
        };
        for value in [0i16, 1, 33, 50, 99, 100] {
            let cpu = fresh::<true>();
            paint_gauge(&cpu, value, &[GAUGE_BOX]);
            let mut w = Widget::DEAD;
            w.kind = KIND_GAUGE;
            w.border_color = 0x3186;
            let mut e = WidgetExt::DEFAULT;
            e.value = value as crate::widget::WValue;
            e.press_color = 0x05FF;
            e.text_color = 0xF800;
            let mut parts = alloc::vec::Vec::new();
            assert!(round_parts(&w, &GAUGE_BOX, &e, |p| parts.push(p)));
            let gpu = fresh::<true>();
            gpu.fill_rect(GAUGE_BOX.x as u16, GAUGE_BOX.y as u16, GAUGE_BOX.w, GAUGE_BOX.h, GAUGE_BG);
            draw_parts(&gpu, &parts);
            same(&cpu, &gpu, &format!("gauge {value}"));
        }
        for (kind, abs, bg, border, t) in [
            (KIND_CIRCLE, Rect::new(5, 9, 90, 70), 0x1234u16, 0xF800u16, 3u8),
            (KIND_CIRCLE, Rect::new(5, 9, 41, 41), 0x1234, 0, 1),
            (KIND_ELLIPSE, Rect::new(3, 20, 110, 60), 0x07E0, 0x001F, 4),
            (KIND_ELLIPSE, Rect::new(3, 20, 51, 90), 0, 0xFFFF, 1),
        ] {
            let mut w = Widget::DEAD;
            w.kind = kind;
            w.background_color = bg;
            w.border_color = border;
            let mut e = WidgetExt::DEFAULT;
            e.border = crate::types::Edges { top: t, right: t, bottom: t, left: t };
            let cpu = fresh::<true>();
            if kind == KIND_CIRCLE {
                draw_circle_shape(&cpu, &w, &abs, &e);
            } else {
                draw_ellipse_shape(&cpu, &w, &abs, &e);
            }
            let mut parts = alloc::vec::Vec::new();
            assert!(round_parts(&w, &abs, &e, |p| parts.push(p)));
            let gpu = fresh::<true>();
            draw_parts(&gpu, &parts);
            same(&cpu, &gpu, &format!("shape {kind} {}x{}", abs.w, abs.h));
        }
        // Lines, value fills and check indicators (what `draw_widget` adds over the box).
        let abs = Rect::new(6, 10, 100, 28);
        let mut cases = alloc::vec::Vec::new();
        for (t, bg, border) in [(1u8, 0u16, 0xF800u16), (3, 0x07E0, 0), (6, 0x001F, 0x1234)] {
            cases.push((KIND_LINE, Rect::new(4, 7, 110, 80), t, 0u16, 0i32, false, bg, border));
            cases.push((KIND_LINE, Rect::new(30, 7, 1, 90), t, 0, 0, false, bg, border));
        }
        for kind in [KIND_PROGRESS, KIND_SLIDER] {
            for (radius, value) in [(0u16, 0i32), (0, 37), (8, 0), (8, 1), (8, 50), (8, 100), (30, 64)] {
                cases.push((kind, abs, 2, radius, value, false, 0x1234, 0x07E0));
            }
        }
        for kind in [KIND_CHECKBOX, KIND_RADIO] {
            for (radius, checked) in [(0u16, false), (0, true), (5, false), (5, true), (40, true)] {
                cases.push((kind, Rect::new(6, 10, 60, 31), 1, radius, 0, checked, 0x1234, 0x07E0));
            }
        }
        for (kind, abs, t, radius, value, checked, bg, border) in cases {
            let mut w = Widget::DEAD;
            w.kind = kind;
            w.background_color = bg;
            w.border_color = border;
            if checked {
                w.flags |= FLAG_CHECKED;
            }
            let mut e = WidgetExt::DEFAULT;
            e.border = crate::types::Edges { top: t, right: t, bottom: t, left: t };
            e.border_radius = radius;
            e.value = value as crate::widget::WValue;
            e.press_color = 0xFD20;
            e.text_color = 0xFFFF;
            e.padding = crate::types::Edges { top: 2, right: 2, bottom: 2, left: 3 };
            let cpu = fresh::<true>();
            let inner = inner_rect(&abs, &e.border);
            match kind {
                KIND_LINE => draw_line_shape(&cpu, &w, &abs, &e),
                KIND_PROGRESS | KIND_SLIDER => draw_value_fill(&cpu, &w, &inner, &e),
                _ => draw_check_indicator(&cpu, &w, &inner, &e),
            }
            let mut parts = alloc::vec::Vec::new();
            assert!(round_parts(&w, &abs, &e, |p| parts.push(p)));
            let gpu = fresh::<true>();
            draw_parts(&gpu, &parts);
            same(&cpu, &gpu, &format!("kind {kind} r {radius} value {value} checked {checked} t {t}"));
        }
    }

    #[test]
    fn gauge_update_matches_full_draw_write_only() {
        gauge_update_matches_full_draw::<false>();
    }

    #[test]
    fn gauge_update_matches_full_draw_readback() {
        gauge_update_matches_full_draw::<true>();
    }

    #[test]
    fn gauge_stays_inside_its_box() {
        for value in [0, 50, 100] {
            let lcd = fresh::<true>();
            paint_gauge(&lcd, value, &[Rect::new(0, 0, FB as u16, FB as u16)]);
            let px = lcd.backend().px.borrow();
            for y in 0..FB {
                for x in 0..FB {
                    let inside = GAUGE_BOX.contains(x as u16, y as u16);
                    if !inside {
                        assert!(px[y * FB + x] == 0, "value {value}: drawn outside at ({x}, {y})");
                    }
                }
            }
        }
    }

    #[test]
    fn restores_fresh_behind_only_until_the_subtree_is_redrawn() {
        let r = |x, w| Rect::new(x, 0, w, 10);
        let now = r(20, 100);
        let mut s = Restores::new();
        s.push(3, r(0, 100));
        // Not restored itself (did not move): nothing is known.
        assert!(!s.fresh_behind(2, now));
        assert!(s.fresh_behind(3, now));
        // A restore behind a widget before 3 draws only what is behind 3.
        s.push(1, r(0, 50));
        assert!(s.fresh_behind(3, now));
        // One for a widget past 3 elsewhere leaves it alone...
        s.push(9, r(300, 10));
        assert!(s.fresh_behind(3, now));
        // ...but one over its new box drew the subtree back in.
        s.push(7, r(110, 5));
        assert!(!s.fresh_behind(3, now));
        // Overflow: no claims at all.
        let mut s = Restores::new();
        for i in 0..17 {
            s.push(i, r(0, 10));
        }
        assert!(!s.fresh_behind(0, now));
    }

    #[test]
    fn transparent_label_does_not_inherit_press_color() {
        let mut tree = ObjPool::new();
        let (button, label) = ghost(&mut tree);
        let ext = &tree.ext(label);
        // No fill of its own: the button's rounded press fill shows through.
        assert_eq!(effective_bg(&tree, tree.get(label), ext), 0);
        // Text without readback still blends against the press color.
        assert_eq!(resolved_text_bg(&tree, tree.get(label), ext, 0), 0x2C9F);
        let bext = &tree.ext(button);
        assert_eq!(effective_bg(&tree, tree.get(button), bext), 0x2C9F);
    }

    #[test]
    fn same_bg_container_still_inherits_press_color() {
        let mut tree = ObjPool::new();
        let (button, _) = ghost(&mut tree);
        let inner = tree.alloc_widget().unwrap();
        tree.get_mut(inner).kind = KIND_BASE;
        tree.add_child(button, inner);
        let ext = &tree.ext(inner);
        assert_eq!(effective_bg(&tree, tree.get(inner), ext), 0x2C9F);
    }
}

/// `render_dirty`'s pass 0 (`shift_moved`): the same scene animated on a
/// backend that moves pixels and on one that cannot must give the same
/// screen after every frame.
#[cfg(all(test, feature = "mock"))]
mod shift_tests {
    use super::*;
    use crate::backlight::BacklightImpl;
    use crate::flash::FlashImpl;
    use crate::font::FontList;
    use crate::gpio::GpioImpl;
    use crate::image::ImageList;
    use crate::mock::*;
    use crate::rtc::RtcImpl;
    use crate::systick::SystickImpl;
    use crate::types::Size;
    use crate::usart::UsartImpl;
    use crate::widget::KIND_BASE;
    use alloc::vec;
    use core::cell::{Cell, RefCell};

    const S: usize = 128;

    /// 128×128 RGB565 readback frame buffer; `MOVE` = it can move pixels.
    struct Fb<const MOVE: bool> {
        px: RefCell<Vec<u16>>,
        /// Pixels moved so far.
        moved: Cell<usize>,
    }

    impl<const MOVE: bool> LcdBackend for Fb<MOVE> {
        const WIDTH: u16 = S as u16;
        const HEIGHT: u16 = S as u16;
        const HAS_ALPHA: bool = true;
        const CAN_MOVE: bool = MOVE;
        fn begin_frame(&mut self) {}
        fn end_frame(&mut self) {}
        fn back_buf(&self) -> u8 {
            0
        }
        fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, c: u16) {
            self.blend_rect(x, y, w, h, c, 255);
        }
        fn blend_rect(&self, x: u16, y: u16, w: u16, h: u16, c: u16, a: u8) {
            let mut px = self.px.borrow_mut();
            for yy in y as usize..(y as usize + h as usize).min(S) {
                for xx in x as usize..(x as usize + w as usize).min(S) {
                    let p = &mut px[yy * S + xx];
                    *p = lcd::lerp_rgb565(*p, c, a as u16, 255);
                }
            }
        }
        fn blend_pixel(&self, x: u16, y: u16, c: u16, a: u8) {
            self.blend_rect(x, y, 1, 1, c, a);
        }
        fn begin_pixels(&self, _: u16, _: u16, _: u16, _: u16) {}
        fn write_pixel(&self, _: u16) {}
        fn send_command(&self, _: u16) {}
        fn send_data(&self, _: u16) {}
        fn move_rects(&self, rects: &[Rect], dx: i16, dy: i16) -> bool {
            let before = self.px.borrow().clone();
            let mut px = self.px.borrow_mut();
            for r in rects {
                for y in r.y..r.bottom() {
                    for x in r.x..r.right() {
                        px[(y + dy) as usize * S + (x + dx) as usize] = before[y as usize * S + x as usize];
                    }
                }
                self.moved.set(self.moved.get() + r.w as usize * r.h as usize);
            }
            true
        }
    }

    struct Plat<const MOVE: bool>;
    impl<const MOVE: bool> Platform for Plat<MOVE> {
        type LcdB = Fb<MOVE>;
        type FlashB = MockFlash;
        type TouchB = MockTouch;
        type BacklightB = MockBacklight;
        type RtcB = MockRtc;
        type UsartB = MockUsart;
        type SystickB = MockSystick;
        #[cfg(feature = "audio")]
        type AudioB = crate::audio::NoAudio;
        #[cfg(feature = "jpeg")]
        type JpegB = crate::jpeg::NoJpeg;
        type GpioB = MockGpio;
        type EepromB = MockEeprom;
        type Renderer = crate::renderer::DefaultRenderer;
    }

    fn ctx<const MOVE: bool>() -> Ctx<Plat<MOVE>> {
        Ctx {
            lcd: LcdImpl::with_backend(Fb { px: RefCell::new(vec![0; S * S]), moved: Cell::new(0) }),
            flash: FlashImpl::with_backend(MockFlash),
            fonts: FontList::new(),
            images: ImageList::new(),
            objs: ObjPool::new(),
            fs: None,
            backlight: BacklightImpl::with_backend(MockBacklight),
            rtc: RtcImpl::with_backend(MockRtc),
            usart: UsartImpl::with_backend(MockUsart),
            systick: SystickImpl::init(),
            #[cfg(feature = "audio")]
            audio: crate::audio::AudioImpl::none(),
            #[cfg(feature = "jpeg")]
            jpeg: crate::jpeg::JpegImpl::none(),
            gpio: GpioImpl::with_backend(MockGpio::default()),
            eeprom: crate::eeprom::EepromImpl::with_backend(MockEeprom::default()),
            cursor_visible: false,
            #[cfg(feature = "multi_program")]
            tasks: crate::task::TaskTable::new(),
        }
    }

    fn rect(objs: &mut ObjPool, parent: WidgetId, r: Rect, bg: u16, radius: u16, alpha: u8) -> WidgetId {
        let id = objs.alloc_widget().unwrap();
        objs.add_child(parent, id);
        let w = objs.get_mut(id);
        w.kind = KIND_BASE;
        w.location = Offset { x: r.x, y: r.y };
        w.size = Size { w: r.w, h: r.h };
        w.background_color = bg;
        objs.update_ext(id, |e| {
            e.border_radius = radius;
            e.alpha = alpha;
        });
        id
    }

    /// Wallpaper-ish backdrop, a rounded opaque panel with an outline and
    /// see-through contents, and two widgets in front of its path. Returns
    /// (panel, front).
    fn scene<const MOVE: bool>(ctx: &mut Ctx<Plat<MOVE>>) -> (WidgetId, WidgetId) {
        let o = &mut ctx.objs;
        let root = o.alloc_widget().unwrap();
        o.root = root;
        o.get_mut(root).size = Size { w: S as u16, h: S as u16 };
        o.get_mut(root).background_color = 0x1082;
        rect(o, root, Rect::new(-10, 40, 90, 90), 0xF81F, 45, 90);
        rect(o, root, Rect::new(70, -20, 70, 60), 0x07E0, 20, 60);
        let panel = rect(o, root, Rect::new(20, 30, 60, 40), 0x39E7, 8, 255);
        o.update_ext(panel, |e| e.border = Edges { top: 2, right: 2, bottom: 2, left: 2 });
        o.get_mut(panel).border_color = 0xFFE0;
        rect(o, panel, Rect::new(5, 5, 20, 10), 0x001F, 0, 255);
        rect(o, panel, Rect::new(30, 4, 22, 22), 0xF800, 6, 128);
        rect(o, panel, Rect::new(8, 20, 40, 12), 0x07FF, 5, 200);
        // A rounded value fill (drawn by `draw_value_fill`).
        let bar = rect(o, panel, Rect::new(3, 31, 50, 6), 0x2104, 3, 255);
        o.get_mut(bar).kind = crate::widget::KIND_PROGRESS;
        o.update_ext(bar, |e| {
            e.press_color = 0x8DBF;
            e.value = 60;
        });
        let front = rect(o, root, Rect::new(55, 60, 20, 18), 0xFFFF, 0, 255);
        rect(o, root, Rect::new(0, 100, 40, 20), 0x8410, 7, 150);
        (panel, front)
    }

    fn move_to<const MOVE: bool>(ctx: &mut Ctx<Plat<MOVE>>, id: WidgetId, x: i16, y: i16) {
        ctx.objs.will_move(id);
        ctx.objs.get_mut(id).location = Offset { x, y };
        ctx.objs.mark_moved(id);
    }

    #[test]
    fn shifted_panel_matches_redrawn_panel() {
        let mut a = ctx::<true>();
        let mut b = ctx::<false>();
        let (panel, front) = scene(&mut a);
        let _ = scene(&mut b);
        render_all(&mut a);
        render_all(&mut b);
        // Down, right, diagonally, partly off each edge, under the widget in
        // front, then back.
        let path = [(20, 34), (26, 34), (14, 22), (-25, 22), (-25, -15), (80, 100), (90, 70), (40, 50), (20, 30)];
        let mut last_moved = 0;
        let mut shifted_frames = 0;
        for (step, &(x, y)) in path.iter().enumerate() {
            move_to(&mut a, panel, x, y);
            move_to(&mut b, panel, x, y);
            if step == 6 {
                // A frame where the panel also changes: drawn the full way.
                for c in [&mut a.objs, &mut b.objs] {
                    c.get_mut(panel).background_color = 0x2945;
                    c.mark_dirty(panel);
                }
            }
            if step == 7 {
                // Something in front changes in the same frame.
                for c in [&mut a.objs, &mut b.objs] {
                    c.get_mut(front).background_color = 0xFD20;
                    c.mark_dirty(front);
                }
            }
            render_dirty(&mut a);
            render_dirty(&mut b);
            let (pa, pb) = (a.lcd.backend().px.borrow(), b.lcd.backend().px.borrow());
            for i in 0..S * S {
                assert!(pa[i] == pb[i], "step {step} ({x}, {y}): pixel ({}, {}) {:04x} vs {:04x}", i % S, i / S, pa[i], pb[i]);
            }
            let moved = a.lcd.backend().moved.get();
            if step == 6 {
                assert_eq!(moved, last_moved, "a changed panel must not be shifted");
            }
            if moved > last_moved {
                shifted_frames += 1;
            }
            last_moved = moved;
        }
        assert!(b.lcd.backend().moved.get() == 0);
        assert!(shifted_frames >= 6, "only {shifted_frames} frames shifted");
    }

    /// A scroll container whose rows (each with a child) scroll under a translucent widget
    /// below it that is redrawn too: the dirty render must match a full redraw. It used to
    /// differ two ways: the restore behind the translucent widget drew rows scrolled out of
    /// the view there, and a partly visible row's child was drawn without its row.
    #[test]
    fn scrolled_children_match_a_full_redraw() {
        let build = |c: &mut Ctx<Plat<false>>| {
            let o = &mut c.objs;
            let root = o.alloc_widget().unwrap();
            o.root = root;
            o.get_mut(root).size = Size { w: S as u16, h: S as u16 };
            o.get_mut(root).background_color = 0x1082;
            let list = rect(o, root, Rect::new(10, 10, 70, 64), 0x2124, 0, 255);
            o.get_mut(list).flags |= FLAG_CLIP_CHILDREN;
            o.update_ext(list, |e| e.padding = Edges { top: 2, right: 2, bottom: 2, left: 2 });
            for i in 0..8 {
                let row = rect(o, list, Rect::new(0, i * 15, 52, 13), 0x4208 + i as u16 * 0x0841, 3, 255);
                rect(o, row, Rect::new(30, 3, 16, 7), 0xFFE0, 0, 255);
            }
            let veil = rect(o, root, Rect::new(0, 70, S as u16, 40), 0xF800, 6, 110);
            (list, veil)
        };
        let mut a = ctx::<false>();
        let mut b = ctx::<false>();
        let (la, va) = build(&mut a);
        let (lb, vb) = build(&mut b);
        render_all(&mut a);
        for (step, scroll) in [6, 13, 20, 33, 52, 41, 0].into_iter().enumerate() {
            for (c, l) in [(&mut a.objs, la), (&mut b.objs, lb)] {
                c.get_mut(l).value = scroll;
                c.mark_dirty(l);
            }
            // The translucent widget below changes too: it restores what is behind it.
            a.objs.get_mut(va).background_color ^= 0x0841;
            a.objs.mark_dirty(va);
            b.objs.get_mut(vb).background_color = a.objs.get(va).background_color;
            render_dirty(&mut a);
            render_all(&mut b);
            let (pa, pb) = (a.lcd.backend().px.borrow(), b.lcd.backend().px.borrow());
            for i in 0..S * S {
                assert!(
                    pa[i] == pb[i],
                    "step {step} (scroll {scroll}): pixel ({}, {}) {:04x} vs {:04x}",
                    i % S,
                    i / S,
                    pa[i],
                    pb[i]
                );
            }
        }
    }

    /// A panel and one of its children moving in the same frame: the
    /// child's own move must not be carried along by the panel's shift (it
    /// used to be: the panel was shifted whole, the child kept its old place
    /// inside it).
    #[test]
    fn child_moving_inside_a_moving_panel() {
        let mut a = ctx::<true>();
        let mut b = ctx::<false>();
        let (panel, _) = scene(&mut a);
        let _ = scene(&mut b);
        render_all(&mut a);
        render_all(&mut b);
        let child = a.objs.get(panel).first_child;
        move_to(&mut a, child, 30, 22);
        move_to(&mut a, panel, 26, 36);
        move_to(&mut b, child, 30, 22);
        move_to(&mut b, panel, 26, 36);
        render_dirty(&mut a);
        render_dirty(&mut b);
        let (pa, pb) = (a.lcd.backend().px.borrow(), b.lcd.backend().px.borrow());
        for i in 0..S * S {
            assert!(pa[i] == pb[i], "pixel ({}, {}) {:04x} vs {:04x}", i % S, i / S, pa[i], pb[i]);
        }
    }

    /// Two see-through widgets moving in the same frame (clock hands): the
    /// earlier one leaves the later one's box out of its clip, so the later
    /// one must restore behind its new box even though pass 1b put back what
    /// was behind its old box. It used to skip that, and the earlier widget
    /// was missing where the later one's new box left its old one.
    #[test]
    fn overlapping_see_through_widgets_moving_together() {
        let build = |a: Rect, b: Rect| {
            let mut c = ctx::<false>();
            let o = &mut c.objs;
            let root = o.alloc_widget().unwrap();
            o.root = root;
            o.get_mut(root).size = Size { w: S as u16, h: S as u16 };
            o.get_mut(root).background_color = 0x1082;
            let wa = rect(o, root, a, 0xFFFF, 0, 200);
            let wb = rect(o, root, b, 0xF800, 0, 200);
            (c, wa, wb)
        };
        let (a0, b0) = (Rect::new(20, 20, 40, 60), Rect::new(10, 44, 80, 6));
        let (a1, b1) = (Rect::new(21, 20, 40, 60), Rect::new(10, 36, 80, 14));
        let (mut c, wa, wb) = build(a0, b0);
        render_all(&mut c);
        for (id, r) in [(wa, a1), (wb, b1)] {
            c.objs.will_move(id);
            let w = c.objs.get_mut(id);
            w.location = Offset { x: r.x, y: r.y };
            w.size = Size { w: r.w, h: r.h };
            c.objs.mark_dirty(id);
        }
        render_dirty(&mut c);
        let (mut want, _, _) = build(a1, b1);
        render_all(&mut want);
        let (got, want) = (c.lcd.backend().px.borrow(), want.lcd.backend().px.borrow());
        for i in 0..S * S {
            assert!(got[i] == want[i], "pixel ({}, {}) {:04x} vs {:04x}", i % S, i / S, got[i], want[i]);
        }
    }

    #[test]
    fn rounded_box_partly_off_screen_keeps_its_shape() {
        // A rounded, bordered, gradient-filled box over a plain background,
        // drawn whole and with its left 30 px off the screen.
        let draw = |x: i16| {
            let mut c = ctx::<false>();
            let o = &mut c.objs;
            let root = o.alloc_widget().unwrap();
            o.root = root;
            o.get_mut(root).size = Size { w: S as u16, h: S as u16 };
            o.get_mut(root).background_color = 0x1082;
            let b = rect(o, root, Rect::new(x, 20, 70, 50), 0x39E7, 12, 255);
            o.update_ext(b, |e| {
                e.border = Edges { top: 3, right: 3, bottom: 3, left: 3 };
                e.gradient_color = 0xF800;
                e.gradient_dir = 1;
            });
            o.get_mut(b).border_color = 0xFFE0;
            render_all(&mut c);
            c.lcd.backend().px.borrow().clone()
        };
        let (whole, cut) = (draw(40), draw(-30));
        for y in 0..S {
            for lx in 30..70 {
                let (a, b) = (whole[y * S + 40 + lx], cut[y * S + lx - 30]);
                assert!(a == b, "box column {lx}, row {y}: {a:04x} vs {b:04x}");
            }
        }
    }

    #[test]
    fn nothing_is_shifted_after_drawing_outside_the_renderer() {
        let mut a = ctx::<true>();
        let (panel, _) = scene(&mut a);
        render_all(&mut a);
        // A VM drawing builtin drew over the screen.
        a.objs.screen_touched();
        move_to(&mut a, panel, 24, 30);
        render_dirty(&mut a);
        assert_eq!(a.lcd.backend().moved.get(), 0);
        // The next frame may shift again.
        move_to(&mut a, panel, 28, 30);
        render_dirty(&mut a);
        assert!(a.lcd.backend().moved.get() > 0);
    }
}
