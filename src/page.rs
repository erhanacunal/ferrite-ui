/// Sayfa yönetimi — root widget altında tam ekran sayfalar
///
/// Her sayfa root'un çocuğu olan tam ekran (800×480) bir widget.
/// Aynı anda sadece bir sayfa görünür. Geçiş: eski gizle, yeni göster.
///
/// Sayfalar flash'tan bytecode ile yüklenebilir (VM çalıştırır)
/// veya runtime'da elle oluşturulabilir.

use crate::flash::Flash;
use crate::font::FontList;
use crate::fs::{Fs, RES_PAGE};
use crate::image::ImageList;
use crate::lcd::Lcd;
use crate::render;
use crate::types::{Color, Size};
use crate::vm::Vm;
use crate::widget::{WidgetId, WidgetTree, FLAG_VISIBLE};

/// Max sayfa sayısı
const MAX_PAGES: usize = 8;

/// Bytecode okuma buffer boyutu (flash'tan sayfa bytecode'u)
const PAGE_CODE_BUF: usize = 512;

pub struct PageManager {
    /// Her sayfanın widget ağacındaki kök WidgetId'si
    pages: [WidgetId; MAX_PAGES],
    /// Kayıtlı sayfa sayısı
    count: u8,
    /// Aktif sayfa indeksi (0xFF = hiçbiri)
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

    /// Kayıtlı sayfa sayısı
    pub fn count(&self) -> u8 {
        self.count
    }

    /// Aktif sayfanın indeksi (None = hiçbiri aktif değil)
    pub fn active_index(&self) -> Option<u8> {
        if self.active == 0xFF {
            None
        } else {
            Some(self.active)
        }
    }

    /// Aktif sayfanın WidgetId'si
    pub fn active_page(&self) -> WidgetId {
        if self.active == 0xFF {
            WidgetId::NONE
        } else {
            self.pages[self.active as usize]
        }
    }

    /// İndekse göre sayfa WidgetId'si
    pub fn page(&self, index: u8) -> WidgetId {
        if (index as usize) < self.count as usize {
            self.pages[index as usize]
        } else {
            WidgetId::NONE
        }
    }

    /// Yeni boş sayfa oluştur ve root'un çocuğu olarak ekle.
    /// Tam ekran (800×480), başlangıçta invisible.
    /// Dönüş: (sayfa indeksi, sayfa WidgetId)
    pub fn create_page(
        &mut self,
        tree: &mut WidgetTree,
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
            // Başlangıçta invisible — show ile açılır
            w.flags &= !FLAG_VISIBLE;
        }

        // Root'un çocuğu olarak ekle
        if tree.root.is_some() {
            tree.add_child(tree.root, page_id);
        }

        let index = self.count;
        self.pages[index as usize] = page_id;
        self.count += 1;

        Some((index, page_id))
    }

    /// Flash'taki sayfa bytecode'unu yükleyerek sayfa oluştur.
    /// Bytecode, page widget'ı target olarak alır ve alt widget'ları oluşturur.
    ///
    /// Flash resource formatı (RES_PAGE):
    ///   - bg_color: u16 LE (2B) — sayfa arka plan rengi
    ///   - bytecode: remaining bytes — VM bytecode (widget'ları oluşturur)
    pub fn load_page(
        &mut self,
        tree: &mut WidgetTree,
        lcd: &Lcd,
        fs: &Fs,
        flash: &Flash,
        resource_name: &[u8],
    ) -> Option<u8> {
        let entry = fs.find(flash, resource_name)?;

        // İlk 2 byte: bg_color
        let mut hdr = [0u8; 2];
        fs.read_resource(flash, &entry, 0, &mut hdr);
        let bg_color = u16::from_le_bytes([hdr[0], hdr[1]]);

        // Sayfa oluştur
        let (index, page_id) = self.create_page(tree, bg_color)?;

        // Bytecode oku ve çalıştır
        let code_size = entry.size.saturating_sub(2) as usize;
        if code_size > 0 && code_size <= PAGE_CODE_BUF {
            let mut code = [0u8; PAGE_CODE_BUF];
            fs.read_resource(flash, &entry, 2, &mut code[..code_size]);

            let mut vm = Vm::new();
            // Page widget'ı target olarak ayarla
            vm.set_target(page_id);
            let mut dummy_fonts = FontList::new();
            let mut dummy_images = ImageList::new();
            vm.run(&code[..code_size], tree, lcd, flash, &mut dummy_fonts, &mut dummy_images, None);
        }

        Some(index)
    }

    /// Load all RES_PAGE resources from flash.
    pub fn load_all_pages(
        &mut self,
        tree: &mut WidgetTree,
        lcd: &Lcd,
        fs: &Fs,
        flash: &Flash,
    ) {
        let page_count = fs.count_by_kind(flash, RES_PAGE);
        for i in 0..page_count {
            if let Some(entry) = fs.find_nth_by_kind(flash, RES_PAGE, i) {
                // İlk 2 byte: bg_color
                let mut hdr = [0u8; 2];
                fs.read_resource(flash, &entry, 0, &mut hdr);
                let bg_color = u16::from_le_bytes([hdr[0], hdr[1]]);

                if let Some((_index, page_id)) = self.create_page(tree, bg_color) {
                    let code_size = entry.size.saturating_sub(2) as usize;
                    if code_size > 0 && code_size <= PAGE_CODE_BUF {
                        let mut code = [0u8; PAGE_CODE_BUF];
                        fs.read_resource(flash, &entry, 2, &mut code[..code_size]);

                        let mut vm = Vm::new();
                        vm.set_target(page_id);
                        let mut dummy_fonts = FontList::new();
                        let mut dummy_images = ImageList::new();
                        vm.run(&code[..code_size], tree, lcd, flash, &mut dummy_fonts, &mut dummy_images, None);
                    }
                }
            }
        }
    }

    /// Check if a page switch is valid. Returns Some((old_index, new_index))
    /// if the switch can proceed, None if invalid or same page.
    /// Caller can use this to invoke on_page_changing callback before executing.
    pub fn prepare_switch(&self, index: u8) -> Option<(u8, u8)> {
        if index as usize >= self.count as usize {
            return None;
        }
        if self.active == index {
            return None;
        }
        Some((self.active, index))
    }

    /// Execute the page switch: hide old, show new, mark dirty, render.
    /// Call this after prepare_switch() and any on_page_changing callback.
    pub fn execute_switch(
        &mut self,
        index: u8,
        tree: &mut WidgetTree,
        lcd: &Lcd,
        flash: &Flash,
        fonts: &FontList,
        images: &ImageList,
    ) {
        // Hide old page
        if self.active != 0xFF {
            let old = self.pages[self.active as usize];
            tree.get_mut(old).flags &= !FLAG_VISIBLE;
        }

        // Show new page
        let new = self.pages[index as usize];
        tree.get_mut(new).flags |= FLAG_VISIBLE;
        tree.mark_dirty(new);
        self.active = index;

        // Mark root dirty (background needs clearing)
        if tree.root.is_some() {
            tree.mark_dirty(tree.root);
        }

        render::render_dirty(tree, lcd, flash, fonts, images);
    }

    /// Show a page (convenience wrapper without callbacks).
    pub fn show(
        &mut self,
        index: u8,
        tree: &mut WidgetTree,
        lcd: &Lcd,
        flash: &Flash,
        fonts: &FontList,
        images: &ImageList,
    ) {
        if self.prepare_switch(index).is_some() {
            self.execute_switch(index, tree, lcd, flash, fonts, images);
        }
    }

    /// Next page (circular).
    pub fn next(&mut self, tree: &mut WidgetTree, lcd: &Lcd, flash: &Flash, fonts: &FontList, images: &ImageList) {
        if self.count == 0 {
            return;
        }
        let next_idx = if self.active == 0xFF {
            0
        } else {
            (self.active + 1) % self.count
        };
        self.show(next_idx, tree, lcd, flash, fonts, images);
    }

    /// Previous page (circular).
    pub fn prev(&mut self, tree: &mut WidgetTree, lcd: &Lcd, flash: &Flash, fonts: &FontList, images: &ImageList) {
        if self.count == 0 {
            return;
        }
        let prev_idx = if self.active == 0xFF || self.active == 0 {
            self.count - 1
        } else {
            self.active - 1
        };
        self.show(prev_idx, tree, lcd, flash, fonts, images);
    }
}
