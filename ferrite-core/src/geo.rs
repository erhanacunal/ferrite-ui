//! Widget geometry by DFS index, for the render passes.
//!
//! The renderer asks for the same things about each widget many times per
//! pass: its absolute rect, whether it and all its ancestors are visible, its
//! scroll parent, where its subtree ends in DFS order. Each of those walks the
//! parent chain, and the occluder search asks for every later widget once per
//! dirty root, so a frame that recolours n widgets did O(n²) chain walks.
//!
//! With the `render_cache` feature, `geo_begin` computes all of them in one
//! DFS pass (a child's values follow from its parent's, which comes first in
//! pre-order), and the `*_at` accessors read the table until `geo_end`.
//! Without it (or if the table can't be allocated) the accessors compute the
//! answer live with the same functions as before, so both give identical
//! results.
//!
//! The table is valid only while nothing changes geometry, visibility or tree
//! shape: the render passes change only flag bits and recorded rects, so they
//! bracket themselves with `geo_begin` / `geo_end`.

use crate::pool::ObjPool;
use crate::types::Rect;
use crate::widget::WidgetId;

#[cfg(feature = "render_cache")]
use crate::widget::{FLAG_CLIP_CHILDREN, FLAG_DESTROYED, KIND_DROPDOWN};

/// One DFS entry's geometry.
#[cfg(feature = "render_cache")]
#[derive(Clone, Copy)]
pub(crate) struct Geo {
    /// `absolute_rect`.
    abs: Rect,
    /// Where this widget's children are placed (their location is added).
    ox: i16,
    oy: i16,
    /// DFS index one past this widget's subtree.
    end: u32,
    /// `scroll_parent`.
    sp: WidgetId,
    /// `is_tree_visible`.
    vis: bool,
}

impl ObjPool {
    /// Fills the geometry table for the current DFS order (`ensure_dfs`
    /// first). No-op without `render_cache`.
    pub(crate) fn geo_begin(&mut self) {
        #[cfg(feature = "render_cache")]
        self.geo_build();
    }

    /// Ends the pass: the accessors compute live again.
    pub(crate) fn geo_end(&mut self) {
        #[cfg(feature = "render_cache")]
        {
            self.render.geo_valid = false;
        }
    }

    #[cfg(feature = "render_cache")]
    fn geo_build(&mut self) {
        self.render.geo_valid = false;
        let n = self.dfs_cache.len();
        let mut geo = core::mem::take(&mut self.render.geo);
        let mut stack = core::mem::take(&mut self.render.geo_stack);
        geo.clear();
        stack.clear();
        if geo.try_reserve(n).is_ok() && stack.try_reserve(n).is_ok() {
            for i in 0..n {
                let id = self.dfs_cache[i];
                let w = self.get(id);
                let parent = w.parent;
                // Close the subtrees this entry is not part of.
                while let Some(&top) = stack.last() {
                    if self.dfs_cache[top as usize] == parent {
                        break;
                    }
                    geo[top as usize].end = i as u32;
                    stack.pop();
                }
                let (abs, vis, sp) = match stack.last() {
                    Some(&t) => {
                        let g = &geo[t as usize];
                        let abs = Rect::new(
                            g.ox.wrapping_add(w.location.x),
                            g.oy.wrapping_add(w.location.y),
                            w.size.w,
                            w.size.h,
                        );
                        let vis = g.vis && w.flags & FLAG_DESTROYED == 0 && w.is_visible();
                        let sp = if self.get(parent).flags & FLAG_CLIP_CHILDREN != 0 { parent } else { g.sp };
                        (abs, vis, sp)
                    }
                    // The root, or a parent missing from the order: live.
                    None => (self.absolute_rect(id), self.is_tree_visible(id), self.scroll_parent(id)),
                };
                let (mut ox, mut oy) = (abs.x, abs.y);
                if w.kind != KIND_DROPDOWN {
                    let (b, p) = (self.border(id), self.padding(id));
                    ox = ox.wrapping_add(b.left as i16 + p.left as i16);
                    oy = oy.wrapping_add(b.top as i16 + p.top as i16);
                }
                if w.flags & FLAG_CLIP_CHILDREN != 0 {
                    oy = oy.wrapping_sub(w.value as i16);
                }
                geo.push(Geo { abs, ox, oy, end: n as u32, sp, vis });
                stack.push(i as u32);
            }
            self.render.geo_valid = true;
        }
        self.render.geo = geo;
        self.render.geo_stack = stack;
    }

    #[cfg(feature = "render_cache")]
    #[inline(always)]
    fn geo(&self, i: usize) -> Option<&Geo> {
        if self.render.geo_valid { self.render.geo.get(i) } else { None }
    }

    /// `absolute_rect` of DFS entry `i`.
    #[inline]
    pub fn abs_at(&self, i: usize) -> Rect {
        #[cfg(feature = "render_cache")]
        if let Some(g) = self.geo(i) {
            #[cfg(feature = "layers")]
            {
                let o = self.layers.origin;
                return Rect::new(g.abs.x.wrapping_sub(o.x), g.abs.y.wrapping_sub(o.y), g.abs.w, g.abs.h);
            }
            #[cfg(not(feature = "layers"))]
            return g.abs;
        }
        self.absolute_rect(self.dfs_at(i))
    }

    /// `is_tree_visible` of DFS entry `i`.
    #[inline]
    pub fn vis_at(&self, i: usize) -> bool {
        // Outside the running pass's layer scope: as if hidden.
        #[cfg(feature = "layers")]
        if !self.in_scope(i) {
            return false;
        }
        #[cfg(feature = "render_cache")]
        if let Some(g) = self.geo(i) {
            return g.vis;
        }
        self.is_tree_visible(self.dfs_at(i))
    }

    /// `scroll_parent` of DFS entry `i`.
    #[inline]
    pub fn scroll_parent_at(&self, i: usize) -> WidgetId {
        #[cfg(feature = "render_cache")]
        if let Some(g) = self.geo(i) {
            return g.sp;
        }
        self.scroll_parent(self.dfs_at(i))
    }

    /// DFS index one past the subtree of entry `i` (its descendants follow
    /// it contiguously).
    pub fn sub_end_at(&self, i: usize) -> usize {
        #[cfg(feature = "render_cache")]
        if let Some(g) = self.geo(i) {
            return g.end as usize;
        }
        let id = self.dfs_at(i);
        let mut end = i + 1;
        while end < self.dfs_len() && self.is_descendant(self.dfs_at(end), id) {
            end += 1;
        }
        end
    }
}

#[cfg(all(test, feature = "render_cache"))]
mod tests {
    use crate::pool::ObjPool;
    use crate::widget::{FLAG_CLIP_CHILDREN, KIND_DROPDOWN, WidgetId};

    /// The table must answer exactly like the live parent-chain walks.
    #[test]
    fn table_matches_live_walks() {
        let mut t = ObjPool::new();
        let mut ids: [WidgetId; 40] = [WidgetId::NONE; 40];
        for k in 0..ids.len() {
            let id = t.alloc_widget().expect("alloc");
            ids[k] = id;
            if k == 0 {
                t.root = id;
            } else {
                // Deterministic shape: a mix of deep chains and wide fans.
                let parent = ids[(k * 7 + 3) % k];
                t.add_child(parent, id);
            }
            let w = t.get_mut(id);
            w.location.x = (k as i16 * 13) % 50 - 10;
            w.location.y = (k as i16 * 29) % 70 - 5;
            w.size.w = 20 + k as u16;
            w.size.h = 10 + (k as u16 * 3) % 17;
            if k % 9 == 4 {
                w.flags &= !crate::widget::FLAG_VISIBLE;
            }
            if k % 11 == 2 {
                w.flags |= FLAG_CLIP_CHILDREN;
                w.value = 7;
            }
            if k % 13 == 6 {
                w.kind = KIND_DROPDOWN;
            }
            if k % 5 == 1 {
                t.update_ext(id, |e| {
                    e.border.left = 2;
                    e.border.top = 3;
                    e.padding.left = 1;
                    e.padding.top = 4;
                });
            }
        }
        t.ensure_dfs();
        let n = t.dfs_len();
        let live: std::vec::Vec<_> = (0..n)
            .map(|i| (t.abs_at(i), t.vis_at(i), t.scroll_parent_at(i).0, t.sub_end_at(i)))
            .collect();
        t.geo_begin();
        assert!(t.render.geo_valid);
        for i in 0..n {
            let got = (t.abs_at(i), t.vis_at(i), t.scroll_parent_at(i).0, t.sub_end_at(i));
            assert!(got == live[i], "entry {i}");
        }
        t.geo_end();
        assert!(!t.render.geo_valid);
    }
}
