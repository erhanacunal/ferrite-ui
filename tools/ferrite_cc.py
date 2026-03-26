#!/usr/bin/env python3
"""ferrite-ui bytecode compiler / assembler / disassembler.

Bytecode format: protobuf tag encoding
  tag = (opcode << 3) | wire_type
  Wire types: 0=varint(zigzag), 1=i16(LE), 2=LEN, 5=no-arg

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
# Constants (proto.rs ile birebir aynı)
# ============================================================


class WT:
    """Wire types."""
    VARINT = 0
    I16 = 1
    LEN = 2
    NO_ARG = 5


class Op:
    """VM opcodes."""
    HALT = 0
    PUSH = 1
    POP = 2
    LOAD = 3
    STORE = 4
    ADD = 5
    SUB = 6
    EQ = 7
    LT = 8
    JMP = 9
    JZ = 10
    JNZ = 11
    W_TARGET = 12
    W_SET = 13
    W_GET = 14
    W_DIRTY = 15
    DUP = 16
    SWAP = 17
    MUL = 18
    DIV = 19
    MOD = 20
    NEG = 21
    AND = 22
    OR = 23
    NOT = 24
    NE = 25
    LE = 26
    GE = 27
    GT = 28
    CALL = 29
    RET = 30
    YIELD = 31
    W_RENDER = 32
    W_ALLOC = 33
    W_PARENT = 34
    F_READ = 35
    F_WRITE = 36
    ARR_ALLOC = 37
    ARR_LOAD = 38
    ARR_STORE = 39
    ARR_LEN = 40
    BUILTIN = 41
    # Float32 (soft-float, all no-arg)
    ITOF = 42
    FTOI = 43
    FADD = 44
    FSUB = 45
    FMUL = 46
    FDIV = 47
    FNEG = 48
    FEQ = 49
    FLT = 50
    FLE = 51
    FGT = 52
    FGE = 53
    FNE = 54


class Builtin:
    """Built-in method IDs for OP_BUILTIN."""
    FILL_RECT = 0    # stack: [color, size, loc]
    RECT = 1         # stack: [color, size, loc]
    LINE = 2         # stack: [color, end, start]
    CIRCLE = 3       # stack: [color, radius, center]
    FILL_CIRCLE = 4  # stack: [color, radius, center]
    DRAW_IMAGE = 5   # stack: [image_id, loc]
    DRAW_TEXT = 6    # stack: [colors, font_id, loc] + LEN payload text
    DELAY = 7        # stack: [ms]
    # String operations
    STR = 8          # LEN payload=bytes → [str_id]
    ITOS = 9         # stack: [i32] → [str_id]
    FTOS = 10        # stack: [f32] → [str_id]
    CONCAT = 11      # stack: [str_b, str_a] → [str_id]
    PARSE_INT = 12   # stack: [str_id] → [i32]
    PARSE_FLOAT = 13 # stack: [str_id] → [f32]
    STR_LEN = 14     # stack: [str_id] → [len]
    SET_TEXT = 15     # stack: [str_id] → sets target widget text
    DRAW_STR = 16    # stack: [str_id, colors, font_id, loc] → draw
    STR_CLEAR = 17   # no args → resets string pool


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
    # Compound (LEN wire type)
    LOCATION = 0x40
    SIZE = 0x41
    MARGIN = 0x42
    BORDER_EDGES = 0x43
    PADDING = 0x44
    TEXT = 0x45


# ============================================================
# Protobuf encoding / decoding
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
    """Low-level bytecode assembler. 1:1 opcode mapping.

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

    def _emit_tag(self, opcode, wt):
        tag = (opcode << 3) | wt
        self._emit(encode_varint(tag))

    def _emit_no_arg(self, opcode):
        self._emit_tag(opcode, WT.NO_ARG)

    def _emit_svarint(self, opcode, val):
        """Emit opcode + zigzag-encoded varint arg."""
        self._emit_tag(opcode, WT.VARINT)
        self._emit(encode_svarint(val))

    def _emit_i16(self, opcode, val):
        self._emit_tag(opcode, WT.I16)
        self._emit(struct.pack('<H', val & 0xFFFF))

    def _emit_len(self, opcode, payload):
        self._emit_tag(opcode, WT.LEN)
        self._emit(encode_varint(len(payload)))
        self._emit(payload)

    # --- No-arg instructions (wt=5) ---

    def halt(self):
        self._emit_no_arg(Op.HALT)

    def pop(self):
        self._emit_no_arg(Op.POP)

    def add(self):
        self._emit_no_arg(Op.ADD)

    def sub(self):
        self._emit_no_arg(Op.SUB)

    def eq(self):
        self._emit_no_arg(Op.EQ)

    def lt(self):
        self._emit_no_arg(Op.LT)

    def w_dirty(self):
        self._emit_no_arg(Op.W_DIRTY)

    def dup(self):
        self._emit_no_arg(Op.DUP)

    def swap(self):
        self._emit_no_arg(Op.SWAP)

    def mul(self):
        self._emit_no_arg(Op.MUL)

    def div(self):
        self._emit_no_arg(Op.DIV)

    def modulo(self):
        self._emit_no_arg(Op.MOD)

    def neg(self):
        self._emit_no_arg(Op.NEG)

    def and_(self):
        self._emit_no_arg(Op.AND)

    def or_(self):
        self._emit_no_arg(Op.OR)

    def not_(self):
        self._emit_no_arg(Op.NOT)

    def ne(self):
        self._emit_no_arg(Op.NE)

    def le(self):
        self._emit_no_arg(Op.LE)

    def ge(self):
        self._emit_no_arg(Op.GE)

    def gt(self):
        self._emit_no_arg(Op.GT)

    def ret(self):
        self._emit_no_arg(Op.RET)

    def yield_(self):
        self._emit_no_arg(Op.YIELD)

    def w_render(self):
        self._emit_no_arg(Op.W_RENDER)

    def w_alloc(self):
        self._emit_no_arg(Op.W_ALLOC)

    # --- Varint arg instructions (wt=0) ---
    # VM decode_signed_varint kullanir, tum varint'ler zigzag encoded.

    def push(self, val):
        self._emit_svarint(Op.PUSH, val)

    def load(self, var_id):
        self._emit_svarint(Op.LOAD, var_id)

    def store(self, var_id):
        self._emit_svarint(Op.STORE, var_id)

    def w_target(self, widget_id):
        self._emit_svarint(Op.W_TARGET, widget_id)

    def w_set(self, prop_id):
        """Scalar W_SET: prop_id varint arg, value from stack."""
        self._emit_svarint(Op.W_SET, prop_id)

    def w_get(self, prop_id):
        self._emit_svarint(Op.W_GET, prop_id)

    def w_parent(self, parent_id):
        self._emit_svarint(Op.W_PARENT, parent_id)

    # --- i16 arg instructions (wt=1) ---

    def jmp(self, target):
        self._emit_i16(Op.JMP, target)

    def jz(self, target):
        self._emit_i16(Op.JZ, target)

    def jnz(self, target):
        self._emit_i16(Op.JNZ, target)

    def call(self, target):
        self._emit_i16(Op.CALL, target)

    # --- Forward jump helpers ---

    def jz_fwd(self):
        """JZ placeholder. Returns patch position for later patching."""
        self._emit_tag(Op.JZ, WT.I16)
        patch_pos = self.pos
        self._emit(b'\x00\x00')
        return patch_pos

    def jnz_fwd(self):
        """JNZ placeholder. Returns patch position."""
        self._emit_tag(Op.JNZ, WT.I16)
        patch_pos = self.pos
        self._emit(b'\x00\x00')
        return patch_pos

    def jmp_fwd(self):
        """JMP placeholder. Returns patch position."""
        self._emit_tag(Op.JMP, WT.I16)
        patch_pos = self.pos
        self._emit(b'\x00\x00')
        return patch_pos

    def patch(self, patch_pos, target=None):
        """Patch a forward jump. Default target = current position."""
        if target is None:
            target = self.pos
        struct.pack_into('<H', self._buf, patch_pos, target)

    # --- LEN payload instructions (wt=2) ---

    def w_set_compound(self, prop_id, values):
        """Compound W_SET: LEN payload = prop_id + packed zigzag varints."""
        payload = bytearray([prop_id])
        for v in values:
            payload.extend(encode_svarint(v))
        self._emit_len(Op.W_SET, bytes(payload))

    def w_set_text(self, text):
        """W_SET with PROP_TEXT: LEN payload = prop_id + raw text bytes."""
        payload = bytes([Prop.TEXT]) + (text.encode('utf-8') if isinstance(text, str) else bytes(text))
        self._emit_len(Op.W_SET, payload)

    def f_read(self, addr, length):
        """F_READ: flash addr (4B LE) + length (2B LE)."""
        payload = struct.pack('<IH', addr, length)
        self._emit_len(Op.F_READ, payload)

    def f_write(self, addr, data):
        """F_WRITE: flash addr (4B LE) + data bytes."""
        payload = struct.pack('<I', addr) + bytes(data)
        self._emit_len(Op.F_WRITE, payload)

    # --- Array instructions ---

    def arr_alloc(self, size):
        """Allocate zero-filled array. Pushes array_id."""
        self._emit_svarint(Op.ARR_ALLOC, size)

    def arr_alloc_init(self, values):
        """Allocate and init array from values. Pushes array_id."""
        payload = bytearray()
        for v in values:
            payload.extend(encode_svarint(v))
        self._emit_len(Op.ARR_ALLOC, bytes(payload))

    def arr_load(self):
        """Pop [arr_id, index], push arr[index]."""
        self._emit_no_arg(Op.ARR_LOAD)

    def arr_store(self):
        """Pop [arr_id, index, value], store value."""
        self._emit_no_arg(Op.ARR_STORE)

    def arr_len(self):
        """Pop arr_id, push length."""
        self._emit_no_arg(Op.ARR_LEN)

    # --- Float32 instructions (soft-float, all no-arg) ---

    def itof(self):
        """Pop i32, push f32 bits."""
        self._emit_no_arg(Op.ITOF)

    def ftoi(self):
        """Pop f32 bits, push i32."""
        self._emit_no_arg(Op.FTOI)

    def fadd(self):
        """Pop two f32, push f32 sum."""
        self._emit_no_arg(Op.FADD)

    def fsub(self):
        """Pop two f32, push f32 difference (a - b)."""
        self._emit_no_arg(Op.FSUB)

    def fmul(self):
        """Pop two f32, push f32 product."""
        self._emit_no_arg(Op.FMUL)

    def fdiv(self):
        """Pop two f32, push f32 quotient (a / b)."""
        self._emit_no_arg(Op.FDIV)

    def fneg(self):
        """Pop f32, push negated f32."""
        self._emit_no_arg(Op.FNEG)

    def feq(self):
        """Pop two f32, push i32 (1 if a == b, else 0)."""
        self._emit_no_arg(Op.FEQ)

    def flt(self):
        """Pop two f32, push i32 (1 if a < b, else 0)."""
        self._emit_no_arg(Op.FLT)

    def fle(self):
        """Pop two f32, push i32 (1 if a <= b, else 0)."""
        self._emit_no_arg(Op.FLE)

    def fgt(self):
        """Pop two f32, push i32 (1 if a > b, else 0)."""
        self._emit_no_arg(Op.FGT)

    def fge(self):
        """Pop two f32, push i32 (1 if a >= b, else 0)."""
        self._emit_no_arg(Op.FGE)

    def fne(self):
        """Pop two f32, push i32 (1 if a != b, else 0)."""
        self._emit_no_arg(Op.FNE)

    # --- Built-in methods (OP_BUILTIN) ---

    def builtin(self, method_id):
        """Emit OP_BUILTIN with varint method_id. Args already on stack."""
        self._emit_svarint(Op.BUILTIN, method_id)

    def builtin_len(self, method_id, payload):
        """Emit OP_BUILTIN with LEN payload. First byte = method_id, rest = data."""
        data = bytes([method_id]) + (payload.encode('utf-8') if isinstance(payload, str) else bytes(payload))
        self._emit_len(Op.BUILTIN, data)

    def fill_rect(self, x, y, w, h, color):
        """Emit fillRect built-in: push packed args + BUILTIN 0."""
        self.push(pack_pair(x, y))
        self.push(pack_pair(w, h))
        self.push(color)
        self.builtin(Builtin.FILL_RECT)

    def draw_rect(self, x, y, w, h, color):
        """Emit rect outline built-in: push packed args + BUILTIN 1."""
        self.push(pack_pair(x, y))
        self.push(pack_pair(w, h))
        self.push(color)
        self.builtin(Builtin.RECT)

    def draw_line(self, x0, y0, x1, y1, color):
        """Emit line built-in: push packed args + BUILTIN 2."""
        self.push(pack_pair(x0, y0))
        self.push(pack_pair(x1, y1))
        self.push(color)
        self.builtin(Builtin.LINE)

    def draw_circle(self, cx, cy, r, color):
        """Emit circle outline built-in: push packed args + BUILTIN 3."""
        self.push(pack_pair(cx, cy))
        self.push(r)
        self.push(color)
        self.builtin(Builtin.CIRCLE)

    def fill_circle(self, cx, cy, r, color):
        """Emit filled circle built-in: push packed args + BUILTIN 4."""
        self.push(pack_pair(cx, cy))
        self.push(r)
        self.push(color)
        self.builtin(Builtin.FILL_CIRCLE)

    def draw_image(self, x, y, image_id):
        """Emit drawImage built-in: push packed args + BUILTIN 5."""
        self.push(pack_pair(x, y))
        self.push(image_id)
        self.builtin(Builtin.DRAW_IMAGE)

    def draw_text(self, x, y, font_id, fg, bg, text):
        """Emit drawText built-in: push stack args + BUILTIN LEN with text payload.
        bg=0 means transparent.
        """
        self.push(pack_pair(x, y))
        self.push(font_id)
        self.push(pack_pair(fg, bg))
        self.builtin_len(Builtin.DRAW_TEXT, text)

    def delay(self, ms):
        """Emit delay_ms built-in: push ms + BUILTIN 7."""
        self.push(ms)
        self.builtin(Builtin.DELAY)

    # --- String operations ---

    def str_alloc(self, text):
        """Create a string from literal text. Pushes str_id."""
        self.builtin_len(Builtin.STR, text)

    def str_itos(self):
        """Pop i32, push str_id of decimal representation."""
        self.builtin(Builtin.ITOS)

    def str_ftos(self):
        """Pop f32 bits, push str_id of float representation."""
        self.builtin(Builtin.FTOS)

    def str_concat(self):
        """Pop [str_b, str_a], push concatenated str_id."""
        self.builtin(Builtin.CONCAT)

    def str_parse_int(self):
        """Pop str_id, push parsed i32."""
        self.builtin(Builtin.PARSE_INT)

    def str_parse_float(self):
        """Pop str_id, push parsed f32 bits."""
        self.builtin(Builtin.PARSE_FLOAT)

    def str_len(self):
        """Pop str_id, push length."""
        self.builtin(Builtin.STR_LEN)

    def str_set_text(self):
        """Pop str_id, set as text on current target widget."""
        self.builtin(Builtin.SET_TEXT)

    def str_draw(self):
        """Pop [str_id, colors, font_id, loc], draw string."""
        self.builtin(Builtin.DRAW_STR)

    def str_clear(self):
        """Reset the string pool. All str_ids become invalid."""
        self.builtin(Builtin.STR_CLEAR)

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
    0: 'HALT', 1: 'PUSH', 2: 'POP', 3: 'LOAD', 4: 'STORE',
    5: 'ADD', 6: 'SUB', 7: 'EQ', 8: 'LT', 9: 'JMP', 10: 'JZ',
    11: 'JNZ', 12: 'W_TARGET', 13: 'W_SET', 14: 'W_GET', 15: 'W_DIRTY',
    16: 'DUP', 17: 'SWAP', 18: 'MUL', 19: 'DIV', 20: 'MOD', 21: 'NEG',
    22: 'AND', 23: 'OR', 24: 'NOT', 25: 'NE', 26: 'LE', 27: 'GE',
    28: 'GT', 29: 'CALL', 30: 'RET', 31: 'YIELD', 32: 'W_RENDER',
    33: 'W_ALLOC', 34: 'W_PARENT', 35: 'F_READ', 36: 'F_WRITE',
    37: 'ARR_ALLOC', 38: 'ARR_LOAD', 39: 'ARR_STORE', 40: 'ARR_LEN',
    41: 'BUILTIN',
    42: 'ITOF', 43: 'FTOI',
    44: 'FADD', 45: 'FSUB', 46: 'FMUL', 47: 'FDIV', 48: 'FNEG',
    49: 'FEQ', 50: 'FLT', 51: 'FLE', 52: 'FGT', 53: 'FGE', 54: 'FNE',
}

BUILTIN_NAMES = {
    0: 'fillRect', 1: 'rect', 2: 'line', 3: 'circle',
    4: 'fillCircle', 5: 'drawImage', 6: 'drawText', 7: 'delay',
    8: 'str', 9: 'itos', 10: 'ftos', 11: 'concat',
    12: 'parseInt', 13: 'parseFloat', 14: 'strLen',
    15: 'setText', 16: 'drawStr', 17: 'strClear',
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
    0x20: 'ON_CLICK', 0x21: 'ON_PAINT', 0x22: 'ON_TAP',
    0x40: 'LOCATION', 0x41: 'SIZE', 0x42: 'MARGIN',
    0x43: 'BORDER_EDGES', 0x44: 'PADDING', 0x45: 'TEXT',
}


def disassemble(data):
    """Disassemble bytecode to human-readable text."""
    lines = []
    pos = 0
    while pos < len(data):
        addr = pos
        try:
            tag, consumed = decode_varint(data, pos)
        except ValueError:
            lines.append(f'{addr:04X}: ??? (truncated)')
            break
        pos += consumed

        wt = tag & 7
        opcode = tag >> 3
        name = OP_NAMES.get(opcode, f'OP_{opcode}')

        if wt == WT.NO_ARG:
            lines.append(f'{addr:04X}: {name}')

        elif wt == WT.VARINT:
            try:
                val, consumed = decode_svarint(data, pos)
            except ValueError:
                lines.append(f'{addr:04X}: {name} ??? (truncated)')
                break
            pos += consumed

            if opcode == Op.BUILTIN:
                bname = BUILTIN_NAMES.get(val, f'method_{val}')
                lines.append(f'{addr:04X}: {name} {bname}')
            elif opcode in (Op.W_SET, Op.W_GET):
                pname = PROP_NAMES.get(val, f'0x{val:02X}')
                lines.append(f'{addr:04X}: {name} {pname}')
            elif opcode == Op.W_TARGET:
                lines.append(f'{addr:04X}: {name} #{val}')
            elif opcode == Op.W_PARENT:
                lines.append(f'{addr:04X}: {name} #{val}')
            elif opcode == Op.PUSH:
                if val > 255:
                    lines.append(f'{addr:04X}: {name} {val} (0x{val & 0xFFFF:04X})')
                else:
                    lines.append(f'{addr:04X}: {name} {val}')
            else:
                lines.append(f'{addr:04X}: {name} {val}')

        elif wt == WT.I16:
            if pos + 2 > len(data):
                lines.append(f'{addr:04X}: {name} ??? (truncated)')
                break
            val = struct.unpack_from('<H', data, pos)[0]
            pos += 2
            lines.append(f'{addr:04X}: {name} @{val:04X}')

        elif wt == WT.LEN:
            try:
                length, consumed = decode_varint(data, pos)
            except ValueError:
                lines.append(f'{addr:04X}: {name} ??? (truncated)')
                break
            pos += consumed
            if pos + length > len(data):
                lines.append(f'{addr:04X}: {name} ??? (payload truncated)')
                break
            payload = data[pos:pos + length]
            pos += length

            if opcode == Op.BUILTIN and len(payload) > 0:
                mid = payload[0]
                bname = BUILTIN_NAMES.get(mid, f'method_{mid}')
                text_data = payload[1:]
                if mid in (Builtin.DRAW_TEXT, Builtin.STR) and text_data:
                    try:
                        txt = text_data.decode('utf-8')
                        lines.append(f'{addr:04X}: {name} {bname} "{txt}"')
                    except UnicodeDecodeError:
                        hex_str = ' '.join(f'{b:02X}' for b in text_data)
                        lines.append(f'{addr:04X}: {name} {bname} [{hex_str}]')
                else:
                    hex_str = ' '.join(f'{b:02X}' for b in text_data)
                    lines.append(f'{addr:04X}: {name} {bname} [{hex_str}]')
            elif opcode == Op.W_SET and len(payload) > 0:
                prop_id = payload[0]
                pname = PROP_NAMES.get(prop_id, f'0x{prop_id:02X}')
                vals = []
                p = 1
                while p < len(payload):
                    try:
                        v, c = decode_svarint(payload, p)
                        vals.append(str(v))
                        p += c
                    except ValueError:
                        break
                lines.append(f'{addr:04X}: {name} {pname} [{", ".join(vals)}]')
            elif opcode == Op.F_READ and len(payload) >= 6:
                flash_addr = struct.unpack_from('<I', payload)[0]
                read_len = struct.unpack_from('<H', payload, 4)[0]
                lines.append(f'{addr:04X}: {name} 0x{flash_addr:08X} len={read_len}')
            elif opcode == Op.ARR_ALLOC:
                vals = []
                p = 0
                while p < len(payload):
                    try:
                        v, c = decode_svarint(payload, p)
                        vals.append(str(v))
                        p += c
                    except ValueError:
                        break
                lines.append(f'{addr:04X}: {name} [{", ".join(vals)}]')
            elif opcode == Op.F_WRITE and len(payload) >= 4:
                flash_addr = struct.unpack_from('<I', payload)[0]
                dlen = len(payload) - 4
                lines.append(f'{addr:04X}: {name} 0x{flash_addr:08X} [{dlen}B]')
            else:
                hex_str = ' '.join(f'{b:02X}' for b in payload)
                lines.append(f'{addr:04X}: {name} [{hex_str}]')

        else:
            lines.append(f'{addr:04X}: ??? wt={wt} opcode={opcode}')
            break

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
    'text': (Prop.TEXT, True),
    'text_id': (Prop.TEXT, True),  # alias — same underlying prop
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
        self._funcs = {}        # name -> (func_id, arg_count, offset)
        self._next_func_id = 1
        self._on_program_start = 0xFFFF
        self._on_page_changing = 0xFFFF
        self._on_page_changed = 0xFFFF
        self._on_user_message = 0xFFFF

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
        """Declare a VM variable (max 16). Returns var slot."""
        if self._next_var >= 16:
            raise RuntimeError("Max 16 VM variables")
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

    # --- Callback functions ---

    def define_func(self, name, arg_count=0):
        """Define a callback function at current bytecode position.

        Returns func_id (1-based). The function body follows this call
        and should end with cc.ret() or cc.halt().

        Example:
            fid = cc.define_func("on_btn_click", arg_count=1)
            # widget_id is on stack as argument
            cc.asm.store(0)   # save to var 0
            # ... callback body ...
            cc.ret()
        """
        func_id = self._next_func_id
        offset = self.asm.pos
        self._funcs[name] = (func_id, arg_count, offset)
        self._next_func_id += 1
        return func_id

    def on_program_start(self, func_name):
        """Register a function as the on_program_start system callback."""
        if func_name not in self._funcs:
            raise ValueError(f"Unknown function: {func_name}")
        self._on_program_start = self._funcs[func_name][2]  # offset

    def on_page_changing(self, func_name):
        """Register on_page_changing callback. Function receives (old_index, new_index),
        must return 0 to prevent or non-zero to allow."""
        if func_name not in self._funcs:
            raise ValueError(f"Unknown function: {func_name}")
        self._on_page_changing = self._funcs[func_name][2]

    def on_page_changed(self, func_name):
        """Register on_page_changed callback. Function receives (index)."""
        if func_name not in self._funcs:
            raise ValueError(f"Unknown function: {func_name}")
        self._on_page_changed = self._funcs[func_name][2]

    def on_user_message(self, func_name):
        """Register on_user_message callback. Function receives (array_id).
        Fires when a UserMessage (field 6) is received via USART."""
        if func_name not in self._funcs:
            raise ValueError(f"Unknown function: {func_name}")
        self._on_user_message = self._funcs[func_name][2]

    def build_meta(self):
        """Build callback metadata binary.

        Format:
          Header (8 bytes):
            func_count(u16) + on_program_start(u16) + on_page_changing(u16)
            + on_page_changed(u16)
          Function table:
            [func_id(u16) + offset(u16) + arg_count(u8)] × N
          Extended system callbacks (after function table):
            on_user_message(u16)
        """
        func_list = list(self._funcs.values())
        buf = bytearray()
        buf.extend(struct.pack('<H', len(func_list)))
        buf.extend(struct.pack('<H', self._on_program_start))
        buf.extend(struct.pack('<H', self._on_page_changing))
        buf.extend(struct.pack('<H', self._on_page_changed))
        for func_id, arg_count, offset in func_list:
            buf.extend(struct.pack('<HHB', func_id, offset, arg_count))
        # Extended: on_user_message (after function table)
        buf.extend(struct.pack('<H', self._on_user_message))
        return bytes(buf)

    def has_callbacks(self):
        """Check if any callback functions have been defined."""
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
