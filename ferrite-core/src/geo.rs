//! Widget geometry by DFS index, for the render passes.
//!
//! The renderer asks for the same things about each widget many times per
//! pass: its absolute rect, whether it and all its ancestors are visible, its
//! scroll parent, where its subtree ends in DFS order. Each of those walks the
//! parent chain, and the occluder search asks for every later widget once per
//! dirty root, so a frame that recolours n widgets does O(n²) chain walks.
//!
//! The accessors compute each answer live from the widget tree.

use crate::pool::ObjPool;
use crate::types::Rect;
use crate::widget::WidgetId;

impl ObjPool {
    /// `absolute_rect` of DFS entry `i`.
    #[inline]
    pub fn abs_at(&self, i: usize) -> Rect {
        self.absolute_rect(self.dfs_at(i))
    }

    /// `is_tree_visible` of DFS entry `i`.
    #[inline]
    pub fn vis_at(&self, i: usize) -> bool {
        self.is_tree_visible(self.dfs_at(i))
    }

    /// `scroll_parent` of DFS entry `i`.
    #[inline]
    pub fn scroll_parent_at(&self, i: usize) -> WidgetId {
        self.scroll_parent(self.dfs_at(i))
    }

    /// DFS index one past the subtree of entry `i` (its descendants follow
    /// it contiguously).
    pub fn sub_end_at(&self, i: usize) -> usize {
        let id = self.dfs_at(i);
        let mut end = i + 1;
        while end < self.dfs_len() && self.is_descendant(self.dfs_at(end), id) {
            end += 1;
        }
        end
    }
}

