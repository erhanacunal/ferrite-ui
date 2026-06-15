#!/usr/bin/env python3
"""ferrite-ui bytecode analyzer --memory and instruction profiler.

Analyzes compiled .fl programs and reports:
  - Estimated runtime memory consumption (widgets, variables, arrays, strings, etc.)
  - Instruction count by category
  - Per-function breakdown (size, instruction count)
  - Opcode frequency histogram

Usage:
  python ferrite_analyze.py source.fl                    # analyze source file
  python ferrite_analyze.py source.fl -I lib/            # with include path
  python ferrite_analyze.py --bin firmware.bin            # analyze compiled binary
  python ferrite_analyze.py source.fl --json              # JSON output
  python ferrite_analyze.py source.fl --verbose           # show per-function details
"""

import argparse
import json
import struct
import sys
import os

# Add tools directory to path for imports
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from ferrite_cc import Op, OP_NAMES, PROP_NAMES, _NO_ARG_OPS


# ============================================================
# Constants --runtime sizes on GD32F103 (Cortex-M3)
# ============================================================

# Widget base struct size in bytes (tree links + flags + kind + ext + location + size + colors)
WIDGET_BASE_SIZE = 18
# WidgetExt struct size in bytes (edges + text + appearance + callbacks + value)
WIDGET_EXT_SIZE = 32
# VmVar entry: { id: u16, val: i32 } = 6 bytes + Vec overhead
VMVAR_ENTRY_SIZE = 6
# VM Vec overhead per allocation (ptr + len + cap) = 12 bytes
VEC_OVERHEAD = 12
# Call stack frame: ret_addr(u16) + frame_base(u8) + frame_size(u8)
CALL_FRAME_SIZE = 4
# Eval stack slot: i32 = 4 bytes
EVAL_STACK_SLOT = 4
# VM eval stack depth
VM_EVAL_STACK_DEPTH = 16
# VM call stack depth
VM_CALL_STACK_DEPTH = 8
# Callback queue slot size
CALLBACK_SLOT_SIZE = 8
CALLBACK_QUEUE_SIZE = 8
# String pool entry overhead (ptr + len + cap + id)
STRING_ENTRY_SIZE = 16
# Array pool entry overhead (Vec<i32> + id)
ARRAY_ENTRY_OVERHEAD = 16
# Function table entry: FuncEntry { id, kind, offset, length } = 12 bytes
FUNC_ENTRY_SIZE = 12
# Root widget (always pre-allocated)
ROOT_WIDGET_COUNT = 1


# ============================================================
# Opcode classification
# ============================================================

CATEGORY_STACK = "stack"
CATEGORY_ARITH = "arithmetic"
CATEGORY_COMPARE = "comparison"
CATEGORY_LOGIC = "logic"
CATEGORY_CONTROL = "control"
CATEGORY_VARIABLE = "variable"
CATEGORY_WIDGET = "widget"
CATEGORY_ARRAY = "array"
CATEGORY_DRAW = "drawing"
CATEGORY_STRING = "string"
CATEGORY_FLASH = "flash"
CATEGORY_FLOAT = "float"
CATEGORY_SYSTEM = "system"

OPCODE_CATEGORIES = {
    # Stack
    Op.POP: CATEGORY_STACK, Op.DUP: CATEGORY_STACK, Op.SWAP: CATEGORY_STACK,
    Op.PUSH_0: CATEGORY_STACK, Op.PUSH_1: CATEGORY_STACK,
    Op.PUSH_2: CATEGORY_STACK, Op.PUSH_M1: CATEGORY_STACK,
    Op.PUSH_I8: CATEGORY_STACK, Op.PUSH_I16: CATEGORY_STACK,
    Op.PUSH_I32: CATEGORY_STACK,
    # Arithmetic
    Op.ADD: CATEGORY_ARITH, Op.SUB: CATEGORY_ARITH,
    Op.MUL: CATEGORY_ARITH, Op.DIV: CATEGORY_ARITH,
    Op.MOD: CATEGORY_ARITH, Op.NEG: CATEGORY_ARITH,
    # Comparison
    Op.EQ: CATEGORY_COMPARE, Op.NE: CATEGORY_COMPARE,
    Op.LT: CATEGORY_COMPARE, Op.LE: CATEGORY_COMPARE,
    Op.GT: CATEGORY_COMPARE, Op.GE: CATEGORY_COMPARE,
    # Logic
    Op.AND: CATEGORY_LOGIC, Op.OR: CATEGORY_LOGIC, Op.NOT: CATEGORY_LOGIC,
    # Control flow
    Op.HALT: CATEGORY_CONTROL, Op.RET: CATEGORY_CONTROL,
    Op.YIELD: CATEGORY_CONTROL, Op.FRAME: CATEGORY_CONTROL,
    Op.JMP: CATEGORY_CONTROL, Op.JZ: CATEGORY_CONTROL,
    Op.JNZ: CATEGORY_CONTROL, Op.CALL: CATEGORY_CONTROL,
    # Variables
    Op.LOAD: CATEGORY_VARIABLE, Op.STORE: CATEGORY_VARIABLE,
    Op.LOAD_0: CATEGORY_VARIABLE, Op.LOAD_1: CATEGORY_VARIABLE,
    Op.LOAD_2: CATEGORY_VARIABLE, Op.LOAD_3: CATEGORY_VARIABLE,
    Op.LOAD_4: CATEGORY_VARIABLE,
    Op.STORE_0: CATEGORY_VARIABLE, Op.STORE_1: CATEGORY_VARIABLE,
    Op.STORE_2: CATEGORY_VARIABLE, Op.STORE_3: CATEGORY_VARIABLE,
    Op.STORE_4: CATEGORY_VARIABLE,
    # Widget
    Op.W_ALLOC: CATEGORY_WIDGET, Op.W_ALLTAR: CATEGORY_WIDGET,
    Op.W_TARGET: CATEGORY_WIDGET, Op.W_SET: CATEGORY_WIDGET,
    Op.W_GET: CATEGORY_WIDGET, Op.W_PARENT: CATEGORY_WIDGET,
    Op.W_SET_LEN: CATEGORY_WIDGET,
    Op.W_DIRTY: CATEGORY_WIDGET, Op.W_RENDER: CATEGORY_WIDGET,
    # Array
    Op.ARR_ALLOC: CATEGORY_ARRAY, Op.ARR_INIT: CATEGORY_ARRAY,
    Op.ARR_LOAD: CATEGORY_ARRAY, Op.ARR_STORE: CATEGORY_ARRAY,
    Op.ARR_LEN: CATEGORY_ARRAY, Op.ARR_FREE: CATEGORY_ARRAY,
    # Drawing builtins
    Op.FILL_RECT: CATEGORY_DRAW, Op.RECT: CATEGORY_DRAW,
    Op.LINE: CATEGORY_DRAW, Op.CIRCLE: CATEGORY_DRAW,
    Op.FILL_CIRCLE: CATEGORY_DRAW, Op.DRAW_IMAGE: CATEGORY_DRAW,
    Op.DRAW_TEXT_LIT: CATEGORY_DRAW, Op.DRAW_STR: CATEGORY_DRAW,
    Op.ROUNDED_RECT: CATEGORY_DRAW, Op.FILL_ROUNDED_RECT: CATEGORY_DRAW,
    Op.ARC: CATEGORY_DRAW,
    # String builtins
    Op.STR_LIT: CATEGORY_STRING, Op.ITOS: CATEGORY_STRING,
    Op.FTOS: CATEGORY_STRING, Op.CONCAT: CATEGORY_STRING,
    Op.PARSE_INT: CATEGORY_STRING, Op.PARSE_FLOAT: CATEGORY_STRING,
    Op.STR_LEN: CATEGORY_STRING, Op.SET_TEXT: CATEGORY_STRING,
    Op.STR_CLEAR: CATEGORY_STRING, Op.STR_FREE: CATEGORY_STRING,
    # Flash
    Op.F_READ: CATEGORY_FLASH, Op.F_WRITE: CATEGORY_FLASH,
    # Float
    Op.ITOF: CATEGORY_FLOAT, Op.FTOI: CATEGORY_FLOAT,
    Op.FADD: CATEGORY_FLOAT, Op.FSUB: CATEGORY_FLOAT,
    Op.FMUL: CATEGORY_FLOAT, Op.FDIV: CATEGORY_FLOAT,
    Op.FNEG: CATEGORY_FLOAT,
    Op.FEQ: CATEGORY_FLOAT, Op.FLT: CATEGORY_FLOAT,
    Op.FLE: CATEGORY_FLOAT, Op.FGT: CATEGORY_FLOAT,
    Op.FGE: CATEGORY_FLOAT, Op.FNE: CATEGORY_FLOAT,
    # System
    Op.DELAY: CATEGORY_SYSTEM,
    Op.BEGIN_FRAME: CATEGORY_SYSTEM, Op.END_FRAME: CATEGORY_SYSTEM,
    Op.SEND_USART: CATEGORY_SYSTEM, Op.SEND_USART_STR: CATEGORY_SYSTEM,
    Op.RTC_READ: CATEGORY_SYSTEM, Op.RTC_WRITE: CATEGORY_SYSTEM,
}

# Add remaining builtins that may exist
for _op in (0x9C, 0x9D, 0x9E, 0x9F, 0xA0, 0xA1):
    if _op not in OPCODE_CATEGORIES:
        OPCODE_CATEGORIES[_op] = CATEGORY_SYSTEM

# Kind names for function table
_KIND_NAMES = {
    0: 'setup', 1: 'loop', 2: 'func', 3: 'on_program_start',
    4: 'on_page_changing', 5: 'on_page_changed', 6: 'on_user_message',
    7: 'on_touch_down', 8: 'on_touch_up', 9: 'on_touch_move',
    10: 'on_swipe', 11: 'on_long_press',
    12: '__on_click_dispatch', 13: '__on_tap_dispatch', 14: '__on_paint_dispatch',
}


# ============================================================
# Image header parser
# ============================================================

def parse_image_header(data):
    """Parse VM image header. Returns (functions, opcode_start, global_count,
    widget_count, ext_count, header_flags).

    functions: list of {id, name, kind, offset, length}
    opcode_start: byte offset where opcodes begin
    global_count: number of global variable slots (v2+, 0 for v1)
    widget_count: alloc() call sites (v4+, 0 for earlier)
    ext_count: widgets needing WidgetExt (v4+, 0 for earlier)
    header_flags: render_mode etc. (v3+, 0 for earlier)

    Header formats:
      v1: version(u8) + func_count(u16)                    = 3 bytes
      v2: version(u8) + func_count(u16) + global_count(u16) = 5 bytes
      v3: version(u8) + func_count(u16) + global_count(u16) + flags(u16) = 7 bytes
      v4: v3 + widget_count(u8) + ext_count(u8)             = 9 bytes
    """
    if len(data) < 3:
        raise ValueError("Image too small for header (need at least 3 bytes)")

    version = data[0]
    if version < 1 or version > 4:
        raise ValueError(
            f"Unsupported image version {version} (expected 1-4). "
            f"Is this a VM image binary? Flash dumps are not supported."
        )

    if version == 1:
        header_size = 3
        global_count = 0
        header_flags = 0
        widget_count = 0
        ext_count = 0
    elif version == 2:
        header_size = 5
        header_flags = 0
        widget_count = 0
        ext_count = 0
    else:  # version 3 or 4
        header_size = 9 if version >= 4 else 7

    if len(data) < header_size:
        raise ValueError(
            f"Image too small for v{version} header (need at least {header_size} bytes)"
        )

    func_count = struct.unpack_from('<H', data, 1)[0]
    global_count = struct.unpack_from('<H', data, 3)[0] if version >= 2 else 0
    header_flags = struct.unpack_from('<H', data, 5)[0] if version >= 3 else 0
    widget_count = data[7] if version >= 4 else 0
    ext_count = data[8] if version >= 4 else 0

    # Sanity checks on func_count
    if func_count > 256:
        raise ValueError(
            f"func_count {func_count} is unreasonably large (max 256). "
            f"Is this a VM image binary?"
        )

    func_table_size = func_count * 12
    opcode_start = header_size + func_table_size
    if opcode_start > len(data):
        raise ValueError(
            f"Image header requires {opcode_start} bytes for {func_count} functions "
            f"but data is only {len(data)} bytes"
        )

    # Get real names if available (from _ImageBytes)
    real_names = getattr(data, 'func_names', {})

    functions = []
    for i in range(func_count):
        base = header_size + i * 12
        # Bounds already verified above, but defensive check
        if base + 12 > len(data):
            break
        func_id = struct.unpack_from('<H', data, base)[0]
        kind = data[base + 2]
        offset = struct.unpack_from('<I', data, base + 4)[0]
        length = struct.unpack_from('<I', data, base + 8)[0]

        if offset in real_names:
            name = real_names[offset]
        else:
            name = _KIND_NAMES.get(kind, f'func_{func_id}')

        functions.append({
            'id': func_id,
            'name': name,
            'kind': kind,
            'kind_name': _KIND_NAMES.get(kind, 'unknown'),
            'offset': offset,
            'length': length,
        })

    return functions, opcode_start, global_count, widget_count, ext_count, header_flags


# ============================================================
# Flash filesystem parser
# ============================================================

# Flash FS layout (W25Q256)
DEFAULT_FS_BASE = 0x1000       # default FS header offset
FS_HEADER_SIZE = 16            # header bytes
FS_ENTRY_SIZE = 32             # resource table entry bytes
FS_MAGIC = b"FERR"             # magic bytes
FS_VERSION = 2                 # current version

# Resource types
RES_FONT = 0
RES_IMAGE = 1
RES_PROGRAM = 2
RES_PAGE = 3
RES_FILE = 4

_RES_TYPE_NAMES = {
    RES_FONT: "Font",
    RES_IMAGE: "Image",
    RES_PROGRAM: "Program",
    RES_PAGE: "Page",
    RES_FILE: "File",
}

# Resource flags
RES_FLAG_FLASH_EXEC = 0x01


def parse_flash_header(data, base=DEFAULT_FS_BASE):
    """Parse flash filesystem header at `base` offset.

    Returns dict with: version, screen_w, screen_h, resource_count, checksum
    Raises ValueError on invalid magic, version, or count.
    """
    if len(data) < base + FS_HEADER_SIZE:
        raise ValueError(
            f"File too small for flash FS header at 0x{base:06X} "
            f"(need {base + FS_HEADER_SIZE} bytes, have {len(data)})"
        )

    hdr = data[base:base + FS_HEADER_SIZE]

    magic = hdr[0:4]
    if magic != FS_MAGIC:
        raise ValueError(
            f"Bad flash FS magic: expected {FS_MAGIC!r}, got {magic!r} at 0x{base:06X}"
        )

    version = struct.unpack_from('<H', hdr, 4)[0]
    if version != FS_VERSION:
        raise ValueError(
            f"Unsupported flash FS version {version} (expected {FS_VERSION})"
        )

    screen_w = struct.unpack_from('<H', hdr, 6)[0]
    screen_h = struct.unpack_from('<H', hdr, 8)[0]
    resource_count = struct.unpack_from('<H', hdr, 10)[0]
    checksum = struct.unpack_from('<I', hdr, 12)[0]

    if resource_count > 1000:
        raise ValueError(f"resource_count {resource_count} is unreasonably large (max 1000)")

    return {
        'version': version,
        'screen_w': screen_w,
        'screen_h': screen_h,
        'resource_count': resource_count,
        'checksum': checksum,
    }


def parse_resource_entries(data, base=DEFAULT_FS_BASE, count=0):
    """Parse resource table entries from flash image.

    Returns list of dicts: {name, kind, kind_name, offset, size, flags}
    """
    table_offset = base + FS_HEADER_SIZE
    entries = []

    for i in range(count):
        off = table_offset + i * FS_ENTRY_SIZE
        if off + FS_ENTRY_SIZE > len(data):
            break

        entry = data[off:off + FS_ENTRY_SIZE]

        # Name: null-terminated, up to 15 chars
        name_bytes = entry[0:16]
        null_pos = name_bytes.find(b'\x00')
        if null_pos >= 0:
            name = name_bytes[:null_pos].decode('ascii', errors='replace')
        else:
            name = name_bytes.decode('ascii', errors='replace')

        kind = entry[16]
        offset = struct.unpack_from('<I', entry, 20)[0]
        size = struct.unpack_from('<I', entry, 24)[0]
        flags = entry[28]

        entries.append({
            'name': name,
            'kind': kind,
            'kind_name': _RES_TYPE_NAMES.get(kind, f'Unknown({kind})'),
            'offset': offset,
            'size': size,
            'flags': flags,
        })

    return entries


def detect_flash_image(data, base=DEFAULT_FS_BASE):
    """Check if data looks like a flash FS image.

    Checks both the given base offset and offset 0.
    Returns (header_offset, detected_base) or (None, None) if not found.
    header_offset is where the FERR magic was found in the file.
    detected_base is the absolute flash address corresponding to header_offset.
    """
    # Check user-specified base first, then offset 0
    candidates = [base, 0] if base != 0 else [0]
    for off in candidates:
        if len(data) >= off + FS_HEADER_SIZE and data[off:off + 4] == FS_MAGIC:
            if off > 0:
                # Header at non-zero offset: file offset = base (typical flash dump)
                return off, off
            else:
                # Header at offset 0: file was built starting from 0,
                # need to infer the absolute fs_base from entries
                return off, None  # None = infer from data
    return None, None


def _infer_fs_base(data, header_offset, resource_count):
    """Infer absolute fs_base from first resource entry's offset.

    When the FS image is built at offset 0, resource offsets are absolute
    flash addresses. The first resource data starts immediately after the
    header + table, so we back-compute: fs_base = first_offset - table_end.
    """
    if resource_count == 0:
        return 0
    table_end = header_offset + FS_HEADER_SIZE + resource_count * FS_ENTRY_SIZE
    first_entry = data[header_offset + FS_HEADER_SIZE:
                       header_offset + FS_HEADER_SIZE + FS_ENTRY_SIZE]
    first_offset = struct.unpack_from('<I', first_entry, 20)[0]
    return first_offset - table_end


def analyze_flash_image(data, base=DEFAULT_FS_BASE, exec_mode=None):
    """Analyze a flash filesystem image.

    Handles two formats:
      - Flash dump: header at non-zero offset; resource offsets are absolute.
      - Build output: header at offset 0; resource offsets are absolute
        (need to infer fs_base to map to file positions).

    Returns dict with:
      - flash: header info + resource summary
      - programs: list of {name, analysis: AnalysisResult} for each Program resource
    """
    hdr_off, detected_base = detect_flash_image(data, base)
    if hdr_off is None:
        raise ValueError("Not a valid flash FS image (no FERR magic found)")

    header = parse_flash_header(data, hdr_off)

    # Determine fs_base (absolute flash address of the header)
    if detected_base is not None:
        fs_base = detected_base
    else:
        fs_base = _infer_fs_base(data, hdr_off, header['resource_count'])

    entries = parse_resource_entries(data, hdr_off, header['resource_count'])

    # Add file_offset to each entry (offset within the binary file)
    for e in entries:
        e['file_offset'] = e['offset'] - fs_base

    # Categorize resources
    by_kind = {}
    for e in entries:
        kind_name = e['kind_name']
        by_kind.setdefault(kind_name, []).append(e)

    # Analyze each program
    programs = []
    for e in entries:
        if e['kind'] == RES_PROGRAM:
            # Convert absolute offset to file offset
            file_off = e['offset'] - fs_base
            end_off = file_off + e['size']
            if file_off < 0:
                prog_data = b''
            elif end_off > len(data):
                prog_data = data[file_off:len(data)]
            else:
                prog_data = data[file_off:end_off]

            prog_exec_mode = exec_mode
            if prog_exec_mode is None and (e['flags'] & RES_FLAG_FLASH_EXEC):
                prog_exec_mode = 'flash'

            try:
                analysis = analyze_image(prog_data, prog_exec_mode)
            except ValueError as ex:
                analysis = str(ex)

            programs.append({
                'name': e['name'],
                'size': e['size'],
                'flags': e['flags'],
                'offset': e['offset'],
                'file_offset': file_off,
                'analysis': analysis,
            })

    # Flash usage stats (in absolute flash address space)
    flash_total = 32 * 1024 * 1024  # W25Q256 = 32 MB
    if entries:
        last = max(entries, key=lambda e: e['offset'] + e['size'])
        data_end = last['offset'] + last['size']
        flash_used = data_end - fs_base
    else:
        flash_used = 0

    return {
        'flash': {
            'header': header,
            'fs_base': fs_base,
            'header_offset': hdr_off,
            'entries': entries,
            'by_kind': by_kind,
            'flash_used': flash_used,
            'flash_total': flash_total,
            'file_size': len(data),
        },
        'programs': programs,
    }


# ============================================================
# Bytecode walker --decode instructions without full disassembly
# ============================================================

def walk_bytecode(data, start=0, end=None):
    """Walk bytecode and yield (offset, opcode, size) tuples.

    size = total instruction size in bytes (opcode + args).
    """
    if end is None:
        end = len(data)
    pos = start

    while pos < end:
        addr = pos
        op = data[pos]
        pos += 1

        if op in _NO_ARG_OPS:
            yield addr, op, 1

        elif op == Op.W_ALLTAR:
            pos += 1  # u8 var_slot
            yield addr, op, 2

        elif op == Op.FRAME:
            pos += 1  # u8 local_count
            yield addr, op, 2

        elif op == Op.PUSH_I8:
            pos += 1
            yield addr, op, 2

        elif op == Op.PUSH_I16:
            pos += 2
            yield addr, op, 3

        elif op == Op.PUSH_I32:
            pos += 4
            yield addr, op, 5

        elif op in (Op.LOAD, Op.STORE):
            pos += 1  # u8 slot
            yield addr, op, 2

        elif op in (Op.JMP, Op.JZ, Op.JNZ, Op.CALL):
            pos += 2  # u16 target
            yield addr, op, 3

        elif op in (Op.W_TARGET, Op.W_PARENT):
            pos += 1  # u8 id
            yield addr, op, 2

        elif op in (Op.W_SET, Op.W_GET):
            pos += 1  # u8 prop
            yield addr, op, 2

        elif op == Op.W_SET_LEN:
            prop = data[pos]; pos += 1
            length = data[pos]; pos += 1
            pos += length
            yield addr, op, 3 + length

        elif op == Op.ARR_ALLOC:
            pos += 1  # u8 size
            yield addr, op, 2

        elif op == Op.ARR_INIT:
            count = data[pos]; pos += 1
            pos += count * 4  # i32 values
            yield addr, op, 2 + count * 4

        elif op == Op.F_READ:
            pos += 6  # u32 addr + u16 len
            yield addr, op, 7

        elif op == Op.F_WRITE:
            pos += 4  # u32 addr
            length = data[pos]; pos += 1
            pos += length
            yield addr, op, 6 + length

        elif op in (Op.DRAW_TEXT_LIT, Op.STR_LIT):
            length = data[pos]; pos += 1
            pos += length
            yield addr, op, 2 + length

        else:
            # Unknown opcode --treat as 1-byte
            yield addr, op, 1


# ============================================================
# Analyzer
# ============================================================

class AnalysisResult:
    """Holds all analysis data."""
    def __init__(self):
        # Image info
        self.image_size = 0
        self.header_size = 0
        self.opcode_size = 0

        # Functions
        self.functions = []  # from image header
        self.func_stats = {}  # name -> {instructions, bytes, categories}

        # Instruction counts
        self.total_instructions = 0
        self.opcode_freq = {}  # opcode -> count
        self.category_counts = {}  # category -> count

        # Memory estimates
        self.widget_allocs = 0  # W_ALLOC + W_ALLTAR count
        self.widgets_with_ext = 0  # widgets that need WidgetExt
        self.global_var_slots = set()  # unique global STORE/LOAD slots
        self.max_local_frame = 0  # max local frame size (from FRAME instructions)
        self.global_count = 0  # from image header
        self.arr_allocs = 0  # ARR_ALLOC + ARR_INIT count
        self.arr_frees = 0  # ARR_FREE count
        self.str_allocs = 0  # STR_LIT + ITOS + FTOS count
        self.str_frees = 0  # STR_FREE + STR_CLEAR count
        self.string_literal_bytes = 0  # total inline string bytes
        self.max_call_depth = 0  # estimated from CALL count per chain
        self.rtc_reads = 0  # RTC_READ (each allocates array)
        self.draw_image_count = 0  # DRAW_IMAGE count
        self.font_ids = set()  # font IDs referenced
        self._ext_widget_targets = set()  # widget IDs that got ext props set

        # Exec mode
        self.exec_mode = None  # 'ram' or 'flash' (set externally)

    def total_widgets(self):
        """Total widgets including root."""
        return ROOT_WIDGET_COUNT + self.widget_allocs

    def estimate_memory(self):
        """Estimate runtime memory consumption in bytes."""
        mem = {}

        # Widget tree (base + extensions)
        n_widgets = self.total_widgets()
        n_ext = self.widgets_with_ext
        mem['widgets_base'] = n_widgets * WIDGET_BASE_SIZE + VEC_OVERHEAD
        mem['widgets_ext'] = n_ext * WIDGET_EXT_SIZE + (VEC_OVERHEAD if n_ext > 0 else 0)
        mem['widgets'] = mem['widgets_base'] + mem['widgets_ext']

        # Variables (sparse map): globals are persistent, locals are per-frame
        # Worst case: globals + deepest call chain's locals
        n_globals = len(self.global_var_slots)
        n_locals = self.max_local_frame
        n_vars = n_globals + n_locals
        mem['variables'] = n_vars * VMVAR_ENTRY_SIZE + VEC_OVERHEAD if n_vars else 0

        # VM fixed overhead (eval stack + call stack + state)
        mem['vm_fixed'] = (VM_EVAL_STACK_DEPTH * EVAL_STACK_SLOT +
                           VM_CALL_STACK_DEPTH * CALL_FRAME_SIZE +
                           CALLBACK_QUEUE_SIZE * CALLBACK_SLOT_SIZE)

        # Function table
        n_funcs = len(self.functions)
        mem['func_table'] = n_funcs * FUNC_ENTRY_SIZE + VEC_OVERHEAD if n_funcs else 0

        # Bytecode (RAM mode only)
        if self.exec_mode == 'flash':
            mem['bytecode'] = 0  # executes from flash
        else:
            mem['bytecode'] = self.opcode_size

        # String pool estimate (active strings --allocs minus frees, minimum 0)
        peak_strings = max(0, self.str_allocs - self.str_frees)
        mem['strings_est'] = peak_strings * STRING_ENTRY_SIZE

        # Array pool estimate
        peak_arrays = max(0, self.arr_allocs - self.arr_frees)
        mem['arrays_est'] = peak_arrays * ARRAY_ENTRY_OVERHEAD

        # Image header (always in flash)
        mem['image_header'] = 0  # not RAM

        mem['total'] = sum(mem.values())
        return mem


# Property IDs that live in WidgetExt (require extension allocation)
_EXT_SCALAR_PROPS = {
    0x10, 0x11, 0x12, 0x13,  # MARGIN_T/R/B/L
    0x14, 0x15, 0x16, 0x17,  # BORDER_T/R/B/L
    0x18, 0x19, 0x1A, 0x1B,  # PADDING_T/R/B/L
    0x0F,  # TEXT_COLOR
    0x1C,  # FONT_ID
    0x1D,  # TEXT_ALIGN
    0x1E,  # PRESS_COLOR
    0x1F,  # IMAGE_ID
    0x20,  # ON_CLICK
    0x21,  # ON_PAINT
    0x22,  # ON_TAP
    0x23,  # BORDER_RADIUS
    0x24,  # VALUE
}
_EXT_COMPOUND_PROPS = {
    0x42,  # MARGIN
    0x43,  # BORDER_EDGES
    0x44,  # PADDING
    0x45,  # TEXT
}


def analyze_bytecode(data, opcode_start=0, opcode_end=None):
    """Analyze raw bytecode. Returns AnalysisResult."""
    result = AnalysisResult()
    current_target = -1  # track W_TARGET for ext estimation

    for addr, op, size in walk_bytecode(data, opcode_start, opcode_end):
        result.total_instructions += 1

        # Opcode frequency
        result.opcode_freq[op] = result.opcode_freq.get(op, 0) + 1

        # Category
        cat = OPCODE_CATEGORIES.get(op, 'unknown')
        result.category_counts[cat] = result.category_counts.get(cat, 0) + 1

        # Widget allocations
        if op in (Op.W_ALLOC, Op.W_ALLTAR):
            result.widget_allocs += 1

        # Track current target for ext estimation
        if op == Op.W_TARGET:
            current_target = data[addr + 1]
        elif op == Op.W_ALLTAR:
            current_target = result.widget_allocs  # just-allocated ID

        # Track which targets get extension properties
        if op == Op.W_SET and addr + 1 < len(data):
            prop = data[addr + 1]
            if prop in _EXT_SCALAR_PROPS and current_target >= 0:
                result._ext_widget_targets.add(current_target)
        if op == Op.W_SET_LEN and addr + 1 < len(data):
            prop = data[addr + 1]
            if prop in _EXT_COMPOUND_PROPS and current_target >= 0:
                result._ext_widget_targets.add(current_target)

        # Variable slots: globals (no high bit) vs locals (0x80+)
        if op == Op.STORE or op == Op.LOAD:
            slot = data[addr + 1]
            if slot & 0x80:
                pass  # local, tracked via FRAME
            else:
                result.global_var_slots.add(slot)
        elif Op.LOAD_0 <= op <= Op.LOAD_4:
            pass  # local short form
        elif Op.STORE_0 <= op <= Op.STORE_4:
            pass  # local short form
        elif op == Op.W_ALLTAR:
            slot = data[addr + 1]
            if slot & 0x80:
                pass  # local
            else:
                result.global_var_slots.add(slot)

        # Track max local frame size from FRAME instructions
        if op == Op.FRAME:
            frame_size = data[addr + 1]
            if frame_size > result.max_local_frame:
                result.max_local_frame = frame_size

        # Array tracking
        if op in (Op.ARR_ALLOC, Op.ARR_INIT):
            result.arr_allocs += 1
        if op == Op.ARR_FREE:
            result.arr_frees += 1

        # String tracking
        if op in (Op.STR_LIT, Op.ITOS, Op.FTOS):
            result.str_allocs += 1
        if op == Op.STR_LIT:
            str_len = data[addr + 1]
            result.string_literal_bytes += str_len
        if op in (Op.STR_FREE, Op.STR_CLEAR):
            result.str_frees += 1

        # RTC reads (each allocates an array)
        if op == Op.RTC_READ:
            result.rtc_reads += 1

        # Draw image
        if op == Op.DRAW_IMAGE:
            result.draw_image_count += 1

        # Font ID tracking (from W_SET FONT_ID)
        if op == Op.W_SET and addr + 1 < len(data):
            prop = data[addr + 1]
            if prop == 0x1C:  # FONT_ID
                result.font_ids.add('dynamic')  # can't know value statically

        if op == Op.W_SET_LEN and addr + 1 < len(data):
            prop = data[addr + 1]
            # TEXT property = string data in widget
            pass

    return result


def analyze_image(data, exec_mode=None):
    """Analyze a compiled VM image (header + bytecode). Returns AnalysisResult."""
    functions, opcode_start, global_count, widget_count, ext_count, header_flags = \
        parse_image_header(data)

    result = analyze_bytecode(data, opcode_start, len(data))
    result.image_size = len(data)
    result.header_size = opcode_start
    result.opcode_size = len(data) - opcode_start
    result.functions = functions
    result.global_count = global_count

    # Exec mode: header flags overrides command-line, which overrides auto
    if exec_mode is None and (header_flags & 0x01):
        exec_mode = 'flash'
    result.exec_mode = exec_mode or ('ram' if result.opcode_size <= 4096 else 'flash')

    # v4+ headers carry exact widget/ext counts from the compiler
    if widget_count > 0:
        result.widget_allocs = widget_count
    if ext_count > 0:
        result.widgets_with_ext = ext_count
    else:
        result.widgets_with_ext = len(result._ext_widget_targets)

    # Per-function analysis
    for func in functions:
        func_start = opcode_start + func['offset']
        func_end = func_start + func['length']
        if func_start >= len(data):
            continue
        if func_end > len(data):
            func_end = len(data)

        fstats = {
            'instructions': 0,
            'bytes': func['length'],
            'categories': {},
            'widget_allocs': 0,
            'local_frame': 0,
        }

        for addr, op, size in walk_bytecode(data, func_start, func_end):
            fstats['instructions'] += 1
            cat = OPCODE_CATEGORIES.get(op, 'unknown')
            fstats['categories'][cat] = fstats['categories'].get(cat, 0) + 1

            if op in (Op.W_ALLOC, Op.W_ALLTAR):
                fstats['widget_allocs'] += 1

            if op == Op.FRAME:
                fstats['local_frame'] = data[addr + 1]

        result.func_stats[func['name']] = fstats

    return result


# ============================================================
# Compile from source
# ============================================================

def compile_source(source_path, include_dirs=None):
    """Compile .fl source and return image bytes."""
    from ferrite_lang import build_image, preprocess

    with open(source_path, 'r', encoding='utf-8') as f:
        source = f.read()

    return build_image(source, source_path, include_dirs or [])


# ============================================================
# Formatting
# ============================================================

def format_bytes(n):
    """Format byte count with unit."""
    if n >= 1024:
        return f"{n:,} bytes ({n / 1024:.1f} KB)"
    return f"{n:,} bytes"


def format_report(result, verbose=False):
    """Format analysis result as human-readable text."""
    lines = []
    lines.append("=" * 60)
    lines.append("  FERRITE BYTECODE ANALYSIS")
    lines.append("=" * 60)

    # -- Image overview --
    lines.append("")
    lines.append("IMAGE OVERVIEW")
    lines.append(f"  Total image size:   {format_bytes(result.image_size)}")
    lines.append(f"  Header size:        {format_bytes(result.header_size)}")
    lines.append(f"  Bytecode size:      {format_bytes(result.opcode_size)}")
    lines.append(f"  Functions:          {len(result.functions)}")
    lines.append(f"  Total instructions: {result.total_instructions}")
    lines.append(f"  Exec mode:          {result.exec_mode or 'auto'}")
    if result.opcode_size > 4096 and result.exec_mode != 'flash':
        lines.append(f"  WARNING: Bytecode > 4KB --must use exec_mode: flash")

    # -- Memory estimate --
    mem = result.estimate_memory()
    lines.append("")
    lines.append("ESTIMATED RUNTIME MEMORY (heap)")
    n_widgets = result.total_widgets()
    n_ext = result.widgets_with_ext
    lines.append(f"  Widget base:        {format_bytes(mem['widgets_base'])}  ({n_widgets} x {WIDGET_BASE_SIZE}B)")
    lines.append(f"  Widget ext:         {format_bytes(mem['widgets_ext'])}  ({n_ext} x {WIDGET_EXT_SIZE}B)")
    lines.append(f"  Widgets total:      {format_bytes(mem['widgets'])}  ({n_widgets - n_ext} base-only, {n_ext} with ext)")
    n_globals = len(result.global_var_slots)
    n_max_local = result.max_local_frame
    lines.append(f"  Variables:          {format_bytes(mem['variables'])}  ({n_globals} globals + {n_max_local} max locals)")
    lines.append(f"  VM fixed:           {format_bytes(mem['vm_fixed'])}  (eval stack + call stack + callbacks)")
    lines.append(f"  Function table:     {format_bytes(mem['func_table'])}  ({len(result.functions)} entries)")
    if result.exec_mode != 'flash':
        lines.append(f"  Bytecode (RAM):     {format_bytes(mem['bytecode'])}")
    else:
        lines.append(f"  Bytecode:           0 bytes  (flash exec)")
    if mem['strings_est']:
        lines.append(f"  Strings (est.):     {format_bytes(mem['strings_est'])}  (peak ~{max(0, result.str_allocs - result.str_frees)} active)")
    if mem['arrays_est']:
        lines.append(f"  Arrays (est.):      {format_bytes(mem['arrays_est'])}  (peak ~{max(0, result.arr_allocs - result.arr_frees)} active)")
    lines.append(f"  {'=' * 31}")
    lines.append(f"  TOTAL (est.):       {format_bytes(mem['total'])}")
    lines.append(f"  Heap remaining:     ~{format_bytes(14336 - mem['total'])}  (of 14 KB)")
    pct = mem['total'] / 14336 * 100
    lines.append(f"  Heap usage:         {pct:.1f}%")
    if pct > 80:
        lines.append(f"  WARNING: High memory usage --risk of OOM")

    # -- Instruction categories --
    lines.append("")
    lines.append("INSTRUCTION CATEGORIES")
    cats = sorted(result.category_counts.items(), key=lambda x: -x[1])
    max_count = max(c for _, c in cats) if cats else 1
    for cat, count in cats:
        pct = count / result.total_instructions * 100
        bar_len = int(count / max_count * 20)
        bar = "#" * bar_len + "." * (20 - bar_len)
        lines.append(f"  {cat:<12} {count:>5} ({pct:>5.1f}%)  {bar}")

    # -- Top opcodes --
    lines.append("")
    lines.append("TOP 15 OPCODES")
    top = sorted(result.opcode_freq.items(), key=lambda x: -x[1])[:15]
    for op, count in top:
        name = OP_NAMES.get(op, f'0x{op:02X}')
        pct = count / result.total_instructions * 100
        lines.append(f"  {name:<18} {count:>5} ({pct:>5.1f}%)")

    # -- Resource usage --
    lines.append("")
    lines.append("RESOURCE USAGE")
    lines.append(f"  Widget allocations: {result.widget_allocs}  (+ 1 root = {result.total_widgets()} total)")
    lines.append(f"  Global vars:        {len(result.global_var_slots)}  (of 128 max)")
    lines.append(f"  Max local frame:    {result.max_local_frame}  (of 128 max)")
    lines.append(f"  Array allocs:       {result.arr_allocs}  (frees: {result.arr_frees})")
    lines.append(f"  String allocs:      {result.str_allocs}  (frees: {result.str_frees})")
    lines.append(f"  String literals:    {format_bytes(result.string_literal_bytes)}")
    if result.rtc_reads:
        lines.append(f"  RTC reads:          {result.rtc_reads}  (each allocs array --use arrFree!)")
    if result.draw_image_count:
        lines.append(f"  Image draws:        {result.draw_image_count}")

    # -- Per-function details --
    if verbose and result.func_stats:
        lines.append("")
        lines.append("PER-FUNCTION BREAKDOWN")
        lines.append(f"  {'Function':<24} {'Bytes':>6} {'Instrs':>7} {'Widgets':>8} {'Frame':>6}")
        lines.append(f"  {'-' * 24} {'-' * 6} {'-' * 7} {'-' * 8} {'-' * 5}")

        for func in result.functions:
            name = func['name']
            stats = result.func_stats.get(name, {})
            nbytes = stats.get('bytes', 0)
            ninstr = stats.get('instructions', 0)
            nwidgets = stats.get('widget_allocs', 0)
            nvars = stats.get('local_frame', 0)
            kind = func.get('kind_name', '')
            label = f"{name}()" if kind in ('setup', 'loop') else f"{name}()"
            lines.append(f"  {label:<24} {nbytes:>6} {ninstr:>7} {nwidgets:>8} {nvars:>5}")

        # Category breakdown per function
        if any(result.func_stats.values()):
            lines.append("")
            lines.append("  Category breakdown per function:")
            for func in result.functions:
                name = func['name']
                stats = result.func_stats.get(name, {})
                cats = stats.get('categories', {})
                if cats:
                    parts = [f"{c}:{n}" for c, n in sorted(cats.items(), key=lambda x: -x[1])[:5]]
                    lines.append(f"    {name + '()':<22} {', '.join(parts)}")

    # -- Warnings --
    warnings = []
    if result.rtc_reads > 0 and result.arr_frees == 0:
        warnings.append("rtcRead() used but no arrFree() --potential memory leak")
    if result.str_allocs > result.str_frees + 5:
        warnings.append(f"String allocs ({result.str_allocs}) >> frees ({result.str_frees}) --use strClear()/strFree()")
    if result.widget_allocs > 60:
        warnings.append(f"Widget count ({result.total_widgets()}) near MAX_WIDGETS limit (64)")
    if len(result.global_var_slots) > 100:
        warnings.append(f"Global variable slots ({len(result.global_var_slots)}) near max (128)")
    if result.opcode_size > 4096 and result.exec_mode != 'flash':
        warnings.append("Bytecode > 4KB but not using flash exec --will be truncated!")

    if warnings:
        lines.append("")
        lines.append("WARNINGS")
        for w in warnings:
            lines.append(f"  ! {w}")

    lines.append("")
    return '\n'.join(lines)


def format_json(result):
    """Format analysis result as JSON."""
    mem = result.estimate_memory()
    return {
        'image': {
            'total_size': result.image_size,
            'header_size': result.header_size,
            'bytecode_size': result.opcode_size,
            'exec_mode': result.exec_mode,
        },
        'instructions': {
            'total': result.total_instructions,
            'categories': result.category_counts,
            'top_opcodes': {
                OP_NAMES.get(op, f'0x{op:02X}'): count
                for op, count in sorted(result.opcode_freq.items(), key=lambda x: -x[1])
            },
        },
        'memory': {
            'estimated_total': mem['total'],
            'widgets': mem['widgets'],
            'widgets_base': mem['widgets_base'],
            'widgets_ext': mem['widgets_ext'],
            'variables': mem['variables'],
            'vm_fixed': mem['vm_fixed'],
            'func_table': mem['func_table'],
            'bytecode': mem['bytecode'],
            'strings_est': mem['strings_est'],
            'arrays_est': mem['arrays_est'],
            'heap_remaining': 14336 - mem['total'],
            'heap_usage_pct': round(mem['total'] / 14336 * 100, 1),
        },
        'resources': {
            'widgets': result.total_widgets(),
            'widgets_with_ext': result.widgets_with_ext,
            'widget_allocs': result.widget_allocs,
            'global_vars': len(result.global_var_slots),
            'max_local_frame': result.max_local_frame,
            'array_allocs': result.arr_allocs,
            'array_frees': result.arr_frees,
            'string_allocs': result.str_allocs,
            'string_frees': result.str_frees,
            'string_literal_bytes': result.string_literal_bytes,
            'rtc_reads': result.rtc_reads,
        },
        'functions': [
            {
                'name': f['name'],
                'kind': f['kind_name'],
                'bytes': f['length'],
                'instructions': result.func_stats.get(f['name'], {}).get('instructions', 0),
                'widget_allocs': result.func_stats.get(f['name'], {}).get('widget_allocs', 0),
            }
            for f in result.functions
        ],
    }


# ============================================================
# Flash image formatting
# ============================================================

def _flash_resource_summary(entries):
    """Build a summary dict: {kind_name: (count, total_size)}"""
    summary = {}
    for e in entries:
        kn = e['kind_name']
        c, s = summary.get(kn, (0, 0))
        summary[kn] = (c + 1, s + e['size'])
    return summary


def format_flash_report(result, verbose=False):
    """Format flash FS analysis as human-readable text."""
    lines = []
    lines.append("=" * 60)
    lines.append("  FERRITE FLASH IMAGE ANALYSIS")
    lines.append("=" * 60)

    flash = result['flash']
    header = flash['header']
    entries = flash['entries']
    programs = result['programs']

    # -- Flash header --
    lines.append("")
    lines.append("FLASH FILESYSTEM HEADER")
    lines.append(f"  FS base:             0x{flash['fs_base']:06X}")
    lines.append(f"  FS version:          {header['version']}")
    lines.append(f"  Screen:              {header['screen_w']} x {header['screen_h']}")
    lines.append(f"  Resource count:      {header['resource_count']}")
    lines.append(f"  Checksum:            0x{header['checksum']:08X}")

    # -- Resource summary --
    lines.append("")
    lines.append("RESOURCE SUMMARY")
    summary = _flash_resource_summary(entries)
    total_data = sum(e['size'] for e in entries)
    lines.append(f"  Total resources:     {len(entries)}")
    lines.append(f"  Total data size:     {format_bytes(total_data)}")
    for kind_name in ('Program', 'Font', 'Image', 'Page', 'File'):
        if kind_name in summary:
            c, s = summary[kind_name]
            lines.append(f"  {kind_name:<10}          {c:>4}  ({format_bytes(s)})")

    # -- Flash usage --
    lines.append("")
    lines.append("FLASH USAGE (W25Q256, 32 MB)")
    used = flash['flash_used']
    pct = used / flash['flash_total'] * 100
    lines.append(f"  FS region used:      {format_bytes(used)}")
    lines.append(f"  Flash total:         {format_bytes(flash['flash_total'])}")
    lines.append(f"  Usage:               {pct:.1f}%")
    if flash['file_size'] < flash['fs_base'] + used:
        lines.append(f"  Note: file is {format_bytes(flash['file_size'])}, "
                     f"truncated before end of data region")

    # -- Resource table --
    lines.append("")
    lines.append("RESOURCE TABLE")
    lines.append(f"  {'Name':<17} {'Type':<10} {'Flash':>10} {'File':>10} {'Size':>10}  Flags")
    lines.append(f"  {'-' * 17} {'-' * 10} {'-' * 10} {'-' * 10} {'-' * 10}  {'-' * 5}")
    for e in entries:
        flags_str = ""
        if e['flags'] & RES_FLAG_FLASH_EXEC:
            flags_str = "flash-exec"
        lines.append(
            f"  {e['name']:<17} {e['kind_name']:<10} "
            f"0x{e['offset']:08X} 0x{e['file_offset']:06X}  "
            f"{format_bytes(e['size']):>10}  {flags_str}"
        )

    # -- Program analysis --
    if programs:
        for prog in programs:
            lines.append("")
            lines.append(f"--- Program: {prog['name']} ---")
            lines.append(f"  Size:       {format_bytes(prog['size'])}")
            lines.append(f"  Offset:     0x{prog['offset']:08X}")
            if prog['flags'] & RES_FLAG_FLASH_EXEC:
                lines.append("  Exec mode:  flash (flag set)")
            else:
                lines.append("  Exec mode:  ram (or overridden)")

            analysis = prog['analysis']
            if isinstance(analysis, str):
                lines.append(f"  ERROR: {analysis}")
                continue

            lines.append(f"  Functions:  {len(analysis.functions)}")
            lines.append(f"  Instrs:     {analysis.total_instructions}")
            lines.append(f"  Bytecode:   {format_bytes(analysis.opcode_size)}")

            mem = analysis.estimate_memory()
            lines.append(f"  RAM est.:   {format_bytes(mem['total'])}")
            heap_pct = mem['total'] / 14336 * 100
            lines.append(f"  Heap usage: {heap_pct:.1f}%  (widgets: {analysis.total_widgets()}, "
                         f"globals: {len(analysis.global_var_slots)})")

            if verbose:
                lines.append("  Functions:")
                for func in analysis.functions:
                    stats = analysis.func_stats.get(func['name'], {})
                    lines.append(
                        f"    {func['name']:<24} "
                        f"{stats.get('bytes', 0):>5}B  "
                        f"{stats.get('instructions', 0):>4} instrs"
                    )

                cats = sorted(analysis.category_counts.items(), key=lambda x: -x[1])
                if cats:
                    lines.append("  Categories:")
                    for cat, count in cats[:5]:
                        lines.append(f"    {cat:<12} {count:>5}")

            # Warnings
            if analysis.rtc_reads > 0 and analysis.arr_frees == 0:
                lines.append("  ! rtcRead() used but no arrFree()")
            if analysis.str_allocs > analysis.str_frees + 5:
                lines.append(f"  ! String allocs ({analysis.str_allocs}) >> frees ({analysis.str_frees})")
            if analysis.widget_allocs > 60:
                lines.append(f"  ! Widgets ({analysis.total_widgets()}) near limit (64)")
            if analysis.opcode_size > 4096 and analysis.exec_mode != 'flash':
                lines.append("  ! Bytecode > 4KB but not flash exec")

    lines.append("")
    return '\n'.join(lines)


def format_flash_json(result):
    """Format flash FS analysis as JSON."""
    flash = result['flash']
    header = flash['header']
    entries = flash['entries']

    summary = _flash_resource_summary(entries)
    by_kind_json = {
        kn: {'count': c, 'total_size': s}
        for kn, (c, s) in summary.items()
    }

    programs_json = []
    for prog in result['programs']:
        analysis = prog['analysis']
        if isinstance(analysis, str):
            prog_json = {
                'name': prog['name'],
                'size': prog['size'],
                'offset': prog['offset'],
                'flags': prog['flags'],
                'error': analysis,
            }
        else:
            mem = analysis.estimate_memory()
            prog_json = {
                'name': prog['name'],
                'size': prog['size'],
                'offset': prog['offset'],
                'flags': prog['flags'],
                'exec_mode': analysis.exec_mode,
                'functions': len(analysis.functions),
                'instructions': analysis.total_instructions,
                'bytecode_size': analysis.opcode_size,
                'widgets': analysis.total_widgets(),
                'widgets_with_ext': analysis.widgets_with_ext,
                'global_vars': len(analysis.global_var_slots),
                'max_local_frame': analysis.max_local_frame,
                'array_allocs': analysis.arr_allocs,
                'array_frees': analysis.arr_frees,
                'string_allocs': analysis.str_allocs,
                'string_frees': analysis.str_frees,
                'ram_estimate': mem['total'],
                'heap_usage_pct': round(mem['total'] / 14336 * 100, 1),
                'categories': analysis.category_counts,
                'functions_detail': [
                    {
                        'name': f['name'],
                        'kind': f['kind_name'],
                        'bytes': f['length'],
                        'instructions': analysis.func_stats.get(f['name'], {}).get('instructions', 0),
                    }
                    for f in analysis.functions
                ],
            }
        programs_json.append(prog_json)

    return {
        'flash': {
            'fs_base': flash['fs_base'],
            'version': header['version'],
            'screen_w': header['screen_w'],
            'screen_h': header['screen_h'],
            'resource_count': header['resource_count'],
            'checksum': f"0x{header['checksum']:08X}",
            'flash_used': flash['flash_used'],
            'flash_total': flash['flash_total'],
            'file_size': flash['file_size'],
            'resources': by_kind_json,
        },
        'resource_table': [
            {
                'name': e['name'],
                'kind': e['kind'],
                'kind_name': e['kind_name'],
                'offset': e['offset'],
                'file_offset': e['file_offset'],
                'size': e['size'],
                'flags': e['flags'],
            }
            for e in entries
        ],
        'programs': programs_json,
    }


# ============================================================
# CLI
# ============================================================

def main():
    parser = argparse.ArgumentParser(
        description='Ferrite bytecode analyzer --memory and instruction profiler',
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""examples:
  %(prog)s demo.fl                     analyze source file
  %(prog)s demo.fl -I lib/             with include path
  %(prog)s --bin firmware.bin           analyze compiled VM image
  %(prog)s --bin flash.bin              auto-detected as flash image
  %(prog)s demo.fl --json              JSON output
  %(prog)s demo.fl -v                  verbose (per-function details)
""")

    parser.add_argument('source', nargs='?', help='.fl source file')
    parser.add_argument('--bin', metavar='FILE', help='analyze compiled VM image or flash dump')
    parser.add_argument('-I', '--include', action='append', default=[], help='include directory (repeatable)')
    parser.add_argument('--exec-mode', choices=['ram', 'flash'], help='override exec mode')
    parser.add_argument('--flash-base', type=lambda x: int(x, 0), default=None,
                        help='flash FS base address (default: 0x1000, auto-detected)')
    parser.add_argument('--json', action='store_true', help='output as JSON')
    parser.add_argument('-v', '--verbose', action='store_true', help='show per-function breakdown')

    args = parser.parse_args()

    if not args.source and not args.bin:
        parser.error("provide a .fl source file or --bin binary")

    try:
        if args.bin:
            with open(args.bin, 'rb') as f:
                data = f.read()

            # Auto-detect: check for flash FS magic
            fs_base = args.flash_base if args.flash_base is not None else DEFAULT_FS_BASE
            hdr_off, _detected = detect_flash_image(data, fs_base)

            if hdr_off is not None:
                result = analyze_flash_image(data, fs_base, args.exec_mode)
                if args.json:
                    print(json.dumps(format_flash_json(result), indent=2))
                else:
                    print(format_flash_report(result, verbose=args.verbose))
            else:
                result = analyze_image(data, args.exec_mode)
                if args.json:
                    print(json.dumps(format_json(result), indent=2))
                else:
                    print(format_report(result, verbose=args.verbose))
        else:
            data = compile_source(args.source, args.include)
            result = analyze_image(data, args.exec_mode)
            if args.json:
                print(json.dumps(format_json(result), indent=2))
            else:
                print(format_report(result, verbose=args.verbose))

    except Exception as e:
        print(f"error: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == '__main__':
    main()
