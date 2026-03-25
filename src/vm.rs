use crate::flash::Flash;
use crate::font::FontList;
use crate::fs::Fs;
use crate::image::ImageList;
use crate::lcd::Lcd;
use crate::proto::{
    self, PROP_BG_COLOR, PROP_BORDER_B,
    PROP_BORDER_COLOR, PROP_BORDER_EDGES, PROP_BORDER_L, PROP_BORDER_R, PROP_BORDER_T,
    PROP_CLICKABLE, PROP_ENABLED, PROP_FONT_ID, PROP_KIND, PROP_LOCATION, PROP_LOC_X, PROP_LOC_Y,
    PROP_MARGIN, PROP_MARGIN_B, PROP_MARGIN_L, PROP_MARGIN_R, PROP_MARGIN_T, PROP_PADDING,
    PROP_IMAGE_ID, PROP_PADDING_B, PROP_PADDING_L, PROP_PADDING_R, PROP_PADDING_T,
    PROP_PRESS_COLOR, PROP_SIZE,
    PROP_SIZE_H, PROP_SIZE_W, PROP_TEXT, PROP_TEXT_ALIGN, PROP_TEXT_COLOR, PROP_VISIBLE, WT_I16,
    WT_LEN, WT_NO_ARG, WT_VARINT,
};
use crate::render;
use crate::types::{Edges, Offset, Size};
use crate::widget::{WidgetId, WidgetTree, FLAG_CLICKABLE, FLAG_ENABLED, FLAG_VISIBLE};

// --- Opcodes ---

// Primary (0–15): 1-byte tag
pub(crate) const OP_HALT: u8 = 0;
pub(crate) const OP_PUSH: u8 = 1;
pub(crate) const OP_POP: u8 = 2;
pub(crate) const OP_LOAD: u8 = 3;
pub(crate) const OP_STORE: u8 = 4;
pub(crate) const OP_ADD: u8 = 5;
pub(crate) const OP_SUB: u8 = 6;
pub(crate) const OP_EQ: u8 = 7;
pub(crate) const OP_LT: u8 = 8;
pub(crate) const OP_JMP: u8 = 9;
pub(crate) const OP_JZ: u8 = 10;
pub(crate) const OP_JNZ: u8 = 11;
pub(crate) const OP_W_TARGET: u8 = 12;
pub(crate) const OP_W_SET: u8 = 13;
pub(crate) const OP_W_GET: u8 = 14;
pub(crate) const OP_W_DIRTY: u8 = 15;

// Extended (16+): 2-byte tag
pub(crate) const OP_DUP: u8 = 16;
pub(crate) const OP_SWAP: u8 = 17;
pub(crate) const OP_MUL: u8 = 18;
pub(crate) const OP_DIV: u8 = 19;
pub(crate) const OP_MOD: u8 = 20;
pub(crate) const OP_NEG: u8 = 21;
pub(crate) const OP_AND: u8 = 22;
pub(crate) const OP_OR: u8 = 23;
pub(crate) const OP_NOT: u8 = 24;
pub(crate) const OP_NE: u8 = 25;
pub(crate) const OP_LE: u8 = 26;
pub(crate) const OP_GE: u8 = 27;
pub(crate) const OP_GT: u8 = 28;
pub(crate) const OP_CALL: u8 = 29;
pub(crate) const OP_RET: u8 = 30;
pub(crate) const OP_YIELD: u8 = 31;
pub(crate) const OP_W_RENDER: u8 = 32;
pub(crate) const OP_W_ALLOC: u8 = 33;
pub(crate) const OP_W_PARENT: u8 = 34;
pub(crate) const OP_F_READ: u8 = 35;
pub(crate) const OP_F_WRITE: u8 = 36;
pub(crate) const OP_ARR_ALLOC: u8 = 37; // varint=size (zero-fill) | LEN=packed init data
pub(crate) const OP_ARR_LOAD: u8 = 38;  // [arr_id, idx] → [value]
pub(crate) const OP_ARR_STORE: u8 = 39; // [arr_id, idx, val] → []
pub(crate) const OP_ARR_LEN: u8 = 40;   // [arr_id] → [len]

// --- VM State ---

#[derive(Clone, Copy, PartialEq)]
pub enum VmState {
    Ready,
    Running,
    Halted,
    Yielded,
    Error,
}

// --- VM ---

const STACK_SIZE: usize = 16;
const VAR_COUNT: usize = 16;
const CALL_STACK_SIZE: usize = 8;
const ARRAY_POOL_SIZE: usize = 64; // 64 × i32 = 256 byte
const MAX_ARRAYS: usize = 8;

#[derive(Clone, Copy)]
struct ArrMeta {
    offset: u16,
    len: u16,
}

impl ArrMeta {
    const fn empty() -> Self {
        Self { offset: 0, len: 0 }
    }
}

pub struct Vm {
    pc: u16,
    stack: [i32; STACK_SIZE],
    sp: u8,
    vars: [i32; VAR_COUNT],
    call_stack: [u16; CALL_STACK_SIZE],
    call_sp: u8,
    target: WidgetId,
    pub state: VmState,
    // Array pool
    arr_pool: [i32; ARRAY_POOL_SIZE],
    arr_meta: [ArrMeta; MAX_ARRAYS],
    arr_count: u8,
    arr_next: u16,
}

impl Vm {
    pub const fn new() -> Self {
        Self {
            pc: 0,
            stack: [0; STACK_SIZE],
            sp: 0,
            vars: [0; VAR_COUNT],
            call_stack: [0; CALL_STACK_SIZE],
            call_sp: 0,
            target: WidgetId::NONE,
            state: VmState::Ready,
            arr_pool: [0; ARRAY_POOL_SIZE],
            arr_meta: [ArrMeta::empty(); MAX_ARRAYS],
            arr_count: 0,
            arr_next: 0,
        }
    }

    /// Dışarıdan target ayarla (sayfa bytecode'u gibi durumlarda).
    pub fn set_target(&mut self, id: WidgetId) {
        self.target = id;
    }

    /// VM'i sıfırla (yeni program için)
    pub fn reset(&mut self) {
        self.pc = 0;
        self.sp = 0;
        self.call_sp = 0;
        self.target = WidgetId::NONE;
        self.state = VmState::Ready;
        self.arr_count = 0;
        self.arr_next = 0;
    }

    /// Run program until halt/yield/error
    pub fn run(
        &mut self,
        code: &[u8],
        tree: &mut WidgetTree,
        lcd: &Lcd,
        flash: &Flash,
        fonts: &mut FontList,
        images: &mut ImageList,
        fs: Option<&Fs>,
    ) {
        self.state = VmState::Running;
        while self.state == VmState::Running {
            self.step(code, tree, lcd, flash, fonts, images, fs);
        }
    }

    /// Execute a single instruction
    pub fn step(
        &mut self,
        code: &[u8],
        tree: &mut WidgetTree,
        lcd: &Lcd,
        flash: &Flash,
        fonts: &mut FontList,
        images: &mut ImageList,
        fs: Option<&Fs>,
    ) {
        let (tag, consumed) = match proto::decode_varint(code, self.pc as usize) {
            Some(v) => v,
            None => {
                self.state = VmState::Error;
                return;
            }
        };
        self.pc += consumed as u16;

        let wire_type = (tag & 0x07) as u8;
        let opcode = (tag >> 3) as u8;

        match wire_type {
            WT_NO_ARG => self.exec_no_arg(opcode, tree, lcd, flash, fonts, images),
            WT_VARINT => {
                let (val, consumed) =
                    match proto::decode_signed_varint(code, self.pc as usize) {
                        Some(v) => v,
                        None => {
                            self.state = VmState::Error;
                            return;
                        }
                    };
                self.pc += consumed as u16;
                self.exec_varint(opcode, val, tree, flash, fonts, images, fs);
            }
            WT_I16 => {
                let pos = self.pc as usize;
                if pos + 2 > code.len() {
                    self.state = VmState::Error;
                    return;
                }
                let val = u16::from_le_bytes([code[pos], code[pos + 1]]);
                self.pc += 2;
                self.exec_i16(opcode, val);
            }
            WT_LEN => {
                let (len, consumed) = match proto::decode_varint(code, self.pc as usize) {
                    Some(v) => v,
                    None => {
                        self.state = VmState::Error;
                        return;
                    }
                };
                self.pc += consumed as u16;
                let start = self.pc as usize;
                let end = start + len as usize;
                if end > code.len() {
                    self.state = VmState::Error;
                    return;
                }
                self.exec_len(opcode, &code[start..end], tree, flash);
                self.pc = end as u16;
            }
            _ => {
                self.state = VmState::Error;
            }
        }
    }

    // --- Stack operations ---

    fn push(&mut self, val: i32) {
        if (self.sp as usize) < STACK_SIZE {
            self.stack[self.sp as usize] = val;
            self.sp += 1;
        } else {
            self.state = VmState::Error;
        }
    }

    fn pop(&mut self) -> i32 {
        if self.sp > 0 {
            self.sp -= 1;
            self.stack[self.sp as usize]
        } else {
            self.state = VmState::Error;
            0
        }
    }

    fn peek(&self) -> i32 {
        if self.sp > 0 {
            self.stack[(self.sp - 1) as usize]
        } else {
            0
        }
    }

    // --- Dispatch: no-arg instructions (wt=5) ---

    fn exec_no_arg(
        &mut self,
        opcode: u8,
        tree: &mut WidgetTree,
        lcd: &Lcd,
        flash: &Flash,
        fonts: &mut FontList,
        images: &mut ImageList,
    ) {
        match opcode {
            OP_HALT => {
                self.state = VmState::Halted;
            }
            OP_POP => {
                self.pop();
            }
            OP_ADD => {
                let b = self.pop();
                let a = self.pop();
                self.push(a.wrapping_add(b));
            }
            OP_SUB => {
                let b = self.pop();
                let a = self.pop();
                self.push(a.wrapping_sub(b));
            }
            OP_EQ => {
                let b = self.pop();
                let a = self.pop();
                self.push(if a == b { 1 } else { 0 });
            }
            OP_LT => {
                let b = self.pop();
                let a = self.pop();
                self.push(if a < b { 1 } else { 0 });
            }
            OP_W_DIRTY => {
                if self.target.is_some() {
                    tree.mark_dirty(self.target);
                }
            }
            OP_DUP => {
                let val = self.peek();
                self.push(val);
            }
            OP_SWAP => {
                if self.sp >= 2 {
                    let i = (self.sp - 1) as usize;
                    let j = (self.sp - 2) as usize;
                    self.stack.swap(i, j);
                } else {
                    self.state = VmState::Error;
                }
            }
            OP_MUL => {
                let b = self.pop();
                let a = self.pop();
                self.push(a.wrapping_mul(b));
            }
            OP_DIV => {
                let b = self.pop();
                let a = self.pop();
                self.push(if b != 0 { a / b } else { 0 });
            }
            OP_MOD => {
                let b = self.pop();
                let a = self.pop();
                self.push(if b != 0 { a % b } else { 0 });
            }
            OP_NEG => {
                let a = self.pop();
                self.push(a.wrapping_neg());
            }
            OP_AND => {
                let b = self.pop();
                let a = self.pop();
                self.push(a & b);
            }
            OP_OR => {
                let b = self.pop();
                let a = self.pop();
                self.push(a | b);
            }
            OP_NOT => {
                let a = self.pop();
                self.push(if a == 0 { 1 } else { 0 });
            }
            OP_NE => {
                let b = self.pop();
                let a = self.pop();
                self.push(if a != b { 1 } else { 0 });
            }
            OP_LE => {
                let b = self.pop();
                let a = self.pop();
                self.push(if a <= b { 1 } else { 0 });
            }
            OP_GE => {
                let b = self.pop();
                let a = self.pop();
                self.push(if a >= b { 1 } else { 0 });
            }
            OP_GT => {
                let b = self.pop();
                let a = self.pop();
                self.push(if a > b { 1 } else { 0 });
            }
            OP_RET => {
                if self.call_sp > 0 {
                    self.call_sp -= 1;
                    self.pc = self.call_stack[self.call_sp as usize];
                } else {
                    self.state = VmState::Error;
                }
            }
            OP_YIELD => {
                self.state = VmState::Yielded;
            }
            OP_W_RENDER => {
                render::render_dirty(tree, lcd, flash, fonts, images);
            }
            OP_W_ALLOC => {
                if let Some(id) = tree.alloc() {
                    self.push(id.0 as i32);
                } else {
                    self.state = VmState::Error;
                }
            }
            OP_ARR_LOAD => {
                let idx = self.pop();
                let arr_id = self.pop();
                self.arr_load(arr_id, idx);
            }
            OP_ARR_STORE => {
                let val = self.pop();
                let idx = self.pop();
                let arr_id = self.pop();
                self.arr_store(arr_id, idx, val);
            }
            OP_ARR_LEN => {
                let arr_id = self.pop();
                self.arr_len(arr_id);
            }
            _ => {
                self.state = VmState::Error;
            }
        }
    }

    // --- Dispatch: varint arg instructions (wt=0) ---

    fn exec_varint(
        &mut self,
        opcode: u8,
        val: i32,
        tree: &mut WidgetTree,
        flash: &Flash,
        fonts: &mut FontList,
        images: &mut ImageList,
        fs: Option<&Fs>,
    ) {
        match opcode {
            OP_PUSH => {
                self.push(val);
            }
            OP_LOAD => {
                let idx = val as usize;
                if idx < VAR_COUNT {
                    self.push(self.vars[idx]);
                } else {
                    self.state = VmState::Error;
                }
            }
            OP_STORE => {
                let idx = val as usize;
                if idx < VAR_COUNT {
                    self.vars[idx] = self.pop();
                } else {
                    self.state = VmState::Error;
                }
            }
            OP_W_TARGET => {
                self.target = WidgetId(val as u8);
            }
            OP_W_SET => {
                let prop_id = val as u8;
                let stack_val = self.pop();
                if self.target.is_some() {
                    self.set_scalar_prop(tree, prop_id, stack_val);
                    // When font_id is set, try to load the font from flash
                    if prop_id == PROP_FONT_ID {
                        if let Some(fs) = fs {
                            fonts.find_or_load(stack_val as u8, fs, flash);
                        }
                    }
                    // When image_id is set, try to load the image from flash
                    if prop_id == PROP_IMAGE_ID {
                        if let Some(fs) = fs {
                            images.find_or_load(stack_val as u8, fs, flash);
                        }
                    }
                }
            }
            OP_W_GET => {
                let prop_id = val as u8;
                if self.target.is_some() {
                    let v = self.get_scalar_prop(tree, prop_id);
                    self.push(v);
                } else {
                    self.push(0);
                }
            }
            OP_W_PARENT => {
                let parent = WidgetId(val as u8);
                if self.target.is_some() {
                    tree.add_child(parent, self.target);
                }
            }
            OP_ARR_ALLOC => {
                self.arr_alloc(val as u16);
            }
            _ => {
                self.state = VmState::Error;
            }
        }
    }

    // --- Dispatch: i16 arg instructions (wt=1) ---

    fn exec_i16(&mut self, opcode: u8, val: u16) {
        match opcode {
            OP_JMP => {
                self.pc = val;
            }
            OP_JZ => {
                let cond = self.pop();
                if cond == 0 {
                    self.pc = val;
                }
            }
            OP_JNZ => {
                let cond = self.pop();
                if cond != 0 {
                    self.pc = val;
                }
            }
            OP_CALL => {
                if (self.call_sp as usize) < CALL_STACK_SIZE {
                    self.call_stack[self.call_sp as usize] = self.pc;
                    self.call_sp += 1;
                    self.pc = val;
                } else {
                    self.state = VmState::Error;
                }
            }
            _ => {
                self.state = VmState::Error;
            }
        }
    }

    // --- Dispatch: LEN payload instructions (wt=2) ---

    fn exec_len(&mut self, opcode: u8, payload: &[u8], tree: &mut WidgetTree, flash: &Flash) {
        match opcode {
            OP_W_SET => {
                if payload.is_empty() {
                    self.state = VmState::Error;
                    return;
                }
                let prop_id = payload[0];
                let data = &payload[1..];
                if self.target.is_some() {
                    self.set_compound_prop(tree, prop_id, data);
                }
            }
            OP_F_READ => {
                if payload.len() < 6 {
                    self.state = VmState::Error;
                    return;
                }
                let addr =
                    u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let len = u16::from_le_bytes([payload[4], payload[5]]) as usize;
                if len > (STACK_SIZE - self.sp as usize) {
                    self.state = VmState::Error;
                    return;
                }
                let mut buf = [0u8; STACK_SIZE];
                let read_len = if len > STACK_SIZE { STACK_SIZE } else { len };
                flash.read(addr, &mut buf[..read_len]);
                for i in 0..read_len {
                    self.push(buf[i] as i32);
                }
            }
            OP_F_WRITE => {
                if payload.len() < 4 {
                    self.state = VmState::Error;
                    return;
                }
                let addr =
                    u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let data = &payload[4..];
                if !data.is_empty() {
                    flash.write(addr, data);
                }
            }
            OP_ARR_ALLOC => {
                self.arr_alloc_init(payload);
            }
            _ => {
                self.state = VmState::Error;
            }
        }
    }

    // --- Property R/W ---

    fn set_scalar_prop(&mut self, tree: &mut WidgetTree, prop_id: u8, val: i32) {
        let w = tree.get_mut(self.target);
        match prop_id {
            PROP_LOC_X => w.location.x = val as i16,
            PROP_LOC_Y => w.location.y = val as i16,
            PROP_SIZE_W => w.size.w = val as u16,
            PROP_SIZE_H => w.size.h = val as u16,
            PROP_VISIBLE => set_flag(&mut w.flags, FLAG_VISIBLE, val != 0),
            PROP_ENABLED => set_flag(&mut w.flags, FLAG_ENABLED, val != 0),
            PROP_CLICKABLE => set_flag(&mut w.flags, FLAG_CLICKABLE, val != 0),
            PROP_BG_COLOR => w.background_color = val as u16,
            PROP_BORDER_COLOR => w.border_color = val as u16,
            PROP_MARGIN_T => w.margin.top = val as u8,
            PROP_MARGIN_R => w.margin.right = val as u8,
            PROP_MARGIN_B => w.margin.bottom = val as u8,
            PROP_MARGIN_L => w.margin.left = val as u8,
            PROP_BORDER_T => w.border.top = val as u8,
            PROP_BORDER_R => w.border.right = val as u8,
            PROP_BORDER_B => w.border.bottom = val as u8,
            PROP_BORDER_L => w.border.left = val as u8,
            PROP_PADDING_T => w.padding.top = val as u8,
            PROP_PADDING_R => w.padding.right = val as u8,
            PROP_PADDING_B => w.padding.bottom = val as u8,
            PROP_PADDING_L => w.padding.left = val as u8,
            PROP_KIND => w.kind = val as u8,
            PROP_TEXT_COLOR => w.text_color = val as u16,
            PROP_FONT_ID => w.font_id = val as u8,
            PROP_TEXT_ALIGN => w.text_align = val as u8,
            PROP_PRESS_COLOR => w.press_color = val as u16,
            PROP_IMAGE_ID => w.image_id = val as u8,
            _ => {}
        }
    }

    fn get_scalar_prop(&self, tree: &WidgetTree, prop_id: u8) -> i32 {
        let w = tree.get(self.target);
        match prop_id {
            PROP_LOC_X => w.location.x as i32,
            PROP_LOC_Y => w.location.y as i32,
            PROP_SIZE_W => w.size.w as i32,
            PROP_SIZE_H => w.size.h as i32,
            PROP_VISIBLE => {
                if w.flags & FLAG_VISIBLE != 0 {
                    1
                } else {
                    0
                }
            }
            PROP_ENABLED => {
                if w.flags & FLAG_ENABLED != 0 {
                    1
                } else {
                    0
                }
            }
            PROP_CLICKABLE => {
                if w.flags & FLAG_CLICKABLE != 0 {
                    1
                } else {
                    0
                }
            }
            PROP_BG_COLOR => w.background_color as i32,
            PROP_BORDER_COLOR => w.border_color as i32,
            PROP_MARGIN_T => w.margin.top as i32,
            PROP_MARGIN_R => w.margin.right as i32,
            PROP_MARGIN_B => w.margin.bottom as i32,
            PROP_MARGIN_L => w.margin.left as i32,
            PROP_BORDER_T => w.border.top as i32,
            PROP_BORDER_R => w.border.right as i32,
            PROP_BORDER_B => w.border.bottom as i32,
            PROP_BORDER_L => w.border.left as i32,
            PROP_PADDING_T => w.padding.top as i32,
            PROP_PADDING_R => w.padding.right as i32,
            PROP_PADDING_B => w.padding.bottom as i32,
            PROP_PADDING_L => w.padding.left as i32,
            PROP_KIND => w.kind as i32,
            PROP_TEXT_COLOR => w.text_color as i32,
            PROP_FONT_ID => w.font_id as i32,
            PROP_TEXT_ALIGN => w.text_align as i32,
            PROP_PRESS_COLOR => w.press_color as i32,
            PROP_IMAGE_ID => w.image_id as i32,
            _ => 0,
        }
    }

    fn set_compound_prop(&mut self, tree: &mut WidgetTree, prop_id: u8, data: &[u8]) {
        // PROP_TEXT: payload = raw text bytes → tree.set_text()
        if prop_id == PROP_TEXT {
            if self.target.is_some() {
                tree.set_text(self.target, data);
            }
            return;
        }

        let (vals, count) = proto::unpack_signed_varints(data);

        let w = tree.get_mut(self.target);
        match prop_id {
            PROP_LOCATION if count >= 2 => {
                w.location = Offset {
                    x: vals[0] as i16,
                    y: vals[1] as i16,
                };
            }
            PROP_SIZE if count >= 2 => {
                w.size = Size {
                    w: vals[0] as u16,
                    h: vals[1] as u16,
                };
            }
            PROP_MARGIN if count >= 4 => {
                w.margin = Edges::new(
                    vals[0] as u8,
                    vals[1] as u8,
                    vals[2] as u8,
                    vals[3] as u8,
                );
            }
            PROP_BORDER_EDGES if count >= 4 => {
                w.border = Edges::new(
                    vals[0] as u8,
                    vals[1] as u8,
                    vals[2] as u8,
                    vals[3] as u8,
                );
            }
            PROP_PADDING if count >= 4 => {
                w.padding = Edges::new(
                    vals[0] as u8,
                    vals[1] as u8,
                    vals[2] as u8,
                    vals[3] as u8,
                );
            }
            _ => {}
        }
    }

    // --- Array operations ---

    fn arr_alloc(&mut self, size: u16) {
        if self.arr_count as usize >= MAX_ARRAYS
            || self.arr_next as usize + size as usize > ARRAY_POOL_SIZE
        {
            self.state = VmState::Error;
            return;
        }
        let id = self.arr_count;
        self.arr_meta[id as usize] = ArrMeta {
            offset: self.arr_next,
            len: size,
        };
        for i in 0..size as usize {
            self.arr_pool[self.arr_next as usize + i] = 0;
        }
        self.arr_next += size;
        self.arr_count += 1;
        self.push(id as i32);
    }

    fn arr_alloc_init(&mut self, data: &[u8]) {
        if self.arr_count as usize >= MAX_ARRAYS {
            self.state = VmState::Error;
            return;
        }
        let offset = self.arr_next;
        let mut pos = 0;
        let mut count = 0u16;
        while pos < data.len() {
            if let Some((v, consumed)) = proto::decode_signed_varint(data, pos) {
                if (offset + count) as usize >= ARRAY_POOL_SIZE {
                    self.state = VmState::Error;
                    return;
                }
                self.arr_pool[(offset + count) as usize] = v;
                count += 1;
                pos += consumed;
            } else {
                break;
            }
        }
        let id = self.arr_count;
        self.arr_meta[id as usize] = ArrMeta { offset, len: count };
        self.arr_next = offset + count;
        self.arr_count += 1;
        self.push(id as i32);
    }

    fn arr_load(&mut self, arr_id: i32, idx: i32) {
        if arr_id < 0 || arr_id as u8 >= self.arr_count {
            self.state = VmState::Error;
            return;
        }
        let meta = self.arr_meta[arr_id as usize];
        if idx < 0 || idx as u16 >= meta.len {
            self.state = VmState::Error;
            return;
        }
        let val = self.arr_pool[meta.offset as usize + idx as usize];
        self.push(val);
    }

    fn arr_store(&mut self, arr_id: i32, idx: i32, val: i32) {
        if arr_id < 0 || arr_id as u8 >= self.arr_count {
            self.state = VmState::Error;
            return;
        }
        let meta = self.arr_meta[arr_id as usize];
        if idx < 0 || idx as u16 >= meta.len {
            self.state = VmState::Error;
            return;
        }
        self.arr_pool[meta.offset as usize + idx as usize] = val;
    }

    fn arr_len(&mut self, arr_id: i32) {
        if arr_id < 0 || arr_id as u8 >= self.arr_count {
            self.state = VmState::Error;
            return;
        }
        self.push(self.arr_meta[arr_id as usize].len as i32);
    }
}

// --- Flag helper ---

#[inline]
fn set_flag(flags: &mut u8, mask: u8, val: bool) {
    if val {
        *flags |= mask;
    } else {
        *flags &= !mask;
    }
}
