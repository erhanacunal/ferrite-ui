#!/usr/bin/env python3
"""ferrite-ui language compiler.

Simple C-like language that compiles to VM bytecode via the assembler.

Usage:
  python ferrite_lang.py source.fl -o output.bin
  python ferrite_lang.py source.fl --disasm
  python ferrite_lang.py source.fl --page 0x0000 -o page.bin

Python API:
  from ferrite_lang import compile
  bytecode = compile(source_code)
"""

import sys
import struct
from ferrite_cc import (Asm, Op, WT, Prop, Builtin, disassemble, encode_svarint,
                        _resolve_prop, PROP_MAP, pack_pair, float_bits)


# ============================================================
# Errors
# ============================================================


class CompileError(Exception):
    def __init__(self, msg, line=0):
        super().__init__(f"line {line}: {msg}" if line else msg)
        self.line = line


# ============================================================
# Tokens
# ============================================================

KEYWORDS = {
    'var', 'fn', 'if', 'else', 'while', 'for',
    'return', 'true', 'false', 'break', 'continue',
}


class Token:
    __slots__ = ('type', 'value', 'line')

    def __init__(self, type, value, line):
        self.type = type
        self.value = value
        self.line = line

    def __repr__(self):
        return f'{self.type}({self.value!r}) L{self.line}'


# ============================================================
# Lexer
# ============================================================


def tokenize(source):
    tokens = []
    i = 0
    line = 1
    n = len(source)

    while i < n:
        ch = source[i]

        # Whitespace
        if ch in ' \t\r':
            i += 1
            continue
        if ch == '\n':
            line += 1
            i += 1
            continue

        # Comments
        if ch == '/' and i + 1 < n:
            if source[i + 1] == '/':
                i += 2
                while i < n and source[i] != '\n':
                    i += 1
                continue
            if source[i + 1] == '*':
                i += 2
                while i + 1 < n and not (source[i] == '*' and source[i + 1] == '/'):
                    if source[i] == '\n':
                        line += 1
                    i += 1
                i += 2
                continue

        # Numbers
        if ch.isdigit():
            start = i
            if ch == '0' and i + 1 < n and source[i + 1] in 'xX':
                i += 2
                while i < n and (source[i].isdigit() or source[i] in 'abcdefABCDEF_'):
                    i += 1
                val = int(source[start:i].replace('_', ''), 16)
            elif ch == '0' and i + 1 < n and source[i + 1] in 'bB':
                i += 2
                while i < n and source[i] in '01_':
                    i += 1
                val = int(source[start:i].replace('_', ''), 2)
            else:
                while i < n and (source[i].isdigit() or source[i] == '_'):
                    i += 1
                # Check for float literal (decimal point)
                if i < n and source[i] == '.' and (i + 1 >= n or source[i + 1] != '.'):
                    i += 1  # skip '.'
                    while i < n and (source[i].isdigit() or source[i] == '_'):
                        i += 1
                    val = float(source[start:i].replace('_', ''))
                    tokens.append(Token('FLOAT', val, line))
                    continue
                val = int(source[start:i].replace('_', ''))
            tokens.append(Token('NUM', val, line))
            continue

        # String literals
        if ch == '"':
            i += 1
            start = i
            while i < n and source[i] != '"':
                if source[i] == '\\':
                    i += 1  # skip escaped char
                if source[i] == '\n':
                    line += 1
                i += 1
            if i >= n:
                raise CompileError("unterminated string literal", line)
            raw = source[start:i]
            # Process escape sequences
            val = raw.replace('\\n', '\n').replace('\\t', '\t').replace('\\"', '"').replace('\\\\', '\\')
            tokens.append(Token('STR', val, line))
            i += 1  # skip closing "
            continue

        # Identifiers / keywords
        if ch.isalpha() or ch == '_':
            start = i
            while i < n and (source[i].isalnum() or source[i] == '_'):
                i += 1
            word = source[start:i]
            if word in KEYWORDS:
                tokens.append(Token(word.upper(), word, line))
            else:
                tokens.append(Token('IDENT', word, line))
            continue

        # Two-char operators
        two = source[i:i + 2] if i + 1 < n else ''
        if two in ('==', '!=', '<=', '>=', '&&', '||'):
            tokens.append(Token(two, two, line))
            i += 2
            continue

        # Single-char operators and punctuation
        single_map = {
            '+': '+', '-': '-', '*': '*', '/': '/', '%': '%',
            '&': '&', '|': '|', '!': '!', '<': '<', '>': '>',
            '=': '=', '(': '(', ')': ')', '{': '{', '}': '}',
            '[': '[', ']': ']', ',': ',', ';': ';',
        }
        if ch in single_map:
            tokens.append(Token(ch, ch, line))
            i += 1
            continue

        raise CompileError(f"unexpected character: {ch!r}", line)

    tokens.append(Token('EOF', None, line))
    return tokens


# ============================================================
# AST nodes
# ============================================================

# -- Expressions --

class NumLit:
    def __init__(self, value, line):
        self.value = value
        self.line = line


class BoolLit:
    def __init__(self, value, line):
        self.value = value
        self.line = line


class StrLit:
    def __init__(self, value, line):
        self.value = value  # str
        self.line = line


class FloatLit:
    def __init__(self, value, line):
        self.value = value  # Python float
        self.line = line


class VarRef:
    def __init__(self, name, line):
        self.name = name
        self.line = line


class IndexExpr:
    def __init__(self, name, index, line):
        self.name = name
        self.index = index
        self.line = line


class BinOp:
    def __init__(self, op, left, right, line):
        self.op = op
        self.left = left
        self.right = right
        self.line = line


class UnaryOp:
    def __init__(self, op, operand, line):
        self.op = op
        self.operand = operand
        self.line = line


class CallExpr:
    def __init__(self, name, args, line):
        self.name = name
        self.args = args
        self.line = line


class ArrayLit:
    def __init__(self, elements, line):
        self.elements = elements
        self.line = line


# -- Statements --

class VarDecl:
    def __init__(self, name, init, line, array_size=None):
        self.name = name
        self.init = init
        self.line = line
        self.array_size = array_size


class Assign:
    def __init__(self, name, value, line, index=None):
        self.name = name
        self.value = value
        self.line = line
        self.index = index


class IfStmt:
    def __init__(self, cond, then_body, else_body, line):
        self.cond = cond
        self.then_body = then_body
        self.else_body = else_body
        self.line = line


class WhileStmt:
    def __init__(self, cond, body, line):
        self.cond = cond
        self.body = body
        self.line = line


class ForStmt:
    def __init__(self, init, cond, update, body, line):
        self.init = init
        self.cond = cond
        self.update = update
        self.body = body
        self.line = line


class ReturnStmt:
    def __init__(self, value, line):
        self.value = value
        self.line = line


class BreakStmt:
    def __init__(self, line):
        self.line = line


class ContinueStmt:
    def __init__(self, line):
        self.line = line


class ExprStmt:
    def __init__(self, expr, line):
        self.expr = expr
        self.line = line


class FnDef:
    def __init__(self, name, params, body, line):
        self.name = name
        self.params = params
        self.body = body
        self.line = line


class Program:
    def __init__(self, functions, statements):
        self.functions = functions
        self.statements = statements


# ============================================================
# Parser (recursive descent)
# ============================================================


class Parser:
    def __init__(self, tokens):
        self.tokens = tokens
        self.pos = 0

    def _cur(self):
        return self.tokens[self.pos]

    def _peek_type(self):
        return self._cur().type

    def _at_end(self):
        return self._peek_type() == 'EOF'

    def _advance(self):
        tok = self.tokens[self.pos]
        self.pos += 1
        return tok

    def _check(self, *types):
        return self._peek_type() in types

    def _match(self, *types):
        if self._peek_type() in types:
            return self._advance()
        return None

    def _expect(self, type):
        tok = self._cur()
        if tok.type != type:
            raise CompileError(f"expected {type}, got {tok.type} ({tok.value!r})", tok.line)
        return self._advance()

    def _error(self, msg):
        raise CompileError(msg, self._cur().line)

    # --- Top level ---

    def parse(self):
        functions = []
        statements = []
        while not self._at_end():
            if self._check('FN'):
                functions.append(self._fn_def())
            else:
                statements.append(self._statement())
        return Program(functions, statements)

    def _fn_def(self):
        line = self._cur().line
        self._expect('FN')
        name = self._expect('IDENT').value
        self._expect('(')
        params = []
        if not self._check(')'):
            params.append(self._expect('IDENT').value)
            while self._match(','):
                params.append(self._expect('IDENT').value)
        self._expect(')')
        body = self._block()
        return FnDef(name, params, body, line)

    def _block(self):
        self._expect('{')
        stmts = []
        while not self._check('}') and not self._at_end():
            stmts.append(self._statement())
        self._expect('}')
        return stmts

    # --- Statements ---

    def _statement(self):
        if self._check('VAR'):
            return self._var_decl()
        if self._check('IF'):
            return self._if_stmt()
        if self._check('WHILE'):
            return self._while_stmt()
        if self._check('FOR'):
            return self._for_stmt()
        if self._check('RETURN'):
            return self._return_stmt()
        if self._check('BREAK'):
            line = self._advance().line
            self._expect(';')
            return BreakStmt(line)
        if self._check('CONTINUE'):
            line = self._advance().line
            self._expect(';')
            return ContinueStmt(line)
        return self._assign_or_expr_stmt()

    def _var_decl(self):
        line = self._cur().line
        self._expect('VAR')
        name = self._expect('IDENT').value
        array_size = None
        if self._match('['):
            array_size = self._expect('NUM').value
            self._expect(']')
        init = None
        if self._match('='):
            init = self._expression()
        self._expect(';')
        return VarDecl(name, init, line, array_size)

    def _if_stmt(self):
        line = self._cur().line
        self._expect('IF')
        self._expect('(')
        cond = self._expression()
        self._expect(')')
        then_body = self._block()
        else_body = None
        if self._match('ELSE'):
            if self._check('IF'):
                else_body = [self._if_stmt()]
            else:
                else_body = self._block()
        return IfStmt(cond, then_body, else_body, line)

    def _while_stmt(self):
        line = self._cur().line
        self._expect('WHILE')
        self._expect('(')
        cond = self._expression()
        self._expect(')')
        body = self._block()
        return WhileStmt(cond, body, line)

    def _for_stmt(self):
        line = self._cur().line
        self._expect('FOR')
        self._expect('(')
        # init
        if self._check('VAR'):
            init = self._var_decl()
        elif self._check(';'):
            self._advance()
            init = None
        else:
            init = self._assign_or_expr_stmt()
        # cond
        if self._check(';'):
            cond = BoolLit(True, line)
        else:
            cond = self._expression()
        self._expect(';')
        # update
        if self._check(')'):
            update = None
        else:
            update = self._parse_assign_or_expr(skip_semi=True)
        self._expect(')')
        body = self._block()
        return ForStmt(init, cond, update, body, line)

    def _return_stmt(self):
        line = self._cur().line
        self._expect('RETURN')
        value = None
        if not self._check(';'):
            value = self._expression()
        self._expect(';')
        return ReturnStmt(value, line)

    def _assign_or_expr_stmt(self):
        return self._parse_assign_or_expr(skip_semi=False)

    def _parse_assign_or_expr(self, skip_semi):
        expr = self._expression()
        # Check for assignment: ident = expr or ident[i] = expr
        if self._match('='):
            value = self._expression()
            if not skip_semi:
                self._expect(';')
            if isinstance(expr, VarRef):
                return Assign(expr.name, value, expr.line)
            elif isinstance(expr, IndexExpr):
                return Assign(expr.name, value, expr.line, expr.index)
            else:
                raise CompileError("invalid assignment target", expr.line)
        if not skip_semi:
            self._expect(';')
        return ExprStmt(expr, expr.line)

    # --- Expressions (precedence climbing) ---

    def _expression(self):
        return self._or_expr()

    def _or_expr(self):
        left = self._and_expr()
        while self._match('||'):
            right = self._and_expr()
            left = BinOp('||', left, right, left.line)
        return left

    def _and_expr(self):
        left = self._bitor_expr()
        while self._match('&&'):
            right = self._bitor_expr()
            left = BinOp('&&', left, right, left.line)
        return left

    def _bitor_expr(self):
        left = self._bitand_expr()
        while self._check('|') and not self._check('||'):
            self._advance()
            right = self._bitand_expr()
            left = BinOp('|', left, right, left.line)
        return left

    def _bitand_expr(self):
        left = self._equality_expr()
        while self._check('&') and not self._check('&&'):
            self._advance()
            right = self._equality_expr()
            left = BinOp('&', left, right, left.line)
        return left

    def _equality_expr(self):
        left = self._comparison_expr()
        while self._check('==', '!='):
            op = self._advance().value
            right = self._comparison_expr()
            left = BinOp(op, left, right, left.line)
        return left

    def _comparison_expr(self):
        left = self._add_expr()
        while self._check('<', '<=', '>', '>='):
            op = self._advance().value
            right = self._add_expr()
            left = BinOp(op, left, right, left.line)
        return left

    def _add_expr(self):
        left = self._mul_expr()
        while self._check('+', '-'):
            op = self._advance().value
            right = self._mul_expr()
            left = BinOp(op, left, right, left.line)
        return left

    def _mul_expr(self):
        left = self._unary_expr()
        while self._check('*', '/', '%'):
            op = self._advance().value
            right = self._unary_expr()
            left = BinOp(op, left, right, left.line)
        return left

    def _unary_expr(self):
        if self._check('-', '!'):
            op = self._advance()
            operand = self._unary_expr()
            return UnaryOp(op.value, operand, op.line)
        return self._postfix_expr()

    def _postfix_expr(self):
        expr = self._primary_expr()
        # Function call
        if isinstance(expr, VarRef) and self._check('('):
            self._advance()
            args = []
            if not self._check(')'):
                args.append(self._expression())
                while self._match(','):
                    args.append(self._expression())
            self._expect(')')
            return CallExpr(expr.name, args, expr.line)
        # Array index
        if isinstance(expr, VarRef) and self._match('['):
            index = self._expression()
            self._expect(']')
            return IndexExpr(expr.name, index, expr.line)
        return expr

    def _primary_expr(self):
        if self._check('NUM'):
            tok = self._advance()
            return NumLit(tok.value, tok.line)
        if self._check('STR'):
            tok = self._advance()
            return StrLit(tok.value, tok.line)
        if self._check('FLOAT'):
            tok = self._advance()
            return FloatLit(tok.value, tok.line)
        if self._check('TRUE'):
            tok = self._advance()
            return BoolLit(True, tok.line)
        if self._check('FALSE'):
            tok = self._advance()
            return BoolLit(False, tok.line)
        if self._check('IDENT'):
            tok = self._advance()
            return VarRef(tok.name if hasattr(tok, 'name') else tok.value, tok.line)
        if self._match('('):
            expr = self._expression()
            self._expect(')')
            return expr
        if self._match('['):
            elements = []
            if not self._check(']'):
                elements.append(self._expression())
                while self._match(','):
                    elements.append(self._expression())
            tok = self._expect(']')
            return ArrayLit(elements, tok.line)
        self._error(f"unexpected token: {self._cur().type}")


# ============================================================
# Code generator
# ============================================================

# Compound property -> scalar decomposition
COMPOUND_SCALARS = {
    Prop.LOCATION: [Prop.LOC_X, Prop.LOC_Y],
    Prop.SIZE: [Prop.SIZE_W, Prop.SIZE_H],
    Prop.MARGIN: [Prop.MARGIN_T, Prop.MARGIN_R, Prop.MARGIN_B, Prop.MARGIN_L],
    Prop.BORDER_EDGES: [Prop.BORDER_T, Prop.BORDER_R, Prop.BORDER_B, Prop.BORDER_L],
    Prop.PADDING: [Prop.PADDING_T, Prop.PADDING_R, Prop.PADDING_B, Prop.PADDING_L],
}

# Built-ins that don't leave a value on the stack
NO_VALUE_BUILTINS = {
    'target', 'set', 'parent', 'dirty', 'render', 'halt', 'yield_op',
    'fillRect', 'rect', 'line', 'circle', 'fillCircle',
    'drawImage', 'drawText', 'delay',
    'setText', 'drawStr', 'strClear',
}


class CodeGen:
    def __init__(self):
        self.asm = Asm()
        self.vars = {}          # name -> slot
        self.next_slot = 0
        self.functions = {}     # name -> {params, addr, patches}
        self.widget_ids = {}    # name -> predicted widget ID
        self.next_widget_id = 0
        self._loop_stack = []   # {continue_target, continue_patches, break_patches}
        self._fn_base = 0       # function vars start here
        self.array_vars = set() # names that hold array_ids

    def generate(self, program):
        # Phase 1: Pre-allocate main variable slots (count only)
        main_slots = self._count_var_slots(program.statements)
        self._fn_base = main_slots

        # Phase 2: Register functions
        for fn in program.functions:
            self.functions[fn.name] = {
                'params': fn.params,
                'addr': None,
                'patches': [],
            }

        # Phase 3: JMP over function bodies + emit them
        main_jmp = None
        if program.functions:
            main_jmp = self.asm.jmp_fwd()

        for fn in program.functions:
            self._gen_fn(fn)

        # Phase 4: Emit main code
        if main_jmp is not None:
            self.asm.patch(main_jmp)

        # Reset vars for main code emission
        self.vars = {}
        self.next_slot = 0

        for stmt in program.statements:
            self._gen_stmt(stmt)

        # Patch forward function calls
        for info in self.functions.values():
            for patch_pos in info['patches']:
                if info['addr'] is not None:
                    self.asm.patch(patch_pos, info['addr'])

        return self.asm.build()

    def _count_var_slots(self, stmts):
        """Recursively count variable slots needed by statements."""
        count = 0
        for stmt in stmts:
            if isinstance(stmt, VarDecl):
                count += 1  # arrays use 1 slot (arr_id), not N
            elif isinstance(stmt, IfStmt):
                count += self._count_var_slots(stmt.then_body)
                if stmt.else_body:
                    count += self._count_var_slots(stmt.else_body)
            elif isinstance(stmt, WhileStmt):
                count += self._count_var_slots(stmt.body)
            elif isinstance(stmt, ForStmt):
                if stmt.init and isinstance(stmt.init, VarDecl):
                    count += stmt.init.array_size if stmt.init.array_size else 1
                count += self._count_var_slots(stmt.body)
        return count

    # --- Variable management ---

    def _alloc_var(self, name):
        if name in self.vars:
            raise CompileError(f"variable already defined: {name}")
        if self.next_slot >= 16:
            raise CompileError(f"out of variable slots (max 16): {name}")
        slot = self.next_slot
        self.vars[name] = slot
        self.next_slot += 1
        return slot

    def _var_slot(self, name, line=0):
        if name not in self.vars:
            raise CompileError(f"undefined variable: {name}", line)
        return self.vars[name]

    # --- Functions ---

    def _gen_fn(self, fn):
        info = self.functions[fn.name]
        info['addr'] = self.asm.pos

        # Function vars start from _fn_base, reset between functions
        saved_vars = dict(self.vars)
        saved_slot = self.next_slot
        self.vars = {}
        self.next_slot = self._fn_base

        # Allocate param slots and pop args (reverse order)
        param_slots = []
        for p in fn.params:
            slot = self._alloc_var(p)
            param_slots.append(slot)
        for slot in reversed(param_slots):
            self.asm.store(slot)

        # Body
        for stmt in fn.body:
            self._gen_stmt(stmt)

        # Implicit return 0
        if not fn.body or not isinstance(fn.body[-1], ReturnStmt):
            self.asm.push(0)
            self.asm.ret()

        # Restore
        self.vars = saved_vars
        self.next_slot = saved_slot

    # --- Statements ---

    def _gen_stmt(self, node):
        if isinstance(node, VarDecl):
            self._gen_var_decl(node)
        elif isinstance(node, Assign):
            self._gen_assign(node)
        elif isinstance(node, IfStmt):
            self._gen_if(node)
        elif isinstance(node, WhileStmt):
            self._gen_while(node)
        elif isinstance(node, ForStmt):
            self._gen_for(node)
        elif isinstance(node, ReturnStmt):
            self._gen_return(node)
        elif isinstance(node, BreakStmt):
            self._gen_break(node)
        elif isinstance(node, ContinueStmt):
            self._gen_continue(node)
        elif isinstance(node, ExprStmt):
            self._gen_expr_stmt(node)
        else:
            raise CompileError(f"unknown statement: {type(node).__name__}")

    def _gen_var_decl(self, node):
        slot = self._alloc_var(node.name)
        if node.array_size is not None:
            # Array: VM pool'da oluştur, arr_id'yi var slot'a sakla
            self.array_vars.add(node.name)
            if node.init is not None:
                if not isinstance(node.init, ArrayLit):
                    raise CompileError("array must be initialized with [...]", node.line)
                if len(node.init.elements) != node.array_size:
                    raise CompileError(
                        f"array size mismatch: {node.name}[{node.array_size}] vs [{len(node.init.elements)}]",
                        node.line)
                all_const = all(isinstance(e, NumLit) for e in node.init.elements)
                if all_const:
                    # Tek instruction ile oluştur + başlat
                    self.asm.arr_alloc_init([e.value for e in node.init.elements])
                else:
                    # Boş oluştur, sonra tek tek yaz
                    self.asm.arr_alloc(node.array_size)
                    self.asm.store(slot)
                    for i, elem in enumerate(node.init.elements):
                        self.asm.load(slot)    # arr_id
                        self.asm.push(i)       # index
                        self._gen_expr(elem)   # value
                        self.asm.arr_store()
                    return  # slot'a zaten yazdık
            else:
                self.asm.arr_alloc(node.array_size)
        else:
            # Scalar variable
            if node.init is not None:
                if isinstance(node.init, CallExpr) and node.init.name == 'alloc':
                    self.widget_ids[node.name] = self.next_widget_id
                    self.next_widget_id += 1
                self._gen_expr(node.init)
            else:
                self.asm.push(0)
        self.asm.store(slot)

    def _gen_assign(self, node):
        if node.index is not None:
            # arr[idx] = value → arr_id, idx, value, ARR_STORE
            slot = self._var_slot(node.name, node.line)
            self.asm.load(slot)         # arr_id
            self._gen_expr(node.index)  # index
            self._gen_expr(node.value)  # value
            self.asm.arr_store()
        else:
            self._gen_expr(node.value)
            slot = self._var_slot(node.name, node.line)
            self.asm.store(slot)

    def _gen_if(self, node):
        self._gen_expr(node.cond)
        exit_patch = self.asm.jz_fwd()
        for stmt in node.then_body:
            self._gen_stmt(stmt)
        if node.else_body:
            else_patch = self.asm.jmp_fwd()
            self.asm.patch(exit_patch)
            for stmt in node.else_body:
                self._gen_stmt(stmt)
            self.asm.patch(else_patch)
        else:
            self.asm.patch(exit_patch)

    def _gen_while(self, node):
        start = self.asm.pos
        ctx = {'continue_target': start, 'continue_patches': [], 'break_patches': []}
        self._loop_stack.append(ctx)

        self._gen_expr(node.cond)
        exit_patch = self.asm.jz_fwd()

        for stmt in node.body:
            self._gen_stmt(stmt)

        self.asm.jmp(start)
        self.asm.patch(exit_patch)

        for p in ctx['break_patches']:
            self.asm.patch(p)
        self._loop_stack.pop()

    def _gen_for(self, node):
        # init
        if node.init:
            self._gen_stmt(node.init)

        loop_start = self.asm.pos
        ctx = {'continue_target': None, 'continue_patches': [], 'break_patches': []}
        self._loop_stack.append(ctx)

        # cond
        self._gen_expr(node.cond)
        exit_patch = self.asm.jz_fwd()

        # body
        for stmt in node.body:
            self._gen_stmt(stmt)

        # continue target = here (before update)
        ctx['continue_target'] = self.asm.pos
        for p in ctx['continue_patches']:
            self.asm.patch(p)

        # update
        if node.update:
            self._gen_stmt(node.update)

        self.asm.jmp(loop_start)
        self.asm.patch(exit_patch)

        for p in ctx['break_patches']:
            self.asm.patch(p)
        self._loop_stack.pop()

    def _gen_return(self, node):
        if node.value:
            self._gen_expr(node.value)
        else:
            self.asm.push(0)
        self.asm.ret()

    def _gen_break(self, node):
        if not self._loop_stack:
            raise CompileError("break outside loop", node.line)
        self._loop_stack[-1]['break_patches'].append(self.asm.jmp_fwd())

    def _gen_continue(self, node):
        if not self._loop_stack:
            raise CompileError("continue outside loop", node.line)
        ctx = self._loop_stack[-1]
        if ctx['continue_target'] is not None:
            self.asm.jmp(ctx['continue_target'])
        else:
            ctx['continue_patches'].append(self.asm.jmp_fwd())

    def _gen_expr_stmt(self, node):
        self._gen_expr(node.expr)
        # Discard value if expression leaves one on stack
        if self._expr_has_value(node.expr):
            self.asm.pop()

    def _expr_has_value(self, node):
        if isinstance(node, CallExpr):
            return node.name not in NO_VALUE_BUILTINS
        return True

    # --- Expressions ---

    def _gen_expr(self, node):
        if isinstance(node, NumLit):
            self.asm.push(node.value)
        elif isinstance(node, FloatLit):
            self.asm.push(float_bits(node.value))
        elif isinstance(node, BoolLit):
            self.asm.push(1 if node.value else 0)
        elif isinstance(node, VarRef):
            if node.name in self.array_vars:
                raise CompileError(f"'{node.name}' is an array, use {node.name}[index]", node.line)
            slot = self._var_slot(node.name, node.line)
            self.asm.load(slot)
        elif isinstance(node, IndexExpr):
            if node.name not in self.array_vars:
                raise CompileError(f"'{node.name}' is not an array", node.line)
            slot = self._var_slot(node.name, node.line)
            self.asm.load(slot)           # arr_id
            self._gen_expr(node.index)    # index (herhangi bir expression)
            self.asm.arr_load()
        elif isinstance(node, BinOp):
            self._gen_binop(node)
        elif isinstance(node, UnaryOp):
            self._gen_unary(node)
        elif isinstance(node, CallExpr):
            self._gen_call(node)
        else:
            raise CompileError(f"unknown expression: {type(node).__name__}")

    def _gen_binop(self, node):
        # Short-circuit logical operators
        if node.op == '&&':
            self._gen_expr(node.left)
            self.asm.dup()
            patch = self.asm.jz_fwd()
            self.asm.pop()
            self._gen_expr(node.right)
            self.asm.patch(patch)
            return
        if node.op == '||':
            self._gen_expr(node.left)
            self.asm.dup()
            patch = self.asm.jnz_fwd()
            self.asm.pop()
            self._gen_expr(node.right)
            self.asm.patch(patch)
            return

        self._gen_expr(node.left)
        self._gen_expr(node.right)

        ops = {
            '+': self.asm.add, '-': self.asm.sub,
            '*': self.asm.mul, '/': self.asm.div, '%': self.asm.modulo,
            '==': self.asm.eq, '!=': self.asm.ne,
            '<': self.asm.lt, '<=': self.asm.le,
            '>': self.asm.gt, '>=': self.asm.ge,
            '&': self.asm.and_, '|': self.asm.or_,
        }
        if node.op not in ops:
            raise CompileError(f"unknown operator: {node.op}", node.line)
        ops[node.op]()

    def _gen_unary(self, node):
        self._gen_expr(node.operand)
        if node.op == '-':
            self.asm.neg()
        elif node.op == '!':
            self.asm.not_()
        else:
            raise CompileError(f"unknown unary operator: {node.op}", node.line)

    def _gen_call(self, node):
        name = node.name

        # --- Built-in: alloc() ---
        if name == 'alloc':
            self.asm.w_alloc()
            return

        # --- Built-in: target(widget) ---
        if name == 'target':
            if len(node.args) != 1:
                raise CompileError("target() takes 1 argument", node.line)
            wid = self._resolve_widget_id(node.args[0])
            self.asm.w_target(wid)
            return

        # --- Built-in: parent(widget) ---
        if name == 'parent':
            if len(node.args) != 1:
                raise CompileError("parent() takes 1 argument", node.line)
            wid = self._resolve_widget_id(node.args[0])
            self.asm.w_parent(wid)
            return

        # --- Built-in: set(prop, values...) ---
        if name == 'set':
            self._gen_set(node)
            return

        # --- Built-in: get(prop) ---
        if name == 'get':
            if len(node.args) != 1:
                raise CompileError("get() takes 1 argument", node.line)
            prop_name = self._prop_name_from_arg(node.args[0], node.line)
            prop_id, _ = _resolve_prop(prop_name)
            self.asm.w_get(prop_id)
            return

        # --- Built-in: dirty(), render(), halt(), yield() ---
        if name == 'dirty':
            self.asm.w_dirty()
            return
        if name == 'render':
            self.asm.w_render()
            return
        if name == 'halt':
            self.asm.halt()
            return
        if name == 'yield_op':
            self.asm.yield_()
            return

        # --- Built-in: drawing primitives ---
        # All use packed u32 args: pack_pair(high, low)
        # Compiler packs (x,y), (w,h), (fg,bg) automatically.

        if name == 'fillRect':
            # fillRect(x, y, w, h, color)
            if len(node.args) != 5:
                raise CompileError("fillRect() takes 5 arguments: x, y, w, h, color", node.line)
            self._gen_packed_pair(node.args[0], node.args[1])  # loc
            self._gen_packed_pair(node.args[2], node.args[3])  # size
            self._gen_expr(node.args[4])                        # color
            self.asm.builtin(Builtin.FILL_RECT)
            return

        if name == 'rect':
            # rect(x, y, w, h, color)
            if len(node.args) != 5:
                raise CompileError("rect() takes 5 arguments: x, y, w, h, color", node.line)
            self._gen_packed_pair(node.args[0], node.args[1])
            self._gen_packed_pair(node.args[2], node.args[3])
            self._gen_expr(node.args[4])
            self.asm.builtin(Builtin.RECT)
            return

        if name == 'line':
            # line(x0, y0, x1, y1, color)
            if len(node.args) != 5:
                raise CompileError("line() takes 5 arguments: x0, y0, x1, y1, color", node.line)
            self._gen_packed_pair(node.args[0], node.args[1])  # start
            self._gen_packed_pair(node.args[2], node.args[3])  # end
            self._gen_expr(node.args[4])                        # color
            self.asm.builtin(Builtin.LINE)
            return

        if name == 'circle':
            # circle(cx, cy, r, color)
            if len(node.args) != 4:
                raise CompileError("circle() takes 4 arguments: cx, cy, r, color", node.line)
            self._gen_packed_pair(node.args[0], node.args[1])  # center
            self._gen_expr(node.args[2])                        # radius
            self._gen_expr(node.args[3])                        # color
            self.asm.builtin(Builtin.CIRCLE)
            return

        if name == 'fillCircle':
            # fillCircle(cx, cy, r, color)
            if len(node.args) != 4:
                raise CompileError("fillCircle() takes 4 arguments: cx, cy, r, color", node.line)
            self._gen_packed_pair(node.args[0], node.args[1])
            self._gen_expr(node.args[2])
            self._gen_expr(node.args[3])
            self.asm.builtin(Builtin.FILL_CIRCLE)
            return

        if name == 'drawImage':
            # drawImage(x, y, image_id)
            if len(node.args) != 3:
                raise CompileError("drawImage() takes 3 arguments: x, y, image_id", node.line)
            self._gen_packed_pair(node.args[0], node.args[1])  # loc
            self._gen_expr(node.args[2])                        # image_id
            self.asm.builtin(Builtin.DRAW_IMAGE)
            return

        if name == 'drawText':
            # drawText(x, y, font_id, fg, bg, "text")
            if len(node.args) != 6:
                raise CompileError("drawText() takes 6 arguments: x, y, font_id, fg, bg, text", node.line)
            self._gen_packed_pair(node.args[0], node.args[1])  # loc
            self._gen_expr(node.args[2])                        # font_id
            self._gen_packed_pair(node.args[3], node.args[4])  # colors (fg, bg)
            # Text must be a string literal — encoded as LEN payload
            text_arg = node.args[5]
            if isinstance(text_arg, StrLit):
                text_bytes = text_arg.value.encode('utf-8')
            else:
                raise CompileError("drawText() text argument must be a string literal", node.line)
            self.asm.builtin_len(Builtin.DRAW_TEXT, text_bytes)
            return

        if name == 'delay':
            # delay(ms)
            if len(node.args) != 1:
                raise CompileError("delay() takes 1 argument: ms", node.line)
            self._gen_expr(node.args[0])
            self.asm.builtin(Builtin.DELAY)
            return

        # --- Built-in: float32 operations ---

        if name == 'itof':
            if len(node.args) != 1:
                raise CompileError("itof() takes 1 argument", node.line)
            self._gen_expr(node.args[0])
            self.asm.itof()
            return

        if name == 'ftoi':
            if len(node.args) != 1:
                raise CompileError("ftoi() takes 1 argument", node.line)
            self._gen_expr(node.args[0])
            self.asm.ftoi()
            return

        if name == 'fneg':
            if len(node.args) != 1:
                raise CompileError("fneg() takes 1 argument", node.line)
            self._gen_expr(node.args[0])
            self.asm.fneg()
            return

        # Two-arg float ops
        _FLOAT_BINOPS = {
            'fadd': 'fadd', 'fsub': 'fsub', 'fmul': 'fmul', 'fdiv': 'fdiv',
            'feq': 'feq', 'fne': 'fne', 'flt': 'flt', 'fle': 'fle',
            'fgt': 'fgt', 'fge': 'fge',
        }
        if name in _FLOAT_BINOPS:
            if len(node.args) != 2:
                raise CompileError(f"{name}() takes 2 arguments", node.line)
            self._gen_expr(node.args[0])
            self._gen_expr(node.args[1])
            getattr(self.asm, _FLOAT_BINOPS[name])()
            return

        # --- Built-in: string operations ---

        if name == 'str':
            # str("literal") → str_id
            if len(node.args) != 1:
                raise CompileError("str() takes 1 argument (string literal)", node.line)
            arg = node.args[0]
            if isinstance(arg, StrLit):
                self.asm.str_alloc(arg.value)
            else:
                raise CompileError("str() argument must be a string literal", node.line)
            return

        if name == 'itos':
            if len(node.args) != 1:
                raise CompileError("itos() takes 1 argument", node.line)
            self._gen_expr(node.args[0])
            self.asm.str_itos()
            return

        if name == 'ftos':
            if len(node.args) != 1:
                raise CompileError("ftos() takes 1 argument", node.line)
            self._gen_expr(node.args[0])
            self.asm.str_ftos()
            return

        if name == 'concat':
            if len(node.args) != 2:
                raise CompileError("concat() takes 2 arguments", node.line)
            self._gen_expr(node.args[0])
            self._gen_expr(node.args[1])
            self.asm.str_concat()
            return

        if name == 'parseInt':
            if len(node.args) != 1:
                raise CompileError("parseInt() takes 1 argument", node.line)
            self._gen_expr(node.args[0])
            self.asm.str_parse_int()
            return

        if name == 'parseFloat':
            if len(node.args) != 1:
                raise CompileError("parseFloat() takes 1 argument", node.line)
            self._gen_expr(node.args[0])
            self.asm.str_parse_float()
            return

        if name == 'strLen':
            if len(node.args) != 1:
                raise CompileError("strLen() takes 1 argument", node.line)
            self._gen_expr(node.args[0])
            self.asm.str_len()
            return

        if name == 'setText':
            # setText(str_id) — sets text on current target widget
            if len(node.args) != 1:
                raise CompileError("setText() takes 1 argument", node.line)
            self._gen_expr(node.args[0])
            self.asm.str_set_text()
            return

        if name == 'drawStr':
            # drawStr(x, y, font_id, fg, bg, str_id)
            if len(node.args) != 6:
                raise CompileError("drawStr() takes 6 arguments: x, y, font_id, fg, bg, str_id", node.line)
            self._gen_packed_pair(node.args[0], node.args[1])  # loc
            self._gen_expr(node.args[2])                        # font_id
            self._gen_packed_pair(node.args[3], node.args[4])  # colors
            self._gen_expr(node.args[5])                        # str_id
            self.asm.str_draw()
            return

        if name == 'strClear':
            if len(node.args) != 0:
                raise CompileError("strClear() takes no arguments", node.line)
            self.asm.str_clear()
            return

        # --- User-defined function ---
        if name not in self.functions:
            raise CompileError(f"undefined function: {name}", node.line)
        info = self.functions[name]
        if len(node.args) != len(info['params']):
            raise CompileError(
                f"{name}() expects {len(info['params'])} args, got {len(node.args)}", node.line)

        # Push args left to right
        for arg in node.args:
            self._gen_expr(arg)

        # CALL (with forward patching if needed)
        if info['addr'] is not None:
            self.asm.call(info['addr'])
        else:
            self.asm._emit_tag(Op.CALL, WT.I16)
            info['patches'].append(self.asm.pos)
            self.asm._emit(b'\x00\x00')

    def _gen_set(self, node):
        if len(node.args) < 2:
            raise CompileError("set() needs at least 2 arguments: set(prop, value...)", node.line)

        prop_name = self._prop_name_from_arg(node.args[0], node.line)
        prop_id, is_compound = _resolve_prop(prop_name)
        value_args = node.args[1:]

        if is_compound:
            # Check if all values are constant
            all_const = all(isinstance(a, NumLit) for a in value_args)
            if all_const:
                self.asm.w_set_compound(prop_id, [a.value for a in value_args])
            else:
                # Decompose into scalar sets
                scalars = COMPOUND_SCALARS.get(prop_id)
                if scalars is None or len(value_args) != len(scalars):
                    raise CompileError(
                        f"set({prop_name}) expects {len(scalars) if scalars else '?'} values",
                        node.line)
                for scalar_id, val_expr in zip(scalars, value_args):
                    self._gen_expr(val_expr)
                    self.asm.w_set(scalar_id)
        else:
            if len(value_args) != 1:
                raise CompileError(f"set({prop_name}) takes 1 value", node.line)
            self._gen_expr(value_args[0])
            self.asm.w_set(prop_id)

    def _gen_packed_pair(self, high_expr, low_expr):
        """Generate code to push a packed pair (high << 16 | low) onto stack.

        If both are constants, emits a single PUSH with the packed value.
        Otherwise, generates runtime packing: high * 65536 | low.
        """
        if isinstance(high_expr, NumLit) and isinstance(low_expr, NumLit):
            self.asm.push(pack_pair(high_expr.value, low_expr.value))
        else:
            # Runtime: (high * 65536) | low
            self._gen_expr(high_expr)
            self.asm.push(65536)
            self.asm.mul()
            self._gen_expr(low_expr)
            self.asm.or_()

    def _resolve_widget_id(self, arg):
        """Resolve widget ID from literal or tracked variable."""
        if isinstance(arg, NumLit):
            return arg.value
        if isinstance(arg, VarRef):
            if arg.name in self.widget_ids:
                return self.widget_ids[arg.name]
            raise CompileError(
                f"'{arg.name}' is not a widget variable (use var {arg.name} = alloc())",
                arg.line)
        raise CompileError("target/parent requires widget variable or integer literal", arg.line)

    def _prop_name_from_arg(self, arg, line):
        """Extract property name from first argument of set()/get()."""
        if isinstance(arg, VarRef):
            return arg.name
        raise CompileError("property name must be an identifier", line)


# ============================================================
# Public API
# ============================================================


def compile(source, filename="<input>"):
    """Compile source code to bytecode. Returns bytes."""
    try:
        tokens = tokenize(source)
        parser = Parser(tokens)
        program = parser.parse()
        codegen = CodeGen()
        return codegen.generate(program)
    except CompileError:
        raise
    except Exception as e:
        raise CompileError(f"internal error: {e}")


def compile_page(source, bg_color=0x0000, filename="<input>"):
    """Compile source to page format: bg_color(u16 LE) + bytecode."""
    bytecode = compile(source, filename)
    return struct.pack('<H', bg_color) + bytecode


# ============================================================
# CLI
# ============================================================


def main():
    import argparse

    parser = argparse.ArgumentParser(description='ferrite-ui language compiler')
    parser.add_argument('source', help='Source file (.fl)')
    parser.add_argument('-o', '--output', help='Output binary file')
    parser.add_argument('--page', type=str, default=None,
                        help='Page mode: bg_color as hex (e.g. 0x0000)')
    parser.add_argument('--disasm', action='store_true', help='Print disassembly')
    parser.add_argument('--hexdump', action='store_true', help='Print hex dump')
    args = parser.parse_args()

    with open(args.source, 'r', encoding='utf-8') as f:
        source = f.read()

    try:
        if args.page is not None:
            bg = int(args.page, 0)
            bytecode = compile_page(source, bg, args.source)
        else:
            bytecode = compile(source, args.source)
    except CompileError as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)

    raw_code = bytecode[2:] if args.page is not None else bytecode

    if args.output:
        with open(args.output, 'wb') as f:
            f.write(bytecode)
        print(f"{args.source} -> {args.output} ({len(bytecode)} bytes)")

    if args.disasm:
        if args.page is not None:
            bg = struct.unpack_from('<H', bytecode)[0]
            print(f'; page bg_color: 0x{bg:04X}')
        print(disassemble(raw_code))

    if args.hexdump:
        a = Asm()
        a._buf = bytearray(bytecode)
        print(a.hexdump())

    if not args.output and not args.disasm and not args.hexdump:
        print(disassemble(raw_code))


if __name__ == '__main__':
    main()
