extern crate alloc;

use alloc::vec::Vec;
use crate::ctx::Ctx;
use crate::flash::Flash;
use crate::strpool;
use crate::systick;
use crate::proto::{
    self, PROP_BG_COLOR, PROP_BORDER_B,
    PROP_BORDER_COLOR, PROP_BORDER_EDGES, PROP_BORDER_L, PROP_BORDER_R, PROP_BORDER_T,
    PROP_CLICKABLE, PROP_ENABLED, PROP_FONT_ID, PROP_GRADIENT_COLOR, PROP_GRADIENT_DIR,
    PROP_KIND, PROP_LOCATION, PROP_LOC_X, PROP_LOC_Y,
    PROP_MARGIN, PROP_MARGIN_B, PROP_MARGIN_L, PROP_MARGIN_R, PROP_MARGIN_T, PROP_PADDING,
    PROP_IMAGE_ID, PROP_ON_CLICK, PROP_ON_PAINT, PROP_ON_TAP, PROP_BORDER_RADIUS, PROP_VALUE, PROP_CHECKED,
    PROP_MAX_LENGTH, PROP_CURSOR_POS, PROP_ON_CHANGE, PROP_SCROLL_Y, PROP_CLIP_CHILDREN,
    PROP_PADDING_B, PROP_PADDING_L, PROP_PADDING_R, PROP_PADDING_T,
    PROP_PRESS_COLOR, PROP_SIZE, PROP_SIZE_H, PROP_SIZE_W,
    PROP_TEXT, PROP_TEXT_ALIGN, PROP_TEXT_COLOR, PROP_VISIBLE,
    PROP_GRAPH_ARR, PROP_GRAPH_COUNT, PROP_GRAPH_FLAGS,
};
use crate::render;
use crate::types::{Edges, Offset, Size};
use crate::widget::{WidgetId, FLAG_CHECKED, FLAG_CLICKABLE, FLAG_CLIP_CHILDREN, FLAG_ENABLED, FLAG_VISIBLE};

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
    Flash {
        flash: Flash,
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
                if pos < data.len() { data[pos] } else { 0 }
            }
            VmCode::Flash { flash, base_addr, code_len,
                            cache, cache_pos, cache_len } => {
                if pos >= *code_len { return 0; }
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
                if pos >= data.len() { return 0; }
                let avail = data.len() - pos;
                let n = buf.len().min(avail);
                buf[..n].copy_from_slice(&data[pos..pos + n]);
                n
            }
            VmCode::Flash { flash, base_addr, code_len,
                            cache, cache_pos, cache_len } => {
                if pos >= *code_len { return 0; }
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
const OP_FRAME: u8 = 0x1D;   // + u8 local_count (function prologue)

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
const OP_W_TARGET_S: u8 = 0x42; // widget id from stack (dynamic target)
const OP_W_PARENT_S: u8 = 0x43; // parent id from stack (dynamic parent)

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
const OP_RTC_READ: u8 = 0x9A;
const OP_RTC_WRITE: u8 = 0x9B;
const OP_MILLIS: u8 = 0x9C;
const OP_FPGA_CMD: u8 = 0x9D; // pop cmd, pop data → send_command(cmd), send_data(data)
const OP_FPGA_DAT: u8 = 0x9E; // pop data → send_data(data)
const OP_CRITICAL: u8 = 0x9F; // enter critical section — run until next yield
const OP_SET_BRIGHTNESS: u8 = 0xA0; // pop percent → set backlight
const OP_BRIGHTNESS: u8 = 0xA1; // push current backlight percent
const OP_FILE_OPEN: u8 = 0xA2;  // pop str_id (name) → push handle (1|2|0xFF)
const OP_FILE_READ: u8 = 0xA3;  // pop handle → push byte (0..255) or -1 on EOF
const OP_FILE_SIZE: u8 = 0xA4;  // pop handle → push size
const OP_FILE_CLOSE: u8 = 0xA5; // pop handle, release slot
const OP_ARR_TO_STR: u8 = 0xA6; // pop len, pop arr_id → alloc string from low bytes of arr elements (len<0 = full array)
const OP_SHOW_MODAL: u8 = 0xA7; // pop click_fn, pop builder_fn → suspend until setDialogResult, push result on resume
const OP_SET_DIALOG_RESULT: u8 = 0xA8; // pop result → record on top modal frame; void return

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
    /// Suspended after `showModal`; the user program is paused until
    /// `setDialogResult` fires (typically from a callback) and the main loop
    /// calls `try_resume_modal`.
    AwaitingModal,
    Error,
}

// --- VM Mode ---

#[derive(Clone, Copy, PartialEq)]
pub enum VmMode {
    Normal,
    Callback,
}

// --- VM ---

const STACK_SIZE: usize = 16;
/// Variable entry — sparse mapping from slot ID to value.
struct VmVar {
    id: u16,
    val: i32,
}
const CALL_STACK_SIZE: usize = 8;

/// Internal heap-allocated VM array.
struct VmArray {
    id: u16,
    data: Vec<i32>,
}

/// Call stack frame: return address + saved frame state.
#[derive(Clone, Copy)]
struct CallFrame {
    ret_addr: u16,
    frame_base: u8,
    frame_size: u8,
}

/// Saved VM state for callback push/pop.
struct VmSnapshot {
    pc: u16,
    sp: u8,
    stack: [i32; STACK_SIZE],
    call_sp: u8,
    call_stack: [CallFrame; CALL_STACK_SIZE],
    target: WidgetId,
    state: VmState,
    wait_until: u32,
    critical: bool,
    frame_base: u8,
    frame_size: u8,
}

/// Maximum number of arguments per queued callback.
const CB_MAX_ARGS: usize = 3;
/// Maximum number of queued callbacks.
const CB_QUEUE_SIZE: usize = 8;

/// A queued callback request.
#[derive(Clone, Copy)]
struct CbRequest {
    offset: u16,
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

pub struct Vm {
    pc: u16,
    stack: [i32; STACK_SIZE],
    sp: u8,
    vars: Vec<VmVar>,
    call_stack: [CallFrame; CALL_STACK_SIZE],
    call_sp: u8,
    target: WidgetId,
    pub state: VmState,
    arrays: Vec<VmArray>,
    next_arr_id: u16,
    pub wait_until: u32,
    // Code source — owned by VM
    code: VmCode,
    // Function table from image header
    functions: Vec<FuncEntry>,
    // Normal or callback execution mode
    pub mode: VmMode,
    // Stack of saved states for nested callback execution. A callback that
    // suspends on a modal (showModal) leaves its frame in place while later
    // callbacks (e.g. setDialogResult) run on top, then resumes once the
    // modal closes — see try_resume_modal.
    saved: Vec<VmSnapshot>,
    // Callback queue — FIFO ring buffer
    cb_queue: [CbRequest; CB_QUEUE_SIZE],
    cb_head: u8,
    cb_tail: u8,
    // Critical section — when true, VM runs until next YIELD without returning to main loop
    critical: bool,
    // Stack-based locals: frame_base = first local slot for current function
    frame_base: u8,
    frame_size: u8,
    global_count: u8,
    // Render mode from image header
    pub render_mode: RenderMode,
    // Widget allocation hint from v4 image header (0 = unknown, use fallback).
    pub widget_count: u8,
    pub ext_count: u8,
    // Open file slots — index 0 = handle 1, index 1 = handle 2.
    open_files: [Option<OpenFile>; 2],
    // Open modal dialogs (innermost = last). Empty when no dialog is showing.
    modal_stack: Vec<ModalFrame>,
}

impl Vm {
    pub fn new() -> Self {
        const EMPTY_CB: CbRequest = CbRequest {
            offset: 0, arg_count: 0, args: [0; CB_MAX_ARGS],
        };
        const EMPTY_FRAME: CallFrame = CallFrame {
            ret_addr: 0, frame_base: 0, frame_size: 0,
        };
        Self {
            pc: 0,
            stack: [0; STACK_SIZE],
            sp: 0,
            vars: Vec::new(),
            call_stack: [EMPTY_FRAME; CALL_STACK_SIZE],
            call_sp: 0,
            target: WidgetId::NONE,
            state: VmState::Ready,
            arrays: Vec::new(),
            next_arr_id: 0,
            wait_until: 0,
            code: VmCode::None,
            functions: Vec::new(),
            mode: VmMode::Normal,
            saved: Vec::new(),
            cb_queue: [EMPTY_CB; CB_QUEUE_SIZE],
            cb_head: 0,
            cb_tail: 0,
            critical: false,
            frame_base: 0,
            frame_size: 0,
            global_count: 0,
            render_mode: RenderMode::Dirty,
            widget_count: 0,
            ext_count: 0,
            open_files: [None, None],
            modal_stack: Vec::new(),
        }
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
            if v.id == slot { return v.val; }
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
        if let Some((funcs, opcode_start, gc, rm, wc, ec)) = parse_image_header(image) {
            let opcodes = image[opcode_start..].to_vec();
            self.code = VmCode::Ram(opcodes);
            self.functions = funcs;
            self.global_count = gc;
            self.frame_base = gc;
            self.frame_size = 0;
            self.render_mode = rm;
            self.widget_count = wc;
            self.ext_count = ec;
            true
        } else {
            false
        }
    }

    /// Load a VM image from flash. Reads only the header into RAM;
    /// opcodes are read on demand from flash via SPI.
    pub fn load_flash(&mut self, flash: &Flash, base: u32, total_len: usize) -> bool {
        // Read header into a stack buffer
        let hdr_max = IMAGE_HEADER_SIZE_V4 + FUNC_ENTRY_SIZE * 64; // max 64 functions
        let read_len = total_len.min(hdr_max);
        let mut hdr_buf = [0u8; IMAGE_HEADER_SIZE_V4 + FUNC_ENTRY_SIZE * 64]; // 777 bytes on stack
        flash.read(base, &mut hdr_buf[..read_len]);

        if let Some((funcs, opcode_start, gc, rm, wc, ec)) = parse_image_header(&hdr_buf[..read_len]) {
            let opcode_base = base + opcode_start as u32;
            let opcode_len = total_len.saturating_sub(opcode_start);
            self.code = VmCode::Flash {
                flash: Flash::new(),
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
            self.render_mode = rm;
            self.widget_count = wc;
            self.ext_count = ec;
            true
        } else {
            false
        }
    }

    /// Load raw bytecode (no image header). Used for page bytecode.
    pub fn load_raw(&mut self, data: &[u8]) {
        self.code = VmCode::Ram(data.to_vec());
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
    pub fn enqueue_callback(&mut self, offset: u16, args: &[i32]) {
        let next_tail = (self.cb_tail + 1) % CB_QUEUE_SIZE as u8;
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

    /// Run one queued callback to completion. Saves/restores VM state.
    /// Returns false if the queue was empty.
    ///
    /// A callback that suspends on a modal dialog (showModal) leaves its
    /// frame on the saved stack — pop_state runs only when the callback
    /// actually finishes (Halted/Yielded). The suspended frame is later
    /// resumed by `try_resume_modal` when `setDialogResult` fires.
    pub fn run_next_callback(&mut self, ctx: &mut Ctx) -> bool {
        if self.cb_head == self.cb_tail {
            return false;
        }
        let req = self.cb_queue[self.cb_head as usize];
        self.cb_head = (self.cb_head + 1) % CB_QUEUE_SIZE as u8;

        self.push_state(req.offset);
        for i in 0..req.arg_count as usize {
            self.push_arg(req.args[i]);
        }
        self.run(ctx);
        if self.state == VmState::AwaitingModal {
            // Callback is suspended on showModal. Leave its frame in place;
            // try_resume_modal will resume + pop_state when result arrives.
            return true;
        }
        self.pop_state();
        true
    }

    /// Drain all queued callbacks, executing each to completion.
    pub fn drain_callbacks(&mut self, ctx: &mut Ctx) {
        while self.run_next_callback(ctx) {}
    }

    /// Run a callback immediately (not queued). Used during startup
    /// when the main loop is not yet running.
    pub fn run_callback(&mut self, offset: u16, ctx: &mut Ctx) {
        self.push_state(offset);
        self.run(ctx);
        self.pop_state();
    }

    // === Modal dialog support ===

    /// Implements OP_SHOW_MODAL. Pops `overlay_click_fn` then `builder_fn`
    /// from the eval stack, pushes a modal frame, and issues a CALL to the
    /// builder function. The user program is left dangling at the byte after
    /// OP_SHOW_MODAL — the builder's eventual OP_RET drops us into
    /// `handle_builder_return`, which transitions VM state to AwaitingModal.
    fn handle_show_modal(&mut self, _ctx: &mut Ctx) {
        let click_fn = self.pop() as u16;
        let builder_fn = self.pop() as u16;

        if self.modal_stack.len() >= MODAL_STACK_MAX {
            // Nested too deep — emulate immediate Cancel.
            self.push(DLG_CANCEL);
            return;
        }

        // Resolve builder function id → entry offset.
        let builder_offset = match self.find_func(builder_fn) {
            Some(f) => f.offset as u16,
            None => {
                // Bad fn id; emulate Cancel rather than VM error so the user
                // program keeps running.
                self.push(DLG_CANCEL);
                return;
            }
        };

        if (self.call_sp as usize) >= CALL_STACK_SIZE {
            self.state = VmState::Error;
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
    fn handle_builder_return(&mut self, ctx: &mut Ctx) {
        let overlay_raw = self.pop();
        let click_fn = match self.modal_stack.last() {
            Some(t) => t.overlay_click_fn,
            None => return,
        };

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
        if let Some(ext) = ctx.tree.ensure_ext(overlay) {
            ext.on_click = click_fn;
        }
        ctx.tree.get_mut(overlay).flags |= crate::widget::FLAG_CLICKABLE;
        // Repaint the whole screen on next frame so the overlay's full-screen
        // backdrop covers everything underneath in both buffers.
        ctx.tree.mark_dirty(WidgetId(0));

        let top = self.modal_stack.last_mut().unwrap();
        top.overlay_id = overlay;
        top.state = ModalState::AwaitingResult;

        self.state = VmState::AwaitingModal;
    }

    /// Called by the main loop each tick. If a modal has had its result set
    /// by `setDialogResult`, destroy the overlay subtree, push the result onto
    /// the eval stack as `showModal`'s return value, and resume the VM.
    ///
    /// If the suspended showModal was issued from inside a callback (saved
    /// stack non-empty), this pumps the callback to completion and then
    /// pop_state's its frame — the callback is effectively synchronous from
    /// the outside, even though main-loop event processing happened during
    /// the dialog.
    pub fn try_resume_modal(&mut self, ctx: &mut Ctx) {
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

        self.push(result);
        self.state = VmState::Running;

        // If we suspended inside a callback, finish it now so its scope's
        // "var r = showModal(...); if (r == YES) { ... }" runs to completion
        // and its frame is properly torn down. Without this the callback
        // would keep its slot on `saved` forever.
        if !self.saved.is_empty() {
            self.run(ctx);
            // Callback completed (Halted/Yielded/Error) or hit another nested
            // showModal. Only pop_state for completion; nested suspension
            // leaves the new frame in place.
            if self.state != VmState::AwaitingModal {
                self.pop_state();
            }
        }
    }

    /// True if a modal dialog is open (regardless of phase). Useful for the
    /// main loop, e.g. to skip routine input if a dialog has visual focus.
    pub fn modal_active(&self) -> bool {
        !self.modal_stack.is_empty()
    }

    // --- Internal: save/restore VM state for callback execution ---

    fn push_state(&mut self, offset: u16) {
        let snapshot = VmSnapshot {
            pc: self.pc,
            sp: self.sp,
            stack: self.stack,
            call_sp: self.call_sp,
            call_stack: self.call_stack,
            target: self.target,
            state: self.state,
            wait_until: self.wait_until,
            critical: self.critical,
            frame_base: self.frame_base,
            frame_size: self.frame_size,
        };
        self.saved.push(snapshot);
        self.pc = offset;
        self.sp = 0;
        self.call_sp = 0;
        self.target = WidgetId::NONE;
        self.mode = VmMode::Callback;
        self.state = VmState::Running;
        self.critical = false;
        // Place the callback's frame above the interrupted function's live
        // locals so its L0.. slots don't alias (and overwrite) them. Matches
        // the OP_CALL convention: new frame_base = frame_base + frame_size.
        // Clamp to at least global_count so the very first callback (setup)
        // doesn't collide with the globals when frame_base is still 0.
        let above = self.frame_base.wrapping_add(self.frame_size);
        self.frame_base = if above > self.global_count { above } else { self.global_count };
        self.frame_size = 0;
    }

    fn pop_state(&mut self) {
        if let Some(snap) = self.saved.pop() {
            self.pc = snap.pc;
            self.sp = snap.sp;
            self.stack = snap.stack;
            self.call_sp = snap.call_sp;
            self.call_stack = snap.call_stack;
            self.target = snap.target;
            self.state = snap.state;
            self.wait_until = snap.wait_until;
            self.critical = snap.critical;
            self.frame_base = snap.frame_base;
            self.frame_size = snap.frame_size;
            // Outer level still in a callback if more frames remain on the
            // stack. Otherwise we're back in the main user program.
            self.mode = if self.saved.is_empty() {
                VmMode::Normal
            } else {
                VmMode::Callback
            };
        }
    }

    // === Public API ===

    pub fn set_target(&mut self, id: WidgetId) {
        self.target = id;
    }

    pub fn set_pc(&mut self, pc: u16) {
        self.pc = pc;
    }

    pub fn pc(&self) -> u16 {
        self.pc
    }

    pub fn is_critical(&self) -> bool {
        self.critical
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
        self.vars.clear();
        self.arrays.clear();
        self.next_arr_id = 0;
        self.wait_until = 0;
        self.code = VmCode::None;
        self.functions.clear();
        self.mode = VmMode::Normal;
        self.saved.clear();
        self.frame_base = 0;
        self.frame_size = 0;
        self.global_count = 0;
        self.cb_head = 0;
        self.cb_tail = 0;
        self.critical = false;
        self.render_mode = RenderMode::Dirty;
        self.open_files = [None, None];
        self.modal_stack.clear();
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

    pub fn run(&mut self, ctx: &mut Ctx) {
        self.state = VmState::Running;
        loop {
            match self.state {
                VmState::Running => self.step(ctx),
                VmState::Waiting => {
                    // Busy-wait for delay to expire (used during setup/callbacks)
                    if systick::millis().wrapping_sub(self.wait_until) < 0x8000_0000 {
                        self.state = VmState::Running;
                    }
                }
                _ => break, // Halted, Yielded, Error → exit
            }
        }
    }

    // --- Byte reading helpers (read from self.code) ---

    #[inline]
    fn read_u8(&mut self) -> u8 {
        let pos = self.pc as usize;
        if pos < self.code.len() {
            self.pc += 1;
            self.code.byte_at(pos)
        } else {
            self.state = VmState::Error;
            0
        }
    }

    #[inline]
    fn read_i8(&mut self) -> i8 {
        self.read_u8() as i8
    }

    #[inline]
    fn read_u16(&mut self) -> u16 {
        let pos = self.pc as usize;
        if pos + 2 <= self.code.len() {
            self.pc += 2;
            let mut buf = [0u8; 2];
            self.code.read_bytes(pos, &mut buf);
            u16::from_le_bytes(buf)
        } else {
            self.state = VmState::Error;
            0
        }
    }

    #[inline]
    fn read_i16(&mut self) -> i16 {
        self.read_u16() as i16
    }

    #[inline]
    fn read_i32(&mut self) -> i32 {
        let pos = self.pc as usize;
        if pos + 4 <= self.code.len() {
            self.pc += 4;
            let mut buf = [0u8; 4];
            self.code.read_bytes(pos, &mut buf);
            i32::from_le_bytes(buf)
        } else {
            self.state = VmState::Error;
            0
        }
    }

    #[inline]
    fn read_u32(&mut self) -> u32 {
        self.read_i32() as u32
    }

    // --- Execute a single instruction ---

    pub fn step(&mut self, ctx: &mut Ctx) {
        let op = self.read_u8();
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
                } else if self.mode == VmMode::Callback {
                    // Callback complete — halt cleanly
                    self.state = VmState::Halted;
                } else {
                    self.state = VmState::Error;
                }
            }
            OP_YIELD => {
                self.critical = false;
                self.state = VmState::Yielded;
            }
            OP_W_DIRTY => {
                if self.target.is_some() { ctx.tree.mark_dirty(self.target); }
            }
            OP_W_RENDER => match self.render_mode {
                RenderMode::Buffered => {
                    if render::buffered_has_dirty(ctx) {
                        // Reborrow self as immutable while ctx stays mut —
                        // render reads array data via Vm::array_slice.
                        render::render_buffered(ctx, &*self);
                    }
                }
                RenderMode::Dirty => render::render_dirty(ctx, &*self),
            },
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
            OP_W_ALLTAR => {
                // Combined: alloc widget, store to var slot, set as target
                let raw = self.read_u8();
                let slot = self.resolve_slot(raw);
                if let Some(id) = ctx.tree.alloc() {
                    self.var_set(slot, id.0 as i32);
                    self.target = id;
                } else {
                    self.state = VmState::Error;
                }
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
            OP_PUSH_0  => self.push(0),
            OP_PUSH_1  => self.push(1),
            OP_PUSH_2  => self.push(2),
            OP_PUSH_M1 => self.push(-1),
            // Short forms: LOAD/STORE local slots 0-4
            OP_LOAD_0  => { let s = self.frame_base as u16; let v = self.var_get(s); self.push(v); }
            OP_LOAD_1  => { let s = self.frame_base as u16 + 1; let v = self.var_get(s); self.push(v); }
            OP_LOAD_2  => { let s = self.frame_base as u16 + 2; let v = self.var_get(s); self.push(v); }
            OP_LOAD_3  => { let s = self.frame_base as u16 + 3; let v = self.var_get(s); self.push(v); }
            OP_LOAD_4  => { let s = self.frame_base as u16 + 4; let v = self.var_get(s); self.push(v); }
            OP_STORE_0 => { let s = self.frame_base as u16; let v = self.pop(); self.var_set(s, v); }
            OP_STORE_1 => { let s = self.frame_base as u16 + 1; let v = self.pop(); self.var_set(s, v); }
            OP_STORE_2 => { let s = self.frame_base as u16 + 2; let v = self.pop(); self.var_set(s, v); }
            OP_STORE_3 => { let s = self.frame_base as u16 + 3; let v = self.pop(); self.var_set(s, v); }
            OP_STORE_4 => { let s = self.frame_base as u16 + 4; let v = self.pop(); self.var_set(s, v); }

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
                let target = self.read_u16();
                self.pc = target;
            }
            OP_JZ => {
                let target = self.read_u16();
                if self.pop() == 0 { self.pc = target; }
            }
            OP_JNZ => {
                let target = self.read_u16();
                if self.pop() != 0 { self.pc = target; }
            }
            OP_CALL => {
                let target = self.read_u16();
                if (self.call_sp as usize) < CALL_STACK_SIZE {
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
                    self.state = VmState::Error;
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
                if self.target.is_some() {
                    ctx.tree.add_child(WidgetId(parent as u16), self.target);
                }
            }
            OP_W_PARENT_S => {
                // Dynamic parent — id from stack. Used when the parent is a
                // widget variable (its slot stores the runtime id from alloc).
                let parent = self.pop() as u16;
                if self.target.is_some() {
                    ctx.tree.add_child(WidgetId(parent), self.target);
                }
            }
            OP_W_SET_LEN => {
                let prop_id = self.read_u8();
                let len = self.read_u8() as usize;
                let start = self.pc as usize;
                let end = start + len;
                if end <= self.code.len() {
                    let mut buf = [0u8; 32];
                    let n = len.min(buf.len());
                    self.code.read_bytes(start, &mut buf[..n]);
                    self.pc = end as u16;
                    if self.target.is_some() {
                        self.set_compound_prop(prop_id, &buf[..n], ctx);
                    }
                } else {
                    self.state = VmState::Error;
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
                let addr = self.read_u32();
                let len = self.read_u16() as usize;
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
                let addr = self.read_u32();
                let len = self.read_u8() as usize;
                let start = self.pc as usize;
                if start + len <= self.code.len() {
                    let mut buf = [0u8; 64];
                    let n = len.min(buf.len());
                    self.code.read_bytes(start, &mut buf[..n]);
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
                let len = self.read_u8() as usize;
                let start = self.pc as usize;
                if start + len <= self.code.len() {
                    let mut buf = [0u8; 64];
                    let n = len.min(buf.len());
                    self.code.read_bytes(start, &mut buf[..n]);
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
                let len = self.read_u8() as usize;
                let start = self.pc as usize;
                if start + len <= self.code.len() {
                    let mut buf = [0u8; 64];
                    let n = len.min(buf.len());
                    self.code.read_bytes(start, &mut buf[..n]);
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
                    if let Some(ext) = ctx.tree.ensure_ext(self.target) {
                        ext.text_id = str_id;
                    }
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
            OP_BEGIN_FRAME => {
                // In buffered mode, render_buffered handles begin/end frame
                if self.render_mode != RenderMode::Buffered {
                    ctx.lcd.begin_frame();
                }
            }
            OP_END_FRAME => {
                if self.render_mode != RenderMode::Buffered {
                    ctx.lcd.end_frame();
                }
            }
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
            OP_RTC_READ => {
                // Read RTC → allocate array [sec, min, hour, day, weekday, month, year]
                let rtc = crate::rtc::Rtc::init();
                let dt = rtc.read_time();
                let data = alloc::vec![
                    dt.second as i32, dt.minute as i32, dt.hour as i32,
                    dt.day as i32, dt.weekday as i32, dt.month as i32,
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
                            let rtc = crate::rtc::Rtc::init();
                            rtc.set_time(&dt);
                        } else {
                            self.state = VmState::Error;
                        }
                    }
                    None => self.state = VmState::Error,
                }
            }

            OP_MILLIS => {
                self.push(systick::millis() as i32);
            }
            OP_FPGA_CMD => {
                // fpgaCmd(cmd, data) — raw FPGA command + data
                let data = self.pop() as u16;
                let cmd = self.pop() as u16;
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
                    None => self.state = VmState::Error,
                }
            }
            OP_FILE_SIZE => {
                let handle = self.pop();
                match self.file_slot(handle) {
                    Some(f) => self.push(f.size as i32),
                    None => self.state = VmState::Error,
                }
            }
            OP_FILE_CLOSE => {
                let handle = self.pop();
                match self.file_slot_index(handle) {
                    Some(idx) => self.open_files[idx] = None,
                    None => self.state = VmState::Error,
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
                let len = self.pop();
                let arr_id = self.pop();
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
                            self.state = VmState::Error;
                        } else {
                            for i in 0..take {
                                buf[i] = data[i] as u8;
                            }
                            match ctx.strpool.alloc(&buf[..take]) {
                                Some(id) => self.push(id as i32),
                                None => self.state = VmState::Error,
                            }
                        }
                    }
                    None => self.state = VmState::Error,
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
        // Base widget fields (no extension needed)
        match prop_id {
            PROP_LOC_X => {
                ctx.tree.get_mut(self.target).location.x = val as i16;
                ctx.tree.mark_dirty(self.target);
                return;
            }
            PROP_LOC_Y => {
                ctx.tree.get_mut(self.target).location.y = val as i16;
                ctx.tree.mark_dirty(self.target);
                return;
            }
            PROP_SIZE_W => {
                ctx.tree.get_mut(self.target).size.w = val as u16;
                ctx.tree.mark_dirty(self.target);
                return;
            }
            PROP_SIZE_H => {
                ctx.tree.get_mut(self.target).size.h = val as u16;
                ctx.tree.mark_dirty(self.target);
                return;
            }
            PROP_VISIBLE => {
                set_flag(&mut ctx.tree.get_mut(self.target).flags, FLAG_VISIBLE, val != 0);
                ctx.tree.mark_dirty(self.target);
                return;
            }
            PROP_ENABLED => { set_flag(&mut ctx.tree.get_mut(self.target).flags, FLAG_ENABLED, val != 0); return; }
            PROP_CLICKABLE => { set_flag(&mut ctx.tree.get_mut(self.target).flags, FLAG_CLICKABLE, val != 0); return; }
            PROP_CHECKED => { set_flag(&mut ctx.tree.get_mut(self.target).flags, FLAG_CHECKED, val != 0); return; }
            PROP_CLIP_CHILDREN => { set_flag(&mut ctx.tree.get_mut(self.target).flags, FLAG_CLIP_CHILDREN, val != 0); return; }
            PROP_BG_COLOR => { ctx.tree.get_mut(self.target).background_color = val as u16; return; }
            PROP_BORDER_COLOR => { ctx.tree.get_mut(self.target).border_color = val as u16; return; }
            PROP_KIND => { ctx.tree.get_mut(self.target).kind = val as u8; return; }
            _ => {}
        }

        // Extension fields (allocate on demand)
        if let Some(ext) = ctx.tree.ensure_ext(self.target) {
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
                PROP_ON_CLICK => ext.on_click = val as u16,
                PROP_ON_PAINT => ext.on_paint = val as u16,
                PROP_ON_TAP => ext.on_tap = val as u16,
                PROP_MAX_LENGTH => ext.max_length = val as u8,
                PROP_CURSOR_POS => ext.value = val as i16,
                PROP_ON_CHANGE => ext.on_tap = val as u16,
                PROP_SCROLL_Y => ext.value = val as i16,
                PROP_GRADIENT_COLOR => ext.gradient_color = val as u16,
                PROP_GRADIENT_DIR => ext.gradient_dir = val as u8,
                // KIND_GRAPH props share storage with value/max_length/image_id;
                // safe because a single widget kind is exclusive.
                PROP_GRAPH_ARR => ext.value = val as i16,
                PROP_GRAPH_COUNT => ext.max_length = val as u8,
                PROP_GRAPH_FLAGS => ext.image_id = val as u8,
                _ => {}
            }
        }
    }

    fn get_scalar_prop(&self, ctx: &Ctx, prop_id: u8) -> i32 {
        let w = ctx.tree.get(self.target);
        match prop_id {
            // Base fields
            PROP_LOC_X => w.location.x as i32,
            PROP_LOC_Y => w.location.y as i32,
            PROP_SIZE_W => w.size.w as i32,
            PROP_SIZE_H => w.size.h as i32,
            PROP_VISIBLE => if w.flags & FLAG_VISIBLE != 0 { 1 } else { 0 },
            PROP_ENABLED => if w.flags & FLAG_ENABLED != 0 { 1 } else { 0 },
            PROP_CLICKABLE => if w.flags & FLAG_CLICKABLE != 0 { 1 } else { 0 },
            PROP_CHECKED => if w.flags & FLAG_CHECKED != 0 { 1 } else { 0 },
            PROP_CLIP_CHILDREN => if w.flags & FLAG_CLIP_CHILDREN != 0 { 1 } else { 0 },
            PROP_BG_COLOR => w.background_color as i32,
            PROP_BORDER_COLOR => w.border_color as i32,
            PROP_KIND => w.kind as i32,
            // Extension fields (return defaults when no extension)
            PROP_MARGIN_T => ctx.tree.margin(self.target).top as i32,
            PROP_MARGIN_R => ctx.tree.margin(self.target).right as i32,
            PROP_MARGIN_B => ctx.tree.margin(self.target).bottom as i32,
            PROP_MARGIN_L => ctx.tree.margin(self.target).left as i32,
            PROP_BORDER_T => ctx.tree.border(self.target).top as i32,
            PROP_BORDER_R => ctx.tree.border(self.target).right as i32,
            PROP_BORDER_B => ctx.tree.border(self.target).bottom as i32,
            PROP_BORDER_L => ctx.tree.border(self.target).left as i32,
            PROP_PADDING_T => ctx.tree.padding(self.target).top as i32,
            PROP_PADDING_R => ctx.tree.padding(self.target).right as i32,
            PROP_PADDING_B => ctx.tree.padding(self.target).bottom as i32,
            PROP_PADDING_L => ctx.tree.padding(self.target).left as i32,
            PROP_TEXT_COLOR => ctx.tree.text_color(self.target) as i32,
            PROP_FONT_ID => ctx.tree.font_id(self.target) as i32,
            PROP_TEXT_ALIGN => ctx.tree.text_align(self.target) as i32,
            PROP_PRESS_COLOR => ctx.tree.press_color(self.target) as i32,
            PROP_IMAGE_ID => ctx.tree.image_id(self.target) as i32,
            PROP_BORDER_RADIUS => ctx.tree.border_radius(self.target) as i32,
            PROP_VALUE => ctx.tree.value(self.target) as i32,
            PROP_ON_CLICK => ctx.tree.on_click(self.target) as i32,
            PROP_ON_PAINT => ctx.tree.on_paint(self.target) as i32,
            PROP_ON_TAP => ctx.tree.on_tap(self.target) as i32,
            PROP_MAX_LENGTH => ctx.tree.max_length(self.target) as i32,
            PROP_CURSOR_POS => ctx.tree.value(self.target) as i32,
            PROP_ON_CHANGE => ctx.tree.on_tap(self.target) as i32,
            PROP_SCROLL_Y => ctx.tree.value(self.target) as i32,
            PROP_GRADIENT_COLOR => ctx.tree.gradient_color(self.target) as i32,
            PROP_GRADIENT_DIR => ctx.tree.gradient_dir(self.target) as i32,
            PROP_GRAPH_ARR => ctx.tree.value(self.target) as i32,
            PROP_GRAPH_COUNT => ctx.tree.max_length(self.target) as i32,
            PROP_GRAPH_FLAGS => ctx.tree.image_id(self.target) as i32,
            _ => 0,
        }
    }

    fn set_compound_prop(&mut self, prop_id: u8, data: &[u8], ctx: &mut Ctx) {
        if prop_id == PROP_TEXT {
            if self.target.is_some() {
                if let Some(str_id) = ctx.strpool.alloc(data) {
                    if let Some(ext) = ctx.tree.ensure_ext(self.target) {
                        ext.text_id = str_id;
                    }
                } else {
                    self.state = VmState::Error;
                }
            }
            return;
        }

        let (vals, count) = proto::unpack_signed_varints(data);

        // Base fields
        match prop_id {
            PROP_LOCATION if count >= 2 => {
                let w = ctx.tree.get_mut(self.target);
                w.location = Offset { x: vals[0] as i16, y: vals[1] as i16 };
                ctx.tree.mark_dirty(self.target);
                return;
            }
            PROP_SIZE if count >= 2 => {
                let w = ctx.tree.get_mut(self.target);
                w.size = Size { w: vals[0] as u16, h: vals[1] as u16 };
                ctx.tree.mark_dirty(self.target);
                return;
            }
            _ => {}
        }

        // Extension fields
        if let Some(ext) = ctx.tree.ensure_ext(self.target) {
            match prop_id {
                PROP_MARGIN if count >= 4 => {
                    ext.margin = Edges::new(vals[0] as u8, vals[1] as u8, vals[2] as u8, vals[3] as u8);
                }
                PROP_BORDER_EDGES if count >= 4 => {
                    ext.border = Edges::new(vals[0] as u8, vals[1] as u8, vals[2] as u8, vals[3] as u8);
                }
                PROP_PADDING if count >= 4 => {
                    ext.padding = Edges::new(vals[0] as u8, vals[1] as u8, vals[2] as u8, vals[3] as u8);
                }
                _ => {}
            }
        }
    }

    // --- Array operations ---

    fn find_array(&self, arr_id: i32) -> Option<usize> {
        if arr_id < 0 { return None; }
        let id = arr_id as u16;
        self.arrays.iter().position(|a| a.id == id)
    }

    fn arr_free(&mut self, arr_id: i32) {
        if let Some(pos) = self.find_array(arr_id) {
            self.arrays.remove(pos);
        }
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

    /// Borrow a VM array's data by id. Used by the renderer (e.g. graph
    /// widget) to read sample buffers without copying. Returns None when the
    /// id does not name a live array.
    pub fn array_slice(&self, arr_id: u16) -> Option<&[i32]> {
        self.arrays.iter().find(|a| a.id == arr_id).map(|a| a.data.as_slice())
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
        if self.open_files[idx].is_some() { Some(idx) } else { None }
    }

    fn file_slot(&self, handle: i32) -> Option<&OpenFile> {
        self.file_slot_index(handle).map(|i| self.open_files[i].as_ref().unwrap())
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
    fn file_open(&mut self, ctx: &Ctx, name: &[u8]) -> i32 {
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
/// widget_count, ext_count). widget_count/ext_count are 0 for v1-v3 images.
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
fn parse_image_header(data: &[u8]) -> Option<(Vec<FuncEntry>, usize, u8, RenderMode, u8, u8)> {
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

    // v3+: flags field after global_count
    let (header_size, render_mode, widget_count, ext_count) = if version >= 4 {
        if data.len() < IMAGE_HEADER_SIZE_V4 {
            return None;
        }
        let flags = u16::from_le_bytes([data[5], data[6]]);
        let rm = if flags & 0x01 != 0 { RenderMode::Buffered } else { RenderMode::Dirty };
        (IMAGE_HEADER_SIZE_V4, rm, data[7], data[8])
    } else if version >= 3 {
        if data.len() < IMAGE_HEADER_SIZE_V3 {
            return None;
        }
        let flags = u16::from_le_bytes([data[5], data[6]]);
        let rm = if flags & 0x01 != 0 { RenderMode::Buffered } else { RenderMode::Dirty };
        (IMAGE_HEADER_SIZE_V3, rm, 0u8, 0u8)
    } else {
        (IMAGE_HEADER_SIZE_V2, RenderMode::Dirty, 0u8, 0u8)
    };

    let table_size = func_count * FUNC_ENTRY_SIZE;
    let opcode_start = header_size + table_size;

    if data.len() < opcode_start {
        return None;
    }

    let mut functions = Vec::with_capacity(func_count);
    for i in 0..func_count {
        let base = header_size + i * FUNC_ENTRY_SIZE;
        let func_id = u16::from_le_bytes([data[base], data[base + 1]]);
        let kind_byte = data[base + 2];
        // data[base + 3] = pad
        let offset = u32::from_le_bytes([
            data[base + 4], data[base + 5], data[base + 6], data[base + 7],
        ]);
        let length = u32::from_le_bytes([
            data[base + 8], data[base + 9], data[base + 10], data[base + 11],
        ]);

        let kind = FunctionKind::from_u8(kind_byte).unwrap_or(FunctionKind::UserFunction);

        functions.push(FuncEntry {
            func_id,
            kind,
            offset,
            length,
        });
    }

    Some((functions, opcode_start, global_count, render_mode, widget_count, ext_count))
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
