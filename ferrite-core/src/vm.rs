extern crate alloc;

use crate::ctx::Ctx;
use crate::flash::{FlashBackend, FlashImpl};
use crate::platform::Platform;
use crate::proto::{
    self, PROP_ALPHA, PROP_BG_COLOR, PROP_BORDER_B, PROP_BORDER_COLOR, PROP_BORDER_EDGES,
    PROP_BORDER_L, PROP_BORDER_R, PROP_BORDER_RADIUS, PROP_BORDER_T, PROP_CHECKED, PROP_CLICKABLE,
    PROP_CLIP_CHILDREN, PROP_CURSOR_POS, PROP_ENABLED, PROP_FONT_ID, PROP_GRADIENT_COLOR,
    PROP_GRADIENT_DIR, PROP_GRAPH_ARR, PROP_GRAPH_COUNT, PROP_GRAPH_FLAGS, PROP_IMAGE_ID,
    PROP_KIND, PROP_LOC_X, PROP_LOC_Y, PROP_LOCATION, PROP_MARGIN, PROP_MARGIN_B, PROP_MARGIN_L,
    PROP_MARGIN_R, PROP_MARGIN_T, PROP_MAX_LENGTH, PROP_MULTI_LINE, PROP_ON_CHANGE, PROP_ON_CLICK,
    PROP_ON_PAINT, PROP_ON_TAP, PROP_PADDING, PROP_PADDING_B, PROP_PADDING_L, PROP_PADDING_R,
    PROP_PADDING_T, PROP_PRESS_COLOR, PROP_SCROLL_Y, PROP_SIZE, PROP_SIZE_H, PROP_SIZE_W,
    PROP_TEXT, PROP_TEXT_ALIGN, PROP_TEXT_COLOR, PROP_VALUE, PROP_VISIBLE,
};
use crate::render;
use crate::strpool;
use crate::strpool::StringPool;
use crate::types::{Edges, Offset, Size};
use crate::widget::{
    FLAG_CHECKED, FLAG_CLICKABLE, FLAG_CLIP_CHILDREN, FLAG_ENABLED, FLAG_HAS_ON_CLICK,
    FLAG_HAS_ON_PAINT, FLAG_HAS_ON_TAP, FLAG_MULTI_LINE, FLAG_VISIBLE, WidgetExt, WidgetId,
};
use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::syscfg::try_alloc_slice;

// === Code source — owned by VM ===

/// Flash read-ahead cache size. Inline in VmCode enum (no heap allocation).
/// 128B covers most loop bodies and avoids per-byte SPI transactions.
const FLASH_CACHE_SIZE: usize = 1024;

/// Code source enum — VM owns its bytecode source.
pub enum VmCode {
    /// No code loaded.
    None,
    /// RAM-backed: VM owns the bytecode buffer.
    Ram(Vec<u8>),
    /// Flash-backed: inline read-ahead cache, no separate heap allocation.
    /// The bytecode source is a type-erased flash handle so `VmCode`/`Vm` stay
    /// non-generic; dyn dispatch only happens on a cache-miss SPI refill.
    Flash {
        flash: alloc::boxed::Box<dyn FlashBackend>,
        base_addr: u32,
        code_len: usize,
        cache: [u8; FLASH_CACHE_SIZE],
        cache_pos: usize,
        cache_len: usize,
    },
}

impl VmCode {
    #[inline]
    pub fn len(&self) -> usize {
        match self {
            VmCode::None => 0,
            VmCode::Ram(data) => data.len(),
            VmCode::Flash { code_len, .. } => *code_len,
        }
    }

    #[inline]
    pub fn byte_at(&mut self, pos: usize) -> u8 {
        match self {
            VmCode::None => 0,
            VmCode::Ram(data) => {
                if pos < data.len() {
                    data[pos]
                } else {
                    0
                }
            }
            VmCode::Flash {
                flash,
                base_addr,
                code_len,
                cache,
                cache_pos,
                cache_len,
            } => {
                if pos >= *code_len {
                    return 0;
                }
                if pos >= *cache_pos && pos < *cache_pos + *cache_len {
                    return cache[pos - *cache_pos];
                }
                // Cache miss — refill
                let avail = code_len.saturating_sub(pos);
                let n = avail.min(FLASH_CACHE_SIZE);
                if n > 0 {
                    flash.read(*base_addr + pos as u32, &mut cache[..n]);
                }
                *cache_pos = pos;
                *cache_len = n;
                if n > 0 { cache[0] } else { 0 }
            }
        }
    }

    #[inline]
    pub fn read_bytes(&mut self, pos: usize, buf: &mut [u8]) -> usize {
        match self {
            VmCode::None => 0,
            VmCode::Ram(data) => {
                if pos >= data.len() {
                    return 0;
                }
                let avail = data.len() - pos;
                let n = buf.len().min(avail);
                buf[..n].copy_from_slice(&data[pos..pos + n]);
                n
            }
            VmCode::Flash {
                flash,
                base_addr,
                code_len,
                cache,
                cache_pos,
                cache_len,
            } => {
                if pos >= *code_len {
                    return 0;
                }
                let avail = *code_len - pos;
                let n = buf.len().min(avail);
                // Check if fully within cache
                if pos >= *cache_pos && pos + n <= *cache_pos + *cache_len {
                    let start = pos - *cache_pos;
                    buf[..n].copy_from_slice(&cache[start..start + n]);
                    return n;
                }
                // Cache miss — refill from pos
                let fill_avail = code_len.saturating_sub(pos);
                let fill_n = fill_avail.min(FLASH_CACHE_SIZE);
                if fill_n > 0 {
                    flash.read(*base_addr + pos as u32, &mut cache[..fill_n]);
                }
                *cache_pos = pos;
                *cache_len = fill_n;
                let from_cache = n.min(fill_n);
                buf[..from_cache].copy_from_slice(&cache[..from_cache]);
                from_cache
            }
        }
    }
}

// === Function metadata (embedded in image header) ===

/// Function kind — determines how the VM dispatches the function.
#[derive(Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum FunctionKind {
    Setup = 0,
    Loop = 1,
    UserFunction = 2,
    OnProgramStart = 3,
    OnPageChanging = 4,
    OnPageChanged = 5,
    OnUserMessage = 6,
    OnTouchDown = 7,
    OnTouchUp = 8,
    OnTouchMove = 9,
    /// Swipe gesture detected on release. Args: direction (0=left 1=right 2=up 3=down), widget_id, dominant_delta.
    OnSwipe = 10,
    /// Long press detected after holding still for 500 ms. Args: widget_id, x, y.
    OnLongPress = 11,
    /// Compile-time generated click dispatcher. Args: widget_id (plus optional extra args for sliders).
    OnClick = 12,
    /// Compile-time generated tap/on_change dispatcher. Args: widget_id, packed_xy (0 for on_change).
    OnTap = 13,
    /// Compile-time generated paint dispatcher. Arg: array_id of dirty widget ids (freed by generated body).
    OnPaint = 14,
}

impl FunctionKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Setup),
            1 => Some(Self::Loop),
            2 => Some(Self::UserFunction),
            3 => Some(Self::OnProgramStart),
            4 => Some(Self::OnPageChanging),
            5 => Some(Self::OnPageChanged),
            6 => Some(Self::OnUserMessage),
            7 => Some(Self::OnTouchDown),
            8 => Some(Self::OnTouchUp),
            9 => Some(Self::OnTouchMove),
            10 => Some(Self::OnSwipe),
            11 => Some(Self::OnLongPress),
            12 => Some(Self::OnClick),
            13 => Some(Self::OnTap),
            14 => Some(Self::OnPaint),
            _ => None,
        }
    }
}

/// Render mode — determines how the main loop renders widgets.
#[derive(Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum RenderMode {
    /// Partial update: only dirty widgets redrawn (default).
    Dirty = 0,
    /// Double-buffered full redraw every frame (flicker-free).
    Buffered = 1,
    /// E-paper partial update: renders dirty rects into a framebuffer, then flushes to EPD.
    EPaper = 2,
}

/// Function entry from the image header.
#[derive(Clone, Copy)]
pub struct FuncEntry {
    pub func_id: u16,
    pub kind: FunctionKind,
    pub offset: u32,
    pub length: u32,
}

/// Image header size v1/v2: version(1) + function_count(2) + reserved(2) = 5 bytes.
/// Image header size v3: + flags(2) = 7 bytes.
/// Image header size v4: + widget_count(1) + ext_count(1) = 9 bytes.
const IMAGE_HEADER_SIZE_V2: usize = 5;
const IMAGE_HEADER_SIZE_V3: usize = 7;
const IMAGE_HEADER_SIZE_V4: usize = 9;
/// Each function entry: func_id(2) + kind(1) + pad(1) + offset(4) + length(4) = 12 bytes.
const FUNC_ENTRY_SIZE: usize = 12;
/// Image header flags bit 2: JMP/JZ/JNZ/CALL targets are u32 LE instead of u16.
/// Set by the compiler when the bytecode exceeds 64 KB. (Bit 1 = e-paper mode.)
const IMAGE_FLAG_WIDE_ADDR: u16 = 0x04;
/// Image header flags bit 3: portrait — the screen is rotated 90° (logical
/// 480x800 on the 800x480 panel, see `LcdImpl::set_portrait`).
const IMAGE_FLAG_PORTRAIT: u16 = 0x08;

/// Bytecode address: pc, return addresses, callback entry points. `u32` with
/// the `wide_code` feature (RAM-rich BSPs), `u16` otherwise so the GD32 build
/// keeps call frames and queued callbacks small.
#[cfg(feature = "wide_code")]
pub type CodeAddr = u32;
#[cfg(not(feature = "wide_code"))]
pub type CodeAddr = u16;

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
const OP_ARR_FREE: u8 = 0x1B;
const OP_W_ALLTAR: u8 = 0x1C; // alloc + target + store (combined)
const OP_FRAME: u8 = 0x1D; // + u8 local_count (function prologue)
const OP_SHL: u8 = 0x1E; // a b → a << (b & 31), logical left shift
const OP_SHR: u8 = 0x1F; // a b → a >> (b & 31), arithmetic right shift (sign-preserving)

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
const OP_PUSH_I8: u8 = 0x30; // + i8
const OP_PUSH_I16: u8 = 0x31; // + i16 LE
const OP_PUSH_I32: u8 = 0x32; // + i32 LE
const OP_LOAD: u8 = 0x33; // + u8 slot
const OP_STORE: u8 = 0x34; // + u8 slot
const OP_JMP: u8 = 0x35; // + u16 target (u32 in wide-address images)
const OP_JZ: u8 = 0x36; // + u16 target (u32 in wide-address images)
const OP_JNZ: u8 = 0x37; // + u16 target (u32 in wide-address images)
const OP_CALL: u8 = 0x38; // + u16 target (u32 in wide-address images)
const OP_W_TARGET: u8 = 0x39; // + u8 widget_id
const OP_W_SET: u8 = 0x3A; // + u8 prop_id (value from stack)
const OP_W_GET: u8 = 0x3B; // + u8 prop_id
const OP_W_PARENT: u8 = 0x3C; // + u8 parent_id
const OP_W_SET_LEN: u8 = 0x3D; // + u8 prop_id + u8 len + data
const OP_ARR_ALLOC: u8 = 0x3E; // + u8 size
const OP_ARR_INIT: u8 = 0x3F; // + u8 count + i32 values LE
const OP_F_READ: u8 = 0x40; // + u32 addr + u16 len
const OP_F_WRITE: u8 = 0x41; // + u32 addr + u8 len + data
const OP_W_TARGET_S: u8 = 0x42; // widget id from stack (dynamic target)
const OP_W_PARENT_S: u8 = 0x43; // parent id from stack (dynamic parent)
const OP_W_LOAD: u8 = 0x44; // + u16 block offset in the widget page (RES_PAGE)

// Widget page (RES_PAGE) ops: static widget setup the compiler moved out of
// the bytecode. 0x01..=0x3F = W_SET <tag> + zigzag varint, 0x40..=0x7F =
// W_SET_LEN <tag> + u8 len + payload, the rest below + operands.
// Keep in sync with tools/ferrite_lang.py (PAGE_*).
// Target/parent ops: bit 0 = operand is a var slot holding the widget id
// (else the id itself), bit 1 = parent (else target).
const PG_STORE: u8 = 0x80; // + u8 slot + zigzag varint
const PG_ALLTAR: u8 = 0x81; // + u8 slot
const PG_TARGET_ID: u8 = 0x84; // + u8 widget id
const PG_TARGET: u8 = 0x85; // + u8 slot
const PG_PARENT_ID: u8 = 0x86; // + u8 widget id
const PG_PARENT: u8 = 0x87; // + u8 slot

// Builtins as first-class opcodes (args on stack)
const OP_FILL_RECT: u8 = 0x80;
const OP_RECT: u8 = 0x81;
const OP_LINE: u8 = 0x82;
const OP_CIRCLE: u8 = 0x83;
const OP_FILL_CIRCLE: u8 = 0x84;
const OP_DRAW_IMAGE: u8 = 0x85;
const OP_DRAW_TEXT_LIT: u8 = 0x86; // + u8 len + text
const OP_DELAY: u8 = 0x87;
const OP_STR_LIT: u8 = 0x88; // + u8 len + text
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
const OP_RTC_READ: u8 = 0x9A;
const OP_RTC_WRITE: u8 = 0x9B;
const OP_MILLIS: u8 = 0x9C;
const OP_FPGA_CMD: u8 = 0x9D; // pop cmd, pop data → send_command(cmd), send_data(data)
const OP_FPGA_DAT: u8 = 0x9E; // pop data → send_data(data)
const OP_CRITICAL: u8 = 0x9F; // enter critical section — run until next yield
const OP_SET_BRIGHTNESS: u8 = 0xA0; // pop percent → set backlight
const OP_BRIGHTNESS: u8 = 0xA1; // push current backlight percent
const OP_STR_CMP: u8 = 0xAC; // pop str_id, pop str_id → push -1/0/1
const OP_FILE_OPEN: u8 = 0xA2; // pop str_id (name) → push handle (1|2|0xFF)
const OP_FILE_READ: u8 = 0xA3; // pop handle → push byte (0..255) or -1 on EOF
const OP_FILE_SIZE: u8 = 0xA4; // pop handle → push size
const OP_FILE_CLOSE: u8 = 0xA5; // pop handle, release slot
const OP_FILE_SEEK: u8 = 0xAA; // pop handle, pop pos → seek to byte position (clamped to [0, size])
const OP_SYSCALL: u8 = 0xAB; // + u8 syscall_id + u8 argc — pop argc args → call handler, push i32 result
const OP_ARR_TO_STR: u8 = 0xA6; // pop len, pop arr_id → alloc string from low bytes of arr elements (len<0 = full array)
const OP_SHOW_MODAL: u8 = 0xA7; // pop click_fn, pop builder_fn → suspend until setDialogResult, push result on resume
const OP_SET_DIALOG_RESULT: u8 = 0xA8; // pop result → record on top modal frame; void return
const OP_SPRINTF: u8 = 0xA9; // + u8 argc — pop argc args + fmt str_id → push result str_id
const OP_ANIMATE: u8 = 0xAD; // pop on_end, ctrl, duration, to, prop, widget → start tween
const OP_STOP_ANIM: u8 = 0xAE; // pop widget → cancel all tweens on the widget
const OP_STOP_ANIM_PROP: u8 = 0xAF; // pop prop, pop widget → cancel that one tween
const OP_PIN_MODE: u8 = 0xB0; // pop mode, pop pin → configure user GPIO pin
const OP_PIN_WRITE: u8 = 0xB1; // pop value, pop pin → drive output pin
const OP_PIN_READ: u8 = 0xB2; // pop pin → push 0/1

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

// Float math (trig / sqrt / rounding) — all no-arg, operate on f32 stack values
const OP_FSIN: u8 = 0xCD;
const OP_FCOS: u8 = 0xCE;
const OP_FSQRT: u8 = 0xCF;
const OP_FABS: u8 = 0xD0;
const OP_FATAN2: u8 = 0xD1; // pop x, pop y → push atan2(y,x)
const OP_FFLOOR: u8 = 0xD2;
const OP_FCEIL: u8 = 0xD3;

// --- VM fault codes ---

/// Why the VM entered `VmState::Error`. Read with `Vm::fault()`; the matching
/// bytecode address is `Vm::fault_pc()` (start of the faulting instruction).
pub mod fault {
    pub const NONE: u8 = 0;
    /// Bytecode read past the end of the program (truncated operand / bad jump).
    pub const CODE_OOB: u8 = 1;
    /// Unknown opcode.
    pub const BAD_OPCODE: u8 = 2;
    /// Eval stack overflow (more values than the configured eval stack holds).
    pub const STACK_OVERFLOW: u8 = 3;
    /// Eval stack underflow (pop from empty stack).
    pub const STACK_UNDERFLOW: u8 = 4;
    /// Call stack overflow (call depth past the configured call stack, e.g.
    /// runaway recursion).
    pub const CALL_OVERFLOW: u8 = 5;
    /// RET with an empty call stack outside a callback.
    pub const RET_EMPTY: u8 = 6;
    /// Widget pool exhausted (W_ALLOC / W_ALLTAR).
    pub const WIDGET_ALLOC: u8 = 7;
    /// String pool full.
    pub const STR_ALLOC: u8 = 8;
    /// Unknown array id (never allocated or already freed).
    pub const BAD_ARRAY: u8 = 9;
    /// Array index out of range.
    pub const ARRAY_INDEX: u8 = 10;
    /// Invalid or unopened file handle.
    pub const BAD_FILE: u8 = 11;
    /// Syscall failed or no syscall handler installed.
    pub const SYSCALL: u8 = 12;
    /// Invalid operand value (array too short / too long for the operation).
    pub const BAD_OPERAND: u8 = 13;
    /// GPIO pin the board doesn't have, or a mode it doesn't support.
    pub const GPIO: u8 = 14;
    /// Widget page (RES_PAGE) missing, truncated or malformed (OP_W_LOAD).
    pub const PAGE: u8 = 15;

    /// Short human-readable name for the error screen.
    pub fn name(code: u8) -> &'static [u8] {
        match code {
            CODE_OOB => b"code out of bounds",
            BAD_OPCODE => b"bad opcode",
            STACK_OVERFLOW => b"stack overflow",
            STACK_UNDERFLOW => b"stack underflow",
            CALL_OVERFLOW => b"call stack overflow",
            RET_EMPTY => b"ret without call",
            WIDGET_ALLOC => b"widget pool full",
            STR_ALLOC => b"string pool full",
            BAD_ARRAY => b"invalid array",
            ARRAY_INDEX => b"array index range",
            BAD_FILE => b"invalid file handle",
            SYSCALL => b"syscall failed",
            BAD_OPERAND => b"bad operand",
            GPIO => b"invalid gpio pin/mode",
            PAGE => b"bad widget page",
            _ => b"unknown fault",
        }
    }
}

// --- VM State ---

#[derive(Clone, Copy, PartialEq)]
pub enum VmState {
    Ready,
    Running,
    Halted,
    Yielded,
    Waiting,
    /// Suspended after `showModal`; the user program is paused until
    /// `setDialogResult` fires (typically from a callback) and the main loop
    /// calls `try_resume_modal`.
    AwaitingModal,
    Error,
}

// --- VM ---

/// Default evaluation stack depth (overridable via RES_SYSCFG `eval_stack`).
const STACK_SIZE: usize = 32;
const MAX_SYSCALL_ARGS: usize = 16;
/// Variable entry — sparse mapping from slot ID to value.
struct VmVar {
    id: u16,
    val: i32,
}
/// Default call stack depth (overridable via RES_SYSCFG `call_stack`).
const CALL_STACK_SIZE: usize = 16;

/// Internal heap-allocated VM array.
struct VmArray {
    id: u16,
    data: Vec<i32>,
}

/// Call stack frame: return address + saved frame state.
#[derive(Clone, Copy)]
struct CallFrame {
    ret_addr: CodeAddr,
    frame_base: u8,
    frame_size: u8,
}

/// What a callback interrupted — the VM's "exception frame". Only registers
/// are saved: the interrupted code's eval-stack values and call frames stay
/// in place, and the callback runs above them (`sp_floor`, and `call_sp` as
/// the depth at which its OP_RET returns).
#[derive(Clone, Copy)]
struct CbFrame {
    pc: CodeAddr,
    sp: u8,
    sp_floor: u8,
    call_sp: u8,
    target: WidgetId,
    state: VmState,
    wait_until: u32,
    critical: bool,
    frame_base: u8,
    frame_size: u8,
}

/// Maximum callback nesting: a callback suspended on each open modal, the
/// dialog-button callback on top of them, and an on_paint pass.
const CB_NEST_MAX: usize = MODAL_STACK_MAX + 2;

/// Maximum number of arguments per queued callback.
const CB_MAX_ARGS: usize = 3;
/// Default callback queue length (overridable via RES_SYSCFG `cb_queue`).
const CB_QUEUE_SIZE: usize = 8;

/// A queued callback request.
#[derive(Clone, Copy)]
struct CbRequest {
    offset: CodeAddr,
    arg_count: u8,
    args: [i32; CB_MAX_ARGS],
}

/// Open file slot — absolute flash offset, total size, current read position.
/// Max 2 files open concurrently (handles 1 and 2; 0xFF means error).
#[derive(Clone, Copy)]
struct OpenFile {
    offset: u32,
    size: u32,
    pos: u32,
}

/// Returned by fileOpen on error. fileRead/Size/Close with this handle → VM Error.
const FILE_HANDLE_ERR: i32 = 0xFF;
/// EOF marker returned by fileRead when past end-of-file.
const FILE_EOF: i32 = -1;

// --- Modal dialog state ---

/// Standard dialog result codes (mirrored in lib/modal.fl).
/// Programs can pass arbitrary i32 values to setDialogResult — these are the
/// conventional set used by Cancel auto-close and by lib/modal.fl handlers.
pub const DLG_OK: i32 = 1;
pub const DLG_CANCEL: i32 = 2;

/// Maximum nested modals. A second `showModal` from inside a callback while
/// a modal is open pushes another frame; this caps that depth.
const MODAL_STACK_MAX: usize = 4;

/// Phase of an open modal frame.
#[derive(Clone, Copy, PartialEq)]
enum ModalState {
    /// `showModal` issued the call to the builder; we're waiting for the
    /// builder to return its overlay widget id via OP_RET.
    AwaitingBuilderReturn,
    /// Builder returned. Overlay is wired and visible. We wait for
    /// `setDialogResult` to set `result`, after which the main loop's
    /// `try_resume_modal` destroys the overlay and resumes the user program.
    AwaitingResult,
}

/// One frame on `Vm::modal_stack`. Each `showModal` pushes a frame; each
/// completed dialog pops one.
#[derive(Clone, Copy)]
struct ModalFrame {
    /// Widget id returned by the builder, or NONE while
    /// `state == AwaitingBuilderReturn`.
    overlay_id: WidgetId,
    /// Function id (from `@func_name`) wired into overlay.on_click. The
    /// compiler injects `@modalDefaultOverlayClick` from lib/modal.fl when the
    /// caller omits the second showModal argument.
    overlay_click_fn: u16,
    /// Result value latched by `setDialogResult`. Only valid when
    /// `has_result == true`.
    result: i32,
    /// Set true the first time `setDialogResult` fires for this frame.
    /// Subsequent calls are ignored (first-write-wins).
    has_result: bool,
    state: ModalState,
    /// `call_sp` at the moment we issued the call to the builder. When the
    /// builder finally returns (its OP_RET drops `call_sp` below this value),
    /// we capture its return value as `overlay_id`.
    builder_call_depth: u8,
}

// --- Animation engine ---

/// Easing curve selector — low 4 bits of `Anim::ctrl`. Mirrored in lib/anim.fl.
const EASE_CURVE_MASK: u8 = 0x0F;
const EASE_IN: u8 = 1;
const EASE_OUT: u8 = 2;
const EASE_IN_OUT: u8 = 3;
/// Repeat-mode flags — high bits of `Anim::ctrl`. Mirrored in lib/anim.fl.
const ANIM_LOOP: u8 = 0x10;
const ANIM_YOYO: u8 = 0x20;

/// One active property tween (~24 B). `Copy` so the tick loop can snapshot a
/// slot by value, release the borrow on `Vm::anims`, then call the
/// mutably-borrowing property setter without aliasing.
#[derive(Clone, Copy)]
struct Anim {
    target: WidgetId,
    prop_id: u8,
    /// Low 4 bits = easing curve; `ANIM_LOOP` / `ANIM_YOYO` in the high bits.
    ctrl: u8,
    from: i32,
    to: i32,
    start_ms: u32,
    duration_ms: u16,
    /// Completion callback func_id (1-based, as pushed by `@fn`); 0 = none.
    /// Resolved to a bytecode offset when fired. Only fires for non-repeating
    /// tweens.
    on_end: u16,
    active: bool,
}

pub struct Vm {
    pc: CodeAddr,
    // Eval / call stacks and the callback queue are heap slices sized by
    // `configure` (RES_SYSCFG); each is at most 255 deep (u8 indices).
    stack: Box<[i32]>,
    sp: u8,
    vars: Vec<VmVar>,
    call_stack: Box<[CallFrame]>,
    call_sp: u8,
    target: WidgetId,
    pub state: VmState,
    /// Fault code (`fault::*`) of the most recent error; valid while `state == Error`.
    fault: u8,
    /// Address of the instruction that raised `fault`.
    fault_pc: CodeAddr,
    /// Start address of the instruction currently executing.
    op_pc: CodeAddr,
    arrays: Vec<VmArray>,
    next_arr_id: u16,
    pub wait_until: u32,
    // Code source — owned by VM
    code: VmCode,
    // Function table from image header
    functions: Vec<FuncEntry>,
    // Active callbacks, innermost last (empty = the main program is running).
    // A callback that suspends (delay, yield, showModal) keeps its frame
    // here and continues on a later tick; while it waits on a modal, the
    // dialog's callbacks run on top of it.
    cb_frames: Vec<CbFrame>,
    // Lowest eval-stack slot the running code may pop: a callback cannot
    // reach the values of the code it interrupted.
    sp_floor: u8,
    // Callback queue — FIFO ring buffer
    cb_queue: Box<[CbRequest]>,
    cb_head: u8,
    cb_tail: u8,
    // Critical section — when true, VM runs until next YIELD without returning to main loop
    critical: bool,
    // Stack-based locals: frame_base = first local slot for current function
    frame_base: u8,
    frame_size: u8,
    global_count: u8,
    // Jump/call operands are u32 (image header IMAGE_FLAG_WIDE_ADDR).
    wide_addr: bool,
    // Render mode from image header
    pub render_mode: RenderMode,
    // Platform policy override: when set, replaces the header's render mode after
    // load (e-paper panels force `EPaper`). `None` honors the image header.
    pub forced_render_mode: Option<RenderMode>,
    // Portrait screen from the image header (IMAGE_FLAG_PORTRAIT).
    pub portrait: bool,
    // `(widgets, extensions)` the program allocates, from a v4 image header
    // (exact — 0 means none). `None` = older image without counts.
    pub tree_hint: Option<(u8, u8)>,
    // Open file slots — index 0 = handle 1, index 1 = handle 2.
    open_files: [Option<OpenFile>; 2],
    // Open modal dialogs (innermost = last). Empty when no dialog is showing.
    modal_stack: Vec<ModalFrame>,
    // Active property tweens, capped at `anim_slots` (0 = P::ANIM_SLOTS) on
    // insert. Ticked once per main-loop iteration by tick_animations,
    // independent of VM state.
    anims: Vec<Anim>,
    anim_slots: u8,
    // Device-specific syscall handler. Called by OP_SYSCALL; returns None → VM Error.
    pub syscall_fn: Option<fn(id: u8, args: &[i32], strpool: &StringPool) -> Option<i32>>,
    // True while `run_paint` executes on_paint: render() is then a no-op (the
    // frame being painted is presented by run_paint's caller).
    in_paint: bool,
    // The program's widget page (RES_PAGE) in flash; page_len 0 = none.
    page_base: u32,
    page_len: u32,
}

const EMPTY_CB: CbRequest = CbRequest {
    offset: 0,
    arg_count: 0,
    args: [0; CB_MAX_ARGS],
};
const EMPTY_FRAME: CallFrame = CallFrame {
    ret_addr: 0,
    frame_base: 0,
    frame_size: 0,
};

/// Top two eval-stack slots as laid out in memory (`a` pushed first, so lower).
/// Two `i32`s come back from a call in r0:r1, not through a stack slot.
#[repr(C)]
#[derive(Clone, Copy)]
struct StackPair {
    a: i32,
    b: i32,
}

impl Vm {
    pub fn new() -> Self {
        Self {
            pc: 0,
            stack: alloc::vec![0; STACK_SIZE].into_boxed_slice(),
            sp: 0,
            vars: Vec::new(),
            call_stack: alloc::vec![EMPTY_FRAME; CALL_STACK_SIZE].into_boxed_slice(),
            call_sp: 0,
            target: WidgetId::NONE,
            state: VmState::Ready,
            fault: fault::NONE,
            fault_pc: 0,
            op_pc: 0,
            arrays: Vec::new(),
            next_arr_id: 0,
            wait_until: 0,
            code: VmCode::None,
            functions: Vec::new(),
            cb_frames: Vec::new(),
            sp_floor: 0,
            cb_queue: alloc::vec![EMPTY_CB; CB_QUEUE_SIZE].into_boxed_slice(),
            cb_head: 0,
            cb_tail: 0,
            critical: false,
            frame_base: 0,
            frame_size: 0,
            global_count: 0,
            wide_addr: false,
            render_mode: RenderMode::Dirty,
            portrait: false,
            forced_render_mode: None,
            in_paint: false,
            page_base: 0,
            page_len: 0,
            tree_hint: None,
            open_files: [None, None],
            modal_stack: Vec::new(),
            anims: Vec::new(),
            anim_slots: 0,
            syscall_fn: None,
        }
    }

    /// Resize the eval stack, call stack and callback queue and set the
    /// animation slot cap (0 in any argument = built-in default). Depths are
    /// clamped to what the u8 indices address (eval >= 4, queue >= 2). Meant
    /// for boot, before any code runs — resets the VM. Returns false if the
    /// heap cannot hold a requested size (that pool keeps its old size).
    pub fn configure(&mut self, eval: usize, call: usize, cb_queue: usize, anim_slots: usize) -> bool {
        let depth = |v: usize, def: usize, min: usize| if v == 0 { def } else { v.clamp(min, 255) };
        self.reset();
        let mut ok = true;
        match try_alloc_slice(depth(eval, STACK_SIZE, 4), 0) {
            Some(b) => self.stack = b,
            None => ok = false,
        }
        match try_alloc_slice(depth(call, CALL_STACK_SIZE, 1), EMPTY_FRAME) {
            Some(b) => self.call_stack = b,
            None => ok = false,
        }
        match try_alloc_slice(depth(cb_queue, CB_QUEUE_SIZE, 2), EMPTY_CB) {
            Some(b) => self.cb_queue = b,
            None => ok = false,
        }
        self.anim_slots = anim_slots.min(255) as u8;
        self.anims = Vec::new();
        if self.anim_slots > 0 && self.anims.try_reserve_exact(self.anim_slots as usize).is_err() {
            ok = false;
        }
        ok
    }

    /// Current `(eval_stack, call_stack, cb_queue)` depths.
    pub fn limits(&self) -> (usize, usize, usize) {
        (self.stack.len(), self.call_stack.len(), self.cb_queue.len())
    }

    /// Diagnostics: `(used, capacity, heap bytes)` of the eval stack, call
    /// stack, callback queue and animation pool, in that order. Stack depths
    /// are current values (≈0 between main-loop iterations).
    pub fn mem_stats<P: Platform>(&self) -> [(usize, usize, usize); 4] {
        use core::mem::size_of;
        let qlen = self.cb_queue.len();
        let queued = if qlen == 0 {
            0
        } else {
            (self.cb_tail as usize + qlen - self.cb_head as usize) % qlen
        };
        [
            (self.sp as usize, self.stack.len(), self.stack.len() * size_of::<i32>()),
            (self.call_sp as usize, self.call_stack.len(), self.call_stack.len() * size_of::<CallFrame>()),
            (queued, qlen, qlen * size_of::<CbRequest>()),
            (self.anims.len(), self.anim_cap::<P>(), self.anims.capacity() * size_of::<Anim>()),
        ]
    }

    // === Variable access (sparse map) ===

    /// Resolve slot byte: bit 7 set = local (frame_base + index), else global.
    #[inline]
    fn resolve_slot(&self, slot: u8) -> u16 {
        if slot & 0x80 != 0 {
            self.frame_base as u16 + (slot & 0x7F) as u16
        } else {
            slot as u16
        }
    }

    fn var_get(&self, slot: u16) -> i32 {
        for v in &self.vars {
            if v.id == slot {
                return v.val;
            }
        }
        0
    }

    #[inline]
    fn var_set(&mut self, slot: u16, val: i32) {
        for v in &mut self.vars {
            if v.id == slot {
                v.val = val;
                return;
            }
        }
        self.vars.push(VmVar { id: slot, val });
    }

    // === Code loading ===

    /// Load a VM image from RAM bytes. Parses the image header and keeps
    /// opcodes in a Vec<u8>. Returns false if the header is invalid.
    pub fn load_ram(&mut self, image: &[u8]) -> bool {
        if let Some((funcs, opcode_start, gc, rm, hint, wide, portrait)) = parse_image_header(image) {
            if !code_addressable(wide, image.len() - opcode_start) {
                return false;
            }
            let opcodes = image[opcode_start..].to_vec();
            self.wide_addr = wide;
            self.code = VmCode::Ram(opcodes);
            self.functions = funcs;
            self.global_count = gc;
            self.frame_base = gc;
            self.frame_size = 0;
            self.render_mode = self.forced_render_mode.unwrap_or(rm);
            self.portrait = portrait;
            self.tree_hint = hint;
            true
        } else {
            false
        }
    }

    /// Load a VM image from flash. Reads only the header into RAM;
    /// opcodes are read on demand from flash via SPI.
    pub fn load_flash<F: FlashBackend + 'static>(
        &mut self,
        flash: FlashImpl<F>,
        base: u32,
        total_len: usize,
    ) -> bool {
        // Fixed header first, then exactly the header + function table it
        // announces, on the heap: the table grows with the program, so a
        // fixed stack buffer would cap the function count.
        let mut fixed = [0u8; IMAGE_HEADER_SIZE_V4];
        let n = total_len.min(IMAGE_HEADER_SIZE_V4);
        flash.read(base, &mut fixed[..n]);
        let Some(hdr_len) = image_header_len(&fixed[..n]) else {
            return false;
        };
        if hdr_len > total_len {
            return false;
        }
        let mut hdr_buf = Vec::new();
        if hdr_buf.try_reserve_exact(hdr_len).is_err() {
            return false;
        }
        hdr_buf.resize(hdr_len, 0);
        flash.read(base, &mut hdr_buf);

        if let Some((funcs, opcode_start, gc, rm, hint, wide, portrait)) = parse_image_header(&hdr_buf) {
            let opcode_base = base + opcode_start as u32;
            let opcode_len = total_len.saturating_sub(opcode_start);
            if !code_addressable(wide, opcode_len) {
                return false;
            }
            self.wide_addr = wide;
            // Move the caller's flash handle into the code source (no singleton).
            self.code = VmCode::Flash {
                flash: alloc::boxed::Box::new(flash.into_backend()),
                base_addr: opcode_base,
                code_len: opcode_len,
                cache: [0u8; FLASH_CACHE_SIZE],
                cache_pos: 0,
                cache_len: 0,
            };
            self.functions = funcs;
            self.global_count = gc;
            self.frame_base = gc;
            self.frame_size = 0;
            self.render_mode = self.forced_render_mode.unwrap_or(rm);
            self.portrait = portrait;
            self.tree_hint = hint;
            true
        } else {
            false
        }
    }

    /// Load raw bytecode (no image header). Used for page bytecode.
    pub fn load_raw(&mut self, data: &[u8]) {
        self.code = VmCode::Ram(data.to_vec());
        self.tree_hint = None;
        self.wide_addr = false;
        self.functions.clear();
    }

    // === Function table lookup ===

    /// Find a function by kind. Returns the first match.
    pub fn find_by_kind(&self, kind: FunctionKind) -> Option<&FuncEntry> {
        self.functions.iter().find(|f| f.kind as u8 == kind as u8)
    }

    /// Find a function by func_id (for widget on_click/on_tap/on_paint callbacks).
    pub fn find_func(&self, func_id: u16) -> Option<&FuncEntry> {
        self.functions.iter().find(|f| f.func_id == func_id)
    }

    // === Callback queue ===

    /// Enqueue a callback to be executed later via drain_callbacks().
    /// Dropped silently if the queue is full.
    pub fn enqueue_callback(&mut self, offset: CodeAddr, args: &[i32]) {
        let next_tail = (self.cb_tail + 1) % self.cb_queue.len() as u8;
        if next_tail == self.cb_head {
            return; // queue full — drop
        }
        let mut req = CbRequest {
            offset,
            arg_count: args.len().min(CB_MAX_ARGS) as u8,
            args: [0; CB_MAX_ARGS],
        };
        for i in 0..req.arg_count as usize {
            req.args[i] = args[i];
        }
        self.cb_queue[self.cb_tail as usize] = req;
        self.cb_tail = next_tail;
    }

    /// Returns true if there are pending callbacks in the queue.
    pub fn has_pending_callbacks(&self) -> bool {
        self.cb_head != self.cb_tail
    }

    /// True if a queued callback may start now. The main program can be
    /// interrupted anywhere outside a critical section. A suspended callback
    /// (delay, yield) holds the queue until it finishes, so handlers stay in
    /// FIFO order — except while it waits on a modal, whose buttons must be
    /// able to run their callbacks on top of it.
    fn can_take_callback(&self) -> bool {
        if self.critical || self.in_paint || self.cb_frames.len() >= CB_NEST_MAX {
            return false;
        }
        match self.state {
            VmState::Error | VmState::Ready => false,
            VmState::AwaitingModal => true,
            _ => self.cb_frames.is_empty(),
        }
    }

    /// Start queued callbacks. Each runs until it returns or suspends; one
    /// that suspends continues from `tick` on a later iteration. Returns
    /// true if any callback ran.
    #[inline(never)]
    pub fn drain_callbacks<P: Platform>(&mut self, ctx: &mut Ctx<P>) -> bool {
        let mut ran = false;
        while self.cb_head != self.cb_tail && self.can_take_callback() {
            let req = self.cb_queue[self.cb_head as usize];
            self.cb_head = (self.cb_head + 1) % self.cb_queue.len() as u8;
            self.enter_callback(req.offset, &req.args[..req.arg_count as usize]);
            self.run_active_callback(ctx);
            ran = true;
        }
        ran
    }

    /// One main-loop slice of execution. A suspended callback whose delay
    /// expired (or that yielded, or whose modal closed) continues until it
    /// returns or suspends again; otherwise the main program runs `budget`
    /// instructions. A critical section always runs to its YIELD.
    #[inline(never)]
    pub fn tick<P: Platform>(&mut self, ctx: &mut Ctx<P>, budget: u32) {
        match self.state {
            VmState::Running => {}
            VmState::Yielded => self.state = VmState::Running,
            VmState::Waiting if self.wait_expired(ctx) => self.state = VmState::Running,
            _ => return,
        }
        if !self.cb_frames.is_empty() {
            self.run_active_callback(ctx);
            return;
        }
        let mut budget = budget.max(1);
        loop {
            self.step(ctx);
            while self.critical && self.state == VmState::Running {
                self.step(ctx);
            }
            budget -= 1;
            if budget == 0 || self.state != VmState::Running {
                break;
            }
        }
    }

    /// Run a function to completion right now, on top of whatever is running:
    /// setup before the main loop starts, on_paint inside a render pass.
    /// Nothing else runs meanwhile, so `delay()` busy-waits and `yield` is a
    /// no-op. A function that opens a modal stays suspended on it; the main
    /// loop resumes it like any callback.
    pub fn call_now<P: Platform>(&mut self, offset: CodeAddr, args: &[i32], ctx: &mut Ctx<P>) {
        if !self.enter_callback(offset, args) {
            return;
        }
        let depth = self.cb_frames.len();
        while self.cb_frames.len() >= depth {
            match self.state {
                VmState::Running => self.step(ctx),
                VmState::Yielded => self.state = VmState::Running,
                VmState::Waiting => {
                    if self.wait_expired(ctx) {
                        self.state = VmState::Running;
                    }
                }
                _ => break, // Error, or suspended on a modal
            }
        }
    }

    /// Point the main program at `offset` and make it runnable. If setup left
    /// a callback suspended on a dialog, the main program starts once that
    /// callback returns.
    pub fn start_main(&mut self, offset: CodeAddr) {
        match self.cb_frames.first_mut() {
            Some(f) => {
                f.pc = offset;
                f.state = VmState::Running;
            }
            None => {
                self.pc = offset;
                self.state = VmState::Running;
            }
        }
    }

    /// Run the on_paint dispatcher now (not queued) for the widgets in
    /// `paint` that are still visible. Called right after the render pass
    /// that drew them, so the custom drawing lands in the same frame — and in
    /// buffered mode the same back buffer, before the swap — as the widget's
    /// background. Queueing it instead let it run after a callback queued
    /// earlier had already hidden the widget (e.g. a tab switch), painting
    /// over whatever replaced it.
    pub fn run_paint<P: Platform>(&mut self, ctx: &mut Ctx<P>, paint: &render::PaintList) {
        if self.in_paint
            || paint.is_empty()
            || self.state == VmState::Error
            || self.cb_frames.len() >= CB_NEST_MAX
        {
            return;
        }
        let offset = match self.find_by_kind(FunctionKind::OnPaint) {
            Some(e) => e.offset as CodeAddr,
            None => return,
        };
        let mut ids = [0i32; render::PAINT_MAX];
        let mut n = 0;
        for &id in paint.ids() {
            if ctx.tree.is_tree_visible(id) {
                ids[n] = id.0 as i32;
                n += 1;
            }
        }
        if n == 0 {
            return;
        }
        let arr_id = self.alloc_array_from_i32s(&ids[..n]);
        self.in_paint = true;
        self.call_now(offset, &[arr_id], ctx);
        self.in_paint = false;
    }

    // === Modal dialog support ===

    /// Implements OP_SHOW_MODAL. Pops `overlay_click_fn` then `builder_fn`
    /// from the eval stack, pushes a modal frame, and issues a CALL to the
    /// builder function. The user program is left dangling at the byte after
    /// OP_SHOW_MODAL — the builder's eventual OP_RET drops us into
    /// `handle_builder_return`, which transitions VM state to AwaitingModal.
    fn handle_show_modal<P: Platform>(&mut self, _ctx: &mut Ctx<P>) {
        let click_fn = self.pop() as u16;
        let builder_fn = self.pop() as u16;

        if self.modal_stack.len() >= MODAL_STACK_MAX {
            // Nested too deep — emulate immediate Cancel.
            self.push(DLG_CANCEL);
            return;
        }

        // Resolve builder function id → entry offset.
        let builder_offset = match self.find_func(builder_fn) {
            Some(f) => f.offset as CodeAddr,
            None => {
                // Bad fn id; emulate Cancel rather than VM error so the user
                // program keeps running.
                self.push(DLG_CANCEL);
                return;
            }
        };

        if (self.call_sp as usize) >= self.call_stack.len() {
            self.fail(fault::CALL_OVERFLOW);
            return;
        }

        // Mirror OP_CALL: push frame, advance frame_base past caller's locals,
        // jump to builder. The builder runs as a normal function call; its
        // OP_RET will eventually pop this frame back into the post-showModal pc.
        self.call_stack[self.call_sp as usize] = CallFrame {
            ret_addr: self.pc,
            frame_base: self.frame_base,
            frame_size: self.frame_size,
        };
        self.call_sp += 1;
        self.frame_base = self.frame_base.wrapping_add(self.frame_size);
        self.frame_size = 0;
        self.pc = builder_offset;

        self.modal_stack.push(ModalFrame {
            overlay_id: WidgetId::NONE,
            overlay_click_fn: click_fn,
            result: 0,
            has_result: false,
            state: ModalState::AwaitingBuilderReturn,
            builder_call_depth: self.call_sp,
        });
    }

    /// Called from OP_RET when the modal builder has just returned. Pops the
    /// overlay widget id from the eval stack, wires it for clicks, and
    /// suspends VM execution until `setDialogResult` fires.
    fn handle_builder_return<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let overlay_raw = self.pop();
        if self.modal_stack.last().is_none() {
            return;
        }

        let overlay = if (0..0xFFFF).contains(&overlay_raw) {
            WidgetId(overlay_raw as u16)
        } else {
            WidgetId::NONE
        };

        if !ctx.tree.is_live(overlay) {
            // Builder returned a bogus id — pop modal, emulate Cancel.
            self.modal_stack.pop();
            self.push(DLG_CANCEL);
            return;
        }

        // Wire overlay click → setDialogResult(CANCEL) by default, or the
        // user-supplied handler (passed as the 2nd showModal arg). The
        // compiler injects @modalDefaultOverlayClick from lib/modal.fl when
        // omitted, so this path always has a real func_id.
        // Mark the overlay as having a click handler so main.rs dispatches it.
        // The actual func_id lives in ModalFrame.overlay_click_fn, not in ext.
        ctx.tree.get_mut(overlay).flags |= FLAG_CLICKABLE | FLAG_HAS_ON_CLICK;
        // Repaint the whole screen on next frame so the overlay's full-screen
        // backdrop covers everything underneath in both buffers.
        ctx.tree.mark_dirty(WidgetId(0));

        let top = self.modal_stack.last_mut().unwrap();
        top.overlay_id = overlay;
        top.state = ModalState::AwaitingResult;

        self.state = VmState::AwaitingModal;
    }

    /// If `widget_id` is the current modal overlay awaiting a result, return
    /// the overlay's click handler func_id from the modal frame. Returns None
    /// for any non-overlay widget so callers can fall through to the global dispatcher.
    pub fn modal_overlay_click_fn(&self, widget_id: WidgetId) -> Option<u16> {
        self.modal_stack.last().and_then(|frame| {
            if frame.state == ModalState::AwaitingResult && frame.overlay_id == widget_id {
                Some(frame.overlay_click_fn)
            } else {
                None
            }
        })
    }

    /// Called by the main loop each tick. If a modal has had its result set
    /// by `setDialogResult`, destroy the overlay subtree, push the result onto
    /// the eval stack as `showModal`'s return value, and make the suspended
    /// code runnable again. A callback that opened the dialog keeps its frame
    /// throughout, so it finishes like any other suspended callback.
    #[inline(never)]
    pub fn try_resume_modal<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        if !matches!(self.state, VmState::AwaitingModal) {
            return;
        }
        let (overlay, result) = match self.modal_stack.last() {
            Some(top) if top.has_result => (top.overlay_id, top.result),
            _ => return,
        };
        self.modal_stack.pop();

        if ctx.tree.is_live(overlay) {
            ctx.tree.destroy_subtree(overlay);
            // Repaint background under the now-gone overlay in both buffers.
            ctx.tree.mark_dirty(WidgetId(0));
        }

        // The code that called showModal — a callback or the main program —
        // continues from the next `tick`.
        self.push(result);
        self.state = VmState::Running;
    }

    /// True if a modal dialog is open (regardless of phase). Useful for the
    /// main loop, e.g. to skip routine input if a dialog has visual focus.
    pub fn modal_active(&self) -> bool {
        !self.modal_stack.is_empty()
    }

    // --- Internal: callback entry/exit ---

    /// Enter the function at `offset` as a callback, on top of whatever is
    /// running — like OP_CALL, but it also saves the registers OP_CALL
    /// doesn't (target, state, wait timer). Its args go on the eval stack
    /// above `sp_floor`. Returns false (nothing entered) at the nesting cap.
    fn enter_callback(&mut self, offset: CodeAddr, args: &[i32]) -> bool {
        if self.cb_frames.len() >= CB_NEST_MAX {
            return false;
        }
        self.cb_frames.push(CbFrame {
            pc: self.pc,
            sp: self.sp,
            sp_floor: self.sp_floor,
            call_sp: self.call_sp,
            target: self.target,
            state: self.state,
            wait_until: self.wait_until,
            critical: self.critical,
            frame_base: self.frame_base,
            frame_size: self.frame_size,
        });
        self.sp_floor = self.sp;
        self.pc = offset;
        self.target = WidgetId::NONE;
        self.state = VmState::Running;
        self.critical = false;
        for &a in args {
            self.push(a);
        }
        // Place the callback's frame above the interrupted function's live
        // locals so its L0.. slots don't alias (and overwrite) them. Matches
        // the OP_CALL convention: new frame_base = frame_base + frame_size.
        // Clamp to at least global_count so the very first callback (setup)
        // doesn't collide with the globals when frame_base is still 0.
        let above = self.frame_base.wrapping_add(self.frame_size);
        self.frame_base = if above > self.global_count {
            above
        } else {
            self.global_count
        };
        self.frame_size = 0;
        true
    }

    /// The active callback finished (OP_RET at its entry depth, or OP_HALT):
    /// drop whatever it left on the eval stack and resume the code it
    /// interrupted, in the state it was in.
    fn leave_callback(&mut self) {
        if let Some(f) = self.cb_frames.pop() {
            self.pc = f.pc;
            self.sp = f.sp;
            self.sp_floor = f.sp_floor;
            self.call_sp = f.call_sp;
            self.target = f.target;
            self.state = f.state;
            self.wait_until = f.wait_until;
            self.critical = f.critical;
            self.frame_base = f.frame_base;
            self.frame_size = f.frame_size;
        }
    }

    /// Run the active callback until it returns or suspends (delay, yield,
    /// showModal), or the VM faults.
    fn run_active_callback<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let depth = self.cb_frames.len();
        while self.state == VmState::Running && self.cb_frames.len() >= depth {
            self.step(ctx);
        }
    }

    fn wait_expired<P: Platform>(&self, ctx: &Ctx<P>) -> bool {
        ctx.systick.millis().wrapping_sub(self.wait_until) < 0x8000_0000
    }

    // === Public API ===

    pub fn set_target(&mut self, id: WidgetId) {
        self.target = id;
    }

    pub fn set_pc(&mut self, pc: CodeAddr) {
        self.pc = pc;
    }

    pub fn pc(&self) -> CodeAddr {
        self.pc
    }

    /// Fault code (`fault::*`) of the most recent VM error.
    pub fn fault(&self) -> u8 {
        self.fault
    }

    /// Address of the instruction that raised the most recent VM error.
    /// Unlike `pc()`, this stays correct after a failed callback restores
    /// the interrupted state.
    pub fn fault_pc(&self) -> CodeAddr {
        self.fault_pc
    }

    /// Enter `VmState::Error` with a reason. The first fault wins: follow-on
    /// failures within the same instruction (e.g. a push after a failed pop)
    /// don't overwrite the root cause.
    #[cold]
    #[inline(never)]
    fn fail(&mut self, code: u8) {
        if self.state != VmState::Error {
            self.fault = code;
            self.fault_pc = self.op_pc;
        }
        self.state = VmState::Error;
    }

    pub fn is_critical(&self) -> bool {
        self.critical
    }

    pub fn pop_result(&mut self) -> i32 {
        if self.sp > self.sp_floor {
            self.sp -= 1;
            self.stack[self.sp as usize]
        } else {
            0
        }
    }

    pub fn push_arg(&mut self, val: i32) {
        if (self.sp as usize) < self.stack.len() {
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
        self.fault = fault::NONE;
        self.vars.clear();
        self.arrays.clear();
        self.next_arr_id = 0;
        self.wait_until = 0;
        self.code = VmCode::None;
        self.functions.clear();
        self.cb_frames.clear();
        self.sp_floor = 0;
        self.frame_base = 0;
        self.frame_size = 0;
        self.global_count = 0;
        self.cb_head = 0;
        self.cb_tail = 0;
        self.critical = false;
        self.render_mode = RenderMode::Dirty;
        self.portrait = false;
        self.open_files = [None, None];
        self.modal_stack.clear();
        self.anims.clear();
        self.page_len = 0;
    }

    /// Attach the program's widget page (RES_PAGE resource at `base` in
    /// flash), read by OP_W_LOAD. Call after loading the program.
    pub fn set_page(&mut self, base: u32, len: u32) {
        self.page_base = base;
        self.page_len = len;
    }

    pub fn has_code(&self) -> bool {
        self.code.len() > 0
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

    /// Allocate a VM array from a native i32 slice. Used by the firmware to
    /// pass batched widget-id lists to generated dispatcher callbacks (e.g. OnPaint).
    pub fn alloc_array_from_i32s(&mut self, values: &[i32]) -> i32 {
        let id = self.next_arr_id;
        self.arrays.push(VmArray {
            id,
            data: values.to_vec(),
        });
        self.next_arr_id = self.next_arr_id.wrapping_add(1);
        id as i32
    }

    pub fn run<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        self.state = VmState::Running;
        loop {
            match self.state {
                VmState::Running => self.step(ctx),
                VmState::Waiting => {
                    // Busy-wait for the delay to expire
                    if self.wait_expired(ctx) {
                        self.state = VmState::Running;
                    }
                }
                _ => break, // Halted, Yielded, Error → exit
            }
        }
    }

    // --- Byte reading helpers (read from self.code) ---

    #[inline(never)]
    fn read_u8(&mut self) -> u8 {
        let pos = self.pc as usize;
        if pos < self.code.len() {
            self.pc += 1;
            self.code.byte_at(pos)
        } else {
            self.fail(fault::CODE_OOB);
            0
        }
    }

    #[inline(always)]
    fn read_i8(&mut self) -> i8 {
        self.read_u8() as i8
    }

    #[inline(never)]
    fn read_u16(&mut self) -> u16 {
        let lo = self.read_u8() as u16;
        lo | (self.read_u8() as u16) << 8
    }

    #[inline(always)]
    fn read_i16(&mut self) -> i16 {
        self.read_u16() as i16
    }

    #[inline(never)]
    fn read_i32(&mut self) -> i32 {
        let lo = self.read_u16() as u32;
        (lo | (self.read_u16() as u32) << 16) as i32
    }

    #[inline(always)]
    fn read_u32(&mut self) -> u32 {
        self.read_i32() as u32
    }

    /// JMP/JZ/JNZ/CALL target: u16, or u32 in wide-address images (only
    /// loadable with the `wide_code` feature, so the cast never truncates;
    /// narrow builds compile the u32 path out).
    #[inline]
    fn read_addr(&mut self) -> CodeAddr {
        if cfg!(feature = "wide_code") && self.wide_addr {
            self.read_u32() as CodeAddr
        } else {
            self.read_u16() as CodeAddr
        }
    }

    // --- Execute a single instruction ---

    pub fn step<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        self.op_pc = self.pc;
        let op = self.read_u8();
        if self.state == VmState::Error {
            return;
        }

        match op {
            // --- No-arg: stack, arithmetic, logic, comparison ---
            // HALT inside a callback ends just that callback.
            OP_HALT if !self.cb_frames.is_empty() => self.leave_callback(),
            OP_HALT => self.state = VmState::Halted,
            OP_POP => {
                self.pop();
            }
            OP_DUP => {
                let v = self.peek();
                self.push(v);
            }
            OP_SWAP => {
                if self.sp - self.sp_floor >= 2 {
                    let i = (self.sp - 1) as usize;
                    let j = (self.sp - 2) as usize;
                    self.stack.swap(i, j);
                } else {
                    self.fail(fault::STACK_UNDERFLOW);
                }
            }
            OP_ADD => {
                let (a, b) = self.pop2();
                self.push(a.wrapping_add(b));
            }
            OP_SUB => {
                let (a, b) = self.pop2();
                self.push(a.wrapping_sub(b));
            }
            OP_MUL => {
                let (a, b) = self.pop2();
                self.push(a.wrapping_mul(b));
            }
            OP_DIV => {
                let (a, b) = self.pop2();
                self.push(if b != 0 { a / b } else { 0 });
            }
            OP_MOD => {
                let (a, b) = self.pop2();
                self.push(if b != 0 { a % b } else { 0 });
            }
            OP_NEG => {
                let a = self.pop();
                self.push(a.wrapping_neg());
            }
            OP_AND => {
                let (a, b) = self.pop2();
                self.push(a & b);
            }
            OP_OR => {
                let (a, b) = self.pop2();
                self.push(a | b);
            }
            OP_SHL => {
                let (a, b) = self.pop2();
                self.push(a << (b as u32 & 31));
            }
            OP_SHR => {
                let (a, b) = self.pop2();
                self.push(a >> (b as u32 & 31));
            }
            OP_NOT => {
                let a = self.pop();
                self.push(if a == 0 { 1 } else { 0 });
            }
            OP_EQ => {
                let (a, b) = self.pop2();
                self.push(if a == b { 1 } else { 0 });
            }
            OP_NE => {
                let (a, b) = self.pop2();
                self.push(if a != b { 1 } else { 0 });
            }
            OP_LT => {
                let (a, b) = self.pop2();
                self.push(if a < b { 1 } else { 0 });
            }
            OP_LE => {
                let (a, b) = self.pop2();
                self.push(if a <= b { 1 } else { 0 });
            }
            OP_GT => {
                let (a, b) = self.pop2();
                self.push(if a > b { 1 } else { 0 });
            }
            OP_GE => {
                let (a, b) = self.pop2();
                self.push(if a >= b { 1 } else { 0 });
            }
            OP_RET => {
                if self.cb_frames.last().is_some_and(|f| f.call_sp == self.call_sp) {
                    // Return from the callback's own entry function.
                    self.leave_callback();
                } else if self.call_sp > 0 {
                    self.call_sp -= 1;
                    let frame = self.call_stack[self.call_sp as usize];
                    self.pc = frame.ret_addr;
                    self.frame_base = frame.frame_base;
                    self.frame_size = frame.frame_size;

                    // Modal builder return interception: if we just popped the
                    // call frame for a modal dialog's builder, capture its
                    // returned overlay id and suspend the VM until
                    // setDialogResult fires.
                    let intercept = match self.modal_stack.last() {
                        Some(top) => {
                            top.state == ModalState::AwaitingBuilderReturn
                                && self.call_sp < top.builder_call_depth
                        }
                        None => false,
                    };
                    if intercept {
                        self.handle_builder_return(ctx);
                    }
                } else {
                    self.fail(fault::RET_EMPTY);
                }
            }
            OP_YIELD => {
                self.critical = false;
                self.state = VmState::Yielded;
            }
            OP_W_DIRTY => {
                if self.target.is_some() {
                    ctx.tree.mark_dirty(self.target);
                }
            }
            OP_W_RENDER => match self.render_mode {
                // render() from inside on_paint: the frame being painted is
                // presented by run_paint's caller — don't nest a frame.
                RenderMode::Buffered | RenderMode::Dirty if self.in_paint => {}
                RenderMode::Buffered => {
                    if render::buffered_has_dirty(ctx) {
                        ctx.lcd.begin_frame();
                        // Reborrow self as immutable while ctx stays mut —
                        // render reads array data via Vm::array_slice.
                        let paint = render::render_buffered_content(ctx, &*self);
                        self.run_paint(ctx, &paint);
                        ctx.lcd.end_frame();
                    }
                }
                RenderMode::Dirty => {
                    let mut paint = render::PaintList::new();
                    render::collect_dirty_paint(ctx, &mut paint);
                    render::render_dirty(ctx, &*self);
                    self.run_paint(ctx, &paint);
                }
                RenderMode::EPaper => {
                    // Ghost-erase: drive all pixels to white so previous dark
                    // content doesn't bleed through the next render. No-op on
                    // framebuffer panels; e-paper backends override the hook.
                    ctx.lcd.pre_full_redraw();
                    // Full redraw — guarantees all rows are tainted for flush.
                    render::render_all(ctx, &*self);
                    ctx.lcd.flush_dirty();
                }
            },
            OP_ARR_LOAD => {
                let (arr_id, idx) = self.pop2();
                self.arr_load(arr_id, idx);
            }
            OP_ARR_STORE => {
                let (idx, val) = self.pop2();
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
                    self.fail(fault::WIDGET_ALLOC);
                }
            }
            OP_W_ALLTAR => {
                let raw = self.read_u8();
                self.w_alltar(ctx, raw);
            }
            OP_FRAME => {
                // Function prologue: set frame_size for this function
                self.frame_size = self.read_u8();
            }
            OP_ARR_FREE => {
                let arr_id = self.pop();
                self.arr_free(arr_id);
            }

            // --- Specialized short forms ---
            OP_PUSH_0 => self.push(0),
            OP_PUSH_1 => self.push(1),
            OP_PUSH_2 => self.push(2),
            OP_PUSH_M1 => self.push(-1),
            // Short forms: LOAD/STORE local slots 0-4
            OP_LOAD_0 => {
                let s = self.frame_base as u16;
                let v = self.var_get(s);
                self.push(v);
            }
            OP_LOAD_1 => {
                let s = self.frame_base as u16 + 1;
                let v = self.var_get(s);
                self.push(v);
            }
            OP_LOAD_2 => {
                let s = self.frame_base as u16 + 2;
                let v = self.var_get(s);
                self.push(v);
            }
            OP_LOAD_3 => {
                let s = self.frame_base as u16 + 3;
                let v = self.var_get(s);
                self.push(v);
            }
            OP_LOAD_4 => {
                let s = self.frame_base as u16 + 4;
                let v = self.var_get(s);
                self.push(v);
            }
            OP_STORE_0 => {
                let s = self.frame_base as u16;
                let v = self.pop();
                self.var_set(s, v);
            }
            OP_STORE_1 => {
                let s = self.frame_base as u16 + 1;
                let v = self.pop();
                self.var_set(s, v);
            }
            OP_STORE_2 => {
                let s = self.frame_base as u16 + 2;
                let v = self.pop();
                self.var_set(s, v);
            }
            OP_STORE_3 => {
                let s = self.frame_base as u16 + 3;
                let v = self.pop();
                self.var_set(s, v);
            }
            OP_STORE_4 => {
                let s = self.frame_base as u16 + 4;
                let v = self.pop();
                self.var_set(s, v);
            }

            // --- With arguments ---
            OP_PUSH_I8 => {
                let val = self.read_i8() as i32;
                self.push(val);
            }
            OP_PUSH_I16 => {
                let val = self.read_i16() as i32;
                self.push(val);
            }
            OP_PUSH_I32 => {
                let val = self.read_i32();
                self.push(val);
            }
            OP_LOAD => {
                let raw = self.read_u8();
                let slot = self.resolve_slot(raw);
                let v = self.var_get(slot);
                self.push(v);
            }
            OP_STORE => {
                let raw = self.read_u8();
                let slot = self.resolve_slot(raw);
                let v = self.pop();
                self.var_set(slot, v);
            }
            OP_JMP => {
                let target = self.read_addr();
                self.pc = target;
            }
            OP_JZ => {
                let target = self.read_addr();
                if self.pop() == 0 {
                    self.pc = target;
                }
            }
            OP_JNZ => {
                let target = self.read_addr();
                if self.pop() != 0 {
                    self.pc = target;
                }
            }
            OP_CALL => {
                let target = self.read_addr();
                if (self.call_sp as usize) < self.call_stack.len() {
                    self.call_stack[self.call_sp as usize] = CallFrame {
                        ret_addr: self.pc,
                        frame_base: self.frame_base,
                        frame_size: self.frame_size,
                    };
                    self.call_sp += 1;
                    // Advance frame_base past caller's locals
                    self.frame_base = self.frame_base.wrapping_add(self.frame_size);
                    self.frame_size = 0; // callee's FRAME will set this
                    self.pc = target;
                } else {
                    self.fail(fault::CALL_OVERFLOW);
                }
            }
            OP_W_TARGET => {
                // Static target with u8-encoded id (0..254). Used for widgets
                // allocated at fixed positions (e.g. root). Dynamically
                // allocated widgets use W_TARGET_S, which takes a u16 id from
                // the stack.
                let wid = self.read_u8();
                self.target = WidgetId(wid as u16);
            }
            OP_W_TARGET_S => {
                let wid = self.pop() as u16;
                self.target = WidgetId(wid);
            }
            OP_W_SET => {
                let prop_id = self.read_u8();
                let val = self.pop();
                self.w_set(ctx, prop_id, val);
            }
            OP_W_GET => {
                let prop_id = self.read_u8();
                if self.target.is_some() {
                    let v = self.get_scalar_prop(ctx, prop_id);
                    self.push(v);
                } else {
                    self.push(0);
                }
            }
            OP_W_PARENT => {
                // Static parent id (u8). Used for parent(0) and other literal
                // widget references that fit in u8.
                let parent = self.read_u8();
                self.w_parent(ctx, parent as u16);
            }
            OP_W_PARENT_S => {
                // Dynamic parent — id from stack. Used when the parent is a
                // widget variable (its slot stores the runtime id from alloc).
                let parent = self.pop() as u16;
                self.w_parent(ctx, parent);
            }
            OP_W_LOAD => {
                let off = self.read_u16();
                self.load_page_block(ctx, off as u32);
            }
            OP_W_SET_LEN => {
                let prop_id = self.read_u8();
                let len = self.read_u8() as usize;
                let start = self.pc as usize;
                let end = start + len;
                if end <= self.code.len() {
                    // Compound values fit the stack buffer; longer payloads
                    // (text literals up to 255 bytes) go through the heap.
                    let mut small = [0u8; 32];
                    let mut big = Vec::new();
                    let buf: Option<&mut [u8]> = if len <= small.len() {
                        Some(&mut small[..len])
                    } else if big.try_reserve_exact(len).is_ok() {
                        big.resize(len, 0);
                        Some(&mut big)
                    } else {
                        None
                    };
                    self.pc = end as CodeAddr;
                    if let Some(buf) = buf {
                        self.code.read_bytes(start, buf);
                        self.w_set_len(ctx, prop_id, buf);
                    } else {
                        self.fail(fault::STR_ALLOC);
                    }
                } else {
                    self.fail(fault::CODE_OOB);
                }
            }
            OP_ARR_ALLOC => {
                let size = self.read_u8() as u16;
                self.arr_alloc(size);
            }
            OP_ARR_INIT => {
                let count = self.read_u8() as usize;
                let start = self.pc as usize;
                let byte_len = count * 4;
                if start + byte_len <= self.code.len() {
                    let mut values: Vec<i32> = Vec::with_capacity(count);
                    let mut tmp = [0u8; 4];
                    for i in 0..count {
                        self.code.read_bytes(start + i * 4, &mut tmp);
                        values.push(i32::from_le_bytes(tmp));
                    }
                    self.pc = (start + byte_len) as CodeAddr;
                    let id = self.next_arr_id;
                    self.arrays.push(VmArray { id, data: values });
                    self.next_arr_id = self.next_arr_id.wrapping_add(1);
                    self.push(id as i32);
                } else {
                    self.fail(fault::CODE_OOB);
                }
            }
            OP_F_READ => {
                let addr = self.read_u32();
                let len = self.read_u16() as usize;
                if len > (self.stack.len() - self.sp as usize) {
                    self.fail(fault::STACK_OVERFLOW);
                    return;
                }
                // Read in fixed chunks: the stack depth is a runtime value.
                let mut buf = [0u8; 16];
                let mut done = 0;
                while done < len {
                    let n = (len - done).min(buf.len());
                    ctx.flash.read(addr + done as u32, &mut buf[..n]);
                    for &b in &buf[..n] {
                        self.push(b as i32);
                    }
                    done += n;
                }
            }
            OP_F_WRITE => {
                let addr = self.read_u32();
                let len = self.read_u8() as usize;
                let start = self.pc as usize;
                if start + len <= self.code.len() {
                    let mut buf = [0u8; 64];
                    let n = len.min(buf.len());
                    self.code.read_bytes(start, &mut buf[..n]);
                    self.pc = (start + len) as CodeAddr;
                    if n > 0 {
                        ctx.flash.write(addr, &buf[..n]);
                    }
                } else {
                    self.fail(fault::CODE_OOB);
                }
            }

            // --- Builtins (args on stack) ---
            OP_FILL_RECT => {
                let (wh, color) = self.pop2();
                let (w, h) = unpack_pair(wh);
                let (x, y) = unpack_pair(self.pop());
                let color = color as u16;
                ctx.lcd.fill_rect(x, y, w, h, color);
            }
            OP_RECT => {
                let (wh, color) = self.pop2();
                let (w, h) = unpack_pair(wh);
                let (x, y) = unpack_pair(self.pop());
                let color = color as u16;
                ctx.lcd.draw_rect(x, y, w, h, color);
            }
            OP_LINE => {
                let (xy1, color) = self.pop2();
                let (x1, y1) = unpack_pair(xy1);
                let color = color as u16;
                let (x0, y0) = unpack_pair(self.pop());
                ctx.lcd
                    .draw_line(x0 as i16, y0 as i16, x1 as i16, y1 as i16, color);
            }
            OP_CIRCLE => {
                let (radius, color) = self.pop2();
                let (radius, color) = (radius as i16, color as u16);
                let (cx, cy) = unpack_pair(self.pop());
                ctx.lcd.draw_circle(cx as i16, cy as i16, radius, color);
            }
            OP_FILL_CIRCLE => {
                let (radius, color) = self.pop2();
                let (radius, color) = (radius as i16, color as u16);
                let (cx, cy) = unpack_pair(self.pop());
                ctx.lcd.fill_circle(cx as i16, cy as i16, radius, color);
            }
            OP_DRAW_IMAGE => {
                let (xy, image_id) = self.pop2();
                let (x, y) = unpack_pair(xy);
                let image_id = image_id as u8;
                if let Some(img) = ctx.images.find(image_id) {
                    img.draw(&ctx.lcd, &ctx.flash, x, y);
                }
            }
            OP_DRAW_TEXT_LIT => {
                // Inline text: u8 len + text bytes, stack: [colors, font_id, loc]
                let len = self.read_u8() as usize;
                let start = self.pc as usize;
                if start + len <= self.code.len() {
                    let mut buf = [0u8; 64];
                    let n = len.min(buf.len());
                    self.code.read_bytes(start, &mut buf[..n]);
                    self.pc = (start + len) as CodeAddr;
                    let (font_id, colors) = self.pop2();
                    let (fg, bg) = unpack_pair(colors);
                    let font_id = font_id as u8;
                    let (x, y) = unpack_pair(self.pop());
                    if let Some(font) = ctx.fonts.resolve(font_id) {
                        let bg_opt = if bg == 0 { None } else { Some(bg) };
                        font.draw_str(
                            &ctx.lcd,
                            &ctx.flash,
                            &buf[..n],
                            x as i16,
                            y as i16,
                            fg,
                            bg_opt,
                        );
                    }
                } else {
                    self.fail(fault::CODE_OOB);
                }
            }
            OP_DELAY => {
                let ms = self.pop() as u32;
                self.wait_until = ctx.systick.millis().wrapping_add(ms);
                self.state = VmState::Waiting;
            }
            OP_STR_LIT => {
                // Inline text: u8 len + text bytes → interned pool string (an
                // identical live literal is reused), push str_id
                let len = self.read_u8() as usize;
                let start = self.pc as usize;
                if start + len <= self.code.len() {
                    let mut buf = [0u8; 64];
                    let n = len.min(buf.len());
                    self.code.read_bytes(start, &mut buf[..n]);
                    self.pc = (start + len) as CodeAddr;
                    match ctx.strpool.intern(&buf[..n]) {
                        Some(id) => self.push(id as i32),
                        None => self.fail(fault::STR_ALLOC),
                    }
                } else {
                    self.fail(fault::CODE_OOB);
                }
            }
            OP_ITOS => {
                let val = self.pop();
                match strpool::itos(&mut ctx.strpool, val) {
                    Some(id) => self.push(id as i32),
                    None => self.fail(fault::STR_ALLOC),
                }
            }
            OP_FTOS => {
                let bits = self.pop() as u32;
                match strpool::ftos(&mut ctx.strpool, bits) {
                    Some(id) => self.push(id as i32),
                    None => self.fail(fault::STR_ALLOC),
                }
            }
            OP_CONCAT => {
                let (a, b) = self.pop2();
                let (a, b) = (a as u16, b as u16);
                match ctx.strpool.concat(a, b) {
                    Some(id) => self.push(id as i32),
                    None => self.fail(fault::STR_ALLOC),
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
            OP_STR_CMP => {
                let (a, b) = self.pop2();
                let (a, b) = (a as u16, b as u16);
                self.push(ctx.strpool.compare(a, b));
            }
            OP_SET_TEXT => {
                let str_id = self.pop() as u16;
                if self.target.is_some() {
                    if let Some(ext) = ctx.tree.ensure_ext(self.target) {
                        ext.text_id = str_id;
                    }
                    // Text change repaints on the next render() — same as the
                    // W_SET text path.
                    ctx.tree.mark_dirty(self.target);
                }
            }
            OP_DRAW_STR => {
                let str_id = self.pop() as u16;
                let (font_id, colors) = self.pop2();
                let (fg, bg) = unpack_pair(colors);
                let font_id = font_id as u8;
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
                let (r, color) = self.pop2();
                let (r, color) = (r as u16, color as u16);
                let (w, h) = unpack_pair(self.pop());
                let (x, y) = unpack_pair(self.pop());
                ctx.lcd.draw_rounded_rect(x, y, w, h, r, color);
            }
            OP_FILL_ROUNDED_RECT => {
                let (r, color) = self.pop2();
                let (r, color) = (r as u16, color as u16);
                let (w, h) = unpack_pair(self.pop());
                let (x, y) = unpack_pair(self.pop());
                ctx.lcd.fill_rounded_rect(x, y, w, h, r, color);
            }
            OP_ARC => {
                let (end, color) = self.pop2();
                let (end, color) = (end as i16, color as u16);
                let start = self.pop() as i16;
                let radius = self.pop() as i16;
                let (cx, cy) = unpack_pair(self.pop());
                ctx.lcd
                    .draw_arc(cx as i16, cy as i16, radius, start, end, color);
            }
            OP_BEGIN_FRAME => {
                // Buffered and EPaper modes manage their own frame lifecycle
                if self.render_mode == RenderMode::Dirty {
                    ctx.lcd.begin_frame();
                }
            }
            OP_END_FRAME => {
                if self.render_mode == RenderMode::Dirty {
                    ctx.lcd.end_frame();
                }
            }
            OP_SEND_USART => {
                let arr_id = self.pop() as u16;
                if let Some(arr) = self.arrays.iter().find(|a| a.id == arr_id) {
                    for &val in &arr.data {
                        ctx.usart.write_byte(val as u8);
                    }
                }
            }
            OP_SEND_USART_STR => {
                let str_id = self.pop() as u16;
                let bytes = ctx.strpool.get(str_id);
                ctx.usart.write(bytes);
            }
            OP_RTC_READ => {
                // Read RTC → allocate array [sec, min, hour, day, weekday, month, year]
                let dt = ctx.rtc.read_time();
                let data = alloc::vec![
                    dt.second as i32,
                    dt.minute as i32,
                    dt.hour as i32,
                    dt.day as i32,
                    dt.weekday as i32,
                    dt.month as i32,
                    dt.year as i32,
                ];
                let id = self.next_arr_id;
                self.arrays.push(VmArray { id, data });
                self.next_arr_id = self.next_arr_id.wrapping_add(1);
                self.push(id as i32);
            }
            OP_RTC_WRITE => {
                // Pop arr_id, read [sec, min, hour, day, weekday, month, year], set RTC
                let arr_id = self.pop();
                match self.find_array(arr_id) {
                    Some(pos) => {
                        let arr = &self.arrays[pos];
                        if arr.data.len() >= 7 {
                            let dt = crate::rtc::DateTime {
                                second: arr.data[0] as u8,
                                minute: arr.data[1] as u8,
                                hour: arr.data[2] as u8,
                                day: arr.data[3] as u8,
                                weekday: arr.data[4] as u8,
                                month: arr.data[5] as u8,
                                year: arr.data[6] as u8,
                            };
                            ctx.rtc.set_time(&dt);
                        } else {
                            self.fail(fault::BAD_OPERAND);
                        }
                    }
                    None => self.fail(fault::BAD_ARRAY),
                }
            }

            OP_MILLIS => {
                self.push(ctx.systick.millis() as i32);
            }
            OP_FPGA_CMD => {
                // fpgaCmd(cmd, data) — raw FPGA command + data
                let (cmd, data) = self.pop2();
                let (cmd, data) = (cmd as u16, data as u16);
                ctx.lcd.send_command(cmd);
                ctx.lcd.send_data(data);
            }
            OP_FPGA_DAT => {
                // fpgaData(data) — raw FPGA data (after a cmd)
                let data = self.pop() as u16;
                ctx.lcd.send_data(data);
            }

            OP_CRITICAL => {
                self.critical = true;
            }
            OP_SET_BRIGHTNESS => {
                let percent = self.pop() as u8;
                ctx.backlight.set_brightness(percent);
            }
            OP_BRIGHTNESS => {
                self.push(ctx.backlight.brightness() as i32);
            }

            // --- User GPIO ---
            OP_PIN_MODE => {
                let (pin, mode) = self.pop2();
                if !ctx.gpio.set_mode(pin, mode) {
                    self.fail(fault::GPIO);
                }
            }
            OP_PIN_WRITE => {
                let (pin, value) = self.pop2();
                if !ctx.gpio.write(pin, value) {
                    self.fail(fault::GPIO);
                }
            }
            OP_PIN_READ => {
                let pin = self.pop();
                match ctx.gpio.read(pin) {
                    Some(level) => self.push(level),
                    None => self.fail(fault::GPIO),
                }
            }

            // --- Animation engine ---
            OP_ANIMATE => {
                // Pushed order: widget, prop, to, duration, ctrl, on_end.
                let (ctrl, on_end) = self.pop2();
                let (ctrl, on_end) = (ctrl as u8, on_end as u16);
                let duration = self.pop() as u16;
                let to = self.pop();
                let (widget, prop) = self.pop2();
                let (widget, prop) = (WidgetId(widget as u16), prop as u8);
                let now = ctx.systick.millis();
                self.start_anim(ctx, widget, prop, to, duration, ctrl, on_end, now);
            }
            OP_STOP_ANIM => {
                let widget = WidgetId(self.pop() as u16);
                self.stop_anim(widget);
            }
            OP_STOP_ANIM_PROP => {
                let (widget, prop) = self.pop2();
                let (widget, prop) = (WidgetId(widget as u16), prop as u8);
                self.stop_anim_prop(widget, prop);
            }

            // --- File ops (flash filesystem, read-only) ---
            OP_FILE_OPEN => {
                let str_id = self.pop() as u16;
                let name = ctx.strpool.get(str_id);
                let result = self.file_open(ctx, name);
                self.push(result);
            }
            OP_FILE_READ => {
                let handle = self.pop();
                match self.file_slot_mut(handle) {
                    Some(f) => {
                        if f.pos >= f.size {
                            self.push(FILE_EOF);
                        } else {
                            let addr = f.offset + f.pos;
                            let mut buf = [0u8; 1];
                            ctx.flash.read(addr, &mut buf);
                            f.pos += 1;
                            self.push(buf[0] as i32);
                        }
                    }
                    None => self.fail(fault::BAD_FILE),
                }
            }
            OP_FILE_SIZE => {
                let handle = self.pop();
                match self.file_slot(handle) {
                    Some(f) => self.push(f.size as i32),
                    None => self.fail(fault::BAD_FILE),
                }
            }
            OP_FILE_CLOSE => {
                let handle = self.pop();
                match self.file_slot_index(handle) {
                    Some(idx) => self.open_files[idx] = None,
                    None => self.fail(fault::BAD_FILE),
                }
            }
            OP_FILE_SEEK => {
                let (handle, pos) = self.pop2();
                match self.file_slot_mut(handle) {
                    Some(f) => f.pos = (pos.max(0) as u32).min(f.size),
                    None => self.fail(fault::BAD_FILE),
                }
            }
            OP_SHOW_MODAL => {
                self.handle_show_modal(ctx);
            }
            OP_SET_DIALOG_RESULT => {
                let result = self.pop();
                if let Some(top) = self.modal_stack.last_mut() {
                    if !top.has_result {
                        top.result = result;
                        top.has_result = true;
                    }
                }
            }
            OP_ARR_TO_STR => {
                let (arr_id, len) = self.pop2();
                match self.find_array(arr_id) {
                    Some(pos) => {
                        let data = &self.arrays[pos].data;
                        let take = if len < 0 {
                            data.len()
                        } else {
                            (len as usize).min(data.len())
                        };
                        let mut buf = [0u8; 256];
                        if take > buf.len() {
                            self.fail(fault::BAD_OPERAND);
                        } else {
                            for i in 0..take {
                                buf[i] = data[i] as u8;
                            }
                            match ctx.strpool.alloc(&buf[..take]) {
                                Some(id) => self.push(id as i32),
                                None => self.fail(fault::STR_ALLOC),
                            }
                        }
                    }
                    None => self.fail(fault::BAD_ARRAY),
                }
            }

            OP_SPRINTF => {
                let argc = self.read_u8() as usize;
                let mut args = [0i32; 8];
                let actual = argc.min(8);
                for i in (0..actual).rev() {
                    args[i] = self.pop();
                }
                let fmt_id = self.pop() as u16;
                let mut fmt_buf = [0u8; 128];
                let fmt_len = {
                    let fmt = ctx.strpool.get(fmt_id);
                    let n = fmt.len().min(128);
                    fmt_buf[..n].copy_from_slice(&fmt[..n]);
                    n
                };
                match strpool::sprintf(&mut ctx.strpool, &fmt_buf[..fmt_len], &args[..actual]) {
                    Some(id) => self.push(id as i32),
                    None => self.fail(fault::STR_ALLOC),
                }
            }

            OP_SYSCALL => {
                let syscall_id = self.read_u8();
                let argc = self.read_u8() as usize;
                let actual = argc.min(MAX_SYSCALL_ARGS);
                let mut args = [0i32; MAX_SYSCALL_ARGS];
                for i in (0..actual).rev() {
                    args[i] = self.pop();
                }
                match self.syscall_fn {
                    Some(f) => match f(syscall_id, &args[..actual], &ctx.strpool) {
                        Some(v) => self.push(v),
                        None => self.fail(fault::SYSCALL),
                    },
                    None => self.fail(fault::SYSCALL),
                }
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
            OP_FADD => {
                let (a, b) = self.pop2_f32();
                self.push((a + b).to_bits() as i32);
            }
            OP_FSUB => {
                let (a, b) = self.pop2_f32();
                self.push((a - b).to_bits() as i32);
            }
            OP_FMUL => {
                let (a, b) = self.pop2_f32();
                self.push((a * b).to_bits() as i32);
            }
            OP_FDIV => {
                let (a, b) = self.pop2_f32();
                self.push(if b != 0.0 { a / b } else { 0.0 }.to_bits() as i32);
            }
            OP_FNEG => {
                let a = f32::from_bits(self.pop() as u32);
                self.push((-a).to_bits() as i32);
            }
            OP_FEQ => {
                let (a, b) = self.pop2_f32();
                self.push(if a == b { 1 } else { 0 });
            }
            OP_FLT => {
                let (a, b) = self.pop2_f32();
                self.push(if a < b { 1 } else { 0 });
            }
            OP_FLE => {
                let (a, b) = self.pop2_f32();
                self.push(if a <= b { 1 } else { 0 });
            }
            OP_FGT => {
                let (a, b) = self.pop2_f32();
                self.push(if a > b { 1 } else { 0 });
            }
            OP_FGE => {
                let (a, b) = self.pop2_f32();
                self.push(if a >= b { 1 } else { 0 });
            }
            OP_FNE => {
                let (a, b) = self.pop2_f32();
                self.push(if a != b { 1 } else { 0 });
            }

            // --- Float math (trig / sqrt / rounding) ---
            OP_FSIN => {
                let v = f32::from_bits(self.pop() as u32);
                self.push(libm::sinf(v).to_bits() as i32);
            }
            OP_FCOS => {
                let v = f32::from_bits(self.pop() as u32);
                self.push(libm::cosf(v).to_bits() as i32);
            }
            OP_FSQRT => {
                let v = f32::from_bits(self.pop() as u32);
                self.push(libm::sqrtf(v).to_bits() as i32);
            }
            OP_FABS => {
                let v = f32::from_bits(self.pop() as u32);
                self.push(libm::fabsf(v).to_bits() as i32);
            }
            OP_FATAN2 => {
                let (y, x) = self.pop2_f32();
                self.push(libm::atan2f(y, x).to_bits() as i32);
            }
            OP_FFLOOR => {
                let v = f32::from_bits(self.pop() as u32);
                self.push(libm::floorf(v).to_bits() as i32);
            }
            OP_FCEIL => {
                let v = f32::from_bits(self.pop() as u32);
                self.push(libm::ceilf(v).to_bits() as i32);
            }

            _ => self.fail(fault::BAD_OPCODE),
        }
    }

    // --- Stack operations ---

    #[inline(never)]
    fn push(&mut self, val: i32) {
        if (self.sp as usize) < self.stack.len() {
            self.stack[self.sp as usize] = val;
            self.sp += 1;
        } else {
            self.fail(fault::STACK_OVERFLOW);
        }
    }

    // Out of line: once unchecked it is small enough for LLVM to inline at
    // some call sites, which costs more flash than the `bl` it replaces.
    #[inline(never)]
    fn pop(&mut self) -> i32 {
        if self.sp > self.sp_floor {
            self.sp -= 1;
            let i = self.sp as usize;
            debug_assert!(i < self.stack.len());
            // SAFETY: sp <= stack.len() invariant (see pop2_raw), so the slot
            // below the old sp is in bounds.
            unsafe { *self.stack.get_unchecked(i) }
        } else {
            self.fail(fault::STACK_UNDERFLOW);
            0
        }
    }

    /// Pops two values with a single underflow check, in push order:
    /// `push a; push b` → `(a, b)`. On underflow the VM faults, `sp` is left
    /// alone and both read as 0.
    #[inline(always)]
    fn pop2(&mut self) -> (i32, i32) {
        let v = self.pop2_raw();
        (v.a, v.b)
    }

    /// `pop2` for two f32 operands (stored as raw bits).
    #[inline(always)]
    fn pop2_f32(&mut self) -> (f32, f32) {
        let (a, b) = self.pop2();
        (f32::from_bits(a as u32), f32::from_bits(b as u32))
    }

    /// `pop2` body, out of line so each call site is just `mov r0, vm; bl`.
    #[inline(never)]
    fn pop2_raw(&mut self) -> StackPair {
        if self.sp as usize >= self.sp_floor as usize + 2 {
            self.sp -= 2;
            let i = self.sp as usize;
            debug_assert!(i + 2 <= self.stack.len());
            // SAFETY: sp <= stack.len() always holds — sp only grows through the
            // bounds-checked push/push_arg, leave_callback restores an older sp,
            // and configure resets sp before swapping the stack. So the two
            // slots below the old sp are in bounds; skipping the slice check
            // saves ~30 B here.
            unsafe { *(self.stack.as_ptr().add(i) as *const StackPair) }
        } else {
            self.fail(fault::STACK_UNDERFLOW);
            StackPair { a: 0, b: 0 }
        }
    }

    fn peek(&self) -> i32 {
        if self.sp > self.sp_floor {
            self.stack[(self.sp - 1) as usize]
        } else {
            0
        }
    }

    // --- Widget ops (shared by the opcodes and the widget page) ---

    /// Alloc a widget, store its id in var `raw` and make it the target.
    #[inline(never)]
    fn w_alltar<P: Platform>(&mut self, ctx: &mut Ctx<P>, raw: u8) {
        let slot = self.resolve_slot(raw);
        if let Some(id) = ctx.tree.alloc() {
            self.var_set(slot, id.0 as i32);
            self.target = id;
        } else {
            self.fail(fault::WIDGET_ALLOC);
        }
    }

    #[inline(never)]
    fn w_parent<P: Platform>(&mut self, ctx: &mut Ctx<P>, parent: u16) {
        if self.target.is_some() {
            ctx.tree.add_child(WidgetId(parent), self.target);
        }
    }

    /// Scalar property write on the target (W_SET).
    #[inline(never)]
    fn w_set<P: Platform>(&mut self, ctx: &mut Ctx<P>, prop_id: u8, val: i32) {
        if self.target.is_none() {
            return;
        }
        self.set_scalar_prop(ctx, prop_id, val);
        if prop_id == PROP_FONT_ID {
            if let Some(fs) = ctx.fs.as_ref() {
                ctx.fonts.find_or_load(val as u8, fs, &ctx.flash, P::CACHE_FONT_BITMAPS);
            }
        }
        if prop_id == PROP_IMAGE_ID {
            if let Some(fs) = ctx.fs.as_ref() {
                ctx.images.find_or_load(val as u8, fs, &ctx.flash);
            }
        }
        // A property write that changes appearance schedules a redraw, so
        // `render()` repaints without an explicit `dirty(widget)` call.
        if prop_is_visual(prop_id) {
            ctx.tree.mark_dirty(self.target);
        }
    }

    /// Compound property write on the target (W_SET_LEN).
    #[inline(never)]
    fn w_set_len<P: Platform>(&mut self, ctx: &mut Ctx<P>, prop_id: u8, data: &[u8]) {
        if self.target.is_some() {
            self.set_compound_prop(prop_id, data, ctx);
            if prop_is_visual(prop_id) {
                ctx.tree.mark_dirty(self.target);
            }
        }
    }

    /// OP_W_LOAD: run one block of the widget page — a run of static widget
    /// setup statements the compiler moved out of the bytecode. Block at
    /// `off`: u16 LE length + ops (see `PG_*`). Each op does exactly what its
    /// bytecode form would, so ids, tree order and the final target match.
    #[inline(never)]
    fn load_page_block<P: Platform>(&mut self, ctx: &mut Ctx<P>, off: u32) {
        let mut hdr = [0u8; 2];
        if off + 2 > self.page_len {
            return self.fail(fault::PAGE);
        }
        ctx.flash.read(self.page_base + off, &mut hdr);
        let len = u16::from_le_bytes(hdr) as usize;
        if off as usize + 2 + len > self.page_len as usize {
            return self.fail(fault::PAGE);
        }
        let mut buf = Vec::new();
        if buf.try_reserve_exact(len).is_err() {
            return self.fail(fault::PAGE);
        }
        buf.resize(len, 0);
        ctx.flash.read(self.page_base + off + 2, &mut buf);
        self.run_page_ops(ctx, &buf);
    }

    #[inline]
    fn run_page_ops<P: Platform>(&mut self, ctx: &mut Ctx<P>, buf: &[u8]) {
        let len = buf.len();
        let mut i = 0;
        while i < len && self.state != VmState::Error {
            let op = buf[i];
            // Every op has at least one operand byte.
            let Some(&arg) = buf.get(i + 1) else {
                return self.fail(fault::PAGE);
            };
            i += 2;
            match op {
                0x01..=0x3F | PG_STORE => {
                    let pos = if op == PG_STORE { i } else { i - 1 };
                    let Some((val, n)) = proto::decode_signed_varint(buf, pos) else {
                        return self.fail(fault::PAGE);
                    };
                    i = pos + n;
                    if op == PG_STORE {
                        let slot = self.resolve_slot(arg);
                        self.var_set(slot, val);
                    } else {
                        self.w_set(ctx, op, val);
                    }
                }
                0x40..=0x7F => {
                    let end = i + arg as usize;
                    let Some(data) = buf.get(i..end) else {
                        return self.fail(fault::PAGE);
                    };
                    i = end;
                    self.w_set_len(ctx, op, data);
                }
                PG_ALLTAR => self.w_alltar(ctx, arg),
                PG_TARGET_ID..=PG_PARENT => {
                    let id = if op & 1 != 0 {
                        self.var_get(self.resolve_slot(arg)) as u16
                    } else {
                        arg as u16
                    };
                    if op & 2 != 0 {
                        self.w_parent(ctx, id);
                    } else {
                        self.target = WidgetId(id);
                    }
                }
                _ => return self.fail(fault::PAGE),
            }
        }
    }

    // --- Property R/W ---

    fn set_scalar_prop<P: Platform>(&mut self, ctx: &mut Ctx<P>, prop_id: u8, val: i32) {
        Self::set_scalar_prop_on(ctx, self.target, prop_id, val);
    }

    /// Write a scalar property of an explicit `target` widget. Body of
    /// `set_scalar_prop` lifted to a target-parameterized associated function
    /// so the animation engine can drive any widget without disturbing the
    /// VM's current `self.target`. Marks the widget dirty exactly as before.
    fn set_scalar_prop_on<P: Platform>(ctx: &mut Ctx<P>, target: WidgetId, prop_id: u8, val: i32) {
        // Boolean flag props: one table lookup + one set_flag instead of a
        // full match arm (and get_mut bounds check) per flag.
        let flag = match prop_id {
            PROP_VISIBLE => FLAG_VISIBLE,
            PROP_ENABLED => FLAG_ENABLED,
            PROP_CLICKABLE => FLAG_CLICKABLE,
            PROP_CHECKED => FLAG_CHECKED,
            PROP_CLIP_CHILDREN => FLAG_CLIP_CHILDREN,
            PROP_MULTI_LINE => FLAG_MULTI_LINE,
            PROP_ON_CLICK => FLAG_HAS_ON_CLICK,
            PROP_ON_PAINT => FLAG_HAS_ON_PAINT,
            PROP_ON_TAP | PROP_ON_CHANGE => FLAG_HAS_ON_TAP,
            _ => 0,
        };
        if flag != 0 {
            set_flag(&mut ctx.tree.get_mut(target).flags, flag, val != 0);
            if flag & (FLAG_VISIBLE | FLAG_ENABLED) != 0 {
                ctx.tree.mark_dirty(target);
            }
            return;
        }

        // Base widget fields (no extension needed)
        let mut dirty = false;
        let mut prop_set = false;
        match prop_id {
            PROP_LOC_X => {
                ctx.tree.get_mut(target).location.x = val as i16;
                dirty = true;
                prop_set = true;
                
            }
            PROP_LOC_Y => {
                ctx.tree.get_mut(target).location.y = val as i16;
                dirty = true;   
                prop_set = true;
            }
            PROP_SIZE_W => {
                ctx.tree.get_mut(target).size.w = val as u16;
                dirty = true;
                prop_set = true;
            }
            PROP_SIZE_H => {
                ctx.tree.get_mut(target).size.h = val as u16;
                dirty = true;
                prop_set = true;
            }
            PROP_BG_COLOR => {
                ctx.tree.get_mut(target).background_color = val as u16;
                prop_set = true;
            }
            PROP_BORDER_COLOR => {
                ctx.tree.get_mut(target).border_color = val as u16;
                prop_set = true;
            }
            PROP_KIND => {
                ctx.tree.get_mut(target).kind = val as u8;
                prop_set = true;
            }
            _ => {}
        }

        if dirty {
            ctx.tree.mark_dirty(target);
        }
        if prop_set {
            return;
        }

        // Extension fields (allocate on demand)
        if let Some(ext) = ctx.tree.ensure_ext(target) {
            match prop_id {
                PROP_MARGIN_T => ext.margin.top = val as u8,
                PROP_MARGIN_R => ext.margin.right = val as u8,
                PROP_MARGIN_B => ext.margin.bottom = val as u8,
                PROP_MARGIN_L => ext.margin.left = val as u8,
                PROP_BORDER_T => ext.border.top = val as u8,
                PROP_BORDER_R => ext.border.right = val as u8,
                PROP_BORDER_B => ext.border.bottom = val as u8,
                PROP_BORDER_L => ext.border.left = val as u8,
                PROP_PADDING_T => ext.padding.top = val as u8,
                PROP_PADDING_R => ext.padding.right = val as u8,
                PROP_PADDING_B => ext.padding.bottom = val as u8,
                PROP_PADDING_L => ext.padding.left = val as u8,
                PROP_TEXT_COLOR => ext.text_color = val as u16,
                PROP_FONT_ID => ext.font_id = val as u8,
                PROP_TEXT_ALIGN => ext.text_align = val as u8,
                PROP_PRESS_COLOR => ext.press_color = val as u16,
                PROP_IMAGE_ID => ext.image_id = val as u8,
                PROP_BORDER_RADIUS => ext.border_radius = val as u16,
                PROP_VALUE => ext.value = val as i16,
                PROP_MAX_LENGTH => ext.max_length = val as u8,
                PROP_CURSOR_POS => ext.value = val as i16,
                PROP_SCROLL_Y => ext.value = val as i16,
                PROP_GRADIENT_COLOR => ext.gradient_color = val as u16,
                PROP_GRADIENT_DIR => ext.gradient_dir = val as u8,
                PROP_ALPHA => ext.alpha = val as u8,
                // KIND_GRAPH props share storage with value/max_length/image_id;
                // safe because a single widget kind is exclusive.
                PROP_GRAPH_ARR => ext.value = val as i16,
                PROP_GRAPH_COUNT => ext.max_length = val as u8,
                PROP_GRAPH_FLAGS => ext.image_id = val as u8,
                _ => {}
            }
        }
    }

    fn get_scalar_prop<P: Platform>(&self, ctx: &Ctx<P>, prop_id: u8) -> i32 {
        Self::get_scalar_prop_on(ctx, self.target, prop_id)
    }

    /// Read a scalar property of an explicit `target` widget. Body of
    /// `get_scalar_prop` lifted to a target-parameterized associated function
    /// so the animation engine can sample any widget without disturbing the
    /// VM's current `self.target`.
    fn get_scalar_prop_on<P: Platform>(ctx: &Ctx<P>, target: WidgetId, prop_id: u8) -> i32 {
        let w = ctx.tree.get(target);
        // One extension lookup for all ext props; no extension reads as the defaults.
        let e = ctx.tree.ext(target).unwrap_or(&WidgetExt::DEFAULT);
        match prop_id {
            // Base fields
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
            PROP_CHECKED => {
                if w.flags & FLAG_CHECKED != 0 {
                    1
                } else {
                    0
                }
            }
            PROP_CLIP_CHILDREN => {
                if w.flags & FLAG_CLIP_CHILDREN != 0 {
                    1
                } else {
                    0
                }
            }
            PROP_MULTI_LINE => {
                if w.flags & FLAG_MULTI_LINE != 0 {
                    1
                } else {
                    0
                }
            }
            PROP_BG_COLOR => w.background_color as i32,
            PROP_BORDER_COLOR => w.border_color as i32,
            PROP_KIND => w.kind as i32,
            // Extension fields (return defaults when no extension)
            PROP_MARGIN_T => e.margin.top as i32,
            PROP_MARGIN_R => e.margin.right as i32,
            PROP_MARGIN_B => e.margin.bottom as i32,
            PROP_MARGIN_L => e.margin.left as i32,
            PROP_BORDER_T => e.border.top as i32,
            PROP_BORDER_R => e.border.right as i32,
            PROP_BORDER_B => e.border.bottom as i32,
            PROP_BORDER_L => e.border.left as i32,
            PROP_PADDING_T => e.padding.top as i32,
            PROP_PADDING_R => e.padding.right as i32,
            PROP_PADDING_B => e.padding.bottom as i32,
            PROP_PADDING_L => e.padding.left as i32,
            // String id of the widget's text (0xFFFF = none → reads as "").
            PROP_TEXT => e.text_id as i32,
            PROP_TEXT_COLOR => e.text_color as i32,
            PROP_FONT_ID => e.font_id as i32,
            PROP_TEXT_ALIGN => e.text_align as i32,
            PROP_PRESS_COLOR => e.press_color as i32,
            PROP_IMAGE_ID => e.image_id as i32,
            PROP_BORDER_RADIUS => e.border_radius as i32,
            PROP_VALUE => e.value as i32,
            PROP_ON_CLICK => (w.flags & FLAG_HAS_ON_CLICK != 0) as i32,
            PROP_ON_PAINT => (w.flags & FLAG_HAS_ON_PAINT != 0) as i32,
            PROP_ON_TAP | PROP_ON_CHANGE => (w.flags & FLAG_HAS_ON_TAP != 0) as i32,
            PROP_MAX_LENGTH => e.max_length as i32,
            PROP_CURSOR_POS => e.value as i32,
            PROP_SCROLL_Y => e.value as i32,
            PROP_GRADIENT_COLOR => e.gradient_color as i32,
            PROP_GRADIENT_DIR => e.gradient_dir as i32,
            PROP_ALPHA => e.alpha as i32,
            PROP_GRAPH_ARR => e.value as i32,
            PROP_GRAPH_COUNT => e.max_length as i32,
            PROP_GRAPH_FLAGS => e.image_id as i32,
            _ => 0,
        }
    }

    // === Animation engine ===

    /// Animation slot cap: RES_SYSCFG `anim_slots`, else the BSP's `P::ANIM_SLOTS`.
    fn anim_cap<P: Platform>(&self) -> usize {
        if self.anim_slots > 0 {
            self.anim_slots as usize
        } else {
            P::ANIM_SLOTS
        }
    }

    /// Begin (or replace) a tween on `target`'s `prop_id`. The starting value
    /// is the property's current value (sampled now). A tween already running
    /// on the same (target, prop) is overwritten so the newest call wins.
    /// Silently dropped when the pool (`anim_cap`) is full.
    fn start_anim<P: Platform>(
        &mut self,
        ctx: &Ctx<P>,
        target: WidgetId,
        prop_id: u8,
        to: i32,
        duration_ms: u16,
        ctrl: u8,
        on_end: u16,
        now: u32,
    ) {
        if target.is_none() {
            return;
        }
        let anim = Anim {
            target,
            prop_id,
            ctrl,
            from: Self::get_scalar_prop_on(ctx, target, prop_id),
            to,
            start_ms: now,
            duration_ms: duration_ms.max(1),
            on_end,
            active: true,
        };
        // A new tween on a property already animating supersedes the old one.
        for a in self.anims.iter_mut() {
            if a.active && a.target == target && a.prop_id == prop_id {
                *a = anim;
                return;
            }
        }
        // Reuse a finished slot, else append while under the per-BSP cap.
        if let Some(slot) = self.anims.iter_mut().find(|a| !a.active) {
            *slot = anim;
        } else if self.anims.len() < self.anim_cap::<P>() {
            self.anims.push(anim);
        }
    }

    /// Cancel all tweens on `target`.
    fn stop_anim(&mut self, target: WidgetId) {
        for a in self.anims.iter_mut() {
            if a.target == target {
                a.active = false;
            }
        }
    }

    /// Cancel the tween on `target`'s specific `prop_id`.
    fn stop_anim_prop(&mut self, target: WidgetId, prop_id: u8) {
        for a in self.anims.iter_mut() {
            if a.target == target && a.prop_id == prop_id {
                a.active = false;
            }
        }
    }

    /// Advance all active tweens to time `now` (millis). Applies each
    /// interpolated value through the same property setter + dirty-mark path
    /// as W_SET, but only when the integer value actually changes — the device
    /// loop spins far faster than one visible step per frame, so this gate
    /// avoids redundant redraws. Repeating tweens restart (loop) or reverse
    /// (yoyo); one-shot tweens deactivate and fire their `on_end` callback.
    /// Called every main-loop iteration regardless of `self.state`.
    #[inline(never)]
    pub fn tick_animations<P: Platform>(&mut self, ctx: &mut Ctx<P>, now: u32) {
        let mut any_done = false;
        let n = self.anims.len();
        for i in 0..n {
            let a = self.anims[i]; // snapshot — releases the borrow on self.anims
            if !a.active {
                continue;
            }
            let dur = a.duration_ms.max(1) as u32;
            let elapsed = now.wrapping_sub(a.start_ms);
            let done = elapsed >= dur;
            let t = if done { 256 } else { (elapsed * 256 / dur) as i32 };
            let te = ease(a.ctrl, t);
            let val = if prop_is_color(a.prop_id) {
                crate::lcd::lerp_rgb565(a.from as u16, a.to as u16, te as u16, 256) as i32
            } else {
                a.from + ((a.to - a.from) * te) / 256
            };
            if val != Self::get_scalar_prop_on(ctx, a.target, a.prop_id) {
                Self::set_scalar_prop_on(ctx, a.target, a.prop_id, val);
                if prop_is_visual(a.prop_id) {
                    ctx.tree.mark_dirty(a.target);
                }
            }
            if done {
                if a.ctrl & ANIM_YOYO != 0 {
                    let s = &mut self.anims[i];
                    core::mem::swap(&mut s.from, &mut s.to);
                    s.start_ms = now;
                } else if a.ctrl & ANIM_LOOP != 0 {
                    self.anims[i].start_ms = now;
                } else {
                    self.anims[i].active = false;
                    any_done = true;
                    // `on_end` is a 1-based func_id (as pushed by `@fn`);
                    // resolve to a bytecode offset before queueing, mirroring
                    // the widget-callback path. Scope the immutable borrow so
                    // the mutable enqueue can follow.
                    if a.on_end != 0 {
                        let off = self.find_func(a.on_end).map(|f| f.offset as CodeAddr);
                        if let Some(off) = off {
                            self.enqueue_callback(off, &[a.target.0 as i32, a.prop_id as i32]);
                        }
                    }
                }
            }
        }
        if any_done {
            self.anims.retain(|a| a.active);
        }
    }

    fn set_compound_prop<P: Platform>(&mut self, prop_id: u8, data: &[u8], ctx: &mut Ctx<P>) {
        if prop_id == PROP_TEXT {
            if self.target.is_some() {
                // `set(text, "…")` literal: interned like OP_STR_LIT, so a
                // handler that runs it repeatedly reuses one slot.
                if let Some(str_id) = ctx.strpool.intern(data) {
                    if let Some(ext) = ctx.tree.ensure_ext(self.target) {
                        ext.text_id = str_id;
                    }
                } else {
                    self.fail(fault::STR_ALLOC);
                }
            }
            return;
        }

        let (vals, count) = proto::unpack_signed_varints(data);

        // Base fields
        match prop_id {
            PROP_LOCATION if count >= 2 => {
                let w = ctx.tree.get_mut(self.target);
                w.location = Offset {
                    x: vals[0] as i16,
                    y: vals[1] as i16,
                };
                ctx.tree.mark_dirty(self.target);
                return;
            }
            PROP_SIZE if count >= 2 => {
                let w = ctx.tree.get_mut(self.target);
                w.size = Size {
                    w: vals[0] as u16,
                    h: vals[1] as u16,
                };
                ctx.tree.mark_dirty(self.target);
                return;
            }
            _ => {}
        }

        // Extension fields
        if let Some(ext) = ctx.tree.ensure_ext(self.target) {
            match prop_id {
                PROP_MARGIN if count >= 4 => {
                    ext.margin =
                        Edges::new(vals[0] as u8, vals[1] as u8, vals[2] as u8, vals[3] as u8);
                }
                PROP_BORDER_EDGES if count >= 4 => {
                    ext.border =
                        Edges::new(vals[0] as u8, vals[1] as u8, vals[2] as u8, vals[3] as u8);
                }
                PROP_PADDING if count >= 4 => {
                    ext.padding =
                        Edges::new(vals[0] as u8, vals[1] as u8, vals[2] as u8, vals[3] as u8);
                }
                _ => {}
            }
        }
    }

    // --- Array operations ---

    fn find_array(&self, arr_id: i32) -> Option<usize> {
        if arr_id < 0 {
            return None;
        }
        let id = arr_id as u16;
        self.arrays.iter().position(|a| a.id == id)
    }

    fn arr_free(&mut self, arr_id: i32) {
        if let Some(pos) = self.find_array(arr_id) {
            // swap_remove is O(1) — array order doesn't matter since lookup is by id.
            self.arrays.swap_remove(pos);
        }
    }

    fn arr_alloc(&mut self, size: u16) {
        let id = self.next_arr_id;
        let data = alloc::vec![0i32; size as usize];
        self.arrays.push(VmArray { id, data });
        self.next_arr_id = self.next_arr_id.wrapping_add(1);
        self.push(id as i32);
    }

    fn arr_load(&mut self, arr_id: i32, idx: i32) {
        match self.find_array(arr_id) {
            Some(pos) => {
                let arr = &self.arrays[pos];
                if idx < 0 || idx as usize >= arr.data.len() {
                    self.fail(fault::ARRAY_INDEX);
                    return;
                }
                let val = arr.data[idx as usize];
                self.push(val);
            }
            None => self.fail(fault::BAD_ARRAY),
        }
    }

    fn arr_store(&mut self, arr_id: i32, idx: i32, val: i32) {
        match self.find_array(arr_id) {
            Some(pos) => {
                let arr = &mut self.arrays[pos];
                if idx < 0 || idx as usize >= arr.data.len() {
                    self.fail(fault::ARRAY_INDEX);
                    return;
                }
                arr.data[idx as usize] = val;
            }
            None => self.fail(fault::BAD_ARRAY),
        }
    }

    fn arr_len(&mut self, arr_id: i32) {
        match self.find_array(arr_id) {
            Some(pos) => self.push(self.arrays[pos].data.len() as i32),
            None => self.fail(fault::BAD_ARRAY),
        }
    }

    /// Borrow a VM array's data by id. Used by the renderer (e.g. graph
    /// widget) to read sample buffers without copying. Returns None when the
    /// id does not name a live array.
    pub fn array_slice(&self, arr_id: u16) -> Option<&[i32]> {
        self.arrays
            .iter()
            .find(|a| a.id == arr_id)
            .map(|a| a.data.as_slice())
    }

    // --- File ops ---

    /// Map a handle (1 or 2) to the open_files index (0 or 1).
    /// Returns None if handle is out of range or slot is empty.
    fn file_slot_index(&self, handle: i32) -> Option<usize> {
        let idx = match handle {
            1 => 0,
            2 => 1,
            _ => return None,
        };
        if self.open_files[idx].is_some() {
            Some(idx)
        } else {
            None
        }
    }

    fn file_slot(&self, handle: i32) -> Option<&OpenFile> {
        self.file_slot_index(handle)
            .map(|i| self.open_files[i].as_ref().unwrap())
    }

    fn file_slot_mut(&mut self, handle: i32) -> Option<&mut OpenFile> {
        let idx = self.file_slot_index(handle)?;
        self.open_files[idx].as_mut()
    }

    /// Open a file by name. Returns handle 1 or 2, or 0xFF on error:
    ///   - fs not mounted
    ///   - name not found
    ///   - resource is not a file (wrong kind)
    ///   - both slots in use
    fn file_open<P: Platform>(&mut self, ctx: &Ctx<P>, name: &[u8]) -> i32 {
        let fs = match ctx.fs.as_ref() {
            Some(f) => f,
            None => return FILE_HANDLE_ERR,
        };
        let entry = match fs.find(&ctx.flash, name) {
            Some(e) if e.kind == crate::fs::RES_FILE => e,
            _ => return FILE_HANDLE_ERR,
        };
        let slot = if self.open_files[0].is_none() {
            0
        } else if self.open_files[1].is_none() {
            1
        } else {
            return FILE_HANDLE_ERR;
        };
        self.open_files[slot] = Some(OpenFile {
            offset: entry.offset,
            size: entry.size,
            pos: 0,
        });
        (slot + 1) as i32
    }
}

// === Image header parser ===

/// Parse VM image header. Returns (function table, opcode start, global_count, render_mode,
/// (widget_count, ext_count) — `None` before v4, wide_addr, portrait). widget_count/ext_count are 0 for v1-v3 images.
///
/// Format v1/v2:
///   version(u8) + function_count(u16 LE) + global_count(u16 LE)
///   + [func_id(u16) + kind(u8) + pad(u8) + offset(u32) + length(u32)] × N
/// Format v3 (adds flags):
///   version(u8) + function_count(u16 LE) + global_count(u16 LE) + flags(u16 LE)
///   + [...]
/// Format v4 (adds widget/ext counts):
///   v3 header + widget_count(u8) + ext_count(u8) + [...]
///   flags bit 0: render_mode (0=dirty, 1=buffered)
///   flags bit 2: wide_addr (jump/call targets are u32)
///   flags bit 3: portrait (screen rotated 90°)
#[allow(clippy::type_complexity)]
fn parse_image_header(
    data: &[u8],
) -> Option<(Vec<FuncEntry>, usize, u8, RenderMode, Option<(u8, u8)>, bool, bool)> {
    if data.len() < IMAGE_HEADER_SIZE_V2 {
        return None;
    }

    let version = data[0];
    if version < 1 || version > 4 {
        return None;
    }

    let func_count = u16::from_le_bytes([data[1], data[2]]) as usize;
    // v1: reserved=0, v2+: global_count
    let global_count = if version >= 2 {
        u16::from_le_bytes([data[3], data[4]]) as u8
    } else {
        0
    };

    // v3+: flags field after global_count; v4 adds widget/ext counts.
    let header_size = match version {
        4 => IMAGE_HEADER_SIZE_V4,
        3 => IMAGE_HEADER_SIZE_V3,
        _ => IMAGE_HEADER_SIZE_V2,
    };
    if data.len() < header_size {
        return None;
    }
    let flags = if version >= 3 {
        u16::from_le_bytes([data[5], data[6]])
    } else {
        0
    };
    let render_mode = if flags & 0x01 != 0 {
        RenderMode::Buffered
    } else {
        RenderMode::Dirty
    };
    let wide = flags & IMAGE_FLAG_WIDE_ADDR != 0;
    let portrait = flags & IMAGE_FLAG_PORTRAIT != 0;
    let tree_hint = if version >= 4 { Some((data[7], data[8])) } else { None };

    let table_size = func_count * FUNC_ENTRY_SIZE;
    let opcode_start = header_size + table_size;

    if data.len() < opcode_start {
        return None;
    }

    let mut functions = Vec::with_capacity(func_count);
    // Fixed-size chunks: the field reads below need no bounds checks.
    for e in data[header_size..opcode_start].chunks_exact(FUNC_ENTRY_SIZE) {
        let func_id = u16::from_le_bytes([e[0], e[1]]);
        let kind_byte = e[2];
        // e[3] = pad
        let offset = u32::from_le_bytes([e[4], e[5], e[6], e[7]]);
        let length = u32::from_le_bytes([e[8], e[9], e[10], e[11]]);

        let kind = FunctionKind::from_u8(kind_byte).unwrap_or(FunctionKind::UserFunction);

        functions.push(FuncEntry {
            func_id,
            kind,
            offset,
            length,
        });
    }

    Some((
        functions,
        opcode_start,
        global_count,
        render_mode,
        tree_hint,
        wide,
        portrait,
    ))
}

/// Size of an image's header including its function table, from the fixed
/// part (`IMAGE_HEADER_SIZE_V4` bytes suffice for every version). `None` if
/// `fixed` is too short or not a known version.
fn image_header_len(fixed: &[u8]) -> Option<usize> {
    let version = *fixed.first()?;
    let size = match version {
        1 | 2 => IMAGE_HEADER_SIZE_V2,
        3 => IMAGE_HEADER_SIZE_V3,
        4 => IMAGE_HEADER_SIZE_V4,
        _ => return None,
    };
    if fixed.len() < size {
        return None;
    }
    let func_count = u16::from_le_bytes([fixed[1], fixed[2]]) as usize;
    Some(size + func_count * FUNC_ENTRY_SIZE)
}

/// Whether this build can address an image's bytecode: every offset in
/// `code_len` must fit `CodeAddr`, and wide (u32-operand) images need the
/// `wide_code` feature to decode at all.
fn code_addressable(wide: bool, code_len: usize) -> bool {
    (cfg!(feature = "wide_code") || !wide) && code_len <= CodeAddr::MAX as usize
}

// --- Packed u32 helper ---

#[inline]
fn unpack_pair(packed: i32) -> (u16, u16) {
    let val = packed as u32;
    ((val >> 16) as u16, (val & 0xFFFF) as u16)
}

// --- Flag helper ---

#[inline]
fn set_flag(flags: &mut u16, mask: u16, val: bool) {
    if val {
        *flags |= mask;
    } else {
        *flags &= !mask;
    }
}

/// Whether writing `prop_id` changes the widget's on-screen appearance and so
/// should schedule a redraw. Defaults to `true`; only behaviour-only props
/// (state flags, event-handler hooks, input metadata) are excluded so they
/// don't trigger needless repaints.
fn prop_is_visual(prop_id: u8) -> bool {
    !matches!(
        prop_id,
        PROP_ENABLED
            | PROP_CLICKABLE
            | PROP_ON_CLICK
            | PROP_ON_PAINT
            | PROP_ON_TAP
            | PROP_ON_CHANGE
            | PROP_MAX_LENGTH
            | PROP_CURSOR_POS
    )
}

// --- Animation helpers ---

/// Apply an easing curve to `t` (Q8 fixed-point, 0..=256), returning the eased
/// position in the same Q8 range. Curve picked from the low 4 bits of `ctrl`.
/// Quadratic in/out/in-out; anything else is linear.
fn ease(ctrl: u8, t: i32) -> i32 {
    match ctrl & EASE_CURVE_MASK {
        EASE_IN => (t * t) >> 8,
        EASE_OUT => {
            let u = 256 - t;
            256 - ((u * u) >> 8)
        }
        EASE_IN_OUT => {
            if t < 128 {
                (2 * t * t) >> 8
            } else {
                let u = 256 - t;
                256 - ((2 * u * u) >> 8)
            }
        }
        _ => t,
    }
}

/// True for properties stored as RGB565 colors — these tween channel-wise via
/// `lerp_rgb565` rather than by linear integer interpolation.
fn prop_is_color(prop_id: u8) -> bool {
    matches!(
        prop_id,
        PROP_BG_COLOR | PROP_BORDER_COLOR | PROP_TEXT_COLOR | PROP_PRESS_COLOR | PROP_GRADIENT_COLOR
    )
}

#[cfg(all(test, feature = "mock"))]
mod anim_tests {
    use super::*;
    use crate::mock::{MockBacklight, MockFlash, MockGpio, MockLcd, MockPlatform, MockRtc, MockUsart};
    use crate::backlight::BacklightImpl;
    use crate::flash::FlashImpl;
    use crate::lcd::LcdImpl;
    use crate::rtc::RtcImpl;
    use crate::systick::SystickImpl;
    use crate::usart::UsartImpl;
    use crate::font::FontList;
    use crate::image::ImageList;
    use crate::strpool::StringPool;
    use crate::widget::WidgetTree;
    use crate::audio::AudioImpl;
    use crate::gpio::GpioImpl;

    fn mock_ctx() -> Ctx<MockPlatform> {
        Ctx {
            lcd: LcdImpl::with_backend(MockLcd),
            flash: FlashImpl::with_backend(MockFlash),
            tree: WidgetTree::new(),
            fonts: FontList::new(),
            images: ImageList::new(),
            strpool: StringPool::new(),
            fs: None,
            backlight: BacklightImpl::with_backend(MockBacklight),
            rtc: RtcImpl::with_backend(MockRtc),
            usart: UsartImpl::with_backend(MockUsart),
            systick: SystickImpl::init(),
            audio: AudioImpl::none(),
            gpio: GpioImpl::with_backend(MockGpio::default()),
            cursor_visible: false,
        }
    }

    // Allocate one widget and return its id.
    fn one_widget(ctx: &mut Ctx<MockPlatform>) -> WidgetId {
        ctx.tree.alloc().expect("alloc")
    }

    #[test]
    fn get_text_reads_the_widgets_own_string() {
        // text_input demo: get(text) on an input after strClear() returned
        // string 0 (the title) because PROP_TEXT had no getter.
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        let title = one_widget(&mut ctx);
        let input = one_widget(&mut ctx);
        let empty = one_widget(&mut ctx);
        vm.target = title;
        vm.set_compound_prop(PROP_TEXT, b"Text Input Demo", &mut ctx);
        vm.target = input;
        vm.set_compound_prop(PROP_TEXT, b"erhan", &mut ctx);
        ctx.strpool.smart_clear(&mut ctx.tree);

        let id = Vm::get_scalar_prop_on(&ctx, input, PROP_TEXT);
        assert_eq!(ctx.strpool.get(id as u16), b"erhan");
        let none = Vm::get_scalar_prop_on(&ctx, empty, PROP_TEXT);
        assert_eq!(ctx.strpool.get(none as u16), b"", "no text reads as empty");
    }

    #[test]
    fn linear_tween_interpolates_and_completes() {
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        let id = one_widget(&mut ctx);
        Vm::set_scalar_prop_on(&mut ctx, id, PROP_LOC_X, 0);

        // 0 -> 100 over 100ms, linear, starting at t=1000.
        vm.start_anim(&ctx, id, PROP_LOC_X, 100, 100, 0, 0, 1000);

        vm.tick_animations(&mut ctx, 1000);
        assert_eq!(Vm::get_scalar_prop_on(&ctx, id, PROP_LOC_X), 0);

        vm.tick_animations(&mut ctx, 1050);
        assert_eq!(Vm::get_scalar_prop_on(&ctx, id, PROP_LOC_X), 50);

        // At/after duration: snaps to target and the slot is freed.
        vm.tick_animations(&mut ctx, 1100);
        assert_eq!(Vm::get_scalar_prop_on(&ctx, id, PROP_LOC_X), 100);
        assert!(vm.anims.is_empty(), "completed one-shot tween must be retired");
    }

    #[test]
    fn color_tween_blends_channelwise() {
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        let id = one_widget(&mut ctx);
        // RED (0xF800) -> BLUE (0x001F), linear.
        Vm::set_scalar_prop_on(&mut ctx, id, PROP_BG_COLOR, 0xF800);
        vm.start_anim(&ctx, id, PROP_BG_COLOR, 0x001F, 100, 0, 0, 0);

        vm.tick_animations(&mut ctx, 50); // midpoint
        let mid = Vm::get_scalar_prop_on(&ctx, id, PROP_BG_COLOR) as u16;
        // Midpoint is a genuine blend: some red left, some blue gained.
        assert!(mid != 0xF800 && mid != 0x001F, "got 0x{mid:04X}");
        assert!((mid >> 11) > 0 && (mid >> 11) < 0x1F, "red channel mid");
        assert!((mid & 0x1F) > 0 && (mid & 0x1F) < 0x1F, "blue channel mid");
    }

    #[test]
    fn yoyo_reverses_and_keeps_running() {
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        let id = one_widget(&mut ctx);
        Vm::set_scalar_prop_on(&mut ctx, id, PROP_LOC_X, 0);
        vm.start_anim(&ctx, id, PROP_LOC_X, 100, 100, ANIM_YOYO, 0, 0);

        // Reach the far end; yoyo swaps endpoints and restarts (stays active).
        vm.tick_animations(&mut ctx, 100);
        assert_eq!(Vm::get_scalar_prop_on(&ctx, id, PROP_LOC_X), 100);
        assert_eq!(vm.anims.len(), 1, "yoyo tween must not be retired");

        // Now heading back toward 0.
        vm.tick_animations(&mut ctx, 150);
        assert_eq!(Vm::get_scalar_prop_on(&ctx, id, PROP_LOC_X), 50);
    }

    #[test]
    fn start_anim_replaces_same_property() {
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        let id = one_widget(&mut ctx);
        Vm::set_scalar_prop_on(&mut ctx, id, PROP_LOC_X, 0);
        vm.start_anim(&ctx, id, PROP_LOC_X, 100, 100, 0, 0, 0);
        vm.start_anim(&ctx, id, PROP_LOC_X, 200, 100, 0, 0, 0);
        assert_eq!(vm.anims.len(), 1, "second tween on same prop replaces the first");
    }

    #[test]
    fn pool_is_capped_per_platform() {
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        // MockPlatform inherits the default ANIM_SLOTS (8). Distinct props so
        // none replace each other.
        let props = [
            PROP_LOC_X, PROP_LOC_Y, PROP_SIZE_W, PROP_SIZE_H,
            PROP_BG_COLOR, PROP_BORDER_COLOR, PROP_TEXT_COLOR, PROP_PRESS_COLOR,
            PROP_BORDER_RADIUS, PROP_ALPHA, // two beyond the cap
        ];
        for &p in &props {
            let id = one_widget(&mut ctx);
            vm.start_anim(&ctx, id, p, 10, 100, 0, 0, 0);
        }
        assert_eq!(vm.anims.len(), MockPlatform::ANIM_SLOTS);
        assert_eq!(MockPlatform::ANIM_SLOTS, 8);
    }

    #[test]
    fn configured_anim_cap_overrides_platform() {
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        assert!(vm.configure(0, 0, 0, 3));
        let props = [PROP_LOC_X, PROP_LOC_Y, PROP_SIZE_W, PROP_SIZE_H, PROP_BG_COLOR];
        for &p in &props {
            let id = one_widget(&mut ctx);
            vm.start_anim(&ctx, id, p, 10, 100, 0, 0, 0);
        }
        assert_eq!(vm.anims.len(), 3);
    }

    #[test]
    fn configure_zero_keeps_defaults() {
        let mut vm = Vm::new();
        assert!(vm.configure(0, 0, 0, 0));
        assert_eq!(vm.limits(), (STACK_SIZE, CALL_STACK_SIZE, CB_QUEUE_SIZE));
    }

    #[test]
    fn configured_eval_stack_sets_overflow_depth() {
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        assert!(vm.configure(4, 0, 0, 0));
        vm.code = VmCode::Ram(alloc::vec![OP_PUSH_1; 5]);
        vm.run(&mut ctx);
        assert_eq!(vm.fault(), fault::STACK_OVERFLOW);
        assert_eq!(vm.fault_pc(), 4, "5th push overflows a 4-deep stack");

        // Deeper than the default 32: 40 pushes then HALT run cleanly.
        let mut vm = Vm::new();
        assert!(vm.configure(48, 0, 0, 0));
        let mut code = alloc::vec![OP_PUSH_1; 40];
        code.push(OP_HALT);
        vm.code = VmCode::Ram(code);
        vm.run(&mut ctx);
        assert!(vm.state != VmState::Error);
        assert_eq!(vm.sp, 40);
    }

    // --- Callback frames ---

    /// VM with `code` loaded and 4 globals (callback locals start above them).
    fn cb_vm(code: &[u8]) -> Vm {
        let mut vm = Vm::new();
        vm.code = VmCode::Ram(code.to_vec());
        vm.global_count = 4;
        vm.state = VmState::Running;
        vm
    }

    #[test]
    fn callback_preempts_main_and_keeps_its_stack() {
        // main @0: 7 9 ADD -> g0, HALT     callback @7: arg -> g1, leaves junk, RET
        let code = [
            OP_PUSH_I8, 7, OP_PUSH_I8, 9, OP_ADD, OP_STORE, 0, // 0..7
            OP_STORE, 1, OP_PUSH_1, OP_PUSH_2, OP_PUSH_0, OP_RET, // 7..13
        ];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 2); // main stops mid-expression: [7, 9]
        assert_eq!(vm.sp, 2);

        vm.enqueue_callback(7, &[5]);
        assert!(vm.drain_callbacks(&mut ctx));
        assert_eq!(vm.var_get(1), 5, "callback got its arg");
        assert!(vm.cb_frames.is_empty());
        assert_eq!(vm.sp, 2, "callback leftovers dropped");
        assert_eq!(&vm.stack[..2], &[7, 9], "interrupted values untouched");
        assert_eq!(vm.pc, 4);
        assert!(vm.state == VmState::Running);

        vm.tick(&mut ctx, 100);
        assert_eq!(vm.var_get(0), 16, "main resumes where it was");
    }

    #[test]
    fn callback_cannot_pop_interrupted_values() {
        // main @0: PUSH 7, HALT     callback @3: pops two values, gets one arg
        let code = [OP_PUSH_I8, 7, OP_HALT, OP_POP, OP_POP, OP_RET];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 1);
        vm.enqueue_callback(3, &[1]);
        vm.drain_callbacks(&mut ctx);
        assert!(vm.state == VmState::Error);
        assert_eq!(vm.fault(), fault::STACK_UNDERFLOW);
        assert_eq!(vm.fault_pc(), 4);
        assert_eq!(vm.stack[0], 7);
    }

    #[test]
    fn delay_in_callback_suspends_without_blocking() {
        // main @0: HALT
        // A @1: delay(1000); g0 = 1; RET      B @9: g1 = 2; RET
        let code = [
            OP_HALT,
            OP_PUSH_I16, 0xE8, 0x03, OP_DELAY, OP_PUSH_1, OP_STORE, 0, OP_RET, // 1..9
            OP_PUSH_2, OP_STORE, 1, OP_RET, // 9..13
        ];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 1);
        assert!(vm.state == VmState::Halted);

        vm.enqueue_callback(1, &[]);
        assert!(vm.drain_callbacks(&mut ctx), "returns instead of busy-waiting");
        assert!(vm.state == VmState::Waiting);
        assert_eq!(vm.cb_frames.len(), 1);
        assert_eq!(vm.var_get(0), 0);

        // B queues behind the suspended A (FIFO order is kept).
        vm.enqueue_callback(9, &[]);
        assert!(!vm.drain_callbacks(&mut ctx));
        assert_eq!(vm.var_get(1), 0);
        vm.tick(&mut ctx, 1); // mock clock stands at 0: still waiting
        assert!(vm.state == VmState::Waiting);

        vm.wait_until = 0; // the delay expires
        vm.tick(&mut ctx, 1);
        assert_eq!(vm.var_get(0), 1, "A finished after its delay");
        assert!(vm.cb_frames.is_empty());
        assert!(vm.state == VmState::Halted, "main state restored");

        assert!(vm.drain_callbacks(&mut ctx));
        assert_eq!(vm.var_get(1), 2);
    }

    #[test]
    fn yield_in_callback_continues_on_next_tick() {
        // main @0: HALT     callback @1: g0 = 1; YIELD; g1 = 2; RET
        let code = [OP_HALT, OP_PUSH_1, OP_STORE, 0, OP_YIELD, OP_PUSH_2, OP_STORE, 1, OP_RET];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 1);
        vm.enqueue_callback(1, &[]);
        vm.drain_callbacks(&mut ctx);
        assert_eq!(vm.var_get(0), 1);
        assert!(vm.state == VmState::Yielded);
        assert_eq!(vm.cb_frames.len(), 1);

        vm.tick(&mut ctx, 1);
        assert_eq!(vm.var_get(1), 2, "code after yield runs");
        assert!(vm.cb_frames.is_empty());
        assert!(vm.state == VmState::Halted);
    }

    #[test]
    fn main_wait_survives_a_callback() {
        // main @0: delay(500); HALT     callback @5: g0 = 1; RET
        let code = [OP_PUSH_I16, 0xF4, 0x01, OP_DELAY, OP_HALT, OP_PUSH_1, OP_STORE, 0, OP_RET];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 10);
        assert!(vm.state == VmState::Waiting);
        vm.enqueue_callback(5, &[]);
        vm.drain_callbacks(&mut ctx);
        assert_eq!(vm.var_get(0), 1);
        assert!(vm.state == VmState::Waiting);
        assert_eq!(vm.wait_until, 500);
    }

    #[test]
    fn halt_in_callback_ends_only_the_callback() {
        // main @0: g0 = 3; HALT     callback @5: g1 = 1; HALT
        let code = [OP_PUSH_I8, 3, OP_STORE, 0, OP_HALT, OP_PUSH_1, OP_STORE, 1, OP_HALT];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 1);
        vm.enqueue_callback(5, &[]);
        vm.drain_callbacks(&mut ctx);
        assert_eq!(vm.var_get(1), 1);
        assert!(vm.state == VmState::Running);
        vm.tick(&mut ctx, 10);
        assert_eq!(vm.var_get(0), 3);
    }

    #[test]
    fn modal_in_callback_lets_dialog_callbacks_run() {
        // builder fn 1 @0: return overlay id 1
        // A @2: g0 = showModal(1, 0); RET
        // B @8: setDialogResult(42); RET
        let code = [
            OP_PUSH_1, OP_RET, // 0..2
            OP_PUSH_1, OP_PUSH_0, OP_SHOW_MODAL, OP_STORE, 0, OP_RET, // 2..8
            OP_PUSH_I8, 42, OP_SET_DIALOG_RESULT, OP_RET, // 8..12
        ];
        let mut ctx = mock_ctx();
        let root = one_widget(&mut ctx);
        let overlay = one_widget(&mut ctx);
        assert_eq!((root.0, overlay.0), (0, 1));
        let mut vm = cb_vm(&code);
        vm.functions.push(FuncEntry { func_id: 1, kind: FunctionKind::UserFunction, offset: 0, length: 2 });
        vm.state = VmState::Halted;

        vm.enqueue_callback(2, &[]);
        vm.drain_callbacks(&mut ctx);
        assert!(vm.state == VmState::AwaitingModal);
        assert_eq!(vm.cb_frames.len(), 1, "A stays suspended on its frame");

        vm.enqueue_callback(8, &[]);
        assert!(vm.drain_callbacks(&mut ctx), "dialog callback runs on top of A");
        assert!(vm.state == VmState::AwaitingModal);
        assert_eq!(vm.cb_frames.len(), 1);

        vm.try_resume_modal(&mut ctx);
        assert!(!ctx.tree.is_live(overlay));
        vm.tick(&mut ctx, 1);
        assert_eq!(vm.var_get(0), 42, "A sees the dialog result");
        assert!(vm.cb_frames.is_empty());
        assert!(vm.state == VmState::Halted);
        assert!(!vm.modal_active());
    }

    #[test]
    fn call_now_runs_through_yield_and_nests_over_a_suspended_callback() {
        // main @0: HALT    A @1: delay(1000); RET    S @6: g0 = 1; YIELD; g1 = 2; RET
        let code = [
            OP_HALT,
            OP_PUSH_I16, 0xE8, 0x03, OP_DELAY, OP_RET, // 1..6
            OP_PUSH_1, OP_STORE, 0, OP_YIELD, OP_PUSH_2, OP_STORE, 1, OP_RET, // 6..14
        ];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 1);
        vm.enqueue_callback(1, &[]);
        vm.drain_callbacks(&mut ctx);
        assert!(vm.state == VmState::Waiting);

        vm.call_now(6, &[], &mut ctx); // e.g. on_paint during A's delay
        assert_eq!((vm.var_get(0), vm.var_get(1)), (1, 2));
        assert_eq!(vm.cb_frames.len(), 1);
        assert!(vm.state == VmState::Waiting, "A is still waiting");
        assert_eq!(vm.wait_until, 1000);
    }

    // --- User GPIO ---

    #[test]
    fn gpio_ops_drive_and_read_pins() {
        // pinMode(1, OUTPUT); pinWrite(1, 5); g0 = pinRead(1);
        // pinMode(2, INPUT_PULLUP); g1 = pinRead(2); HALT
        let code = [
            OP_PUSH_1, OP_PUSH_1, OP_PIN_MODE,
            OP_PUSH_1, OP_PUSH_I8, 5, OP_PIN_WRITE,
            OP_PUSH_1, OP_PIN_READ, OP_STORE, 0,
            OP_PUSH_2, OP_PUSH_2, OP_PIN_MODE,
            OP_PUSH_2, OP_PIN_READ, OP_STORE, 1,
            OP_HALT,
        ];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 100);
        assert!(vm.state == VmState::Halted);
        assert_eq!(vm.var_get(0), 1, "non-zero writes drive high");
        assert_eq!(vm.var_get(1), 1, "pull-up input reads high");
    }

    #[test]
    fn gpio_faults_on_a_missing_pin_or_mode() {
        let mut ctx = mock_ctx();
        // MockGpio has 4 pins: pinRead(4)
        let mut vm = cb_vm(&[OP_PUSH_I8, 4, OP_PIN_READ, OP_HALT]);
        vm.tick(&mut ctx, 10);
        assert_eq!(vm.fault(), fault::GPIO);
        assert_eq!(vm.fault_pc(), 2);
        // pinMode(0, INPUT_PULLDOWN): unsupported on this board
        let mut vm = cb_vm(&[OP_PUSH_0, OP_PUSH_I8, 3, OP_PIN_MODE, OP_HALT]);
        vm.tick(&mut ctx, 10);
        assert_eq!(vm.fault(), fault::GPIO);
        // pinWrite(-1, 1)
        let mut vm = cb_vm(&[OP_PUSH_I8, 0xFF, OP_PUSH_1, OP_PIN_WRITE, OP_HALT]);
        vm.tick(&mut ctx, 10);
        assert_eq!(vm.fault(), fault::GPIO);
    }

    #[test]
    fn nesting_is_capped() {
        let mut vm = cb_vm(&[OP_HALT]);
        for _ in 0..CB_NEST_MAX {
            assert!(vm.enter_callback(0, &[]));
        }
        assert!(!vm.enter_callback(0, &[]));
        assert_eq!(vm.cb_frames.len(), CB_NEST_MAX);
    }

    #[test]
    fn start_main_waits_for_a_suspended_setup() {
        let mut vm = cb_vm(&[OP_HALT]);
        vm.state = VmState::Ready;
        assert!(vm.enter_callback(0, &[]));
        vm.state = VmState::AwaitingModal;
        vm.start_main(9);
        assert!(vm.state == VmState::AwaitingModal);
        vm.leave_callback();
        assert_eq!(vm.pc, 9);
        assert!(vm.state == VmState::Running);
    }

    #[test]
    fn configured_cb_queue_capacity() {
        let mut vm = Vm::new();
        assert!(vm.configure(0, 0, 3, 0));
        for i in 0..5 {
            vm.enqueue_callback(i, &[]);
        }
        // A ring of N slots holds N-1 entries.
        let mut n = 0;
        while vm.cb_head != vm.cb_tail {
            vm.cb_head = (vm.cb_head + 1) % vm.cb_queue.len() as u8;
            n += 1;
        }
        assert_eq!(n, 2);
    }

    fn run_code(code: &[u8]) -> Vm {
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        vm.code = VmCode::Ram(code.to_vec());
        vm.run(&mut ctx);
        vm
    }

    #[test]
    fn fault_reports_reason_and_instruction_pc() {
        // DUP, POP, POP → underflow raised by the instruction at pc 2.
        let vm = run_code(&[OP_DUP, OP_POP, OP_POP]);
        assert!(vm.state == VmState::Error);
        assert_eq!(vm.fault(), fault::STACK_UNDERFLOW);
        assert_eq!(vm.fault_pc(), 2);
    }

    #[test]
    fn fault_bad_opcode() {
        let vm = run_code(&[0xFF]);
        assert_eq!(vm.fault(), fault::BAD_OPCODE);
        assert_eq!(vm.fault_pc(), 0);
    }

    #[test]
    fn first_fault_wins_within_instruction() {
        // ADD on an empty stack: two failed pops, then the push succeeds —
        // the recorded cause must stay STACK_UNDERFLOW.
        let vm = run_code(&[OP_ADD]);
        assert_eq!(vm.fault(), fault::STACK_UNDERFLOW);
    }

    #[test]
    fn pop2_keeps_push_order() {
        // g0 = 9 - 2, g1 = 2 < 9
        let vm = run_code(&[
            OP_PUSH_I8, 9, OP_PUSH_I8, 2, OP_SUB, OP_STORE, 0,
            OP_PUSH_I8, 2, OP_PUSH_I8, 9, OP_LT, OP_STORE, 1, OP_HALT,
        ]);
        assert!(vm.state != VmState::Error);
        assert_eq!(vm.var_get(0), 7);
        assert_eq!(vm.var_get(1), 1);
    }

    #[test]
    fn pop2_underflow_with_one_value() {
        let vm = run_code(&[OP_PUSH_I8, 5, OP_SUB]);
        assert_eq!(vm.fault(), fault::STACK_UNDERFLOW);
        assert_eq!(vm.fault_pc(), 2);
        assert_eq!(vm.stack[0], 5);
    }

    #[test]
    fn callback_pop2_respects_floor() {
        // main @0: PUSH 7, HALT     callback @3: SUB with one arg
        let code = [OP_PUSH_I8, 7, OP_HALT, OP_SUB, OP_RET];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 1);
        vm.enqueue_callback(3, &[1]);
        vm.drain_callbacks(&mut ctx);
        assert_eq!(vm.fault(), fault::STACK_UNDERFLOW);
        assert_eq!(vm.fault_pc(), 3);
        assert_eq!(vm.stack[0], 7);
    }

    #[test]
    fn page_ops_build_widgets_like_bytecode() {
        let mut ctx = mock_ctx();
        ctx.tree.root = ctx.tree.alloc().unwrap();
        let mut vm = cb_vm(&[OP_HALT]);
        vm.frame_base = 4; // locals after the 4 globals
        #[rustfmt::skip]
        let ops = [
            PG_STORE, 1, 14,                    // g1 = 7
            PG_ALLTAR, 0,                       // g0 = alloc(); target(g0)
            PROP_SIZE, 3, 0xC0, 0x0C, 0x64,     // set(size, 800, 50)
            PROP_BG_COLOR, 0x82, 0x21,          // set(bg_color, 0x0841)
            PG_PARENT_ID, 0,                    // parent(0)
            PG_ALLTAR, 0x80,                    // var l = alloc(); target(l)
            PROP_TEXT, 2, b'H', b'i',           // set(text, "Hi")
            PG_PARENT, 0,                       // parent(g0)
        ];
        vm.run_page_ops(&mut ctx, &ops);
        assert_eq!(vm.fault(), fault::NONE);
        assert_eq!(vm.var_get(1), 7);
        let (a, b) = (WidgetId(1), WidgetId(2));
        assert_eq!(vm.var_get(0), 1);
        assert_eq!(vm.var_get(4), 2);
        let w = ctx.tree.get(a);
        assert!(w.parent == ctx.tree.root);
        assert_eq!((w.size.w, w.size.h), (800, 50));
        assert_eq!(w.background_color, 0x0841);
        assert!(ctx.tree.get(b).parent == a);
        assert_eq!(ctx.strpool.get(ctx.tree.text_id(b)), b"Hi");
        assert!(vm.target == b);
    }

    #[test]
    fn malformed_page_ops_fault() {
        let cases: [&[u8]; 4] = [
            &[PROP_BG_COLOR, 0x82],   // truncated varint
            &[PROP_SIZE, 5, 1],       // payload past the block
            &[0x90, 0],               // unknown op
            &[PG_ALLTAR],             // missing operand
        ];
        for ops in cases {
            let mut ctx = mock_ctx();
            ctx.tree.root = ctx.tree.alloc().unwrap();
            let mut vm = cb_vm(&[OP_HALT]);
            vm.run_page_ops(&mut ctx, ops);
            assert_eq!(vm.fault(), fault::PAGE, "{ops:?}");
        }
    }

    #[test]
    fn w_set_len_keeps_long_text() {
        let text = b"Spline graph (Catmull-Rom + fill, vs linear)";
        assert!(text.len() > 32);
        let mut code = alloc::vec![OP_W_TARGET, 1, OP_W_SET_LEN, PROP_TEXT, text.len() as u8];
        code.extend_from_slice(text);
        code.push(OP_HALT);
        let mut ctx = mock_ctx();
        ctx.tree.root = ctx.tree.alloc().unwrap();
        let w = ctx.tree.alloc().unwrap();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 8);
        assert_eq!(vm.fault(), fault::NONE);
        assert_eq!(ctx.strpool.get(ctx.tree.text_id(w)), text);
    }

    #[test]
    fn w_load_without_page_faults() {
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&[OP_W_LOAD, 0, 0, OP_HALT]);
        vm.tick(&mut ctx, 4);
        assert_eq!(vm.fault(), fault::PAGE);
        assert_eq!(vm.fault_pc(), 0);
    }

    /// v4 image with no functions: header flags + opcodes.
    fn image(flags: u16, code: &[u8]) -> Vec<u8> {
        let [f0, f1] = flags.to_le_bytes();
        let mut img = alloc::vec![4, 0, 0, 0, 0, f0, f1, 0, 0];
        img.extend_from_slice(code);
        img
    }

    /// Flash backed by a byte vector (reads past the end return 0xFF).
    struct RomFlash(Vec<u8>);
    impl FlashBackend for RomFlash {
        fn read(&self, addr: u32, buf: &mut [u8]) {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = *self.0.get(addr as usize + i).unwrap_or(&0xFF);
            }
        }
        fn write(&self, _: u32, _: &[u8]) {}
        fn erase_sector(&self, _: u32) {}
        fn read_id(&self) -> (u8, u8, u8) {
            (0, 0, 0)
        }
    }

    #[test]
    fn load_flash_reads_the_whole_function_table() {
        // 300 functions — well past the old 64-entry stack buffer.
        const FUNCS: u16 = 300;
        let [c0, c1] = FUNCS.to_le_bytes();
        let mut img = alloc::vec![4, c0, c1, 0, 0, 0, 0, 0, 0];
        for i in 0..FUNCS {
            img.extend_from_slice(&(i + 1).to_le_bytes()); // func_id
            img.extend_from_slice(&[FunctionKind::UserFunction as u8, 0]);
            img.extend_from_slice(&(i as u32).to_le_bytes()); // offset
            img.extend_from_slice(&1u32.to_le_bytes()); // length
        }
        let code = alloc::vec![OP_HALT; FUNCS as usize];
        img.extend_from_slice(&code);

        // Image at a non-zero flash offset, followed by unrelated bytes.
        let base = 0x100u32;
        let mut rom = alloc::vec![0xAA; base as usize];
        rom.extend_from_slice(&img);
        rom.extend_from_slice(&[0x55; 64]);

        let mut vm = Vm::new();
        let flash = FlashImpl::with_backend(RomFlash(rom));
        assert!(vm.load_flash(flash, base, img.len()));
        assert_eq!(vm.functions.len(), FUNCS as usize);
        assert_eq!(vm.find_func(FUNCS).map(|f| f.offset), Some(FUNCS as u32 - 1));
        assert_eq!(vm.code.len(), code.len());
    }

    #[test]
    fn load_flash_rejects_a_truncated_function_table() {
        // Header announces 300 functions but the image is cut short.
        let img = alloc::vec![4, 0x2C, 0x01, 0, 0, 0, 0, 0, 0, 1, 0, 0];
        let mut vm = Vm::new();
        let flash = FlashImpl::with_backend(RomFlash(img.clone()));
        assert!(!vm.load_flash(flash, 0, img.len()));
    }

    #[test]
    fn wide_image_needs_wide_code() {
        let mut vm = Vm::new();
        let ok = vm.load_ram(&image(IMAGE_FLAG_WIDE_ADDR, &[OP_HALT]));
        assert_eq!(ok, cfg!(feature = "wide_code"));
    }

    #[test]
    fn narrow_build_rejects_code_past_64k() {
        let mut vm = Vm::new();
        let ok = vm.load_ram(&image(0, &alloc::vec![OP_HALT; 0x1_0001]));
        assert_eq!(ok, cfg!(feature = "wide_code"));
    }

    #[cfg(feature = "wide_code")]
    #[test]
    fn wide_jump_and_call_past_64k() {
        // 0x00000: JMP 0x10000
        // 0x10000: CALL 0x10010   → ret_addr 0x10005 must survive as u32
        // 0x10005: <bad opcode>   ← RET lands here
        // 0x10010: RET
        let mut code = alloc::vec![OP_HALT; 0x1_0011];
        code[..5].copy_from_slice(&[OP_JMP, 0x00, 0x00, 0x01, 0x00]);
        code[0x1_0000..0x1_0005].copy_from_slice(&[OP_CALL, 0x10, 0x00, 0x01, 0x00]);
        code[0x1_0005] = 0xFF;
        code[0x1_0010] = OP_RET;

        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        assert!(vm.load_ram(&image(IMAGE_FLAG_WIDE_ADDR, &code)));
        vm.run(&mut ctx);
        assert_eq!(vm.fault(), fault::BAD_OPCODE);
        assert_eq!(vm.fault_pc(), 0x1_0005);
    }
}
