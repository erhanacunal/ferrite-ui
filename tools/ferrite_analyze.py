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
    """Parse VM image header. Returns (functions, opcode_start, global_count).

    functions: list of {id, name, kind, offset, length}
    opcode_start: byte offset where opcodes begin
    global_count: number of global variable slots (v2+, 0 for v1)
    """
    if len(data) < 5:
        raise ValueError("Image too small for header")

    version = data[0]
    func_count = struct.unpack_from('<H', data, 1)[0]
    global_count = struct.unpack_from('<H', data, 3)[0] if version >= 2 else 0
    opcode_start = 5 + func_count * 12

    # Get real names if available (from _ImageBytes)
    real_names = getattr(data, 'func_names', {})

    functions = []
    for i in range(func_count):
        base = 5 + i * 12
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

    return functions, opcode_start, global_count


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
                pass  # local — tracked via FRAME
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
    functions, opcode_start, global_count = parse_image_header(data)

    result = analyze_bytecode(data, opcode_start, len(data))
    result.image_size = len(data)
    result.header_size = opcode_start
    result.opcode_size = len(data) - opcode_start
    result.functions = functions
    result.exec_mode = exec_mode or ('ram' if result.opcode_size <= 4096 else 'flash')
    result.widgets_with_ext = len(result._ext_widget_targets)
    result.global_count = global_count

    # Per-function analysis
    for func in functions:
        func_start = opcode_start + func['offset']
        func_end = func_start + func['length']
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
# CLI
# ============================================================

def main():
    parser = argparse.ArgumentParser(
        description='Ferrite bytecode analyzer --memory and instruction profiler',
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""examples:
  %(prog)s demo.fl                     analyze source file
  %(prog)s demo.fl -I lib/             with include path
  %(prog)s --bin firmware.bin           analyze compiled binary
  %(prog)s demo.fl --json              JSON output
  %(prog)s demo.fl -v                  verbose (per-function details)
""")

    parser.add_argument('source', nargs='?', help='.fl source file')
    parser.add_argument('--bin', metavar='FILE', help='analyze compiled binary instead of source')
    parser.add_argument('-I', '--include', action='append', default=[], help='include directory (repeatable)')
    parser.add_argument('--exec-mode', choices=['ram', 'flash'], help='override exec mode')
    parser.add_argument('--json', action='store_true', help='output as JSON')
    parser.add_argument('-v', '--verbose', action='store_true', help='show per-function breakdown')

    args = parser.parse_args()

    if not args.source and not args.bin:
        parser.error("provide a .fl source file or --bin binary")

    try:
        if args.bin:
            with open(args.bin, 'rb') as f:
                data = f.read()
            result = analyze_image(data, args.exec_mode)
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
