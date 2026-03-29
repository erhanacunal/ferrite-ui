extern crate alloc;

use alloc::vec::Vec;
use crate::ctx::Ctx;
use crate::flash::Flash;
use crate::strpool;
use crate::systick;
use crate::proto::{
    self, PROP_BG_COLOR, PROP_BORDER_B,
    PROP_BORDER_COLOR, PROP_BORDER_EDGES, PROP_BORDER_L, PROP_BORDER_R, PROP_BORDER_T,
    PROP_CLICKABLE, PROP_ENABLED, PROP_FONT_ID, PROP_KIND, PROP_LOCATION, PROP_LOC_X, PROP_LOC_Y,
    PROP_MARGIN, PROP_MARGIN_B, PROP_MARGIN_L, PROP_MARGIN_R, PROP_MARGIN_T, PROP_PADDING,
    PROP_IMAGE_ID, PROP_ON_CLICK, PROP_ON_PAINT, PROP_ON_TAP,
    PROP_PADDING_B, PROP_PADDING_L, PROP_PADDING_R, PROP_PADDING_T,
    PROP_PRESS_COLOR, PROP_SIZE, PROP_SIZE_H, PROP_SIZE_W,
    PROP_TEXT, PROP_TEXT_ALIGN, PROP_TEXT_COLOR, PROP_VISIBLE,
};
use crate::render;
use crate::types::{Edges, Offset, Size};
use crate::widget::{WidgetId, FLAG_CLICKABLE, FLAG_ENABLED, FLAG_VISIBLE};

// === Code source abstraction ===

/// Trait for reading bytecode — either from RAM or flash.
pub trait CodeSource {
    /// Total bytecode length in bytes.
    fn len(&self) -> usize;
    /// Read a single byte at absolute position. Returns 0 if out of bounds.
    fn byte_at(&self, pos: usize) -> u8;
    /// Read a slice of bytes into buf. Returns number of bytes actually read.
    fn read_bytes(&self, pos: usize, buf: &mut [u8]) -> usize;
}

/// RAM-backed code source — wraps a &[u8] slice (zero-cost).
pub struct RamCode<'a> {
    data: &'a [u8],
}

impl<'a> RamCode<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }
}

impl CodeSource for RamCode<'_> {
    #[inline]
    fn len(&self) -> usize {
        self.data.len()
    }

    #[inline]
    fn byte_at(&self, pos: usize) -> u8 {
        if pos < self.data.len() { self.data[pos] } else { 0 }
    }

    #[inline]
    fn read_bytes(&self, pos: usize, buf: &mut [u8]) -> usize {
        if pos >= self.data.len() {
            return 0;
        }
        let avail = self.data.len() - pos;
        let n = buf.len().min(avail);
        buf[..n].copy_from_slice(&self.data[pos..pos + n]);
        n
    }
}


/// Flash-backed code source — reads bytecode directly from SPI flash.
/// Each read goes straight to flash via SPI (13.5MHz, ~1μs per byte).
/// No RAM buffer needed — saves 4KB code_buf for large programs.
///
/// Flash is a zero-sized hardware accessor (no state), so FlashCode
/// owns its own instance to avoid borrow conflicts with Ctx.
pub struct FlashCode {
    flash: Flash,
    base_addr: u32,
    code_len: usize,
}

impl FlashCode {
    /// Create a flash code source. `base_addr` is the absolute flash address
    /// of the bytecode resource. `code_len` is the size in bytes.
    pub fn new(base_addr: u32, code_len: usize) -> Self {
        Self { flash: Flash::new(), base_addr, code_len }
    }
}

impl CodeSource for FlashCode {
    #[inline]
    fn len(&self) -> usize {
        self.code_len
    }

    #[inline]
    fn byte_at(&self, pos: usize) -> u8 {
        if pos >= self.code_len {
            return 0;
        }
        let mut b = [0u8; 1];
        self.flash.read(self.base_addr + pos as u32, &mut b);
        b[0]
    }

    fn read_bytes(&self, pos: usize, buf: &mut [u8]) -> usize {
        if pos >= self.code_len {
            return 0;
        }
        let avail = self.code_len - pos;
        let n = buf.len().min(avail);
        if n > 0 {
            self.flash.read(self.base_addr + pos as u32, &mut buf[..n]);
        }
        n
    }
}

// --- 1-byte opcode map (IL-style) ---

// No-arg instructions (1 byte total)
const OP_HALT: u8 = 0x00;
const OP_POP: u8 = 0x01;
const OP_DUP: u8 = 0x02;
const OP_SWAP: u8 = 0x03;
const OP_ADD: u8 = 0x04;
const OP_SUB: u8 = 0x05;
const OP_MUL: u8 = 0x06;
const OP_DIV: u8 = 0x07;
const OP_MOD: u8 = 0x08;
const OP_NEG: u8 = 0x09;
const OP_AND: u8 = 0x0A;
const OP_OR: u8 = 0x0B;
const OP_NOT: u8 = 0x0C;
const OP_EQ: u8 = 0x0D;
const OP_NE: u8 = 0x0E;
const OP_LT: u8 = 0x0F;
const OP_LE: u8 = 0x10;
const OP_GT: u8 = 0x11;
const OP_GE: u8 = 0x12;
const OP_RET: u8 = 0x13;
const OP_YIELD: u8 = 0x14;
const OP_W_DIRTY: u8 = 0x15;
const OP_W_RENDER: u8 = 0x16;
const OP_ARR_LOAD: u8 = 0x17;
const OP_ARR_STORE: u8 = 0x18;
const OP_ARR_LEN: u8 = 0x19;
const OP_W_ALLOC: u8 = 0x1A;

// Specialized short forms (1 byte, no args)
const OP_PUSH_0: u8 = 0x20;
const OP_PUSH_1: u8 = 0x21;
const OP_PUSH_2: u8 = 0x22;
const OP_PUSH_M1: u8 = 0x23;
const OP_LOAD_0: u8 = 0x24;
const OP_LOAD_1: u8 = 0x25;
const OP_LOAD_2: u8 = 0x26;
const OP_LOAD_3: u8 = 0x27;
const OP_LOAD_4: u8 = 0x28;
const OP_STORE_0: u8 = 0x29;
const OP_STORE_1: u8 = 0x2A;
const OP_STORE_2: u8 = 0x2B;
const OP_STORE_3: u8 = 0x2C;
const OP_STORE_4: u8 = 0x2D;

// With fixed-size arguments
const OP_PUSH_I8: u8 = 0x30;   // + i8
const OP_PUSH_I16: u8 = 0x31;  // + i16 LE
const OP_PUSH_I32: u8 = 0x32;  // + i32 LE
const OP_LOAD: u8 = 0x33;      // + u8 slot
const OP_STORE: u8 = 0x34;     // + u8 slot
const OP_JMP: u8 = 0x35;       // + u16 target
const OP_JZ: u8 = 0x36;        // + u16 target
const OP_JNZ: u8 = 0x37;       // + u16 target
const OP_CALL: u8 = 0x38;      // + u16 target
const OP_W_TARGET: u8 = 0x39;  // + u8 widget_id
const OP_W_SET: u8 = 0x3A;     // + u8 prop_id (value from stack)
const OP_W_GET: u8 = 0x3B;     // + u8 prop_id
const OP_W_PARENT: u8 = 0x3C;  // + u8 parent_id
const OP_W_SET_LEN: u8 = 0x3D; // + u8 prop_id + u8 len + data
const OP_ARR_ALLOC: u8 = 0x3E; // + u8 size
const OP_ARR_INIT: u8 = 0x3F;  // + u8 count + i32 values LE
const OP_F_READ: u8 = 0x40;    // + u32 addr + u16 len
const OP_F_WRITE: u8 = 0x41;   // + u32 addr + u8 len + data

// Builtins as first-class opcodes (args on stack)
const OP_FILL_RECT: u8 = 0x80;
const OP_RECT: u8 = 0x81;
const OP_LINE: u8 = 0x82;
const OP_CIRCLE: u8 = 0x83;
const OP_FILL_CIRCLE: u8 = 0x84;
const OP_DRAW_IMAGE: u8 = 0x85;
const OP_DRAW_TEXT_LIT: u8 = 0x86; // + u8 len + text
const OP_DELAY: u8 = 0x87;
const OP_STR_LIT: u8 = 0x88;       // + u8 len + text
const OP_ITOS: u8 = 0x89;
const OP_FTOS: u8 = 0x8A;
const OP_CONCAT: u8 = 0x8B;
const OP_PARSE_INT: u8 = 0x8C;
const OP_PARSE_FLOAT: u8 = 0x8D;
const OP_STR_LEN: u8 = 0x8E;
const OP_SET_TEXT: u8 = 0x8F;
const OP_DRAW_STR: u8 = 0x90;
const OP_STR_CLEAR: u8 = 0x91;
const OP_STR_FREE: u8 = 0x92;
const OP_ROUNDED_RECT: u8 = 0x93;
const OP_FILL_ROUNDED_RECT: u8 = 0x94;
const OP_ARC: u8 = 0x95;
const OP_BEGIN_FRAME: u8 = 0x96;
const OP_END_FRAME: u8 = 0x97;
const OP_SEND_USART: u8 = 0x98;
const OP_SEND_USART_STR: u8 = 0x99;

// Float ops (all no-arg)
const OP_ITOF: u8 = 0xC0;
const OP_FTOI: u8 = 0xC1;
const OP_FADD: u8 = 0xC2;
const OP_FSUB: u8 = 0xC3;
const OP_FMUL: u8 = 0xC4;
const OP_FDIV: u8 = 0xC5;
const OP_FNEG: u8 = 0xC6;
const OP_FEQ: u8 = 0xC7;
const OP_FLT: u8 = 0xC8;
const OP_FLE: u8 = 0xC9;
const OP_FGT: u8 = 0xCA;
const OP_FGE: u8 = 0xCB;
const OP_FNE: u8 = 0xCC;

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
const VAR_COUNT: usize = 32;
const CALL_STACK_SIZE: usize = 8;

/// Internal heap-allocated VM array.
struct VmArray {
    id: u16,
    data: Vec<i32>,
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
    arrays: Vec<VmArray>,
    next_arr_id: u16,
    pub wait_until: u32,
}

impl Vm {
    pub fn new() -> Self {
        Self {
            pc: 0,
            stack: [0; STACK_SIZE],
            sp: 0,
            vars: [0; VAR_COUNT],
            call_stack: [0; CALL_STACK_SIZE],
            call_sp: 0,
            target: WidgetId::NONE,
            state: VmState::Ready,
            arrays: Vec::new(),
            next_arr_id: 0,
            wait_until: 0,
        }
    }

    pub fn set_target(&mut self, id: WidgetId) {
        self.target = id;
    }

    pub fn set_pc(&mut self, pc: u16) {
        self.pc = pc;
    }

    pub fn pc(&self) -> u16 {
        self.pc
    }

    pub fn pop_result(&mut self) -> i32 {
        if self.sp > 0 {
            self.sp -= 1;
            self.stack[self.sp as usize]
        } else {
            0
        }
    }

    pub fn push_arg(&mut self, val: i32) {
        if (self.sp as usize) < STACK_SIZE {
            self.stack[self.sp as usize] = val;
            self.sp += 1;
        }
    }

    pub fn reset(&mut self) {
        self.pc = 0;
        self.sp = 0;
        self.call_sp = 0;
        self.target = WidgetId::NONE;
        self.state = VmState::Ready;
        self.arrays.clear();
        self.next_arr_id = 0;
        self.wait_until = 0;
    }

    pub fn alloc_array_from(&mut self, data: &[u8]) -> Option<i32> {
        if data.is_empty() {
            return None;
        }
        let id = self.next_arr_id;
        let arr_data: Vec<i32> = data.iter().map(|&b| b as i32).collect();
        self.arrays.push(VmArray { id, data: arr_data });
        self.next_arr_id = self.next_arr_id.wrapping_add(1);
        Some(id as i32)
    }

    pub fn run(&mut self, code: &dyn CodeSource, ctx: &mut Ctx) {
        self.state = VmState::Running;
        while self.state == VmState::Running {
            self.step(code, ctx);
        }
    }

    // --- Byte reading helpers ---

    #[inline]
    fn read_u8(&mut self, code: &dyn CodeSource) -> u8 {
        let pos = self.pc as usize;
        if pos < code.len() {
            self.pc += 1;
            code.byte_at(pos)
        } else {
            self.state = VmState::Error;
            0
        }
    }

    #[inline]
    fn read_i8(&mut self, code: &dyn CodeSource) -> i8 {
        self.read_u8(code) as i8
    }

    #[inline]
    fn read_u16(&mut self, code: &dyn CodeSource) -> u16 {
        let pos = self.pc as usize;
        if pos + 2 <= code.len() {
            self.pc += 2;
            let mut buf = [0u8; 2];
            code.read_bytes(pos, &mut buf);
            u16::from_le_bytes(buf)
        } else {
            self.state = VmState::Error;
            0
        }
    }

    #[inline]
    fn read_i16(&mut self, code: &dyn CodeSource) -> i16 {
        self.read_u16(code) as i16
    }

    #[inline]
    fn read_i32(&mut self, code: &dyn CodeSource) -> i32 {
        let pos = self.pc as usize;
        if pos + 4 <= code.len() {
            self.pc += 4;
            let mut buf = [0u8; 4];
            code.read_bytes(pos, &mut buf);
            i32::from_le_bytes(buf)
        } else {
            self.state = VmState::Error;
            0
        }
    }

    #[inline]
    fn read_u32(&mut self, code: &dyn CodeSource) -> u32 {
        self.read_i32(code) as u32
    }

    // --- Execute a single instruction ---

    pub fn step(&mut self, code: &dyn CodeSource, ctx: &mut Ctx) {
        let op = self.read_u8(code);
        if self.state == VmState::Error {
            return;
        }

        match op {
            // --- No-arg: stack, arithmetic, logic, comparison ---
            OP_HALT => self.state = VmState::Halted,
            OP_POP => { self.pop(); }
            OP_DUP => { let v = self.peek(); self.push(v); }
            OP_SWAP => {
                if self.sp >= 2 {
                    let i = (self.sp - 1) as usize;
                    let j = (self.sp - 2) as usize;
                    self.stack.swap(i, j);
                } else {
                    self.state = VmState::Error;
                }
            }
            OP_ADD => { let b = self.pop(); let a = self.pop(); self.push(a.wrapping_add(b)); }
            OP_SUB => { let b = self.pop(); let a = self.pop(); self.push(a.wrapping_sub(b)); }
            OP_MUL => { let b = self.pop(); let a = self.pop(); self.push(a.wrapping_mul(b)); }
            OP_DIV => { let b = self.pop(); let a = self.pop(); self.push(if b != 0 { a / b } else { 0 }); }
            OP_MOD => { let b = self.pop(); let a = self.pop(); self.push(if b != 0 { a % b } else { 0 }); }
            OP_NEG => { let a = self.pop(); self.push(a.wrapping_neg()); }
            OP_AND => { let b = self.pop(); let a = self.pop(); self.push(a & b); }
            OP_OR  => { let b = self.pop(); let a = self.pop(); self.push(a | b); }
            OP_NOT => { let a = self.pop(); self.push(if a == 0 { 1 } else { 0 }); }
            OP_EQ  => { let b = self.pop(); let a = self.pop(); self.push(if a == b { 1 } else { 0 }); }
            OP_NE  => { let b = self.pop(); let a = self.pop(); self.push(if a != b { 1 } else { 0 }); }
            OP_LT  => { let b = self.pop(); let a = self.pop(); self.push(if a < b { 1 } else { 0 }); }
            OP_LE  => { let b = self.pop(); let a = self.pop(); self.push(if a <= b { 1 } else { 0 }); }
            OP_GT  => { let b = self.pop(); let a = self.pop(); self.push(if a > b { 1 } else { 0 }); }
            OP_GE  => { let b = self.pop(); let a = self.pop(); self.push(if a >= b { 1 } else { 0 }); }
            OP_RET => {
                if self.call_sp > 0 {
                    self.call_sp -= 1;
                    self.pc = self.call_stack[self.call_sp as usize];
                } else {
                    self.state = VmState::Error;
                }
            }
            OP_YIELD => self.state = VmState::Yielded,
            OP_W_DIRTY => {
                if self.target.is_some() { ctx.tree.mark_dirty(self.target); }
            }
            OP_W_RENDER => render::render_dirty(ctx),
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
            OP_W_ALLOC => {
                if let Some(id) = ctx.tree.alloc() {
                    self.push(id.0 as i32);
                } else {
                    self.state = VmState::Error;
                }
            }

            // --- Specialized short forms ---
            OP_PUSH_0  => self.push(0),
            OP_PUSH_1  => self.push(1),
            OP_PUSH_2  => self.push(2),
            OP_PUSH_M1 => self.push(-1),
            OP_LOAD_0  => self.push(self.vars[0]),
            OP_LOAD_1  => self.push(self.vars[1]),
            OP_LOAD_2  => self.push(self.vars[2]),
            OP_LOAD_3  => self.push(self.vars[3]),
            OP_LOAD_4  => self.push(self.vars[4]),
            OP_STORE_0 => { let v = self.pop(); self.vars[0] = v; }
            OP_STORE_1 => { let v = self.pop(); self.vars[1] = v; }
            OP_STORE_2 => { let v = self.pop(); self.vars[2] = v; }
            OP_STORE_3 => { let v = self.pop(); self.vars[3] = v; }
            OP_STORE_4 => { let v = self.pop(); self.vars[4] = v; }

            // --- With arguments ---
            OP_PUSH_I8 => {
                let val = self.read_i8(code) as i32;
                self.push(val);
            }
            OP_PUSH_I16 => {
                let val = self.read_i16(code) as i32;
                self.push(val);
            }
            OP_PUSH_I32 => {
                let val = self.read_i32(code);
                self.push(val);
            }
            OP_LOAD => {
                let slot = self.read_u8(code) as usize;
                if slot < VAR_COUNT {
                    self.push(self.vars[slot]);
                } else {
                    self.state = VmState::Error;
                }
            }
            OP_STORE => {
                let slot = self.read_u8(code) as usize;
                if slot < VAR_COUNT {
                    self.vars[slot] = self.pop();
                } else {
                    self.state = VmState::Error;
                }
            }
            OP_JMP => {
                let target = self.read_u16(code);
                self.pc = target;
            }
            OP_JZ => {
                let target = self.read_u16(code);
                if self.pop() == 0 { self.pc = target; }
            }
            OP_JNZ => {
                let target = self.read_u16(code);
                if self.pop() != 0 { self.pc = target; }
            }
            OP_CALL => {
                let target = self.read_u16(code);
                if (self.call_sp as usize) < CALL_STACK_SIZE {
                    self.call_stack[self.call_sp as usize] = self.pc;
                    self.call_sp += 1;
                    self.pc = target;
                } else {
                    self.state = VmState::Error;
                }
            }
            OP_W_TARGET => {
                let wid = self.read_u8(code);
                self.target = WidgetId(wid);
            }
            OP_W_SET => {
                let prop_id = self.read_u8(code);
                let val = self.pop();
                if self.target.is_some() {
                    self.set_scalar_prop(ctx, prop_id, val);
                    if prop_id == PROP_FONT_ID {
                        if let Some(fs) = ctx.fs.as_ref() {
                            ctx.fonts.find_or_load(val as u8, fs, &ctx.flash);
                        }
                    }
                    if prop_id == PROP_IMAGE_ID {
                        if let Some(fs) = ctx.fs.as_ref() {
                            ctx.images.find_or_load(val as u8, fs, &ctx.flash);
                        }
                    }
                }
            }
            OP_W_GET => {
                let prop_id = self.read_u8(code);
                if self.target.is_some() {
                    let v = self.get_scalar_prop(ctx, prop_id);
                    self.push(v);
                } else {
                    self.push(0);
                }
            }
            OP_W_PARENT => {
                let parent = self.read_u8(code);
                if self.target.is_some() {
                    ctx.tree.add_child(WidgetId(parent), self.target);
                }
            }
            OP_W_SET_LEN => {
                let prop_id = self.read_u8(code);
                let len = self.read_u8(code) as usize;
                let start = self.pc as usize;
                let end = start + len;
                if end <= code.len() {
                    let mut buf = [0u8; 32];
                    let n = len.min(buf.len());
                    code.read_bytes(start, &mut buf[..n]);
                    self.pc = end as u16;
                    if self.target.is_some() {
                        self.set_compound_prop(prop_id, &buf[..n], ctx);
                    }
                } else {
                    self.state = VmState::Error;
                }
            }
            OP_ARR_ALLOC => {
                let size = self.read_u8(code) as u16;
                self.arr_alloc(size);
            }
            OP_ARR_INIT => {
                let count = self.read_u8(code) as usize;
                let start = self.pc as usize;
                let byte_len = count * 4;
                if start + byte_len <= code.len() {
                    let mut values: Vec<i32> = Vec::with_capacity(count);
                    let mut tmp = [0u8; 4];
                    for i in 0..count {
                        code.read_bytes(start + i * 4, &mut tmp);
                        values.push(i32::from_le_bytes(tmp));
                    }
                    self.pc = (start + byte_len) as u16;
                    let id = self.next_arr_id;
                    self.arrays.push(VmArray { id, data: values });
                    self.next_arr_id = self.next_arr_id.wrapping_add(1);
                    self.push(id as i32);
                } else {
                    self.state = VmState::Error;
                }
            }
            OP_F_READ => {
                let addr = self.read_u32(code);
                let len = self.read_u16(code) as usize;
                if len > (STACK_SIZE - self.sp as usize) {
                    self.state = VmState::Error;
                    return;
                }
                let mut buf = [0u8; STACK_SIZE];
                let read_len = if len > STACK_SIZE { STACK_SIZE } else { len };
                ctx.flash.read(addr, &mut buf[..read_len]);
                for i in 0..read_len {
                    self.push(buf[i] as i32);
                }
            }
            OP_F_WRITE => {
                let addr = self.read_u32(code);
                let len = self.read_u8(code) as usize;
                let start = self.pc as usize;
                if start + len <= code.len() {
                    let mut buf = [0u8; 64];
                    let n = len.min(buf.len());
                    code.read_bytes(start, &mut buf[..n]);
                    self.pc = (start + len) as u16;
                    if n > 0 {
                        ctx.flash.write(addr, &buf[..n]);
                    }
                } else {
                    self.state = VmState::Error;
                }
            }

            // --- Builtins (args on stack) ---
            OP_FILL_RECT => {
                let color = self.pop() as u16;
                let (w, h) = unpack_pair(self.pop());
                let (x, y) = unpack_pair(self.pop());
                ctx.lcd.fill_rect(x, y, w, h, color);
            }
            OP_RECT => {
                let color = self.pop() as u16;
                let (w, h) = unpack_pair(self.pop());
                let (x, y) = unpack_pair(self.pop());
                ctx.lcd.draw_rect(x, y, w, h, color);
            }
            OP_LINE => {
                let color = self.pop() as u16;
                let (x1, y1) = unpack_pair(self.pop());
                let (x0, y0) = unpack_pair(self.pop());
                ctx.lcd.draw_line(x0 as i16, y0 as i16, x1 as i16, y1 as i16, color);
            }
            OP_CIRCLE => {
                let color = self.pop() as u16;
                let radius = self.pop() as i16;
                let (cx, cy) = unpack_pair(self.pop());
                ctx.lcd.draw_circle(cx as i16, cy as i16, radius, color);
            }
            OP_FILL_CIRCLE => {
                let color = self.pop() as u16;
                let radius = self.pop() as i16;
                let (cx, cy) = unpack_pair(self.pop());
                ctx.lcd.fill_circle(cx as i16, cy as i16, radius, color);
            }
            OP_DRAW_IMAGE => {
                let image_id = self.pop() as u8;
                let (x, y) = unpack_pair(self.pop());
                if let Some(img) = ctx.images.find(image_id) {
                    img.draw(&ctx.lcd, &ctx.flash, x, y);
                }
            }
            OP_DRAW_TEXT_LIT => {
                // Inline text: u8 len + text bytes, stack: [colors, font_id, loc]
                let len = self.read_u8(code) as usize;
                let start = self.pc as usize;
                if start + len <= code.len() {
                    let mut buf = [0u8; 64];
                    let n = len.min(buf.len());
                    code.read_bytes(start, &mut buf[..n]);
                    self.pc = (start + len) as u16;
                    let colors = self.pop();
                    let (fg, bg) = unpack_pair(colors);
                    let font_id = self.pop() as u8;
                    let (x, y) = unpack_pair(self.pop());
                    if let Some(font) = ctx.fonts.resolve(font_id) {
                        let bg_opt = if bg == 0 { None } else { Some(bg) };
                        font.draw_str(&ctx.lcd, &ctx.flash, &buf[..n], x as i16, y as i16, fg, bg_opt);
                    }
                } else {
                    self.state = VmState::Error;
                }
            }
            OP_DELAY => {
                let ms = self.pop() as u32;
                self.wait_until = systick::millis().wrapping_add(ms);
                self.state = VmState::Waiting;
            }
            OP_STR_LIT => {
                // Inline text: u8 len + text bytes → allocate in pool, push str_id
                let len = self.read_u8(code) as usize;
                let start = self.pc as usize;
                if start + len <= code.len() {
                    let mut buf = [0u8; 64];
                    let n = len.min(buf.len());
                    code.read_bytes(start, &mut buf[..n]);
                    self.pc = (start + len) as u16;
                    match ctx.strpool.alloc(&buf[..n]) {
                        Some(id) => self.push(id as i32),
                        None => self.state = VmState::Error,
                    }
                } else {
                    self.state = VmState::Error;
                }
            }
            OP_ITOS => {
                let val = self.pop();
                match strpool::itos(&mut ctx.strpool, val) {
                    Some(id) => self.push(id as i32),
                    None => self.state = VmState::Error,
                }
            }
            OP_FTOS => {
                let bits = self.pop() as u32;
                match strpool::ftos(&mut ctx.strpool, bits) {
                    Some(id) => self.push(id as i32),
                    None => self.state = VmState::Error,
                }
            }
            OP_CONCAT => {
                let b = self.pop() as u16;
                let a = self.pop() as u16;
                match ctx.strpool.concat(a, b) {
                    Some(id) => self.push(id as i32),
                    None => self.state = VmState::Error,
                }
            }
            OP_PARSE_INT => {
                let id = self.pop() as u16;
                self.push(strpool::parse_int(&ctx.strpool, id));
            }
            OP_PARSE_FLOAT => {
                let id = self.pop() as u16;
                self.push(strpool::parse_float(&ctx.strpool, id) as i32);
            }
            OP_STR_LEN => {
                let id = self.pop() as u16;
                self.push(ctx.strpool.len(id) as i32);
            }
            OP_SET_TEXT => {
                let str_id = self.pop() as u16;
                if self.target.is_some() {
                    ctx.tree.get_mut(self.target).text_id = str_id;
                }
            }
            OP_DRAW_STR => {
                let str_id = self.pop() as u16;
                let colors = self.pop();
                let (fg, bg) = unpack_pair(colors);
                let font_id = self.pop() as u8;
                let (x, y) = unpack_pair(self.pop());
                let data = ctx.strpool.get(str_id);
                if let Some(font) = ctx.fonts.resolve(font_id) {
                    let bg_opt = if bg == 0 { None } else { Some(bg) };
                    font.draw_str(&ctx.lcd, &ctx.flash, data, x as i16, y as i16, fg, bg_opt);
                }
            }
            OP_STR_CLEAR => ctx.strpool.smart_clear(&mut ctx.tree),
            OP_STR_FREE => {
                let str_id = self.pop() as u16;
                ctx.strpool.free(str_id);
            }
            OP_ROUNDED_RECT => {
                let color = self.pop() as u16;
                let r = self.pop() as u16;
                let (w, h) = unpack_pair(self.pop());
                let (x, y) = unpack_pair(self.pop());
                ctx.lcd.draw_rounded_rect(x, y, w, h, r, color);
            }
            OP_FILL_ROUNDED_RECT => {
                let color = self.pop() as u16;
                let r = self.pop() as u16;
                let (w, h) = unpack_pair(self.pop());
                let (x, y) = unpack_pair(self.pop());
                ctx.lcd.fill_rounded_rect(x, y, w, h, r, color);
            }
            OP_ARC => {
                let color = self.pop() as u16;
                let end = self.pop() as i16;
                let start = self.pop() as i16;
                let radius = self.pop() as i16;
                let (cx, cy) = unpack_pair(self.pop());
                ctx.lcd.draw_arc(cx as i16, cy as i16, radius, start, end, color);
            }
            OP_BEGIN_FRAME => ctx.lcd.begin_frame(),
            OP_END_FRAME => ctx.lcd.end_frame(),
            OP_SEND_USART => {
                let arr_id = self.pop() as u16;
                if let Some(arr) = self.arrays.iter().find(|a| a.id == arr_id) {
                    for &val in &arr.data {
                        crate::usart::dbg(&[val as u8]);
                    }
                }
            }
            OP_SEND_USART_STR => {
                let str_id = self.pop() as u16;
                let bytes = ctx.strpool.get(str_id);
                crate::usart::dbg(bytes);
            }

            // --- Float32 (soft-float) ---
            OP_ITOF => {
                let i = self.pop();
                self.push((i as f32).to_bits() as i32);
            }
            OP_FTOI => {
                let bits = self.pop() as u32;
                self.push(f32::from_bits(bits) as i32);
            }
            OP_FADD => { let b = f32::from_bits(self.pop() as u32); let a = f32::from_bits(self.pop() as u32); self.push((a + b).to_bits() as i32); }
            OP_FSUB => { let b = f32::from_bits(self.pop() as u32); let a = f32::from_bits(self.pop() as u32); self.push((a - b).to_bits() as i32); }
            OP_FMUL => { let b = f32::from_bits(self.pop() as u32); let a = f32::from_bits(self.pop() as u32); self.push((a * b).to_bits() as i32); }
            OP_FDIV => { let b = f32::from_bits(self.pop() as u32); let a = f32::from_bits(self.pop() as u32); self.push(if b != 0.0 { a / b } else { 0.0 }.to_bits() as i32); }
            OP_FNEG => { let a = f32::from_bits(self.pop() as u32); self.push((-a).to_bits() as i32); }
            OP_FEQ => { let b = f32::from_bits(self.pop() as u32); let a = f32::from_bits(self.pop() as u32); self.push(if a == b { 1 } else { 0 }); }
            OP_FLT => { let b = f32::from_bits(self.pop() as u32); let a = f32::from_bits(self.pop() as u32); self.push(if a < b { 1 } else { 0 }); }
            OP_FLE => { let b = f32::from_bits(self.pop() as u32); let a = f32::from_bits(self.pop() as u32); self.push(if a <= b { 1 } else { 0 }); }
            OP_FGT => { let b = f32::from_bits(self.pop() as u32); let a = f32::from_bits(self.pop() as u32); self.push(if a > b { 1 } else { 0 }); }
            OP_FGE => { let b = f32::from_bits(self.pop() as u32); let a = f32::from_bits(self.pop() as u32); self.push(if a >= b { 1 } else { 0 }); }
            OP_FNE => { let b = f32::from_bits(self.pop() as u32); let a = f32::from_bits(self.pop() as u32); self.push(if a != b { 1 } else { 0 }); }

            _ => self.state = VmState::Error,
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

    // --- Property R/W ---

    fn set_scalar_prop(&mut self, ctx: &mut Ctx, prop_id: u8, val: i32) {
        let w = ctx.tree.get_mut(self.target);
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

    fn get_scalar_prop(&self, ctx: &Ctx, prop_id: u8) -> i32 {
        let w = ctx.tree.get(self.target);
        match prop_id {
            PROP_LOC_X => w.location.x as i32,
            PROP_LOC_Y => w.location.y as i32,
            PROP_SIZE_W => w.size.w as i32,
            PROP_SIZE_H => w.size.h as i32,
            PROP_VISIBLE => if w.flags & FLAG_VISIBLE != 0 { 1 } else { 0 },
            PROP_ENABLED => if w.flags & FLAG_ENABLED != 0 { 1 } else { 0 },
            PROP_CLICKABLE => if w.flags & FLAG_CLICKABLE != 0 { 1 } else { 0 },
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

    fn set_compound_prop(&mut self, prop_id: u8, data: &[u8], ctx: &mut Ctx) {
        if prop_id == PROP_TEXT {
            if self.target.is_some() {
                if let Some(str_id) = ctx.strpool.alloc(data) {
                    ctx.tree.get_mut(self.target).text_id = str_id;
                } else {
                    self.state = VmState::Error;
                }
            }
            return;
        }

        let (vals, count) = proto::unpack_signed_varints(data);

        let w = ctx.tree.get_mut(self.target);
        match prop_id {
            PROP_LOCATION if count >= 2 => {
                w.location = Offset { x: vals[0] as i16, y: vals[1] as i16 };
            }
            PROP_SIZE if count >= 2 => {
                w.size = Size { w: vals[0] as u16, h: vals[1] as u16 };
            }
            PROP_MARGIN if count >= 4 => {
                w.margin = Edges::new(vals[0] as u8, vals[1] as u8, vals[2] as u8, vals[3] as u8);
            }
            PROP_BORDER_EDGES if count >= 4 => {
                w.border = Edges::new(vals[0] as u8, vals[1] as u8, vals[2] as u8, vals[3] as u8);
            }
            PROP_PADDING if count >= 4 => {
                w.padding = Edges::new(vals[0] as u8, vals[1] as u8, vals[2] as u8, vals[3] as u8);
            }
            _ => {}
        }
    }

    // --- Array operations ---

    fn find_array(&self, arr_id: i32) -> Option<usize> {
        if arr_id < 0 { return None; }
        let id = arr_id as u16;
        self.arrays.iter().position(|a| a.id == id)
    }

    fn arr_alloc(&mut self, size: u16) {
        let id = self.next_arr_id;
        let data = Vec::from(alloc::vec![0i32; size as usize]);
        self.arrays.push(VmArray { id, data });
        self.next_arr_id = self.next_arr_id.wrapping_add(1);
        self.push(id as i32);
    }

    fn arr_load(&mut self, arr_id: i32, idx: i32) {
        match self.find_array(arr_id) {
            Some(pos) => {
                let arr = &self.arrays[pos];
                if idx < 0 || idx as usize >= arr.data.len() {
                    self.state = VmState::Error;
                    return;
                }
                let val = arr.data[idx as usize];
                self.push(val);
            }
            None => self.state = VmState::Error,
        }
    }

    fn arr_store(&mut self, arr_id: i32, idx: i32, val: i32) {
        match self.find_array(arr_id) {
            Some(pos) => {
                let arr = &mut self.arrays[pos];
                if idx < 0 || idx as usize >= arr.data.len() {
                    self.state = VmState::Error;
                    return;
                }
                arr.data[idx as usize] = val;
            }
            None => self.state = VmState::Error,
        }
    }

    fn arr_len(&mut self, arr_id: i32) {
        match self.find_array(arr_id) {
            Some(pos) => self.push(self.arrays[pos].data.len() as i32),
            None => self.state = VmState::Error,
        }
    }
}

// --- Packed u32 helper ---

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
