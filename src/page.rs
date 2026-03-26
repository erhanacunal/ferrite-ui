/// Page management — full-screen pages under root widget.
///
/// Each page is a child of root, full-screen (800x480).
/// Only one page visible at a time. Transition: hide old, show new.
///
/// Pages can be loaded from flash via bytecode (VM executes it)
/// or created at runtime manually.

use crate::ctx::Ctx;
use crate::fs::RES_PAGE;
use crate::render;
use crate::types::{Color, Size};
use crate::vm::Vm;
use crate::widget::{WidgetId, FLAG_VISIBLE};

const MAX_PAGES: usize = 8;
const PAGE_CODE_BUF: usize = 512;

pub struct PageManager {
    pages: [WidgetId; MAX_PAGES],
    count: u8,
    active: u8,
}

impl PageManager {
    pub const fn new() -> Self {
        Self {
            pages: [WidgetId::NONE; MAX_PAGES],
            count: 0,
            active: 0xFF,
        }
    }

    pub fn count(&self) -> u8 {
        self.count
    }

    pub fn active_index(&self) -> Option<u8> {
        if self.active == 0xFF { None } else { Some(self.active) }
    }

    pub fn active_page(&self) -> WidgetId {
        if self.active == 0xFF { WidgetId::NONE } else { self.pages[self.active as usize] }
    }

    pub fn page(&self, index: u8) -> WidgetId {
        if (index as usize) < self.count as usize {
            self.pages[index as usize]
        } else {
            WidgetId::NONE
        }
    }

    /// Create a new empty page as child of root. Initially invisible.
    pub fn create_page(
        &mut self,
        tree: &mut crate::widget::WidgetTree,
        bg_color: Color,
    ) -> Option<(u8, WidgetId)> {
        if self.count as usize >= MAX_PAGES {
            return None;
        }

        let page_id = tree.alloc()?;
        {
            let w = tree.get_mut(page_id);
            w.size = Size { w: 800, h: 480 };
            w.background_color = bg_color;
            w.flags &= !FLAG_VISIBLE;
        }

        if tree.root.is_some() {
            tree.add_child(tree.root, page_id);
        }

        let index = self.count;
        self.pages[index as usize] = page_id;
        self.count += 1;

        Some((index, page_id))
    }

    /// Load page from flash bytecode resource.
    pub fn load_page(
        &mut self,
        ctx: &mut Ctx,
        resource_name: &[u8],
    ) -> Option<u8> {
        // Phase 1: Read from flash (scoped borrow of ctx.fs)
        let (bg_color, code, code_size) = {
            let fs = ctx.fs.as_ref()?;
            let entry = fs.find(&ctx.flash, resource_name)?;
            let mut hdr = [0u8; 2];
            fs.read_resource(&ctx.flash, &entry, 0, &mut hdr);
            let bg_color = u16::from_le_bytes([hdr[0], hdr[1]]);
            let code_size = entry.size.saturating_sub(2) as usize;
            let mut code = [0u8; PAGE_CODE_BUF];
            if code_size > 0 && code_size <= PAGE_CODE_BUF {
                fs.read_resource(&ctx.flash, &entry, 2, &mut code[..code_size]);
            }
            (bg_color, code, code_size)
        };

        // Phase 2: Create page and run VM (fs borrow dropped)
        let (index, page_id) = self.create_page(&mut ctx.tree, bg_color)?;

        if code_size > 0 && code_size <= PAGE_CODE_BUF {
            let mut vm = Vm::new();
            vm.set_target(page_id);
            vm.run(&code[..code_size], ctx);
        }

        Some(index)
    }

    /// Load all RES_PAGE resources from flash.
    pub fn load_all_pages(&mut self, ctx: &mut Ctx) {
        // Phase 1: Count pages (scoped borrow)
        let page_count = {
            let fs = match ctx.fs.as_ref() {
                Some(fs) => fs,
                None => return,
            };
            fs.count_by_kind(&ctx.flash, RES_PAGE)
        };

        for i in 0..page_count {
            // Read entry data (scoped borrow)
            let page_data = {
                let fs = match ctx.fs.as_ref() {
                    Some(fs) => fs,
                    None => return,
                };
                if let Some(entry) = fs.find_nth_by_kind(&ctx.flash, RES_PAGE, i) {
                    let mut hdr = [0u8; 2];
                    fs.read_resource(&ctx.flash, &entry, 0, &mut hdr);
                    let bg_color = u16::from_le_bytes([hdr[0], hdr[1]]);
                    let code_size = entry.size.saturating_sub(2) as usize;
                    let mut code = [0u8; PAGE_CODE_BUF];
                    if code_size > 0 && code_size <= PAGE_CODE_BUF {
                        fs.read_resource(&ctx.flash, &entry, 2, &mut code[..code_size]);
                    }
                    Some((bg_color, code, code_size))
                } else {
                    None
                }
            };

            // Create page and run VM (fs borrow dropped)
            if let Some((bg_color, code, code_size)) = page_data {
                if let Some((_index, page_id)) = self.create_page(&mut ctx.tree, bg_color) {
                    if code_size > 0 && code_size <= PAGE_CODE_BUF {
                        let mut vm = Vm::new();
                        vm.set_target(page_id);
                        vm.run(&code[..code_size], ctx);
                    }
                }
            }
        }
    }

    pub fn prepare_switch(&self, index: u8) -> Option<(u8, u8)> {
        if index as usize >= self.count as usize {
            return None;
        }
        if self.active == index {
            return None;
        }
        Some((self.active, index))
    }

    pub fn execute_switch(&mut self, index: u8, ctx: &mut Ctx) {
        if self.active != 0xFF {
            let old = self.pages[self.active as usize];
            ctx.tree.get_mut(old).flags &= !FLAG_VISIBLE;
        }

        let new = self.pages[index as usize];
        ctx.tree.get_mut(new).flags |= FLAG_VISIBLE;
        ctx.tree.mark_dirty(new);
        self.active = index;

        if ctx.tree.root.is_some() {
            let root = ctx.tree.root;
            ctx.tree.mark_dirty(root);
        }

        render::render_dirty(ctx);
    }

    pub fn show(&mut self, index: u8, ctx: &mut Ctx) {
        if self.prepare_switch(index).is_some() {
            self.execute_switch(index, ctx);
        }
    }

    pub fn next(&mut self, ctx: &mut Ctx) {
        if self.count == 0 {
            return;
        }
        let next_idx = if self.active == 0xFF { 0 } else { (self.active + 1) % self.count };
        self.show(next_idx, ctx);
    }

    pub fn prev(&mut self, ctx: &mut Ctx) {
        if self.count == 0 {
            return;
        }
        let prev_idx = if self.active == 0xFF || self.active == 0 {
            self.count - 1
        } else {
            self.active - 1
        };
        self.show(prev_idx, ctx);
    }
}
