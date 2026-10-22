/// On-screen keyboard for KIND_INPUT widgets.
///
/// Built-in overlay rendered by firmware — not individual widgets.
/// Layout tables in ROM (~420 bytes), state struct ~10 bytes on stack.
/// Three layers: lowercase, uppercase, symbols.
use crate::font::FontList;
use crate::widget::WidgetId;
use crate::flash::{FlashBackend, FlashImpl};
use crate::lcd::{LcdBackend, LcdImpl};

// --- Layout constants ---

pub const KB_H: u16 = 200;
/// Width the layout tables are designed for. Key x/w are scaled to the
/// actual screen width (`Keyboard::set_screen`) — identity on 800 px
/// landscape, 0.6 in portrait (480 px). The keyboard docks at the bottom.
const DESIGN_W: u16 = 800;
const ROW_H: u16 = 50;
const KEY_GAP: u16 = 4;

// Colors (RGB565)
const KB_BG: u16 = 0x2104; // dark gray
const KEY_BG: u16 = 0x4208; // medium gray
const KEY_PRESSED_BG: u16 = 0x6B4D; // lighter gray
const KEY_SPECIAL_BG: u16 = 0x3186; // slightly different for special keys
const KEY_TEXT: u16 = 0xFFFF; // white

// --- Special key codes ---

pub const KEY_NONE: u8 = 0;
const KEY_SHIFT: u8 = 0x80;
const KEY_BACKSPACE: u8 = 0x81;
const KEY_ENTER: u8 = 0x82;
const KEY_SYMBOLS: u8 = 0x83;
const KEY_ABC: u8 = 0x84;

// --- Key definition ---

#[derive(Clone, Copy)]
struct KeyDef {
    x: u16,
    w: u16,
    code: u8,
}

impl KeyDef {
    const fn new(x: u16, w: u16, code: u8) -> Self {
        Self { x, w, code }
    }
}

// --- Layout tables (ROM) ---

// Row 0: 10 letter keys, 76px each, 4px gap, starting at x=2
const fn letter_row_10(chars: &[u8; 10]) -> [KeyDef; 10] {
    let mut row = [KeyDef::new(0, 0, 0); 10];
    let mut i = 0;
    let mut x = 2u16;
    while i < 10 {
        row[i] = KeyDef::new(x, 76, chars[i]);
        x += 76 + KEY_GAP as u16;
        i += 1;
    }
    row
}

// Row 1: 9 letter keys, 76px each, indented 42px
const fn letter_row_9(chars: &[u8; 9]) -> [KeyDef; 9] {
    let mut row = [KeyDef::new(0, 0, 0); 9];
    let mut i = 0;
    let mut x = 42u16;
    while i < 9 {
        row[i] = KeyDef::new(x, 76, chars[i]);
        x += 76 + KEY_GAP as u16;
        i += 1;
    }
    row
}

// --- Layer 0: Lowercase ---

static ROW0_LOWER: [KeyDef; 10] = letter_row_10(b"qwertyuiop");
static ROW1_LOWER: [KeyDef; 9] = letter_row_9(b"asdfghjkl");
static ROW2_LOWER: [KeyDef; 9] = [
    KeyDef::new(2, 96, KEY_SHIFT),
    KeyDef::new(102, 68, b'z'),
    KeyDef::new(174, 68, b'x'),
    KeyDef::new(246, 68, b'c'),
    KeyDef::new(318, 68, b'v'),
    KeyDef::new(390, 68, b'b'),
    KeyDef::new(462, 68, b'n'),
    KeyDef::new(534, 68, b'm'),
    KeyDef::new(606, 192, KEY_BACKSPACE),
];
static ROW3_LOWER: [KeyDef; 3] = [
    KeyDef::new(2, 118, KEY_SYMBOLS),
    KeyDef::new(124, 436, b' '),
    KeyDef::new(564, 234, KEY_ENTER),
];

// --- Layer 1: Uppercase ---

static ROW0_UPPER: [KeyDef; 10] = letter_row_10(b"QWERTYUIOP");
static ROW1_UPPER: [KeyDef; 9] = letter_row_9(b"ASDFGHJKL");
static ROW2_UPPER: [KeyDef; 9] = [
    KeyDef::new(2, 96, KEY_SHIFT),
    KeyDef::new(102, 68, b'Z'),
    KeyDef::new(174, 68, b'X'),
    KeyDef::new(246, 68, b'C'),
    KeyDef::new(318, 68, b'V'),
    KeyDef::new(390, 68, b'B'),
    KeyDef::new(462, 68, b'N'),
    KeyDef::new(534, 68, b'M'),
    KeyDef::new(606, 192, KEY_BACKSPACE),
];
// Row 3 same geometry as lowercase
static ROW3_UPPER: [KeyDef; 3] = [
    KeyDef::new(2, 118, KEY_SYMBOLS),
    KeyDef::new(124, 436, b' '),
    KeyDef::new(564, 234, KEY_ENTER),
];

// --- Layer 2: Symbols ---

static ROW0_SYMBOLS: [KeyDef; 10] = letter_row_10(b"1234567890");
static ROW1_SYMBOLS: [KeyDef; 9] = letter_row_9(b"@#$%&-+()");
static ROW2_SYMBOLS: [KeyDef; 9] = [
    KeyDef::new(2, 96, KEY_SHIFT),
    KeyDef::new(102, 68, b'*'),
    KeyDef::new(174, 68, b'"'),
    KeyDef::new(246, 68, b'\''),
    KeyDef::new(318, 68, b':'),
    KeyDef::new(390, 68, b';'),
    KeyDef::new(462, 68, b'!'),
    KeyDef::new(534, 68, b'?'),
    KeyDef::new(606, 192, KEY_BACKSPACE),
];
static ROW3_SYMBOLS: [KeyDef; 3] = [
    KeyDef::new(2, 118, KEY_ABC),
    KeyDef::new(124, 436, b' '),
    KeyDef::new(564, 234, KEY_ENTER),
];

// --- Key action result ---

pub enum KeyAction {
    Char(u8),
    Backspace,
    Enter,
    Shift,
    Symbols,
    Abc,
}

// --- Keyboard state ---

pub struct Keyboard {
    pub visible: bool,
    pub focused: WidgetId,
    layer: u8,
    pub pressed_key: u8,
    blink_last: u32,
    pub cursor_visible: bool,
    pub dirty: bool,
    /// Screen size the keyboard is laid out for (logical, see `set_screen`).
    sw: u16,
    sh: u16,
}

impl Keyboard {
    pub fn new() -> Self {
        Self {
            visible: false,
            focused: WidgetId::NONE,
            layer: 0,
            pressed_key: KEY_NONE,
            blink_last: 0,
            cursor_visible: false,
            dirty: false,
            sw: DESIGN_W,
            sh: 480,
        }
    }

    /// Lay the keyboard out for a `w`x`h` screen (call before showing it;
    /// portrait mode changes the logical screen size).
    pub fn set_screen(&mut self, w: u16, h: u16) {
        self.sw = w;
        self.sh = h;
    }

    /// Screen y of the keyboard's top edge (docked at the bottom).
    pub fn top(&self) -> u16 {
        self.sh.saturating_sub(KB_H)
    }

    /// A key's screen x and width (design layout scaled to the screen).
    fn key_span(&self, key: &KeyDef) -> (u16, u16) {
        let sw = self.sw as u32;
        let x0 = key.x as u32 * sw / DESIGN_W as u32;
        let x1 = (key.x + key.w) as u32 * sw / DESIGN_W as u32;
        (x0 as u16, (x1 - x0) as u16)
    }

    /// Handle touch press. Returns true if the press was on a keyboard key.
    pub fn handle_press(&mut self, x: u16, y: u16) -> bool {
        if !self.visible || y < self.top() {
            return false;
        }
        if let Some(code) = self.key_at(x, y) {
            self.pressed_key = code;
            true
        } else {
            self.pressed_key = KEY_NONE;
            false
        }
    }

    /// Handle touch release. Returns action if release matches pressed key.
    pub fn handle_release(&mut self, x: u16, y: u16) -> Option<KeyAction> {
        let pressed = self.pressed_key;
        self.pressed_key = KEY_NONE;
        if pressed == KEY_NONE {
            return None;
        }

        // Check if release is on the same key
        if y < self.top() {
            return None;
        }
        let released = self.key_at(x, y).unwrap_or(KEY_NONE);
        if released != pressed {
            return None;
        }

        match pressed {
            KEY_SHIFT => {
                self.layer = if self.layer == 1 { 0 } else { 1 };
                self.dirty = true; // full redraw — all keys change
                Some(KeyAction::Shift)
            }
            KEY_BACKSPACE => Some(KeyAction::Backspace),
            KEY_ENTER => Some(KeyAction::Enter),
            KEY_SYMBOLS => {
                self.layer = 2;
                self.dirty = true;
                Some(KeyAction::Symbols)
            }
            KEY_ABC => {
                self.layer = 0;
                self.dirty = true;
                Some(KeyAction::Abc)
            }
            ch => {
                // After typing a character in uppercase, revert to lowercase
                if self.layer == 1 {
                    self.layer = 0;
                    self.dirty = true; // layer changed — full redraw
                }
                Some(KeyAction::Char(ch))
            }
        }
    }

    /// Update cursor blink state. Returns true if state changed.
    pub fn update_blink(&mut self, now: u32) -> bool {
        if now.wrapping_sub(self.blink_last) >= 500 {
            self.cursor_visible = !self.cursor_visible;
            self.blink_last = now;
            true
        } else {
            false
        }
    }

    /// Reset blink timer (cursor visible immediately on focus).
    pub fn reset_blink(&mut self, now: u32) {
        self.cursor_visible = true;
        self.blink_last = now;
    }

    /// Find key code at screen position.
    fn key_at(&self, x: u16, y: u16) -> Option<u8> {
        let top = self.top();
        if y < top || y >= top + KB_H || x >= self.sw {
            return None;
        }
        let row_idx = ((y - top) / ROW_H) as usize;
        if row_idx >= 4 {
            return None;
        }

        match self.layer {
            0 => self.search_row(
                x,
                row_idx,
                &ROW0_LOWER,
                &ROW1_LOWER,
                &ROW2_LOWER,
                &ROW3_LOWER,
            ),
            1 => self.search_row(
                x,
                row_idx,
                &ROW0_UPPER,
                &ROW1_UPPER,
                &ROW2_UPPER,
                &ROW3_UPPER,
            ),
            _ => self.search_row(
                x,
                row_idx,
                &ROW0_SYMBOLS,
                &ROW1_SYMBOLS,
                &ROW2_SYMBOLS,
                &ROW3_SYMBOLS,
            ),
        }
    }

    fn search_row(
        &self,
        x: u16,
        row_idx: usize,
        row0: &[KeyDef],
        row1: &[KeyDef],
        row2: &[KeyDef],
        row3: &[KeyDef],
    ) -> Option<u8> {
        let row = match row_idx {
            0 => row0,
            1 => row1,
            2 => row2,
            3 => row3,
            _ => return None,
        };
        for key in row {
            let (kx, kw) = self.key_span(key);
            if x >= kx && x < kx + kw {
                return Some(key.code);
            }
        }
        None
    }

    /// Draw the keyboard overlay.
    pub fn draw<L: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<L>,
        fonts: &FontList,
        flash: &FlashImpl<F>,
    ) {
        if !self.visible {
            return;
        }

        // Background
        lcd.fill_rect(0, self.top(), self.sw, KB_H, KB_BG);

        let font = match fonts.resolve(0) {
            Some(f) => f,
            None => return,
        };

        match self.layer {
            0 => {
                self.draw_rows(
                    lcd,
                    font,
                    flash,
                    &ROW0_LOWER,
                    &ROW1_LOWER,
                    &ROW2_LOWER,
                    &ROW3_LOWER,
                );
            }
            1 => {
                self.draw_rows(
                    lcd,
                    font,
                    flash,
                    &ROW0_UPPER,
                    &ROW1_UPPER,
                    &ROW2_UPPER,
                    &ROW3_UPPER,
                );
            }
            _ => {
                self.draw_rows(
                    lcd,
                    font,
                    flash,
                    &ROW0_SYMBOLS,
                    &ROW1_SYMBOLS,
                    &ROW2_SYMBOLS,
                    &ROW3_SYMBOLS,
                );
            }
        }
    }

    fn draw_rows<L: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<L>,
        font: &crate::font::Font,
        flash: &FlashImpl<F>,
        row0: &[KeyDef],
        row1: &[KeyDef],
        row2: &[KeyDef],
        row3: &[KeyDef],
    ) {
        self.draw_row(lcd, font, flash, row0, 0);
        self.draw_row(lcd, font, flash, row1, 1);
        self.draw_row(lcd, font, flash, row2, 2);
        self.draw_row(lcd, font, flash, row3, 3);
    }

    /// Draw a single key by its code. Used for press/release highlight
    /// without redrawing the entire keyboard.
    pub fn draw_key_by_code<L: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<L>,
        fonts: &FontList,
        flash: &FlashImpl<F>,
        code: u8,
        pressed: bool,
    ) {
        if !self.visible || code == KEY_NONE {
            return;
        }
        let font = match fonts.resolve(0) {
            Some(f) => f,
            None => return,
        };

        let rows: [&[KeyDef]; 4] = match self.layer {
            0 => [&ROW0_LOWER, &ROW1_LOWER, &ROW2_LOWER, &ROW3_LOWER],
            1 => [&ROW0_UPPER, &ROW1_UPPER, &ROW2_UPPER, &ROW3_UPPER],
            _ => [&ROW0_SYMBOLS, &ROW1_SYMBOLS, &ROW2_SYMBOLS, &ROW3_SYMBOLS],
        };

        for (row_idx, row) in rows.iter().enumerate() {
            for key in *row {
                if key.code == code {
                    let ky = self.top() + row_idx as u16 * ROW_H + 2;
                    let kh = ROW_H - 4;
                    let (kx, kw) = self.key_span(key);
                    draw_single_key(lcd, font, flash, key.code, kx, kw, ky, kh, pressed);
                    return;
                }
            }
        }
    }

    fn draw_row<L: LcdBackend, F: FlashBackend>(
        &self,
        lcd: &LcdImpl<L>,
        font: &crate::font::Font,
        flash: &FlashImpl<F>,
        row: &[KeyDef],
        row_idx: u16,
    ) {
        let ky = self.top() + row_idx * ROW_H + 2;
        let kh = ROW_H - 4;

        for key in row {
            let is_pressed = self.pressed_key == key.code && key.code != KEY_NONE;
            let (kx, kw) = self.key_span(key);
            draw_single_key(lcd, font, flash, key.code, kx, kw, ky, kh, is_pressed);
        }
    }
}

/// Draw one key (`code`) in the screen rect (kx, ky, kw, kh).
#[allow(clippy::too_many_arguments)]
fn draw_single_key<L: LcdBackend, F: FlashBackend>(
    lcd: &LcdImpl<L>,
    font: &crate::font::Font,
    flash: &FlashImpl<F>,
    code: u8,
    kx: u16,
    kw: u16,
    ky: u16,
    kh: u16,
    pressed: bool,
) {
    let is_special = code >= 0x80;
    let bg = if pressed {
        KEY_PRESSED_BG
    } else if is_special {
        KEY_SPECIAL_BG
    } else {
        KEY_BG
    };

    lcd.fill_rect(kx, ky, kw, kh, bg);

    let lh = font.line_height() as i16;

    // Draw key label
    let label: &[u8] = match code {
        KEY_SHIFT => b"SHIFT",
        KEY_BACKSPACE => b"DEL",
        KEY_ENTER => b"ENTER",
        KEY_SYMBOLS => b"123",
        KEY_ABC => b"ABC",
        b' ' => b"SPACE",
        ch => {
            // Single character
            let cw = font.char_width(ch as char) as i16;
            let tx = kx as i16 + (kw as i16 - cw) / 2;
            let ty = ky as i16 + (kh as i16 - lh) / 2 + (lh * 3) / 4;
            font.draw_char(lcd, flash, ch as char, tx, ty, KEY_TEXT, Some(bg));
            return;
        }
    };

    let tw = font.text_width(label) as i16;
    let tx = kx as i16 + (kw as i16 - tw) / 2;
    let ty = ky as i16 + (kh as i16 - lh) / 2 + (lh * 3) / 4;
    font.draw_str(lcd, flash, label, tx, ty, KEY_TEXT, Some(bg));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landscape_layout_is_the_design_table() {
        let kb = Keyboard::new();
        assert_eq!(kb.top(), 280);
        for key in ROW2_LOWER.iter().chain(ROW3_LOWER.iter()) {
            assert_eq!(kb.key_span(key), (key.x, key.w));
        }
    }

    #[test]
    fn portrait_layout_scales_keys_and_docks_at_the_bottom() {
        let mut kb = Keyboard::new();
        kb.set_screen(480, 800);
        assert_eq!(kb.top(), 600);
        // Design 'q' = x 2..78, 'p' = x 722..798 → scaled by 0.6.
        assert_eq!(kb.key_at(2, 610), Some(b'q'));
        assert_eq!(kb.key_at(470, 610), Some(b'p'));
        // Bottom row: ENTER spans the right side.
        assert_eq!(kb.key_at(470, 790), Some(KEY_ENTER));
        // Above the keyboard / past the screen: no key.
        assert_eq!(kb.key_at(100, 599), None);
        assert_eq!(kb.key_at(480, 610), None);
        // Keys stay inside the screen.
        for key in ROW0_LOWER.iter().chain(ROW3_LOWER.iter()) {
            let (x, w) = kb.key_span(key);
            assert!(x + w <= 480);
        }
    }
}
