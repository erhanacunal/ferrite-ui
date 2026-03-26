use crate::flash::Flash;
use crate::font::FontList;
use crate::fs::Fs;
use crate::image::ImageList;
use crate::lcd::Lcd;
use crate::strpool;
use crate::systick;
use crate::proto::{
    self, PROP_BG_COLOR, PROP_BORDER_B,
    PROP_BORDER_COLOR, PROP_BORDER_EDGES, PROP_BORDER_L, PROP_BORDER_R, PROP_BORDER_T,
    PROP_CLICKABLE, PROP_ENABLED, PROP_FONT_ID, PROP_KIND, PROP_LOCATION, PROP_LOC_X, PROP_LOC_Y,
    PROP_MARGIN, PROP_MARGIN_B, PROP_MARGIN_L, PROP_MARGIN_R, PROP_MARGIN_T, PROP_PADDING,
    PROP_IMAGE_ID, PROP_ON_CLICK, PROP_ON_PAINT, PROP_ON_TAP,
    PROP_PADDING_B, PROP_PADDING_L, PROP_PADDING_R, PROP_PADDING_T,
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
pub(crate) const OP_BUILTIN: u8 = 41;   // varint=method_id (stack args) | LEN=method_id+payload

// Float32 (soft-float, all no-arg — operands from stack)
pub(crate) const OP_ITOF: u8 = 42;  // [i32] → [f32 bits]
pub(crate) const OP_FTOI: u8 = 43;  // [f32 bits] → [i32]
pub(crate) const OP_FADD: u8 = 44;  // [a, b] → [a + b]  (f32)
pub(crate) const OP_FSUB: u8 = 45;  // [a, b] → [a - b]  (f32)
pub(crate) const OP_FMUL: u8 = 46;  // [a, b] → [a * b]  (f32)
pub(crate) const OP_FDIV: u8 = 47;  // [a, b] → [a / b]  (f32)
pub(crate) const OP_FNEG: u8 = 48;  // [a] → [-a]        (f32)
pub(crate) const OP_FEQ: u8 = 49;   // [a, b] → [i32 0/1]
pub(crate) const OP_FLT: u8 = 50;   // [a, b] → [i32 0/1]
pub(crate) const OP_FLE: u8 = 51;   // [a, b] → [i32 0/1]
pub(crate) const OP_FGT: u8 = 52;   // [a, b] → [i32 0/1]
pub(crate) const OP_FGE: u8 = 53;   // [a, b] → [i32 0/1]
pub(crate) const OP_FNE: u8 = 54;   // [a, b] → [i32 0/1]

// --- Built-in method IDs ---

const BUILTIN_FILL_RECT: u8 = 0;  // stack: [color, size, loc]
const BUILTIN_RECT: u8 = 1;       // stack: [color, size, loc]
const BUILTIN_LINE: u8 = 2;       // stack: [color, end, start]
const BUILTIN_CIRCLE: u8 = 3;     // stack: [color, radius, center]
const BUILTIN_FILL_CIRCLE: u8 = 4;// stack: [color, radius, center]
const BUILTIN_DRAW_IMAGE: u8 = 5; // stack: [image_id, loc]
const BUILTIN_DRAW_TEXT: u8 = 6;  // stack: [colors, font_id, loc] + LEN payload=text
const BUILTIN_DELAY: u8 = 7;      // stack: [ms]

// String operations (strpool)
const BUILTIN_STR: u8 = 8;         // LEN: payload=bytes → [str_id]
const BUILTIN_ITOS: u8 = 9;        // stack: [i32] → [str_id]
const BUILTIN_FTOS: u8 = 10;       // stack: [f32_bits] → [str_id]
const BUILTIN_CONCAT: u8 = 11;     // stack: [str_b, str_a] → [str_id]
const BUILTIN_PARSE_INT: u8 = 12;  // stack: [str_id] → [i32]
const BUILTIN_PARSE_FLOAT: u8 = 13;// stack: [str_id] → [f32_bits]
const BUILTIN_STR_LEN: u8 = 14;    // stack: [str_id] → [len]
const BUILTIN_SET_TEXT: u8 = 15;    // stack: [str_id] → sets target widget text
const BUILTIN_DRAW_STR: u8 = 16;   // stack: [str_id, colors, font_id, loc] → draw
const BUILTIN_STR_CLEAR: u8 = 17;  // no args → smart clear (preserves widget text)
const BUILTIN_STR_FREE: u8 = 18;   // stack: [str_id] → marks string for next clear

// --- VM State ---

#[derive(Clone, Copy, PartialEq)]
pub enum VmState {
    Ready,
    Running,
    Halted,
    Yielded,
    Waiting,
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
    // Non-blocking delay target tick
    pub wait_until: u32,
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
            wait_until: 0,
        }
    }

    /// Set target widget (for page bytecode etc.).
    pub fn set_target(&mut self, id: WidgetId) {
        self.target = id;
    }

    /// Set program counter (used to jump to callback function offset).
    pub fn set_pc(&mut self, pc: u16) {
        self.pc = pc;
    }

    /// Read the return value left on stack after a callback runs.
    /// Returns 0 if stack is empty.
    pub fn pop_result(&mut self) -> i32 {
        if self.sp > 0 {
            self.sp -= 1;
            self.stack[self.sp as usize]
        } else {
            0
        }
    }

    /// Push an argument onto the stack (used before running a callback).
    pub fn push_arg(&mut self, val: i32) {
        if (self.sp as usize) < STACK_SIZE {
            self.stack[self.sp as usize] = val;
            self.sp += 1;
        }
    }

    /// Reset VM state (for new program or callback invocation)
    pub fn reset(&mut self) {
        self.pc = 0;
        self.sp = 0;
        self.call_sp = 0;
        self.target = WidgetId::NONE;
        self.state = VmState::Ready;
        self.arr_count = 0;
        self.arr_next = 0;
        self.wait_until = 0;
    }

    /// Allocate an array from external data and return its ID.
    /// Used by main loop to pass user message data to callbacks.
    pub fn alloc_array_from(&mut self, data: &[u8]) -> Option<i32> {
        if self.arr_count as usize >= MAX_ARRAYS {
            return None;
        }
        let size = data.len().min(ARRAY_POOL_SIZE - self.arr_next as usize);
        if size == 0 {
            return None;
        }
        let id = self.arr_count;
        let offset = self.arr_next;
        for i in 0..size {
            self.arr_pool[offset as usize + i] = data[i] as i32;
        }
        self.arr_meta[id as usize] = ArrMeta {
            offset,
            len: size as u16,
        };
        self.arr_next += size as u16;
        self.arr_count += 1;
        Some(id as i32)
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
                self.exec_varint(opcode, val, tree, lcd, flash, fonts, images, fs);
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
                self.exec_len(opcode, &code[start..end], tree, lcd, flash, fonts);
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
            // --- Float32 (soft-float) ---
            OP_ITOF => {
                let i = self.pop();
                let f = (i as f32).to_bits() as i32;
                self.push(f);
            }
            OP_FTOI => {
                let bits = self.pop() as u32;
                let f = f32::from_bits(bits);
                self.push(f as i32);
            }
            OP_FADD => {
                let b = f32::from_bits(self.pop() as u32);
                let a = f32::from_bits(self.pop() as u32);
                self.push((a + b).to_bits() as i32);
            }
            OP_FSUB => {
                let b = f32::from_bits(self.pop() as u32);
                let a = f32::from_bits(self.pop() as u32);
                self.push((a - b).to_bits() as i32);
            }
            OP_FMUL => {
                let b = f32::from_bits(self.pop() as u32);
                let a = f32::from_bits(self.pop() as u32);
                self.push((a * b).to_bits() as i32);
            }
            OP_FDIV => {
                let b = f32::from_bits(self.pop() as u32);
                let a = f32::from_bits(self.pop() as u32);
                let r = if b != 0.0 { a / b } else { 0.0 };
                self.push(r.to_bits() as i32);
            }
            OP_FNEG => {
                let a = f32::from_bits(self.pop() as u32);
                self.push((-a).to_bits() as i32);
            }
            OP_FEQ => {
                let b = f32::from_bits(self.pop() as u32);
                let a = f32::from_bits(self.pop() as u32);
                self.push(if a == b { 1 } else { 0 });
            }
            OP_FLT => {
                let b = f32::from_bits(self.pop() as u32);
                let a = f32::from_bits(self.pop() as u32);
                self.push(if a < b { 1 } else { 0 });
            }
            OP_FLE => {
                let b = f32::from_bits(self.pop() as u32);
                let a = f32::from_bits(self.pop() as u32);
                self.push(if a <= b { 1 } else { 0 });
            }
            OP_FGT => {
                let b = f32::from_bits(self.pop() as u32);
                let a = f32::from_bits(self.pop() as u32);
                self.push(if a > b { 1 } else { 0 });
            }
            OP_FGE => {
                let b = f32::from_bits(self.pop() as u32);
                let a = f32::from_bits(self.pop() as u32);
                self.push(if a >= b { 1 } else { 0 });
            }
            OP_FNE => {
                let b = f32::from_bits(self.pop() as u32);
                let a = f32::from_bits(self.pop() as u32);
                self.push(if a != b { 1 } else { 0 });
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
        lcd: &Lcd,
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
            OP_BUILTIN => {
                self.exec_builtin_varint(val as u8, tree, lcd, flash, fonts, images);
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

    fn exec_len(&mut self, opcode: u8, payload: &[u8], tree: &mut WidgetTree, lcd: &Lcd, flash: &Flash, fonts: &FontList) {
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
            OP_BUILTIN => {
                if payload.is_empty() {
                    self.state = VmState::Error;
                    return;
                }
                let method_id = payload[0];
                let data = &payload[1..];
                self.exec_builtin_len(method_id, data, lcd, flash, fonts);
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
            PROP_ON_CLICK => w.on_click = val as u16,
            PROP_ON_PAINT => w.on_paint = val as u16,
            PROP_ON_TAP => w.on_tap = val as u16,
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
            PROP_ON_CLICK => w.on_click as i32,
            PROP_ON_PAINT => w.on_paint as i32,
            PROP_ON_TAP => w.on_tap as i32,
            _ => 0,
        }
    }

    fn set_compound_prop(&mut self, tree: &mut WidgetTree, prop_id: u8, data: &[u8]) {
        // PROP_TEXT: payload = raw text bytes → allocate in StringPool, set text_id
        if prop_id == PROP_TEXT {
            if self.target.is_some() {
                if let Some(str_id) = strpool::pool().alloc(data) {
                    tree.get_mut(self.target).text_id = str_id;
                } else {
                    self.state = VmState::Error;
                }
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

    // --- Built-in methods (varint variant: stack-only args) ---

    fn exec_builtin_varint(
        &mut self,
        method_id: u8,
        tree: &mut WidgetTree,
        lcd: &Lcd,
        flash: &Flash,
        fonts: &FontList,
        images: &ImageList,
    ) {
        match method_id {
            BUILTIN_FILL_RECT => {
                // stack: [color, size, loc]
                let color = self.pop() as u16;
                let (w, h) = unpack_pair(self.pop());
                let (x, y) = unpack_pair(self.pop());
                lcd.fill_rect(x, y, w, h, color);
            }
            BUILTIN_RECT => {
                // stack: [color, size, loc]
                let color = self.pop() as u16;
                let (w, h) = unpack_pair(self.pop());
                let (x, y) = unpack_pair(self.pop());
                lcd.draw_rect(x, y, w, h, color);
            }
            BUILTIN_LINE => {
                // stack: [color, end, start]
                let color = self.pop() as u16;
                let (x1, y1) = unpack_pair(self.pop());
                let (x0, y0) = unpack_pair(self.pop());
                lcd.draw_line(x0 as i16, y0 as i16, x1 as i16, y1 as i16, color);
            }
            BUILTIN_CIRCLE => {
                // stack: [color, radius, center]
                let color = self.pop() as u16;
                let radius = self.pop() as i16;
                let (cx, cy) = unpack_pair(self.pop());
                lcd.draw_circle(cx as i16, cy as i16, radius, color);
            }
            BUILTIN_FILL_CIRCLE => {
                // stack: [color, radius, center]
                let color = self.pop() as u16;
                let radius = self.pop() as i16;
                let (cx, cy) = unpack_pair(self.pop());
                lcd.fill_circle(cx as i16, cy as i16, radius, color);
            }
            BUILTIN_DRAW_IMAGE => {
                // stack: [image_id, loc]
                let image_id = self.pop() as u8;
                let (x, y) = unpack_pair(self.pop());
                if let Some(img) = images.find(image_id) {
                    img.draw(lcd, flash, x, y);
                }
            }
            BUILTIN_DELAY => {
                // stack: [ms]
                let ms = self.pop() as u32;
                self.wait_until = systick::millis().wrapping_add(ms);
                self.state = VmState::Waiting;
            }
            BUILTIN_ITOS => {
                let val = self.pop();
                match strpool::itos(val) {
                    Some(id) => self.push(id as i32),
                    None => self.state = VmState::Error,
                }
            }
            BUILTIN_FTOS => {
                let bits = self.pop() as u32;
                match strpool::ftos(bits) {
                    Some(id) => self.push(id as i32),
                    None => self.state = VmState::Error,
                }
            }
            BUILTIN_CONCAT => {
                let b = self.pop() as u8;
                let a = self.pop() as u8;
                match strpool::pool().concat(a, b) {
                    Some(id) => self.push(id as i32),
                    None => self.state = VmState::Error,
                }
            }
            BUILTIN_PARSE_INT => {
                let id = self.pop() as u8;
                self.push(strpool::parse_int(id));
            }
            BUILTIN_PARSE_FLOAT => {
                let id = self.pop() as u8;
                self.push(strpool::parse_float(id) as i32);
            }
            BUILTIN_STR_LEN => {
                let id = self.pop() as u8;
                self.push(strpool::pool().len(id) as i32);
            }
            BUILTIN_SET_TEXT => {
                let str_id = self.pop() as u8;
                if self.target.is_some() {
                    tree.get_mut(self.target).text_id = str_id;
                }
            }
            BUILTIN_DRAW_STR => {
                // stack: [str_id, colors, font_id, loc]
                let str_id = self.pop() as u8;
                let colors = self.pop();
                let (fg, bg) = unpack_pair(colors);
                let font_id = self.pop() as u8;
                let (x, y) = unpack_pair(self.pop());
                let data = strpool::pool().get(str_id);
                if let Some(font) = fonts.resolve(font_id) {
                    let bg_opt = if bg == 0 { None } else { Some(bg) };
                    font.draw_str(lcd, flash, data, x as i16, y as i16, fg, bg_opt);
                }
            }
            BUILTIN_STR_CLEAR => {
                strpool::smart_clear(tree);
            }
            BUILTIN_STR_FREE => {
                let str_id = self.pop() as u8;
                strpool::pool().free(str_id);
            }
            _ => {
                self.state = VmState::Error;
            }
        }
    }

    // --- Built-in methods (LEN variant: payload + stack args) ---

    fn exec_builtin_len(
        &mut self,
        method_id: u8,
        data: &[u8],
        lcd: &Lcd,
        flash: &Flash,
        fonts: &FontList,
    ) {
        match method_id {
            BUILTIN_DRAW_TEXT => {
                // stack: [colors, font_id, loc], payload: text bytes
                let colors = self.pop();
                let (fg, bg) = unpack_pair(colors);
                let font_id = self.pop() as u8;
                let (x, y) = unpack_pair(self.pop());
                if let Some(font) = fonts.resolve(font_id) {
                    let bg_opt = if bg == 0 { None } else { Some(bg) };
                    font.draw_str(lcd, flash, data, x as i16, y as i16, fg, bg_opt);
                }
            }
            BUILTIN_STR => {
                // LEN payload = string bytes → allocate in pool, push str_id
                match strpool::pool().alloc(data) {
                    Some(id) => self.push(id as i32),
                    None => self.state = VmState::Error,
                }
            }
            _ => {
                self.state = VmState::Error;
            }
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

// --- Packed u32 helper ---

/// Unpack a packed i32 into two u16 values: (high, low).
/// Convention: location=(x, y), size=(w, h), colors=(fg, bg).
#[inline]
fn unpack_pair(packed: i32) -> (u16, u16) {
    let val = packed as u32;
    ((val >> 16) as u16, (val & 0xFFFF) as u16)
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
