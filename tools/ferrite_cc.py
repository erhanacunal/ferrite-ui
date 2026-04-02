#!/usr/bin/env python3
"""ferrite-ui bytecode compiler / assembler / disassembler.

Bytecode format: 1-byte instruction set with .NET IL-style specialized short forms.
  - No-arg ops: single byte
  - Short forms: PUSH_0..2, PUSH_M1, LOAD_0..4, STORE_0..4 (1 byte)
  - With args: opcode + fixed-size arguments (i8/i16/i32/u8/u16)
  - Builtins: first-class opcodes at 0x80+
  - Float ops: at 0xC0+

Usage:
  # CLI
  python ferrite_cc.py disasm firmware.bin
  python ferrite_cc.py disasm --page page0.bin
  python ferrite_cc.py hexdump firmware.bin

  # Python API
  from ferrite_cc import Asm, Compiler, Prop
"""

import struct
import sys

# ============================================================
# Constants
# ============================================================


class Op:
    """1-byte opcode map -- IL-style with specialized short forms."""
    # No-arg instructions (1 byte total)
    HALT       = 0x00
    POP        = 0x01
    DUP        = 0x02
    SWAP       = 0x03
    ADD        = 0x04
    SUB        = 0x05
    MUL        = 0x06
    DIV        = 0x07
    MOD        = 0x08
    NEG        = 0x09
    AND        = 0x0A
    OR         = 0x0B
    NOT        = 0x0C
    EQ         = 0x0D
    NE         = 0x0E
    LT         = 0x0F
    LE         = 0x10
    GT         = 0x11
    GE         = 0x12
    RET        = 0x13
    YIELD      = 0x14
    W_DIRTY    = 0x15
    W_RENDER   = 0x16
    ARR_LOAD   = 0x17
    ARR_STORE  = 0x18
    ARR_LEN    = 0x19
    W_ALLOC    = 0x1A
    ARR_FREE   = 0x1B

    # Specialized short forms (1 byte, no args)
    PUSH_0     = 0x20
    PUSH_1     = 0x21
    PUSH_2     = 0x22
    PUSH_M1    = 0x23  # push -1
    LOAD_0     = 0x24
    LOAD_1     = 0x25
    LOAD_2     = 0x26
    LOAD_3     = 0x27
    LOAD_4     = 0x28
    STORE_0    = 0x29
    STORE_1    = 0x2A
    STORE_2    = 0x2B
    STORE_3    = 0x2C
    STORE_4    = 0x2D

    # With fixed-size arguments
    PUSH_I8    = 0x30  # + i8  (1 byte signed)
    PUSH_I16   = 0x31  # + i16 (2 bytes LE signed)
    PUSH_I32   = 0x32  # + i32 (4 bytes LE signed)
    LOAD       = 0x33  # + u8 slot
    STORE      = 0x34  # + u8 slot
    JMP        = 0x35  # + u16 target
    JZ         = 0x36  # + u16 target
    JNZ        = 0x37  # + u16 target
    CALL       = 0x38  # + u16 target
    W_TARGET   = 0x39  # + u8 widget_id
    W_SET      = 0x3A  # + u8 prop_id (value from stack)
    W_GET      = 0x3B  # + u8 prop_id (pushes to stack)
    W_PARENT   = 0x3C  # + u8 parent_id
    W_SET_LEN  = 0x3D  # + u8 prop_id + u8 len + data (compound prop)
    ARR_ALLOC  = 0x3E  # + u8 size
    ARR_INIT   = 0x3F  # + u8 count + i32 values (LE)
    F_READ     = 0x40  # + u32 addr + u16 len
    F_WRITE    = 0x41  # + u32 addr + u8 len + data

    # Builtins as first-class opcodes (all no-arg, operands on stack)
    FILL_RECT       = 0x80
    RECT            = 0x81
    LINE            = 0x82
    CIRCLE          = 0x83
    FILL_CIRCLE     = 0x84
    DRAW_IMAGE      = 0x85
    DRAW_TEXT_LIT   = 0x86  # + u8 len + text (inline literal)
    DELAY           = 0x87
    STR_LIT         = 0x88  # + u8 len + text (inline literal)
    ITOS            = 0x89
    FTOS            = 0x8A
    CONCAT          = 0x8B
    PARSE_INT       = 0x8C
    PARSE_FLOAT     = 0x8D
    STR_LEN         = 0x8E
    SET_TEXT         = 0x8F
    DRAW_STR        = 0x90
    STR_CLEAR       = 0x91
    STR_FREE        = 0x92
    ROUNDED_RECT    = 0x93
    FILL_ROUNDED_RECT = 0x94
    ARC             = 0x95
    BEGIN_FRAME     = 0x96
    END_FRAME       = 0x97
    SEND_USART      = 0x98
    SEND_USART_STR  = 0x99
    RTC_READ        = 0x9A
    RTC_WRITE       = 0x9B

    # Float ops (all no-arg)
    ITOF = 0xC0
    FTOI = 0xC1
    FADD = 0xC2
    FSUB = 0xC3
    FMUL = 0xC4
    FDIV = 0xC5
    FNEG = 0xC6
    FEQ  = 0xC7
    FLT  = 0xC8
    FLE  = 0xC9
    FGT  = 0xCA
    FGE  = 0xCB
    FNE  = 0xCC


# Backwards compatibility -- Builtin constants map to method IDs (0-based).
# Used by ferrite_lang.py via asm.builtin(Builtin.XXX)
class Builtin:
    FILL_RECT = 0
    RECT = 1
    LINE = 2
    CIRCLE = 3
    FILL_CIRCLE = 4
    DRAW_IMAGE = 5
    DRAW_TEXT = 6
    DELAY = 7
    STR = 8
    ITOS = 9
    FTOS = 10
    CONCAT = 11
    PARSE_INT = 12
    PARSE_FLOAT = 13
    STR_LEN = 14
    SET_TEXT = 15
    DRAW_STR = 16
    STR_CLEAR = 17
    STR_FREE = 18
    ROUNDED_RECT = 19
    FILL_ROUNDED_RECT = 20
    ARC = 21
    BEGIN_FRAME = 22
    END_FRAME = 23
    SEND_USART = 24
    SEND_USART_STR = 25
    RTC_READ = 26
    RTC_WRITE = 27
    MILLIS = 28
    FPGA_CMD = 29
    FPGA_DAT = 30
    CRITICAL = 31
    SET_BRIGHTNESS = 32
    BRIGHTNESS = 33


class Prop:
    """Widget property IDs."""
    # Scalar
    LOC_X = 0x01
    LOC_Y = 0x02
    SIZE_W = 0x03
    SIZE_H = 0x04
    VISIBLE = 0x05
    ENABLED = 0x06
    CLICKABLE = 0x07
    BG_COLOR = 0x08
    BORDER_COLOR = 0x09
    FLAGS = 0x0A
    PARENT = 0x0B
    FIRST_CHILD = 0x0C
    NEXT_SIBLING = 0x0D
    KIND = 0x0E
    TEXT_COLOR = 0x0F
    MARGIN_T = 0x10
    MARGIN_R = 0x11
    MARGIN_B = 0x12
    MARGIN_L = 0x13
    BORDER_T = 0x14
    BORDER_R = 0x15
    BORDER_B = 0x16
    BORDER_L = 0x17
    PADDING_T = 0x18
    PADDING_R = 0x19
    PADDING_B = 0x1A
    PADDING_L = 0x1B
    FONT_ID = 0x1C
    TEXT_ALIGN = 0x1D
    PRESS_COLOR = 0x1E
    IMAGE_ID = 0x1F
    ON_CLICK = 0x20
    ON_PAINT = 0x21
    ON_TAP = 0x22
    BORDER_RADIUS = 0x23
    VALUE = 0x24
    CHECKED = 0x25
    # Compound (LEN wire type)
    LOCATION = 0x40
    SIZE = 0x41
    MARGIN = 0x42
    BORDER_EDGES = 0x43
    PADDING = 0x44
    TEXT = 0x45


# ============================================================
# Varint encoding / decoding (still used by USART protocol
# and compound property zigzag encoding in W_SET_LEN payload)
# ============================================================


def encode_varint(val):
    """Unsigned varint encode. Returns bytes."""
    assert val >= 0
    buf = bytearray()
    while val >= 0x80:
        buf.append((val & 0x7F) | 0x80)
        val >>= 7
    buf.append(val & 0x7F)
    return bytes(buf)


def decode_varint(data, pos=0):
    """Unsigned varint decode. Returns (value, bytes_consumed)."""
    result = 0
    shift = 0
    i = pos
    while True:
        if i >= len(data):
            raise ValueError("Truncated varint")
        byte = data[i]
        result |= (byte & 0x7F) << shift
        i += 1
        if byte & 0x80 == 0:
            return result, i - pos
        shift += 7
        if shift >= 35:
            raise ValueError("Varint overflow")


def zigzag_encode(n):
    """ZigZag: 0->0, -1->1, 1->2, -2->3, ..."""
    return (n << 1) ^ (n >> 31) if n >= 0 else (n << 1) ^ (n >> 31)


def zigzag_decode(n):
    """ZigZag decode: 0->0, 1->-1, 2->1, 3->-2, ..."""
    return (n >> 1) ^ -(n & 1)


def encode_svarint(val):
    """Signed varint (zigzag + varint)."""
    # Handle negative values for zigzag
    if val < 0:
        zz = ((-val) << 1) - 1
    else:
        zz = val << 1
    return encode_varint(zz)


def decode_svarint(data, pos=0):
    """Signed varint decode. Returns (value, bytes_consumed)."""
    raw, consumed = decode_varint(data, pos)
    return zigzag_decode(raw), consumed


# Alias for disassembler internal use
_decode_svarint = decode_svarint


def float_bits(f):
    """Convert a Python float to its f32 bit pattern as a signed i32.

    Usage: float_bits(3.14) -> i32 representation of f32
    """
    import struct as _st
    bits = _st.unpack('<I', _st.pack('<f', f))[0]
    if bits >= 0x80000000:
        return bits - 0x100000000
    return bits


def pack_pair(high, low):
    """Pack two u16 values into a single i32: (high << 16) | low.

    Convention:
      location: pack_pair(x, y)
      size:     pack_pair(w, h)
      colors:   pack_pair(fg, bg)
    """
    val = ((high & 0xFFFF) << 16) | (low & 0xFFFF)
    # Convert to signed i32 for VM stack
    if val >= 0x80000000:
        val -= 0x100000000
    return val


# ============================================================
# Assembler
# ============================================================


class Asm:
    """Low-level bytecode assembler. 1-byte opcode instruction set.

    Example:
        a = Asm()
        a.w_alloc()
        a.push(0)
        a.store(0)
        a.w_target(0)
        a.push(0xF800)
        a.w_set(Prop.BG_COLOR)
        a.halt()
        code = a.build()
    """

    def __init__(self):
        self._buf = bytearray()

    @property
    def pos(self):
        """Current write position (byte offset)."""
        return len(self._buf)

    # --- Emit primitives ---

    def _emit(self, data):
        if isinstance(data, int):
            self._buf.append(data)
        else:
            self._buf.extend(data)

    def _emit_u8(self, val):
        self._buf.append(val & 0xFF)

    def _emit_i8(self, val):
        self._buf.append(val & 0xFF)

    def _emit_u16(self, val):
        self._buf.extend(struct.pack('<H', val & 0xFFFF))

    def _emit_i16(self, val):
        self._buf.extend(struct.pack('<h', val))

    def _emit_i32(self, val):
        self._buf.extend(struct.pack('<i', val))

    # --- No-arg instructions (single byte) ---

    def halt(self):
        self._emit(Op.HALT)

    def pop(self):
        self._emit(Op.POP)

    def dup(self):
        self._emit(Op.DUP)

    def swap(self):
        self._emit(Op.SWAP)

    def add(self):
        self._emit(Op.ADD)

    def sub(self):
        self._emit(Op.SUB)

    def mul(self):
        self._emit(Op.MUL)

    def div(self):
        self._emit(Op.DIV)

    def modulo(self):
        self._emit(Op.MOD)

    # Alias for compat
    def mod_(self):
        self._emit(Op.MOD)

    def neg(self):
        self._emit(Op.NEG)

    def and_(self):
        self._emit(Op.AND)

    def or_(self):
        self._emit(Op.OR)

    def not_(self):
        self._emit(Op.NOT)

    def eq(self):
        self._emit(Op.EQ)

    def ne(self):
        self._emit(Op.NE)

    def lt(self):
        self._emit(Op.LT)

    def le(self):
        self._emit(Op.LE)

    def gt(self):
        self._emit(Op.GT)

    def ge(self):
        self._emit(Op.GE)

    def ret(self):
        self._emit(Op.RET)

    def yield_(self):
        self._emit(Op.YIELD)

    def w_dirty(self):
        self._emit(Op.W_DIRTY)

    def w_render(self):
        self._emit(Op.W_RENDER)

    def arr_load(self):
        """Pop [arr_id, index], push arr[index]."""
        self._emit(Op.ARR_LOAD)

    def arr_store(self):
        """Pop [arr_id, index, value], store value."""
        self._emit(Op.ARR_STORE)

    def arr_len(self):
        """Pop arr_id, push length."""
        self._emit(Op.ARR_LEN)

    def w_alloc(self):
        self._emit(Op.W_ALLOC)

    def arr_free(self):
        """Pop arr_id, free the array."""
        self._emit(Op.ARR_FREE)

    # --- Specialized short forms + general forms ---

    def push(self, val):
        if val == 0:
            self._emit(Op.PUSH_0)
        elif val == 1:
            self._emit(Op.PUSH_1)
        elif val == 2:
            self._emit(Op.PUSH_2)
        elif val == -1:
            self._emit(Op.PUSH_M1)
        elif -128 <= val <= 127:
            self._emit(Op.PUSH_I8)
            self._emit_i8(val)
        elif -32768 <= val <= 32767:
            self._emit(Op.PUSH_I16)
            self._emit_i16(val)
        else:
            self._emit(Op.PUSH_I32)
            self._emit_i32(val)

    def load(self, slot):
        if 0 <= slot <= 4:
            self._emit(Op.LOAD_0 + slot)
        else:
            self._emit(Op.LOAD)
            self._emit_u8(slot)

    def store(self, slot):
        if 0 <= slot <= 4:
            self._emit(Op.STORE_0 + slot)
        else:
            self._emit(Op.STORE)
            self._emit_u8(slot)

    # --- Jump/call -- opcode + u16 LE target ---

    def jmp(self, target):
        self._emit(Op.JMP)
        self._emit_u16(target)

    def jz(self, target):
        self._emit(Op.JZ)
        self._emit_u16(target)

    def jnz(self, target):
        self._emit(Op.JNZ)
        self._emit_u16(target)

    def call(self, target):
        self._emit(Op.CALL)
        self._emit_u16(target)

    # --- Forward jump helpers ---

    def jmp_fwd(self):
        """JMP placeholder. Returns patch position for later patching."""
        self._emit(Op.JMP)
        pos = self.pos
        self._emit_u16(0)
        return pos

    def jz_fwd(self):
        """JZ placeholder. Returns patch position."""
        self._emit(Op.JZ)
        pos = self.pos
        self._emit_u16(0)
        return pos

    def jnz_fwd(self):
        """JNZ placeholder. Returns patch position."""
        self._emit(Op.JNZ)
        pos = self.pos
        self._emit_u16(0)
        return pos

    def call_fwd(self):
        """CALL placeholder. Returns patch position."""
        self._emit(Op.CALL)
        pos = self.pos
        self._emit_u16(0)
        return pos

    def patch(self, patch_pos, target=None):
        """Patch a forward jump. Default target = current position."""
        if target is None:
            target = self.pos
        struct.pack_into('<H', self._buf, patch_pos, target & 0xFFFF)

    # --- Widget ops ---

    def w_target(self, widget_id):
        self._emit(Op.W_TARGET)
        self._emit_u8(widget_id)

    def w_set(self, prop_id):
        """Set scalar property (value from stack)."""
        self._emit(Op.W_SET)
        self._emit_u8(prop_id)

    def w_set_compound(self, prop_id, values):
        """Set compound property (inline data, zigzag-encoded values)."""
        payload = bytearray()
        for v in values:
            payload.extend(encode_svarint(v))
        self._emit(Op.W_SET_LEN)
        self._emit_u8(prop_id)
        self._emit_u8(len(payload))
        self._emit(payload)

    def w_set_text(self, text):
        """Set text property (inline text bytes)."""
        text_bytes = text.encode('utf-8') if isinstance(text, str) else bytes(text)
        self._emit(Op.W_SET_LEN)
        self._emit_u8(Prop.TEXT)
        self._emit_u8(len(text_bytes))
        self._emit(text_bytes)

    def w_get(self, prop_id):
        self._emit(Op.W_GET)
        self._emit_u8(prop_id)

    def w_parent(self, parent_id):
        self._emit(Op.W_PARENT)
        self._emit_u8(parent_id)

    # --- Array ops ---

    def arr_alloc(self, size):
        """Allocate zero-filled array. Pushes array_id."""
        self._emit(Op.ARR_ALLOC)
        self._emit_u8(size)

    def arr_alloc_init(self, values):
        """Allocate and init array from values. Pushes array_id."""
        self._emit(Op.ARR_INIT)
        self._emit_u8(len(values))
        for v in values:
            self._emit_i32(v)

    # --- Flash ops ---

    def f_read(self, addr, length):
        """F_READ: flash addr (4B LE) + length (2B LE)."""
        self._emit(Op.F_READ)
        self._emit(struct.pack('<IH', addr, length))

    def f_write(self, addr, data):
        """F_WRITE: flash addr (4B LE) + len (1B) + data bytes."""
        self._emit(Op.F_WRITE)
        self._emit(struct.pack('<I', addr))
        self._emit_u8(len(data))
        self._emit(data)

    # --- Float32 instructions (soft-float, all no-arg) ---

    def itof(self):
        """Pop i32, push f32 bits."""
        self._emit(Op.ITOF)

    def ftoi(self):
        """Pop f32 bits, push i32."""
        self._emit(Op.FTOI)

    def fadd(self):
        """Pop two f32, push f32 sum."""
        self._emit(Op.FADD)

    def fsub(self):
        """Pop two f32, push f32 difference (a - b)."""
        self._emit(Op.FSUB)

    def fmul(self):
        """Pop two f32, push f32 product."""
        self._emit(Op.FMUL)

    def fdiv(self):
        """Pop two f32, push f32 quotient (a / b)."""
        self._emit(Op.FDIV)

    def fneg(self):
        """Pop f32, push negated f32."""
        self._emit(Op.FNEG)

    def feq(self):
        """Pop two f32, push i32 (1 if a == b, else 0)."""
        self._emit(Op.FEQ)

    def flt(self):
        """Pop two f32, push i32 (1 if a < b, else 0)."""
        self._emit(Op.FLT)

    def fle(self):
        """Pop two f32, push i32 (1 if a <= b, else 0)."""
        self._emit(Op.FLE)

    def fgt(self):
        """Pop two f32, push i32 (1 if a > b, else 0)."""
        self._emit(Op.FGT)

    def fge(self):
        """Pop two f32, push i32 (1 if a >= b, else 0)."""
        self._emit(Op.FGE)

    def fne(self):
        """Pop two f32, push i32 (1 if a != b, else 0)."""
        self._emit(Op.FNE)

    # --- Built-in methods ---

    def builtin(self, method_id):
        """Emit a builtin as its opcode. Maps old Builtin constant (0-based)
        to new opcode (0x80+)."""
        opcode = Op.FILL_RECT + method_id
        self._emit(opcode)

    def builtin_len(self, method_id, payload):
        """Emit a builtin with inline payload (drawText or str literal)."""
        if method_id == Builtin.DRAW_TEXT:
            self._emit(Op.DRAW_TEXT_LIT)
        elif method_id == Builtin.STR:
            self._emit(Op.STR_LIT)
        else:
            # Fallback: should not happen
            self._emit(Op.FILL_RECT + method_id)
            return
        text = payload.encode('utf-8') if isinstance(payload, str) else bytes(payload)
        self._emit_u8(len(text))
        self._emit(text)

    # --- High-level drawing helpers ---

    def fill_rect(self, x, y, w, h, color):
        """Emit fillRect built-in: push packed args + opcode."""
        self.push(pack_pair(x, y))
        self.push(pack_pair(w, h))
        self.push(color)
        self._emit(Op.FILL_RECT)

    def draw_rect(self, x, y, w, h, color):
        """Emit rect outline built-in: push packed args + opcode."""
        self.push(pack_pair(x, y))
        self.push(pack_pair(w, h))
        self.push(color)
        self._emit(Op.RECT)

    def draw_line(self, x0, y0, x1, y1, color):
        """Emit line built-in: push packed args + opcode."""
        self.push(pack_pair(x0, y0))
        self.push(pack_pair(x1, y1))
        self.push(color)
        self._emit(Op.LINE)

    def draw_circle(self, cx, cy, r, color):
        """Emit circle outline built-in: push packed args + opcode."""
        self.push(pack_pair(cx, cy))
        self.push(r)
        self.push(color)
        self._emit(Op.CIRCLE)

    def fill_circle(self, cx, cy, r, color):
        """Emit filled circle built-in: push packed args + opcode."""
        self.push(pack_pair(cx, cy))
        self.push(r)
        self.push(color)
        self._emit(Op.FILL_CIRCLE)

    def draw_image(self, x, y, image_id):
        """Emit drawImage built-in: push packed args + opcode."""
        self.push(pack_pair(x, y))
        self.push(image_id)
        self._emit(Op.DRAW_IMAGE)

    def draw_text(self, x, y, font_id, fg, bg, text):
        """Emit drawText built-in: push stack args + inline text payload.
        bg=0 means transparent.
        """
        self.push(pack_pair(x, y))
        self.push(font_id)
        self.push(pack_pair(fg, bg))
        self.builtin_len(Builtin.DRAW_TEXT, text)

    def delay(self, ms):
        """Emit delay_ms built-in: push ms + opcode."""
        self.push(ms)
        self._emit(Op.DELAY)

    # --- String operations ---

    def str_alloc(self, text):
        """Create a string from literal text. Pushes str_id."""
        self.builtin_len(Builtin.STR, text)

    def str_itos(self):
        """Pop i32, push str_id of decimal representation."""
        self._emit(Op.ITOS)

    def str_ftos(self):
        """Pop f32 bits, push str_id of float representation."""
        self._emit(Op.FTOS)

    def str_concat(self):
        """Pop [str_b, str_a], push concatenated str_id."""
        self._emit(Op.CONCAT)

    def str_parse_int(self):
        """Pop str_id, push parsed i32."""
        self._emit(Op.PARSE_INT)

    def str_parse_float(self):
        """Pop str_id, push parsed f32 bits."""
        self._emit(Op.PARSE_FLOAT)

    def str_len(self):
        """Pop str_id, push length."""
        self._emit(Op.STR_LEN)

    def str_set_text(self):
        """Pop str_id, set as text on current target widget."""
        self._emit(Op.SET_TEXT)

    def str_draw(self):
        """Pop [str_id, colors, font_id, loc], draw string."""
        self._emit(Op.DRAW_STR)

    def str_clear(self):
        """Smart clear: free unreferenced strings, preserve widget text."""
        self._emit(Op.STR_CLEAR)

    def str_free(self):
        """Pop str_id, mark for reclamation on next strClear()."""
        self._emit(Op.STR_FREE)

    # --- Rounded rect & arc ---

    def draw_rounded_rect(self, x, y, w, h, r, color):
        """Emit roundedRect built-in."""
        self.push(pack_pair(x, y))
        self.push(pack_pair(w, h))
        self.push(r)
        self.push(color)
        self._emit(Op.ROUNDED_RECT)

    def fill_rounded_rect(self, x, y, w, h, r, color):
        """Emit fillRoundedRect built-in."""
        self.push(pack_pair(x, y))
        self.push(pack_pair(w, h))
        self.push(r)
        self.push(color)
        self._emit(Op.FILL_ROUNDED_RECT)

    def draw_arc(self, cx, cy, r, start, end, color):
        """Emit arc built-in. Angles in degrees (0=right, 90=down)."""
        self.push(pack_pair(cx, cy))
        self.push(r)
        self.push(start)
        self.push(end)
        self.push(color)
        self._emit(Op.ARC)

    # --- Output ---

    def build(self):
        """Return assembled bytecode as bytes."""
        return bytes(self._buf)

    def save(self, path):
        """Save raw bytecode to file."""
        with open(path, 'wb') as f:
            f.write(self.build())

    def hexdump(self):
        """Return hex dump string."""
        data = self.build()
        lines = []
        for i in range(0, len(data), 16):
            chunk = data[i:i + 16]
            hex_part = ' '.join(f'{b:02X}' for b in chunk)
            ascii_part = ''.join(chr(b) if 32 <= b < 127 else '.' for b in chunk)
            lines.append(f'{i:04X}: {hex_part:<48s} {ascii_part}')
        return '\n'.join(lines)


# ============================================================
# Disassembler
# ============================================================

OP_NAMES = {
    0x00: 'HALT', 0x01: 'POP', 0x02: 'DUP', 0x03: 'SWAP',
    0x04: 'ADD', 0x05: 'SUB', 0x06: 'MUL', 0x07: 'DIV',
    0x08: 'MOD', 0x09: 'NEG', 0x0A: 'AND', 0x0B: 'OR',
    0x0C: 'NOT', 0x0D: 'EQ', 0x0E: 'NE', 0x0F: 'LT',
    0x10: 'LE', 0x11: 'GT', 0x12: 'GE', 0x13: 'RET',
    0x14: 'YIELD', 0x15: 'W_DIRTY', 0x16: 'W_RENDER',
    0x17: 'ARR_LOAD', 0x18: 'ARR_STORE', 0x19: 'ARR_LEN',
    0x1A: 'W_ALLOC', 0x1B: 'arrFree',

    0x20: 'PUSH_0', 0x21: 'PUSH_1', 0x22: 'PUSH_2', 0x23: 'PUSH_M1',
    0x24: 'LOAD_0', 0x25: 'LOAD_1', 0x26: 'LOAD_2', 0x27: 'LOAD_3', 0x28: 'LOAD_4',
    0x29: 'STORE_0', 0x2A: 'STORE_1', 0x2B: 'STORE_2', 0x2C: 'STORE_3', 0x2D: 'STORE_4',

    0x30: 'PUSH_I8', 0x31: 'PUSH_I16', 0x32: 'PUSH_I32',
    0x33: 'LOAD', 0x34: 'STORE',
    0x35: 'JMP', 0x36: 'JZ', 0x37: 'JNZ', 0x38: 'CALL',
    0x39: 'W_TARGET', 0x3A: 'W_SET', 0x3B: 'W_GET', 0x3C: 'W_PARENT',
    0x3D: 'W_SET_LEN', 0x3E: 'ARR_ALLOC', 0x3F: 'ARR_INIT',
    0x40: 'F_READ', 0x41: 'F_WRITE',

    0x80: 'fillRect', 0x81: 'rect', 0x82: 'line', 0x83: 'circle',
    0x84: 'fillCircle', 0x85: 'drawImage', 0x86: 'drawTextLit',
    0x87: 'delay', 0x88: 'strLit', 0x89: 'itos', 0x8A: 'ftos',
    0x8B: 'concat', 0x8C: 'parseInt', 0x8D: 'parseFloat',
    0x8E: 'strLen', 0x8F: 'setText', 0x90: 'drawStr',
    0x91: 'strClear', 0x92: 'strFree',
    0x93: 'roundedRect', 0x94: 'fillRoundedRect', 0x95: 'arc',
    0x96: 'beginFrame', 0x97: 'endFrame',
    0x98: 'sendUsart', 0x99: 'sendUsartStr',
    0x9A: 'rtcRead', 0x9B: 'rtcWrite', 0x9C: 'millis',
    0x9D: 'fpgaCmd', 0x9E: 'fpgaData', 0x9F: 'critical',
    0xA0: 'setBrightness', 0xA1: 'brightness',

    0xC0: 'itof', 0xC1: 'ftoi', 0xC2: 'fadd', 0xC3: 'fsub',
    0xC4: 'fmul', 0xC5: 'fdiv', 0xC6: 'fneg',
    0xC7: 'feq', 0xC8: 'flt', 0xC9: 'fle',
    0xCA: 'fgt', 0xCB: 'fge', 0xCC: 'fne',
}

PROP_NAMES = {
    0x01: 'LOC_X', 0x02: 'LOC_Y', 0x03: 'SIZE_W', 0x04: 'SIZE_H',
    0x05: 'VISIBLE', 0x06: 'ENABLED', 0x07: 'CLICKABLE',
    0x08: 'BG_COLOR', 0x09: 'BORDER_COLOR',
    0x0A: 'FLAGS', 0x0B: 'PARENT', 0x0C: 'FIRST_CHILD', 0x0D: 'NEXT_SIBLING',
    0x0E: 'KIND', 0x0F: 'TEXT_COLOR',
    0x1C: 'FONT_ID', 0x1D: 'TEXT_ALIGN', 0x1E: 'PRESS_COLOR',
    0x10: 'MARGIN_T', 0x11: 'MARGIN_R', 0x12: 'MARGIN_B', 0x13: 'MARGIN_L',
    0x14: 'BORDER_T', 0x15: 'BORDER_R', 0x16: 'BORDER_B', 0x17: 'BORDER_L',
    0x18: 'PADDING_T', 0x19: 'PADDING_R', 0x1A: 'PADDING_B', 0x1B: 'PADDING_L',
    0x1F: 'IMAGE_ID',
    0x20: 'ON_CLICK', 0x21: 'ON_PAINT', 0x22: 'ON_TAP', 0x23: 'BORDER_RADIUS', 0x24: 'VALUE', 0x25: 'CHECKED',
    0x40: 'LOCATION', 0x41: 'SIZE', 0x42: 'MARGIN',
    0x43: 'BORDER_EDGES', 0x44: 'PADDING', 0x45: 'TEXT',
}

# Set of no-arg opcodes for disassembler
_NO_ARG_OPS = (
    set(range(0x00, 0x1C)) |           # 0x00-0x1B
    set(range(0x20, 0x2E)) |           # 0x20-0x2D
    {op for op in range(0x80, 0xA2)    # 0x80-0xA1 except 0x86, 0x88
     if op not in (0x86, 0x88)} |
    set(range(0xC0, 0xCD))             # 0xC0-0xCC
)


def disassemble(data, labels=None):
    """Disassemble bytecode to human-readable text.

    labels: optional dict {offset: "label"} — inserts '; --- label ---'
            comment lines before the instruction at that offset.
    """
    labels = labels or {}
    lines = []
    pos = 0
    while pos < len(data):
        addr = pos
        if addr in labels:
            lines.append(f'')
            lines.append(f'; --- {labels[addr]} ---')
        op = data[pos]
        pos += 1
        name = OP_NAMES.get(op, f'??? (0x{op:02X})')

        # No-arg ops
        if op in _NO_ARG_OPS:
            lines.append(f'{addr:04X}: {name}')

        # PUSH_I8
        elif op == Op.PUSH_I8:
            val = struct.unpack_from('<b', data, pos)[0]
            pos += 1
            lines.append(f'{addr:04X}: PUSH {val}')

        # PUSH_I16
        elif op == Op.PUSH_I16:
            val = struct.unpack_from('<h', data, pos)[0]
            pos += 2
            lines.append(f'{addr:04X}: PUSH {val} (0x{val & 0xFFFF:04X})')

        # PUSH_I32
        elif op == Op.PUSH_I32:
            val = struct.unpack_from('<i', data, pos)[0]
            pos += 4
            lines.append(f'{addr:04X}: PUSH {val} (0x{val & 0xFFFFFFFF:08X})')

        # LOAD / STORE with u8 slot
        elif op == Op.LOAD:
            slot = data[pos]; pos += 1
            lines.append(f'{addr:04X}: LOAD {slot}')
        elif op == Op.STORE:
            slot = data[pos]; pos += 1
            lines.append(f'{addr:04X}: STORE {slot}')

        # JMP/JZ/JNZ/CALL with u16 target
        elif op in (Op.JMP, Op.JZ, Op.JNZ, Op.CALL):
            target = struct.unpack_from('<H', data, pos)[0]
            pos += 2
            lines.append(f'{addr:04X}: {name} @{target:04X}')

        # W_TARGET, W_SET, W_GET, W_PARENT with u8 arg
        elif op == Op.W_TARGET:
            wid = data[pos]; pos += 1
            lines.append(f'{addr:04X}: W_TARGET {wid}')
        elif op == Op.W_SET:
            prop = data[pos]; pos += 1
            pname = PROP_NAMES.get(prop, f'0x{prop:02X}')
            lines.append(f'{addr:04X}: W_SET {pname}')
        elif op == Op.W_GET:
            prop = data[pos]; pos += 1
            pname = PROP_NAMES.get(prop, f'0x{prop:02X}')
            lines.append(f'{addr:04X}: W_GET {pname}')
        elif op == Op.W_PARENT:
            pid = data[pos]; pos += 1
            lines.append(f'{addr:04X}: W_PARENT {pid}')

        # W_SET_LEN: prop_id + len + data
        elif op == Op.W_SET_LEN:
            prop = data[pos]; pos += 1
            length = data[pos]; pos += 1
            payload = data[pos:pos+length]; pos += length
            pname = PROP_NAMES.get(prop, f'0x{prop:02X}')
            if prop == Prop.TEXT:
                try:
                    text = bytes(payload).decode('utf-8')
                    lines.append(f'{addr:04X}: W_SET {pname} "{text}"')
                except Exception:
                    lines.append(f'{addr:04X}: W_SET {pname} [{" ".join(f"0x{b:02X}" for b in payload)}]')
            else:
                vals = []
                p = 0
                while p < len(payload):
                    v, c = _decode_svarint(payload, p)
                    vals.append(str(v))
                    p += c
                lines.append(f'{addr:04X}: W_SET {pname} ({", ".join(vals)})')

        # ARR_ALLOC: u8 size
        elif op == Op.ARR_ALLOC:
            size = data[pos]; pos += 1
            lines.append(f'{addr:04X}: ARR_ALLOC {size}')

        # ARR_INIT: u8 count + i32 values
        elif op == Op.ARR_INIT:
            count = data[pos]; pos += 1
            vals = []
            for _ in range(count):
                v = struct.unpack_from('<i', data, pos)[0]
                vals.append(str(v))
                pos += 4
            lines.append(f'{addr:04X}: ARR_INIT [{", ".join(vals)}]')

        # F_READ: u32 addr + u16 len
        elif op == Op.F_READ:
            faddr = struct.unpack_from('<I', data, pos)[0]; pos += 4
            flen = struct.unpack_from('<H', data, pos)[0]; pos += 2
            lines.append(f'{addr:04X}: F_READ 0x{faddr:08X} len={flen}')

        # F_WRITE: u32 addr + u8 len + data
        elif op == Op.F_WRITE:
            faddr = struct.unpack_from('<I', data, pos)[0]; pos += 4
            flen = data[pos]; pos += 1
            pos += flen
            lines.append(f'{addr:04X}: F_WRITE 0x{faddr:08X} len={flen}')

        # DRAW_TEXT_LIT, STR_LIT: u8 len + text
        elif op in (Op.DRAW_TEXT_LIT, Op.STR_LIT):
            length = data[pos]; pos += 1
            text_bytes = data[pos:pos+length]; pos += length
            try:
                text = bytes(text_bytes).decode('utf-8')
                lines.append(f'{addr:04X}: {name} "{text}"')
            except Exception:
                lines.append(f'{addr:04X}: {name} [{length} bytes]')

        else:
            lines.append(f'{addr:04X}: ??? 0x{op:02X}')

    return '\n'.join(lines)


# ============================================================
# Property name resolution
# ============================================================

# (prop_id, is_compound)
PROP_MAP = {
    'loc_x': (Prop.LOC_X, False),
    'loc_y': (Prop.LOC_Y, False),
    'size_w': (Prop.SIZE_W, False),
    'width': (Prop.SIZE_W, False),
    'size_h': (Prop.SIZE_H, False),
    'height': (Prop.SIZE_H, False),
    'visible': (Prop.VISIBLE, False),
    'enabled': (Prop.ENABLED, False),
    'clickable': (Prop.CLICKABLE, False),
    'bg_color': (Prop.BG_COLOR, False),
    'background_color': (Prop.BG_COLOR, False),
    'border_color': (Prop.BORDER_COLOR, False),
    'flags': (Prop.FLAGS, False),
    'location': (Prop.LOCATION, True),
    'pos': (Prop.LOCATION, True),
    'size': (Prop.SIZE, True),
    'margin': (Prop.MARGIN, True),
    'border': (Prop.BORDER_EDGES, True),
    'padding': (Prop.PADDING, True),
    'margin_top': (Prop.MARGIN_T, False),
    'margin_right': (Prop.MARGIN_R, False),
    'margin_bottom': (Prop.MARGIN_B, False),
    'margin_left': (Prop.MARGIN_L, False),
    'border_top': (Prop.BORDER_T, False),
    'border_right': (Prop.BORDER_R, False),
    'border_bottom': (Prop.BORDER_B, False),
    'border_left': (Prop.BORDER_L, False),
    'padding_top': (Prop.PADDING_T, False),
    'padding_right': (Prop.PADDING_R, False),
    'padding_bottom': (Prop.PADDING_B, False),
    'padding_left': (Prop.PADDING_L, False),
    'kind': (Prop.KIND, False),
    'text_color': (Prop.TEXT_COLOR, False),
    'font_id': (Prop.FONT_ID, False),
    'text_align': (Prop.TEXT_ALIGN, False),
    'press_color': (Prop.PRESS_COLOR, False),
    'image_id': (Prop.IMAGE_ID, False),
    'on_click': (Prop.ON_CLICK, False),
    'on_paint': (Prop.ON_PAINT, False),
    'on_tap': (Prop.ON_TAP, False),
    'border_radius': (Prop.BORDER_RADIUS, False),
    'radius': (Prop.BORDER_RADIUS, False),
    'value': (Prop.VALUE, False),
    'checked': (Prop.CHECKED, False),
    'text': (Prop.TEXT, True),
    'text_id': (Prop.TEXT, True),  # alias -- same underlying prop
}


def _resolve_prop(name):
    """Resolve property name or ID to (prop_id, is_compound)."""
    if isinstance(name, int):
        return name, name >= 0x40
    key = name.lower()
    if key not in PROP_MAP:
        raise ValueError(f"Unknown property: {name}")
    return PROP_MAP[key]


# ============================================================
# Compiler (high-level API)
# ============================================================


class FunctionKind:
    """Function kind enum — matches Rust FunctionKind."""
    SETUP = 0
    LOOP = 1
    USER_FUNCTION = 2
    ON_PROGRAM_START = 3
    ON_PAGE_CHANGING = 4
    ON_PAGE_CHANGED = 5
    ON_USER_MESSAGE = 6
    ON_TOUCH_DOWN = 7
    ON_TOUCH_UP = 8
    ON_TOUCH_MOVE = 9


# Map system callback names to FunctionKind values
SYSTEM_CALLBACK_KINDS = {
    'setup':            FunctionKind.SETUP,
    'loop':             FunctionKind.LOOP,
    'on_program_start': FunctionKind.ON_PROGRAM_START,
    'on_page_changing': FunctionKind.ON_PAGE_CHANGING,
    'on_page_changed':  FunctionKind.ON_PAGE_CHANGED,
    'on_user_message':  FunctionKind.ON_USER_MESSAGE,
    'on_touch_down':    FunctionKind.ON_TOUCH_DOWN,
    'on_touch_up':      FunctionKind.ON_TOUCH_UP,
    'on_touch_move':    FunctionKind.ON_TOUCH_MOVE,
}


class Compiler:
    """High-level bytecode compiler with widget and control flow helpers.

    Example:
        cc = Compiler()

        cc.alloc("root")
        cc.target("root")
        cc.set_prop("size", 800, 480)
        cc.set_prop("bg_color", 0x0000)

        cc.alloc("panel")
        cc.target("panel")
        cc.set_prop("location", 150, 100)
        cc.set_prop("size", 500, 280)
        cc.set_prop("bg_color", 0x001F)
        cc.set_prop("border", 3, 3, 3, 3)
        cc.set_prop("border_color", 0xFFFF)
        cc.parent("root")

        cc.dirty()
        cc.render()
        cc.halt()

        cc.save("page0.bin", page_bg=0x0000)
        print(cc.disassemble())
    """

    def __init__(self, base_id=0):
        self.asm = Asm()
        self._widgets = {}      # name -> predicted widget ID
        self._next_id = base_id
        self._vars = {}         # name -> var slot (0-15)
        self._next_var = 0
        self._funcs = {}        # name -> (func_id, kind, offset, length)
        self._next_func_id = 1

    # --- Widget management ---

    def alloc(self, name):
        """Allocate a widget, register with name. Returns predicted ID.

        Widget IDs are sequential: base_id, base_id+1, ...
        """
        widget_id = self._next_id
        self._widgets[name] = widget_id
        self._next_id += 1
        self.asm.w_alloc()
        return widget_id

    def target(self, name_or_id):
        """Set target widget for subsequent property operations."""
        wid = self._resolve_widget(name_or_id)
        self.asm.w_target(wid)

    def set_prop(self, name, *values):
        """Set property on current target.

        Scalar:   cc.set_prop("bg_color", 0xF800)
        Compound: cc.set_prop("size", 800, 480)
                  cc.set_prop("border", 2, 2, 2, 2)
        """
        prop_id, is_compound = _resolve_prop(name)
        if is_compound:
            self.asm.w_set_compound(prop_id, list(values))
        else:
            if len(values) != 1:
                raise ValueError(f"Scalar property '{name}' takes 1 value, got {len(values)}")
            self.asm.push(values[0])
            self.asm.w_set(prop_id)

    def get_prop(self, name):
        """Push property value of current target onto stack."""
        prop_id, _ = _resolve_prop(name)
        self.asm.w_get(prop_id)

    def parent(self, name_or_id):
        """Set current target's parent."""
        pid = self._resolve_widget(name_or_id)
        self.asm.w_parent(pid)

    def set_text(self, text):
        """Set text on current target (label widget)."""
        self.asm.w_set_text(text)

    def dirty(self):
        """Mark current target as dirty."""
        self.asm.w_dirty()

    def render(self):
        """Trigger dirty render."""
        self.asm.w_render()

    def halt(self):
        self.asm.halt()

    def ret(self):
        self.asm.ret()

    def yield_(self):
        self.asm.yield_()

    # --- Variables ---

    def var(self, name, init_value=0):
        """Declare a VM variable (max 32). Returns var slot."""
        if self._next_var >= 32:
            raise RuntimeError("Max 32 VM variables")
        var_id = self._next_var
        self._vars[name] = var_id
        self._next_var += 1
        self.asm.push(init_value)
        self.asm.store(var_id)
        return var_id

    def set_var(self, name, value):
        """Set variable to immediate value."""
        var_id = self._resolve_var(name)
        self.asm.push(value)
        self.asm.store(var_id)

    def load_var(self, name):
        """Push variable value onto stack."""
        var_id = self._resolve_var(name)
        self.asm.load(var_id)

    # --- Control flow ---

    def if_begin(self):
        """Consume top of stack, jump forward if zero.
        Returns handle for if_end() or else_begin().

        Usage:
            cc.load_var("x")
            cc.asm.push(10)
            cc.asm.lt()           # x < 10 ?
            h = cc.if_begin()
            # ... then block ...
            cc.if_end(h)
        """
        return self.asm.jz_fwd()

    def else_begin(self, if_handle):
        """Start else block. Returns handle for if_end().

        Usage:
            h = cc.if_begin()
            # ... then ...
            h2 = cc.else_begin(h)
            # ... else ...
            cc.if_end(h2)
        """
        else_handle = self.asm.jmp_fwd()
        self.asm.patch(if_handle)
        return else_handle

    def if_end(self, handle):
        """End if or else block."""
        self.asm.patch(handle)

    def while_begin(self):
        """Mark loop start. Returns start position.

        Usage:
            loop = cc.while_begin()
            cc.load_var("i")
            cc.asm.push(10)
            cc.asm.lt()
            exit_h = cc.while_cond()
            # ... loop body ...
            cc.while_end(loop, exit_h)
        """
        return self.asm.pos

    def while_cond(self):
        """Consume top of stack. Jump past loop if zero. Returns handle."""
        return self.asm.jz_fwd()

    def while_end(self, start, cond_handle):
        """Jump back to loop start, patch exit jump."""
        self.asm.jmp(start)
        self.asm.patch(cond_handle)

    # --- Functions ---

    def define_func(self, name, kind=FunctionKind.USER_FUNCTION):
        """Define a function at current bytecode position.

        Returns func_id (1-based). The function body follows this call
        and should end with cc.ret() or cc.halt().

        kind: FunctionKind constant. Auto-detected from name if in
              SYSTEM_CALLBACK_KINDS.

        Example:
            fid = cc.define_func("on_btn_click")
            cc.asm.store(0)   # save widget_id arg to var 0
            # ... callback body ...
            cc.ret()
        """
        # Auto-detect kind from name
        if name in SYSTEM_CALLBACK_KINDS:
            kind = SYSTEM_CALLBACK_KINDS[name]

        func_id = self._next_func_id
        offset = self.asm.pos
        # length will be set later via set_func_length or build_image_header
        self._funcs[name] = (func_id, kind, offset, 0)
        self._next_func_id += 1
        return func_id

    def set_func_length(self, name, length):
        """Set the bytecode length for a previously defined function."""
        if name not in self._funcs:
            raise ValueError(f"Unknown function: {name}")
        fid, kind, offset, _ = self._funcs[name]
        self._funcs[name] = (fid, kind, offset, length)

    def build_image_header(self):
        """Build VM image header binary.

        Format:
          version(u8) + function_count(u16 LE) + reserved(u16)
          + [func_id(u16) + kind(u8) + pad(u8) + offset(u32) + length(u32)] × N

        Each function entry is 12 bytes.
        """
        func_list = list(self._funcs.values())
        buf = bytearray()
        buf.append(1)  # version
        buf.extend(struct.pack('<H', len(func_list)))
        buf.extend(struct.pack('<H', 0))  # reserved
        for func_id, kind, offset, length in func_list:
            buf.extend(struct.pack('<HBBI', func_id, kind, 0, offset))
            buf.extend(struct.pack('<I', length))
        return bytes(buf)

    def has_functions(self):
        """Check if any functions have been defined."""
        return len(self._funcs) > 0

    # --- Output ---

    def build(self):
        """Return raw bytecode."""
        return self.asm.build()

    def build_page(self, bg_color=0x0000):
        """Build page format: bg_color (u16 LE) + bytecode."""
        return struct.pack('<H', bg_color) + self.build()

    def save(self, path, page_bg=None):
        """Save to file. If page_bg is set, use page format."""
        data = self.build_page(page_bg) if page_bg is not None else self.build()
        with open(path, 'wb') as f:
            f.write(data)
        return len(data)

    def disassemble(self):
        """Return disassembly of generated bytecode."""
        return disassemble(self.build())

    def hexdump(self):
        """Return hex dump."""
        return self.asm.hexdump()

    # --- Internal ---

    def _resolve_widget(self, name_or_id):
        if isinstance(name_or_id, str):
            if name_or_id not in self._widgets:
                raise ValueError(f"Unknown widget: {name_or_id}")
            return self._widgets[name_or_id]
        return int(name_or_id)

    def _resolve_var(self, name):
        if isinstance(name, str):
            if name not in self._vars:
                raise ValueError(f"Unknown variable: {name}")
            return self._vars[name]
        return int(name)


# ============================================================
# RGB565 color helpers
# ============================================================


def rgb565(r, g, b):
    """Convert 8-bit RGB to RGB565.

    Usage: rgb565(255, 0, 0) -> 0xF800 (red)
    """
    return ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b >> 3)


# Common colors
COLOR_BLACK = 0x0000
COLOR_WHITE = 0xFFFF
COLOR_RED = 0xF800
COLOR_GREEN = 0x07E0
COLOR_BLUE = 0x001F


# ============================================================
# CLI
# ============================================================


def main():
    if len(sys.argv) < 2:
        print("Usage:")
        print("  ferrite_cc.py disasm [--page] <file.bin>")
        print("  ferrite_cc.py hexdump <file.bin>")
        print("")
        print("Python API:")
        print("  from ferrite_cc import Compiler, Prop, rgb565")
        sys.exit(0)

    cmd = sys.argv[1]

    if cmd == 'disasm':
        page_mode = '--page' in sys.argv
        path = [a for a in sys.argv[2:] if not a.startswith('-')][0]

        with open(path, 'rb') as f:
            data = f.read()

        if page_mode:
            if len(data) < 2:
                print("Error: file too small for page format")
                sys.exit(1)
            bg = struct.unpack_from('<H', data)[0]
            print(f'; page bg_color: 0x{bg:04X} ({bg})')
            print(f'; bytecode: {len(data) - 2} bytes')
            print()
            data = data[2:]

        print(disassemble(data))

    elif cmd == 'hexdump':
        path = sys.argv[2]
        with open(path, 'rb') as f:
            data = f.read()
        a = Asm()
        a._buf = bytearray(data)
        print(a.hexdump())

    else:
        print(f"Unknown command: {cmd}")
        sys.exit(1)


if __name__ == '__main__':
    main()
