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

import os
import re
import sys
import struct
import json
from ferrite_cc import (Asm, Op, Prop, Builtin, Compiler as CcCompiler,
                        FunctionKind as CcFunctionKind,
                        SYSTEM_CALLBACK_KINDS,
                        disassemble, encode_svarint,
                        _resolve_prop, PROP_MAP, pack_pair, float_bits)


# ============================================================
# Errors
# ============================================================


class CompileError(Exception):
    def __init__(self, msg, line=0):
        super().__init__(f"line {line}: {msg}" if line else msg)
        self.line = line


# ============================================================
# Preprocessor — #include "file.fl"  /  #hint_widgets N [M]
# ============================================================

_INCLUDE_RE = re.compile(r'^[ \t]*#include\s+"([^"]+)"', re.MULTILINE)
# #hint_widgets <widget_count> [<ext_count>]
# Overrides the compiler's static alloc()-site count for Vec pre-allocation.
# Use when alloc() is called inside loops so the firmware reserves enough capacity.
_HINT_WIDGETS_RE = re.compile(
    r'^[ \t]*#hint_widgets\s+(\d+)(?:\s+(\d+))?[ \t]*(?://[^\n]*)?$',
    re.MULTILINE,
)


def _extract_hint_widgets(source):
    """Return (widget_count, ext_count) from a #hint_widgets directive, or (0, 0)."""
    m = _HINT_WIDGETS_RE.search(source)
    if not m:
        return 0, 0
    wc = min(int(m.group(1)), 255)
    ec = min(int(m.group(2)), 255) if m.group(2) else wc
    return wc, ec


def preprocess(source, filename="<input>", include_dirs=None, _included=None):
    """Expand #include directives. Resolves paths relative to the
    including file, then searches include_dirs. Detects circular includes."""
    if _included is None:
        _included = set()
    if include_dirs is None:
        include_dirs = []

    filepath = os.path.abspath(filename)
    if filepath in _included:
        raise CompileError(f'circular include: {filename}')
    _included.add(filepath)
    base_dir = os.path.dirname(filepath)

    def _resolve(inc_path):
        """Resolve include path: relative to current file first, then include_dirs."""
        candidate = os.path.normpath(os.path.join(base_dir, inc_path))
        if os.path.isfile(candidate):
            return candidate
        for d in include_dirs:
            candidate = os.path.normpath(os.path.join(d, inc_path))
            if os.path.isfile(candidate):
                return candidate
        return None

    def _replace(m):
        inc_path = m.group(1)
        full_path = _resolve(inc_path)
        if full_path is None:
            searched = [base_dir] + list(include_dirs)
            raise CompileError(f'include file not found: {inc_path} (searched: {searched})')
        with open(full_path, 'r', encoding='utf-8') as f:
            inc_source = f.read()
        # Recursively preprocess the included file
        return preprocess(inc_source, full_path, include_dirs, _included)

    source = _INCLUDE_RE.sub(_replace, source)
    # Strip #hint_widgets lines so the tokenizer never sees them.
    # _extract_hint_widgets() reads them from the raw source before this call.
    source = _HINT_WIDGETS_RE.sub('', source)
    return source


# ============================================================
# Tokens
# ============================================================

KEYWORDS = {
    'var', 'const', 'fn', 'if', 'else', 'while', 'for',
    'return', 'true', 'false', 'break', 'continue',
    'not',
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

        # Function reference: @func_name
        if ch == '@':
            i += 1
            if i < n and (source[i].isalpha() or source[i] == '_'):
                start = i
                while i < n and (source[i].isalnum() or source[i] == '_'):
                    i += 1
                tokens.append(Token('FUNCREF', source[start:i], line))
                continue
            raise CompileError("expected function name after @", line)

        # Two-char operators
        two = source[i:i + 2] if i + 1 < n else ''
        if two in ('==', '!=', '<=', '>=', '&&', '||',
                    '++', '--', '+=', '-=', '*=', '/=', '%='):
            tokens.append(Token(two, two, line))
            i += 2
            continue

        # Single-char operators and punctuation
        single_map = {
            '+': '+', '-': '-', '*': '*', '/': '/', '%': '%',
            '&': '&', '|': '|', '!': '!', '<': '<', '>': '>',
            '=': '=', '(': '(', ')': ')', '{': '{', '}': '}',
            '[': '[', ']': ']', ',': ',', ';': ';', '.': '.', ':': ':',
            '?': '?',
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


class FuncRef:
    """Reference to a function by name: @func_name → resolved to func_id."""
    def __init__(self, name, line):
        self.name = name
        self.line = line


class IndexExpr:
    def __init__(self, name, index, line):
        self.name = name
        self.index = index
        self.line = line


class DotExpr:
    """Property read: widget.prop → target(widget) + get(prop)."""
    def __init__(self, obj, prop, line):
        self.obj = obj    # str — variable name
        self.prop = prop   # str — property name
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


class TernaryExpr:
    """cond ? then_expr : else_expr"""
    def __init__(self, cond, then_expr, else_expr, line):
        self.cond = cond
        self.then_expr = then_expr
        self.else_expr = else_expr
        self.line = line


class LambdaExpr:
    """Capture-less lambda: |params| { body }
    Compiles to an anonymous function, evaluates to its func_id."""
    def __init__(self, params, param_types, body, line):
        self.params = params
        self.param_types = param_types
        self.body = body
        self.line = line
        self.func_name = None  # assigned during lambda collection phase


class IncDecExpr:
    """Pre/post increment/decrement: ++i, i++, --i, i--
    op is '+' or '-', pre is True for prefix."""
    def __init__(self, name, op, pre, line):
        self.name = name  # variable name
        self.op = op      # '+' or '-'
        self.pre = pre    # True = prefix (++i), False = postfix (i++)
        self.line = line


class CompoundAssign:
    """x += expr, x -= expr, etc.
    op is the arithmetic operator: +, -, *, /, %"""
    def __init__(self, name, op, value, line, index=None):
        self.name = name
        self.op = op      # '+', '-', '*', '/', '%'
        self.value = value
        self.line = line
        self.index = index  # for arr[i] += expr


class DotCompoundAssign:
    """widget.prop += expr"""
    def __init__(self, obj, prop, op, value, line):
        self.obj = obj
        self.prop = prop
        self.op = op
        self.value = value
        self.line = line


# -- Statements --

class VarDecl:
    def __init__(self, name, init, line, array_size=None, is_const=False):
        self.name = name
        self.init = init
        self.line = line
        self.array_size = array_size
        self.is_const = is_const


class Assign:
    def __init__(self, name, value, line, index=None):
        self.name = name
        self.value = value
        self.line = line
        self.index = index


class DotAssign:
    """Property write: widget.prop = value → target(widget) + set(prop, value)."""
    def __init__(self, obj, prop, value, line):
        self.obj = obj    # str — variable name
        self.prop = prop   # str — property name
        self.value = value # expression
        self.line = line


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
    def __init__(self, name, params, body, line, param_types=None):
        self.name = name
        self.params = params
        self.param_types = param_types  # ["int"|"float", ...] or None
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
        param_types = []
        if not self._check(')'):
            pname = self._expect('IDENT').value
            params.append(pname)
            param_types.append(self._parse_type_annotation())
            while self._match(','):
                pname = self._expect('IDENT').value
                params.append(pname)
                param_types.append(self._parse_type_annotation())
        self._expect(')')
        body = self._block()
        return FnDef(name, params, body, line, param_types)

    def _parse_type_annotation(self):
        """Parse optional ': float' type annotation. Returns 'int' or 'float'."""
        if self._match(':'):
            tok = self._expect('IDENT')
            if tok.value == 'float':
                return 'float'
            elif tok.value == 'int':
                return 'int'
            else:
                raise CompileError(
                    f"unknown type '{tok.value}' (expected 'int' or 'float')",
                    tok.line)
        return 'int'

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
        if self._check('CONST'):
            return self._const_decl()
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

    def _const_decl(self):
        line = self._cur().line
        self._expect('CONST')
        name = self._expect('IDENT').value
        self._expect('=')
        init = self._expression()
        self._expect(';')
        return VarDecl(name, init, line, is_const=True)

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

    _COMPOUND_OPS = {'+=': '+', '-=': '-', '*=': '*', '/=': '/', '%=': '%'}

    def _parse_assign_or_expr(self, skip_semi):
        expr = self._expression()
        # Check for assignment: ident = expr, ident[i] = expr, or widget.prop = expr
        if self._match('='):
            value = self._expression()
            if not skip_semi:
                self._expect(';')
            if isinstance(expr, VarRef):
                return Assign(expr.name, value, expr.line)
            elif isinstance(expr, IndexExpr):
                return Assign(expr.name, value, expr.line, expr.index)
            elif isinstance(expr, DotExpr):
                return DotAssign(expr.obj, expr.prop, value, expr.line)
            else:
                raise CompileError("invalid assignment target", expr.line)
        # Compound assignment: x += expr, arr[i] += expr, widget.prop += expr
        if self._peek_type() in self._COMPOUND_OPS:
            op_tok = self._advance()
            arith_op = self._COMPOUND_OPS[op_tok.type]
            value = self._expression()
            if not skip_semi:
                self._expect(';')
            if isinstance(expr, VarRef):
                return CompoundAssign(expr.name, arith_op, value, expr.line)
            elif isinstance(expr, IndexExpr):
                return CompoundAssign(expr.name, arith_op, value, expr.line, expr.index)
            elif isinstance(expr, DotExpr):
                return DotCompoundAssign(expr.obj, expr.prop, arith_op, value, expr.line)
            else:
                raise CompileError("invalid compound assignment target", expr.line)
        if not skip_semi:
            self._expect(';')
        return ExprStmt(expr, expr.line)

    # --- Expressions (precedence climbing) ---

    def _expression(self):
        expr = self._or_expr()
        # Ternary: cond ? then_expr : else_expr
        if self._match('?'):
            then_expr = self._expression()
            self._expect(':')
            else_expr = self._expression()
            return TernaryExpr(expr, then_expr, else_expr, expr.line)
        return expr

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
        # Prefix increment/decrement: ++i, --i
        if self._check('++', '--'):
            tok = self._advance()
            operand = self._unary_expr()
            if not isinstance(operand, VarRef):
                raise CompileError("increment/decrement requires a variable", tok.line)
            return IncDecExpr(operand.name, '+' if tok.type == '++' else '-', True, tok.line)
        if self._check('-', '!'):
            op = self._advance()
            operand = self._unary_expr()
            return UnaryOp(op.value, operand, op.line)
        if self._check('NOT'):
            op = self._advance()
            operand = self._unary_expr()
            return UnaryOp('!', operand, op.line)
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
        # Dot access: widget.prop
        if isinstance(expr, VarRef) and self._match('.'):
            prop = self._expect('IDENT').value
            return DotExpr(expr.name, prop, expr.line)
        # Postfix increment/decrement: i++, i--
        if isinstance(expr, VarRef) and self._check('++', '--'):
            tok = self._advance()
            return IncDecExpr(expr.name, '+' if tok.type == '++' else '-', False, tok.line)
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
        if self._check('FUNCREF'):
            tok = self._advance()
            return FuncRef(tok.value, tok.line)
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
        if self._check('|', '||'):
            return self._lambda_expr()
        self._error(f"unexpected token: {self._cur().type}")

    def _lambda_expr(self):
        """Parse |params| { body } or || { body } — capture-less lambda."""
        line = self._cur().line
        params = []
        param_types = []
        if self._match('||'):
            # || { body } — zero-param lambda (tokenizer merges || into one token)
            pass
        else:
            self._expect('|')
            if not self._check('|'):
                pname = self._expect('IDENT').value
                params.append(pname)
                param_types.append(self._parse_type_annotation())
                while self._match(','):
                    pname = self._expect('IDENT').value
                    params.append(pname)
                    param_types.append(self._parse_type_annotation())
            self._expect('|')
        body = self._block()
        return LambdaExpr(params, param_types, body, line)


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
    'setText', 'drawStr', 'strClear', 'strFree', 'arrFree',
    'fpgaCmd', 'fpgaData', 'critical',
    'setBrightness',
    'roundedRect', 'fillRoundedRect', 'arc',
    'beginFrame', 'endFrame',
    'sendUsart',
    'rtcWrite',
    'fileClose',
    'setDialogResult',
}


class CodeGen:
    # Type constants for compile-time type inference
    T_INT = "int"
    T_FLOAT = "float"

    def __init__(self):
        self.asm = Asm()
        self.vars = {}          # name -> slot
        self.var_types = {}     # name -> T_INT | T_FLOAT
        self.global_vars = {}   # name -> slot (global variables visible to all functions)
        self.global_var_types = {}  # name -> T_INT | T_FLOAT
        self.next_slot = 0
        self.functions = {}     # name -> {params, addr, end_addr, patches}
        self.func_param_types = {}  # name -> [T_INT|T_FLOAT, ...]
        self.widget_ids = {}    # name -> predicted widget ID
        self.next_widget_id = 1 # 0 = pre-created root widget
        self._loop_stack = []   # {continue_target, continue_patches, break_patches}
        self._fn_base = 0       # function vars start here
        self._last_alltar_var = None  # peephole: skip target() after alloc+target+store
        self._current_target = None  # track targeted widget for dot-access optimization
        self.array_vars = set() # names that hold array_ids
        self.const_vars = set() # names declared with const (immutable)
        self.const_values = {}  # name -> (value, type) for inlineable consts (no slot needed)
        self._global_init_stmts = None  # set during generate_image()
        self._in_function = False  # True when emitting function body
        self._current_fn_name = None
        self.debug_globals = {}    # slot -> source name
        self.debug_functions = {}  # function name -> {offset, length, locals}

    def generate(self, program):
        """Compile program to bytecode (raw, no image header).

        Used for page bytecode and legacy compilation.
        Top-level statements are compiled as before.
        """
        # Phase 1: Pre-allocate main variable slots (count only)
        main_slots = self._count_var_slots(program.statements)
        self._fn_base = main_slots

        # Phase 1b: Pre-scan main statements for alloc() calls to populate widget_ids.
        for stmt in program.statements:
            if isinstance(stmt, VarDecl) and stmt.init is not None:
                if isinstance(stmt.init, CallExpr) and stmt.init.name == 'alloc':
                    self.widget_ids[stmt.name] = self.next_widget_id
                    self.next_widget_id += 1

        # Phase 2: Register functions and assign func_ids (1-based, source order)
        self.func_ids = {}
        next_fid = 1
        for fn in program.functions:
            self.functions[fn.name] = {
                'params': fn.params,
                'addr': None,
                'end_addr': None,
                'patches': [],
            }
            self.func_ids[fn.name] = next_fid
            next_fid += 1

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
        self.var_types = {}
        self.next_slot = 0

        for stmt in program.statements:
            self._gen_stmt(stmt)

        # Patch forward function calls
        for info in self.functions.values():
            for patch_pos in info['patches']:
                if info['addr'] is not None:
                    self.asm.patch(patch_pos, info['addr'])

        return self.asm.build()

    def generate_image(self, program):
        """Compile program to bytecode for the new VM image format.

        Requires fn setup() and fn loop(). Top-level var declarations
        become global variables accessible from all functions.
        Top-level imperative statements are not allowed.
        """
        # Phase 1: Collect global variable declarations
        global_var_stmts = []
        for stmt in program.statements:
            if isinstance(stmt, VarDecl):
                global_var_stmts.append(stmt)
            else:
                raise CompileError(
                    "only var/const declarations are allowed at top level "
                    "(use fn setup() for initialization code)",
                    getattr(stmt, 'line', 0))

        # Count global var slots (inlineable consts don't need slots)
        global_slots = sum(
            1 for s in global_var_stmts
            if not (s.is_const and self._const_literal_value(s.init) is not None)
        )
        self._fn_base = global_slots

        # Pre-scan for alloc() in global var inits
        for stmt in global_var_stmts:
            if stmt.init is not None:
                if isinstance(stmt.init, CallExpr) and stmt.init.name == 'alloc':
                    self.widget_ids[stmt.name] = self.next_widget_id
                    self.next_widget_id += 1

        # Pre-scan function bodies for alloc() calls to populate widget_ids.
        # This enables target()/parent() in callbacks generated before setup().
        for fn in program.functions:
            self._scan_alloc_in_stmts(fn.body, global_var_stmts)

        # Reset counter — codegen will re-count in the same order,
        # producing identical IDs while also tracking local reassignment.
        self.next_widget_id = 1  # 0 = pre-created root widget

        # Phase 1.5: Collect lambdas from AST, detect captures, register as functions
        global_names = {stmt.name for stmt in global_var_stmts}
        self._next_lambda_id = 0
        self._lambda_fns = []
        for fn in program.functions:
            self._collect_lambdas(fn.body, fn.params, global_names)
        # Append lambda FnDefs so they're registered and emitted as regular functions
        program.functions.extend(self._lambda_fns)

        # Phase 2: Register all functions and assign func_ids (1-based, source order)
        self.func_ids = {}
        next_fid = 1
        for fn in program.functions:
            self.functions[fn.name] = {
                'params': fn.params,
                'addr': None,
                'end_addr': None,
                'patches': [],
            }
            self.func_ids[fn.name] = next_fid
            next_fid += 1

        # Validate: setup and loop must exist
        if 'setup' not in self.functions:
            raise CompileError("missing required function: fn setup()")
        if 'loop' not in self.functions:
            raise CompileError("missing required function: fn loop()")

        # Phase 3: Emit all functions (no JMP-over needed — functions are the code)
        # Allocate global variable slots first (visible to all functions)
        self.vars = {}
        self.var_types = {}
        self.next_slot = 0
        for stmt in global_var_stmts:
            if stmt.is_const:
                self.const_vars.add(stmt.name)
                # Inline literal consts — no slot, no runtime code
                lit = self._const_literal_value(stmt.init)
                if lit is not None:
                    val, vtype = lit
                    self.const_values[stmt.name] = (val, vtype)
                    self.var_types[stmt.name] = vtype
                    self.global_var_types[stmt.name] = vtype
                    continue
            slot = self._alloc_var(stmt.name, getattr(stmt, 'line', 0))
            self.global_vars[stmt.name] = slot
            if stmt.array_size is not None:
                self.array_vars.add(stmt.name)
            # Infer global variable type from initializer
            if stmt.init is not None:
                vtype = self._infer_type(stmt.init)
            else:
                vtype = self.T_INT
            self.var_types[stmt.name] = vtype
            self.global_var_types[stmt.name] = vtype

        # Emit functions. Global var init code is injected at the start of setup().
        # Order: setup first (runs once, evicted from cache), user functions
        # in the middle, loop + event handlers + lambdas last (hot path, stays in cache).
        def _fn_emit_order(fn):
            if fn.name == 'setup':
                return 0
            if fn.name in SYSTEM_CALLBACK_KINDS or fn.name.startswith('__lambda_'):
                return 2
            return 1
        emit_order = sorted(program.functions, key=_fn_emit_order)

        self._global_init_stmts = global_var_stmts
        for fn in emit_order:
            self._gen_fn(fn)
        self._global_init_stmts = None

        # Patch forward function calls
        for info in self.functions.values():
            for patch_pos in info['patches']:
                if info['addr'] is not None:
                    self.asm.patch(patch_pos, info['addr'])

        return self.asm.build()

    # --- Lambda collection and capture detection ---

    def _collect_lambdas(self, stmts, enclosing_params, global_names):
        """Walk statements to find LambdaExpr nodes. For each lambda:
        1. Check for captured variables (error if found)
        2. Assign a unique name and create a FnDef
        3. Store the name back in the LambdaExpr node for codegen
        """
        for stmt in stmts:
            self._collect_lambdas_in_stmt(stmt, enclosing_params, global_names)

    def _collect_lambdas_in_stmt(self, stmt, enclosing_params, global_names):
        if isinstance(stmt, VarDecl):
            if stmt.init is not None:
                self._collect_lambdas_in_expr(
                    stmt.init, enclosing_params, global_names)
        elif isinstance(stmt, Assign):
            self._collect_lambdas_in_expr(
                stmt.value, enclosing_params, global_names)
        elif isinstance(stmt, DotAssign):
            self._collect_lambdas_in_expr(
                stmt.value, enclosing_params, global_names)
        elif isinstance(stmt, ExprStmt):
            self._collect_lambdas_in_expr(
                stmt.expr, enclosing_params, global_names)
        elif isinstance(stmt, ReturnStmt):
            if stmt.value is not None:
                self._collect_lambdas_in_expr(
                    stmt.value, enclosing_params, global_names)
        elif isinstance(stmt, IfStmt):
            self._collect_lambdas_in_expr(
                stmt.cond, enclosing_params, global_names)
            self._collect_lambdas(
                stmt.then_body, enclosing_params, global_names)
            if stmt.else_body:
                self._collect_lambdas(
                    stmt.else_body, enclosing_params, global_names)
        elif isinstance(stmt, WhileStmt):
            self._collect_lambdas_in_expr(
                stmt.cond, enclosing_params, global_names)
            self._collect_lambdas(
                stmt.body, enclosing_params, global_names)
        elif isinstance(stmt, ForStmt):
            if stmt.init:
                self._collect_lambdas_in_stmt(
                    stmt.init, enclosing_params, global_names)
            if stmt.cond:
                self._collect_lambdas_in_expr(
                    stmt.cond, enclosing_params, global_names)
            if stmt.update:
                self._collect_lambdas_in_stmt(
                    stmt.update, enclosing_params, global_names)
            self._collect_lambdas(
                stmt.body, enclosing_params, global_names)

    def _collect_lambdas_in_expr(self, expr, enclosing_params, global_names):
        if isinstance(expr, LambdaExpr):
            self._register_lambda(expr, enclosing_params, global_names)
        elif isinstance(expr, BinOp):
            self._collect_lambdas_in_expr(
                expr.left, enclosing_params, global_names)
            self._collect_lambdas_in_expr(
                expr.right, enclosing_params, global_names)
        elif isinstance(expr, UnaryOp):
            self._collect_lambdas_in_expr(
                expr.operand, enclosing_params, global_names)
        elif isinstance(expr, CallExpr):
            for arg in expr.args:
                self._collect_lambdas_in_expr(
                    arg, enclosing_params, global_names)
        elif isinstance(expr, TernaryExpr):
            self._collect_lambdas_in_expr(
                expr.cond, enclosing_params, global_names)
            self._collect_lambdas_in_expr(
                expr.then_expr, enclosing_params, global_names)
            self._collect_lambdas_in_expr(
                expr.else_expr, enclosing_params, global_names)
        elif isinstance(expr, IndexExpr):
            self._collect_lambdas_in_expr(
                expr.index, enclosing_params, global_names)
        elif isinstance(expr, ArrayLit):
            for elem in expr.elements:
                self._collect_lambdas_in_expr(
                    elem, enclosing_params, global_names)
        elif isinstance(expr, CompoundAssign):
            self._collect_lambdas_in_expr(
                expr.value, enclosing_params, global_names)
        elif isinstance(expr, DotCompoundAssign):
            self._collect_lambdas_in_expr(
                expr.value, enclosing_params, global_names)

    def _register_lambda(self, expr, enclosing_params, global_names):
        """Check captures and register a lambda as an anonymous function."""
        # Collect all variable references in the lambda body
        refs = set()
        self._collect_var_refs(expr.body, refs)
        # Collect variables declared inside the lambda body
        declared = set(expr.params)
        self._collect_declared_vars(expr.body, declared)
        # Any ref that's not a param, not declared locally, and not global = capture
        for name in refs:
            if name not in declared and name not in global_names:
                raise CompileError(
                    f"lambda captures local variable '{name}' "
                    f"(closures are not supported; use a global variable instead)",
                    expr.line)
        # Assign unique name and create FnDef
        func_name = f'__lambda_{self._next_lambda_id}'
        self._next_lambda_id += 1
        expr.func_name = func_name
        fn_def = FnDef(func_name, expr.params, expr.body, expr.line,
                       expr.param_types)
        self._lambda_fns.append(fn_def)

    def _collect_var_refs(self, stmts, refs):
        """Recursively collect all variable names referenced in statements."""
        for stmt in stmts:
            self._collect_var_refs_stmt(stmt, refs)

    def _collect_var_refs_stmt(self, stmt, refs):
        if isinstance(stmt, VarDecl):
            if stmt.init is not None:
                self._collect_var_refs_expr(stmt.init, refs)
        elif isinstance(stmt, Assign):
            refs.add(stmt.name)
            self._collect_var_refs_expr(stmt.value, refs)
        elif isinstance(stmt, ExprStmt):
            self._collect_var_refs_expr(stmt.expr, refs)
        elif isinstance(stmt, ReturnStmt):
            if stmt.value is not None:
                self._collect_var_refs_expr(stmt.value, refs)
        elif isinstance(stmt, IfStmt):
            self._collect_var_refs_expr(stmt.cond, refs)
            self._collect_var_refs(stmt.then_body, refs)
            if stmt.else_body:
                self._collect_var_refs(stmt.else_body, refs)
        elif isinstance(stmt, WhileStmt):
            self._collect_var_refs_expr(stmt.cond, refs)
            self._collect_var_refs(stmt.body, refs)
        elif isinstance(stmt, ForStmt):
            if stmt.init:
                self._collect_var_refs_stmt(stmt.init, refs)
            if stmt.cond:
                self._collect_var_refs_expr(stmt.cond, refs)
            if stmt.update:
                self._collect_var_refs_stmt(stmt.update, refs)
            self._collect_var_refs(stmt.body, refs)

    def _collect_var_refs_expr(self, expr, refs):
        if isinstance(expr, VarRef):
            refs.add(expr.name)
        elif isinstance(expr, IndexExpr):
            refs.add(expr.name)
            self._collect_var_refs_expr(expr.index, refs)
        elif isinstance(expr, DotExpr):
            refs.add(expr.obj)
        elif isinstance(expr, BinOp):
            self._collect_var_refs_expr(expr.left, refs)
            self._collect_var_refs_expr(expr.right, refs)
        elif isinstance(expr, UnaryOp):
            self._collect_var_refs_expr(expr.operand, refs)
        elif isinstance(expr, CallExpr):
            # set(prop, ...) and get(prop): first arg is a property name, not a variable
            start = 1 if expr.name in ('set', 'get') and expr.args else 0
            for arg in expr.args[start:]:
                self._collect_var_refs_expr(arg, refs)
        elif isinstance(expr, TernaryExpr):
            self._collect_var_refs_expr(expr.cond, refs)
            self._collect_var_refs_expr(expr.then_expr, refs)
            self._collect_var_refs_expr(expr.else_expr, refs)
        elif isinstance(expr, IncDecExpr):
            refs.add(expr.name)
        elif isinstance(expr, CompoundAssign):
            refs.add(expr.name)
            self._collect_var_refs_expr(expr.value, refs)
        elif isinstance(expr, DotCompoundAssign):
            refs.add(expr.obj)
            self._collect_var_refs_expr(expr.value, refs)
        elif isinstance(expr, ArrayLit):
            for elem in expr.elements:
                self._collect_var_refs_expr(elem, refs)

    def _collect_declared_vars(self, stmts, declared):
        """Collect variable names declared (via var) in statements."""
        for stmt in stmts:
            if isinstance(stmt, VarDecl):
                declared.add(stmt.name)
            elif isinstance(stmt, IfStmt):
                self._collect_declared_vars(stmt.then_body, declared)
                if stmt.else_body:
                    self._collect_declared_vars(stmt.else_body, declared)
            elif isinstance(stmt, WhileStmt):
                self._collect_declared_vars(stmt.body, declared)
            elif isinstance(stmt, ForStmt):
                if stmt.init and isinstance(stmt.init, VarDecl):
                    declared.add(stmt.init.name)
                self._collect_declared_vars(stmt.body, declared)

    def _count_var_slots(self, stmts):
        """Recursively count variable slots needed by statements."""
        count = 0
        for stmt in stmts:
            if isinstance(stmt, VarDecl):
                # Inlineable consts don't need a slot
                if stmt.is_const and self._const_literal_value(stmt.init) is not None:
                    continue
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

    def _scan_alloc_in_stmts(self, stmts, global_var_stmts):
        """Pre-scan statements for alloc() calls to track widget IDs.

        Counts ALL allocs (globals, locals, reassignments) in execution order
        so that global widget IDs are correct for callbacks generated before setup().
        """
        for stmt in stmts:
            # x = alloc() (any assignment — global or local reassignment)
            if isinstance(stmt, Assign):
                if isinstance(stmt.value, CallExpr) and stmt.value.name == 'alloc':
                    self.widget_ids[stmt.name] = self.next_widget_id
                    self.next_widget_id += 1
            # var x = alloc() (local declaration)
            elif isinstance(stmt, VarDecl) and stmt.init is not None:
                if isinstance(stmt.init, CallExpr) and stmt.init.name == 'alloc':
                    self.widget_ids[stmt.name] = self.next_widget_id
                    self.next_widget_id += 1
            elif isinstance(stmt, IfStmt):
                self._scan_alloc_in_stmts(stmt.then_body, global_var_stmts)
                if stmt.else_body:
                    self._scan_alloc_in_stmts(stmt.else_body, global_var_stmts)
            elif isinstance(stmt, WhileStmt):
                self._scan_alloc_in_stmts(stmt.body, global_var_stmts)
            elif isinstance(stmt, ForStmt):
                self._scan_alloc_in_stmts(stmt.body, global_var_stmts)

    # --- Function reference resolution ---

    def _resolve_func_id(self, name, line):
        if name not in self.func_ids:
            raise CompileError(f"unknown function '{name}' in @{name}", line)
        return self.func_ids[name]

    # --- Variable management ---

    def _alloc_var(self, name, line=0):
        if name in self.vars:
            raise CompileError(f"variable already defined: {name}", line)
        if self._in_function:
            # Local variable: high bit set, index 0-127
            local_idx = self.next_slot - self._fn_base
            if local_idx >= 128:
                raise CompileError(f"too many local variables (max 128): {name}", line)
            slot = 0x80 | local_idx
        else:
            # Global variable: no high bit, index 0-127
            slot = self.next_slot
            if slot >= 128:
                raise CompileError(f"too many global variables (max 128): {name}", line)
        self.vars[name] = slot
        self._record_var_symbol(name, slot)
        self.next_slot += 1
        return slot

    def _var_slot(self, name, line=0):
        if name not in self.vars:
            raise CompileError(f"undefined variable: {name}", line)
        return self.vars[name]

    def _record_var_symbol(self, name, slot):
        if self._in_function and self._current_fn_name:
            fn = self.debug_functions.setdefault(
                self._current_fn_name,
                {'name': self._current_fn_name, 'offset': None,
                 'length': None, 'locals': {}})
            fn['locals'][slot & 0x7F] = name
        elif not (slot & 0x80):
            self.debug_globals[slot] = name

    def debug_info(self, filename="<input>"):
        functions = []
        for name, info in self.functions.items():
            addr = info.get('addr')
            if addr is None:
                continue
            end_addr = info.get('end_addr') or addr
            dbg = self.debug_functions.get(name, {})
            functions.append({
                'name': name,
                'offset': addr,
                'length': end_addr - addr,
                'locals': {str(k): v for k, v in sorted(
                    dbg.get('locals', {}).items())},
            })
        functions.sort(key=lambda fn: fn['offset'])
        return {
            'version': 1,
            'source': filename,
            'globals': {str(k): v for k, v in sorted(self.debug_globals.items())},
            'functions': functions,
        }

    # --- Compile-time type inference (no code emission) ---

    # Builtins that return float
    _FLOAT_RETURNING = {
        'itof', 'fadd', 'fsub', 'fmul', 'fdiv', 'fneg',
        'parseFloat',
        'sin', 'cos', 'sqrt', 'abs', 'atan2', 'floor', 'ceil',
    }
    # Builtins that return int regardless of argument types
    _INT_RETURNING = {
        'ftoi', 'feq', 'fne', 'flt', 'fle', 'fgt', 'fge',
        'alloc', 'get', 'str', 'itos', 'ftos', 'concat', 'sprintf',
        'parseInt', 'strLen', 'millis', 'brightness',
        'arrFree', 'strFree', 'strClear',
    }

    def _infer_type(self, node):
        """Infer compile-time type of an expression (T_INT or T_FLOAT).
        Pure analysis — does not emit any code."""
        if isinstance(node, NumLit):
            return self.T_INT
        if isinstance(node, FloatLit):
            return self.T_FLOAT
        if isinstance(node, BoolLit):
            return self.T_INT
        if isinstance(node, StrLit):
            return self.T_INT  # str_id
        if isinstance(node, FuncRef):
            return self.T_INT  # func_id
        if isinstance(node, VarRef):
            return self.var_types.get(node.name, self.T_INT)
        if isinstance(node, IndexExpr):
            return self.T_INT  # array elements are untyped
        if isinstance(node, DotExpr):
            return self.T_INT  # widget properties are int
        if isinstance(node, UnaryOp):
            if node.op == '!':
                return self.T_INT
            return self._infer_type(node.operand)  # - preserves type
        if isinstance(node, BinOp):
            if node.op in ('&&', '||', '&', '|'):
                return self.T_INT
            # Comparison operators always return int (boolean)
            if node.op in ('==', '!=', '<', '<=', '>', '>='):
                return self.T_INT
            # Arithmetic: float if either side is float
            lt = self._infer_type(node.left)
            rt = self._infer_type(node.right)
            if lt == self.T_FLOAT or rt == self.T_FLOAT:
                return self.T_FLOAT
            return self.T_INT
        if isinstance(node, CallExpr):
            if node.name in self._FLOAT_RETURNING:
                return self.T_FLOAT
            if node.name in self._INT_RETURNING:
                return self.T_INT
            # User-defined function: default to int
            return self.T_INT
        if isinstance(node, ArrayLit):
            return self.T_INT
        if isinstance(node, TernaryExpr):
            tt = self._infer_type(node.then_expr)
            et = self._infer_type(node.else_expr)
            return self.T_FLOAT if (tt == self.T_FLOAT or et == self.T_FLOAT) else self.T_INT
        if isinstance(node, IncDecExpr):
            return self.var_types.get(node.name, self.T_INT)
        if isinstance(node, CompoundAssign):
            return self.var_types.get(node.name, self.T_INT)
        return self.T_INT

    # --- Functions ---

    def _gen_fn(self, fn):
        info = self.functions[fn.name]
        info['addr'] = self.asm.pos
        self.debug_functions.setdefault(
            fn.name,
            {'name': fn.name, 'offset': info['addr'], 'length': None, 'locals': {}})
        self.debug_functions[fn.name]['offset'] = info['addr']

        # Function locals use stack frames (high bit encoding).
        # Each function starts locals from index 0 (0x80).
        saved_vars = dict(self.vars)
        saved_types = dict(self.var_types)
        saved_slot = self.next_slot
        saved_in_fn = self._in_function
        saved_current_fn = self._current_fn_name
        self.vars = dict(self.global_vars)  # globals visible in all functions
        self.var_types = dict(self.global_var_types)
        self.next_slot = self._fn_base  # locals start after globals
        self._in_function = True
        self._current_fn_name = fn.name

        # Emit FRAME prologue: tells VM how many local slots this function needs
        local_count = len(fn.params) + self._count_var_slots(fn.body)
        self.asm.frame(local_count)

        # Allocate param slots and pop args (reverse order)
        # Set parameter types from annotations (default: int)
        param_types = getattr(fn, 'param_types', None) or [self.T_INT] * len(fn.params)
        param_slots = []
        for i, p in enumerate(fn.params):
            slot = self._alloc_var(p, getattr(fn, 'line', 0))
            self.var_types[p] = param_types[i] if i < len(param_types) else self.T_INT
            param_slots.append(slot)
        for slot in reversed(param_slots):
            self.asm.store(slot)

        # Inject global var init code at the start of setup()
        if fn.name == 'setup' and self._global_init_stmts:
            for stmt in self._global_init_stmts:
                self._gen_global_init(stmt)

        if fn.name == 'loop' and self._global_init_stmts is not None:
            # Compiler-generated: while(1) { <user body>; yield; }
            loop_top = self.asm.pos
            for stmt in fn.body:
                self._gen_stmt(stmt)
            self.asm.yield_()
            self.asm.jmp(loop_top)
            # No implicit return — loop never exits
        else:
            # Body
            for stmt in fn.body:
                self._gen_stmt(stmt)

            # Implicit return 0
            if not fn.body or not isinstance(fn.body[-1], ReturnStmt):
                self.asm.push(0)
                self.asm.ret()

        info['end_addr'] = self.asm.pos
        self.debug_functions[fn.name]['length'] = info['end_addr'] - info['addr']

        # Restore
        self.vars = saved_vars
        self.var_types = saved_types
        self.next_slot = saved_slot
        self._in_function = saved_in_fn
        self._current_fn_name = saved_current_fn

    # --- Statements ---

    def _gen_stmt(self, node):
        # ExprStmt with target() can consume _last_alltar_var, all others clear it
        if not (isinstance(node, ExprStmt)
                and isinstance(node.expr, CallExpr)
                and node.expr.name == 'target'):
            self._last_alltar_var = None
        # Clear target tracking on control flow (branches may diverge)
        if isinstance(node, (IfStmt, WhileStmt, ForStmt)):
            self._current_target = None
        if isinstance(node, VarDecl):
            self._gen_var_decl(node)
        elif isinstance(node, Assign):
            self._gen_assign(node)
        elif isinstance(node, DotAssign):
            self._gen_dot_assign(node)
        elif isinstance(node, CompoundAssign):
            self._gen_compound_assign(node)
        elif isinstance(node, DotCompoundAssign):
            self._gen_dot_compound_assign(node)
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
        if node.is_const:
            self.const_vars.add(node.name)
            # Inline literal consts — no slot, no runtime code
            lit = self._const_literal_value(node.init)
            if lit is not None:
                val, vtype = lit
                self.const_values[node.name] = (val, vtype)
                self.var_types[node.name] = vtype
                return
        slot = self._alloc_var(node.name, node.line)
        if node.array_size is not None:
            # Array: VM pool'da oluştur, arr_id'yi var slot'a sakla
            self.array_vars.add(node.name)
            self.var_types[node.name] = self.T_INT
            if node.init is not None:
                if not isinstance(node.init, ArrayLit):
                    raise CompileError("array must be initialized with [...]", node.line)
                if len(node.init.elements) != node.array_size:
                    raise CompileError(
                        f"array size mismatch: {node.name}[{node.array_size}] vs [{len(node.init.elements)}]",
                        node.line)
                all_const = all(isinstance(e, NumLit) for e in node.init.elements)
                if all_const:
                    self.asm.arr_alloc_init([e.value for e in node.init.elements])
                else:
                    self.asm.arr_alloc(node.array_size)
                    self.asm.store(slot)
                    for i, elem in enumerate(node.init.elements):
                        self.asm.load(slot)    # arr_id
                        self.asm.push(i)       # index
                        self._gen_expr(elem)   # value
                        self.asm.arr_store()
                    return
            else:
                self.asm.arr_alloc(node.array_size)
        else:
            # Scalar variable — infer type from initializer
            if node.init is not None:
                vtype = self._infer_type(node.init)
                self.var_types[node.name] = vtype
                if isinstance(node.init, CallExpr) and node.init.name == 'alloc':
                    self.widget_ids[node.name] = self.next_widget_id
                    self.next_widget_id += 1
                    self.asm.w_alltar(slot)
                    self._last_alltar_var = node.name
                    self._current_target = node.name
                    return
                self._gen_expr(node.init)
            else:
                self.var_types[node.name] = self.T_INT
                self.asm.push(0)
        self.asm.store(slot)

    def _gen_global_init(self, node):
        """Emit init code for a global variable whose slot is already allocated."""
        # Inlineable consts have no slot — skip
        if node.name in self.const_values:
            return
        slot = self.global_vars[node.name]
        if node.array_size is not None:
            if node.init is not None:
                if not isinstance(node.init, ArrayLit):
                    raise CompileError("array must be initialized with [...]", node.line)
                if len(node.init.elements) != node.array_size:
                    raise CompileError(
                        f"array size mismatch: {node.name}[{node.array_size}] vs [{len(node.init.elements)}]",
                        node.line)
                all_const = all(isinstance(e, NumLit) for e in node.init.elements)
                if all_const:
                    self.asm.arr_alloc_init([e.value for e in node.init.elements])
                else:
                    self.asm.arr_alloc(node.array_size)
                    self.asm.store(slot)
                    for i, elem in enumerate(node.init.elements):
                        self.asm.load(slot)
                        self.asm.push(i)
                        self._gen_expr(elem)
                        self.asm.arr_store()
                    return
            else:
                self.asm.arr_alloc(node.array_size)
        else:
            if node.init is not None:
                if isinstance(node.init, CallExpr) and node.init.name == 'alloc':
                    self.widget_ids[node.name] = self.next_widget_id
                    self.next_widget_id += 1
                self._gen_expr(node.init)
            else:
                self.asm.push(0)
        self.asm.store(slot)

    def _check_const(self, name, line):
        if name in self.const_vars:
            raise CompileError(f"cannot assign to const variable: {name}", line)

    @staticmethod
    def _const_literal_value(init):
        """Return (value, type) if init is a compile-time literal, else None."""
        if isinstance(init, NumLit):
            return (init.value, CodeGen.T_INT)
        if isinstance(init, FloatLit):
            return (init.value, CodeGen.T_FLOAT)
        if isinstance(init, BoolLit):
            return (1 if init.value else 0, CodeGen.T_INT)
        return None

    def _gen_assign(self, node):
        self._check_const(node.name, node.line)
        if node.index is not None:
            # arr[idx] = value → arr_id, idx, value, ARR_STORE
            slot = self._var_slot(node.name, node.line)
            self.asm.load(slot)         # arr_id
            self._gen_expr(node.index)  # index
            self._gen_expr(node.value)  # value
            self.asm.arr_store()
        else:
            # Track array-returning builtins
            if isinstance(node.value, CallExpr) and node.value.name == 'rtcRead':
                self.array_vars.add(node.name)
            # Track widget ID reassignment: lbl = alloc()
            if isinstance(node.value, CallExpr) and node.value.name == 'alloc':
                self.widget_ids[node.name] = self.next_widget_id
                self.next_widget_id += 1
                slot = self._var_slot(node.name, node.line)
                self.asm.w_alltar(slot)
                self._last_alltar_var = node.name
                self._current_target = node.name
                return
            self._gen_expr(node.value)
            var_type = self.var_types.get(node.name, self.T_INT)
            val_type = self._infer_type(node.value)
            # Auto-promote int→float if variable is typed float
            if var_type == self.T_FLOAT and val_type == self.T_INT:
                self.asm.itof()
            # Update variable type if RHS is float (for uninitialized vars)
            if val_type == self.T_FLOAT and var_type == self.T_INT:
                self.var_types[node.name] = self.T_FLOAT
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
        elif isinstance(node, FuncRef):
            func_id = self._resolve_func_id(node.name, node.line)
            self.asm.push(func_id)
        elif isinstance(node, VarRef):
            if node.name in self.array_vars:
                raise CompileError(f"'{node.name}' is an array, use {node.name}[index]", node.line)
            # Inline const: emit PUSH instead of LOAD
            if node.name in self.const_values:
                val, vtype = self.const_values[node.name]
                if vtype == self.T_FLOAT:
                    self.asm.push(float_bits(val))
                else:
                    self.asm.push(val)
            else:
                slot = self._var_slot(node.name, node.line)
                self.asm.load(slot)
        elif isinstance(node, IndexExpr):
            if node.name not in self.array_vars:
                raise CompileError(f"'{node.name}' is not an array", node.line)
            slot = self._var_slot(node.name, node.line)
            self.asm.load(slot)           # arr_id
            self._gen_expr(node.index)    # index (herhangi bir expression)
            self.asm.arr_load()
        elif isinstance(node, DotExpr):
            self._gen_dot_read(node)
        elif isinstance(node, BinOp):
            self._gen_binop(node)
        elif isinstance(node, UnaryOp):
            self._gen_unary(node)
        elif isinstance(node, CallExpr):
            self._gen_call(node)
        elif isinstance(node, IncDecExpr):
            self._gen_incdec_expr(node)
        elif isinstance(node, TernaryExpr):
            self._gen_ternary(node)
        elif isinstance(node, LambdaExpr):
            func_id = self._resolve_func_id(node.func_name, node.line)
            self.asm.push(func_id)
        else:
            raise CompileError(f"unknown expression: {type(node).__name__}")

    # Float-aware operator maps
    _INT_OPS = {
        '+': 'add', '-': 'sub', '*': 'mul', '/': 'div', '%': 'modulo',
        '==': 'eq', '!=': 'ne', '<': 'lt', '<=': 'le', '>': 'gt', '>=': 'ge',
        '&': 'and_', '|': 'or_',
    }
    _FLOAT_ARITH = {
        '+': 'fadd', '-': 'fsub', '*': 'fmul', '/': 'fdiv',
    }
    _FLOAT_CMP = {
        '==': 'feq', '!=': 'fne', '<': 'flt', '<=': 'fle', '>': 'fgt', '>=': 'fge',
    }

    def _gen_binop(self, node):
        # Short-circuit logical operators — always int
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

        # Bitwise — always int, no float promotion
        if node.op in ('&', '|'):
            self._gen_expr(node.left)
            self._gen_expr(node.right)
            getattr(self.asm, self._INT_OPS[node.op])()
            return

        # Infer operand types
        lt = self._infer_type(node.left)
        rt = self._infer_type(node.right)
        use_float = (lt == self.T_FLOAT or rt == self.T_FLOAT)

        # Arithmetic or comparison with float promotion
        if use_float and (node.op in self._FLOAT_ARITH or node.op in self._FLOAT_CMP):
            # Generate left, promote if needed
            self._gen_expr(node.left)
            if lt == self.T_INT:
                self.asm.itof()
            # Generate right, promote if needed
            self._gen_expr(node.right)
            if rt == self.T_INT:
                self.asm.itof()
            # Emit float op
            if node.op in self._FLOAT_ARITH:
                getattr(self.asm, self._FLOAT_ARITH[node.op])()
            else:
                getattr(self.asm, self._FLOAT_CMP[node.op])()
            return

        # Modulo with floats is not supported — fall through to int
        if use_float and node.op == '%':
            raise CompileError("modulo (%) is not supported for float operands", node.line)

        # Pure int path
        self._gen_expr(node.left)
        self._gen_expr(node.right)
        if node.op not in self._INT_OPS:
            raise CompileError(f"unknown operator: {node.op}", node.line)
        getattr(self.asm, self._INT_OPS[node.op])()

    def _gen_unary(self, node):
        self._gen_expr(node.operand)
        if node.op == '-':
            if self._infer_type(node.operand) == self.T_FLOAT:
                self.asm.fneg()
            else:
                self.asm.neg()
        elif node.op == '!':
            self.asm.not_()
        else:
            raise CompileError(f"unknown unary operator: {node.op}", node.line)

    def _gen_incdec_expr(self, node):
        """Compile ++i / i++ / --i / i--.
        Prefix: new value on stack.  Postfix: old value on stack."""
        self._check_const(node.name, node.line)
        slot = self._var_slot(node.name, node.line)
        use_float = self.var_types.get(node.name, self.T_INT) == self.T_FLOAT
        if node.pre:
            # ++i: load, add 1, dup (keep new), store
            self.asm.load(slot)
            if use_float:
                self.asm.push(float_bits(1.0))
                self.asm.fadd() if node.op == '+' else self.asm.fsub()
            else:
                self.asm.push(1)
                self.asm.add() if node.op == '+' else self.asm.sub()
            self.asm.dup()
            self.asm.store(slot)
        else:
            # i++: load, dup (keep old), add 1, store
            self.asm.load(slot)
            self.asm.dup()
            if use_float:
                self.asm.push(float_bits(1.0))
                self.asm.fadd() if node.op == '+' else self.asm.fsub()
            else:
                self.asm.push(1)
                self.asm.add() if node.op == '+' else self.asm.sub()
            self.asm.store(slot)

    def _emit_arith_op(self, op, use_float, line):
        """Emit the arithmetic opcode for a compound assignment operator."""
        if use_float:
            fops = {'+': 'fadd', '-': 'fsub', '*': 'fmul', '/': 'fdiv'}
            if op == '%':
                raise CompileError("modulo (%) is not supported for float operands", line)
            getattr(self.asm, fops[op])()
        else:
            iops = {'+': 'add', '-': 'sub', '*': 'mul', '/': 'div', '%': 'modulo'}
            getattr(self.asm, iops[op])()

    def _gen_compound_assign(self, node):
        """Compile x += expr, arr[i] += expr."""
        self._check_const(node.name, node.line)
        if node.index is not None:
            # arr[i] += expr → load arr, idx, arr_load, gen_expr, op, arr_store
            slot = self._var_slot(node.name, node.line)
            self.asm.load(slot)           # arr_id
            self._gen_expr(node.index)    # index
            # We need arr_id and index again for store, but also the current value
            # Strategy: arr_load, compute, then re-push arr_id+index for store
            self.asm.load(slot)           # arr_id (again)
            self._gen_expr(node.index)    # index (again)
            self.asm.arr_load()           # current value
            self._gen_expr(node.value)    # rhs
            self._emit_arith_op(node.op, False, node.line)  # arrays are int
            self.asm.arr_store()
        else:
            slot = self._var_slot(node.name, node.line)
            use_float = self.var_types.get(node.name, self.T_INT) == self.T_FLOAT
            val_type = self._infer_type(node.value)
            self.asm.load(slot)
            if use_float and val_type == self.T_INT:
                # Promote: load is already float, but RHS needs itof
                self._gen_expr(node.value)
                self.asm.itof()
            elif not use_float and val_type == self.T_FLOAT:
                # Variable becomes float
                self.asm.itof()
                self.var_types[node.name] = self.T_FLOAT
                use_float = True
                self._gen_expr(node.value)
            else:
                self._gen_expr(node.value)
            self._emit_arith_op(node.op, use_float, node.line)
            self.asm.store(slot)

    def _gen_dot_compound_assign(self, node):
        """Compile widget.prop += expr → target, get, expr, op, set."""
        self._emit_target(node.obj, node.line)
        prop_id, is_compound = _resolve_prop(node.prop)
        if is_compound:
            raise CompileError(
                f"compound assignment not supported for compound property '{node.prop}'",
                node.line)
        if prop_id == Prop.TEXT:
            raise CompileError("compound assignment not supported for text property", node.line)
        self.asm.w_get(prop_id)
        self._gen_expr(node.value)
        self._emit_arith_op(node.op, False, node.line)  # widget props are int
        # Re-target (value expr may have changed target)
        self._current_target = None
        self._emit_target(node.obj, node.line)
        self.asm.w_set(prop_id)

    def _gen_ternary(self, node):
        """Compile cond ? then_expr : else_expr."""
        self._gen_expr(node.cond)
        else_patch = self.asm.jz_fwd()
        self._gen_expr(node.then_expr)
        end_patch = self.asm.jmp_fwd()
        self.asm.patch(else_patch)
        self._gen_expr(node.else_expr)
        self.asm.patch(end_patch)

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
            # Skip if previous statement was W_ALLTAR for the same variable
            arg = node.args[0]
            if (self._last_alltar_var is not None
                    and isinstance(arg, VarRef)
                    and arg.name == self._last_alltar_var):
                self._last_alltar_var = None
                self._current_target = arg.name
                return
            self._last_alltar_var = None
            if isinstance(arg, NumLit):
                # Literal integer (e.g. target(0) for root) → static W_TARGET
                self.asm.w_target(arg.value)
                self._current_target = None
            elif isinstance(arg, VarRef):
                # Named widget variable → load runtime ID, use W_TARGET_S
                # Static prediction fails when widgets are allocated inside loops,
                # so always resolve at runtime.
                if arg.name not in self.widget_ids:
                    raise CompileError(
                        f"'{arg.name}' is not a widget variable", arg.line)
                self._gen_expr(arg)
                self.asm.w_target_s()
                self._current_target = arg.name
            else:
                # General expression (array subscript, etc.) → evaluate, W_TARGET_S
                self._gen_expr(arg)
                self.asm.w_target_s()
                self._current_target = None
            return

        # --- Built-in: parent(widget) ---
        if name == 'parent':
            if len(node.args) != 1:
                raise CompileError("parent() takes 1 argument", node.line)
            arg = node.args[0]
            if isinstance(arg, NumLit):
                # Literal id (typically parent(0) = root) → static W_PARENT.
                self.asm.w_parent(arg.value)
            elif isinstance(arg, VarRef):
                # Widget variable → load runtime id, use W_PARENT_S so this
                # stays correct after destroy/realloc cycles (modal dialogs).
                if arg.name not in self.widget_ids:
                    raise CompileError(
                        f"'{arg.name}' is not a widget variable", arg.line)
                self._gen_expr(arg)
                self.asm.w_parent_s()
            else:
                # General expression → evaluate and use W_PARENT_S.
                self._gen_expr(arg)
                self.asm.w_parent_s()
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

        # --- Built-in: trig / math (all operate on f32, auto-promote int args) ---

        _FLOAT_UNARY_MATH = {
            'sin': 'fsin', 'cos': 'fcos', 'sqrt': 'fsqrt',
            'abs': 'fabs', 'floor': 'ffloor', 'ceil': 'fceil',
        }
        if name in _FLOAT_UNARY_MATH:
            if len(node.args) != 1:
                raise CompileError(f"{name}() takes 1 argument", node.line)
            self._gen_expr(node.args[0])
            if self._infer_type(node.args[0]) == self.T_INT:
                self.asm.itof()
            getattr(self.asm, _FLOAT_UNARY_MATH[name])()
            return

        if name == 'atan2':
            if len(node.args) != 2:
                raise CompileError("atan2() takes 2 arguments (y, x)", node.line)
            self._gen_expr(node.args[0])
            if self._infer_type(node.args[0]) == self.T_INT:
                self.asm.itof()
            self._gen_expr(node.args[1])
            if self._infer_type(node.args[1]) == self.T_INT:
                self.asm.itof()
            self.asm.fatan2()
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

        if name == 'sprintf':
            if len(node.args) < 1:
                raise CompileError("sprintf() requires at least 1 argument (format string)", node.line)
            n_fmt_args = len(node.args) - 1
            if n_fmt_args > 8:
                raise CompileError("sprintf() supports at most 8 format arguments", node.line)
            for arg in node.args:
                self._gen_expr(arg)
            self.asm.str_sprintf(n_fmt_args)
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

        if name == 'strFree':
            if len(node.args) != 1:
                raise CompileError("strFree() takes 1 argument: str_id", node.line)
            self._gen_expr(node.args[0])
            self.asm.str_free()
            return

        if name == 'arrFree':
            if len(node.args) != 1:
                raise CompileError("arrFree() takes 1 argument: arr_id", node.line)
            arg = node.args[0]
            if isinstance(arg, VarRef) and arg.name in self.array_vars:
                slot = self._var_slot(arg.name, node.line)
                self.asm.load(slot)
            else:
                self._gen_expr(arg)
            self.asm.arr_free()
            return

        if name == 'arrToStr':
            # arrToStr(arr) or arrToStr(arr, len) → str_id
            # Bytes are the low byte of each array element (arr[i] & 0xFF).
            # If len omitted, full array is used. Typical use: after fileRead loop.
            if len(node.args) not in (1, 2):
                raise CompileError("arrToStr() takes 1 or 2 arguments: arr, [len]", node.line)
            arg = node.args[0]
            if isinstance(arg, VarRef) and arg.name in self.array_vars:
                slot = self._var_slot(arg.name, node.line)
                self.asm.load(slot)
            else:
                self._gen_expr(arg)
            if len(node.args) == 2:
                self._gen_expr(node.args[1])
            else:
                self.asm.push(-1)  # sentinel: use full array
            self.asm.builtin(Builtin.ARR_TO_STR)
            return

        # --- Built-in: rounded rect & arc ---

        if name == 'roundedRect':
            # roundedRect(x, y, w, h, r, color)
            if len(node.args) != 6:
                raise CompileError("roundedRect() takes 6 arguments: x, y, w, h, r, color", node.line)
            self._gen_packed_pair(node.args[0], node.args[1])  # loc
            self._gen_packed_pair(node.args[2], node.args[3])  # size
            self._gen_expr(node.args[4])                        # radius
            self._gen_expr(node.args[5])                        # color
            self.asm.builtin(Builtin.ROUNDED_RECT)
            return

        if name == 'fillRoundedRect':
            # fillRoundedRect(x, y, w, h, r, color)
            if len(node.args) != 6:
                raise CompileError("fillRoundedRect() takes 6 arguments: x, y, w, h, r, color", node.line)
            self._gen_packed_pair(node.args[0], node.args[1])
            self._gen_packed_pair(node.args[2], node.args[3])
            self._gen_expr(node.args[4])
            self._gen_expr(node.args[5])
            self.asm.builtin(Builtin.FILL_ROUNDED_RECT)
            return

        if name == 'arc':
            # arc(cx, cy, r, start, end, color)
            if len(node.args) != 6:
                raise CompileError("arc() takes 6 arguments: cx, cy, r, start, end, color", node.line)
            self._gen_packed_pair(node.args[0], node.args[1])  # center
            self._gen_expr(node.args[2])                        # radius
            self._gen_expr(node.args[3])                        # start deg
            self._gen_expr(node.args[4])                        # end deg
            self._gen_expr(node.args[5])                        # color
            self.asm.builtin(Builtin.ARC)
            return

        if name == 'beginFrame':
            if len(node.args) != 0:
                raise CompileError("beginFrame() takes no arguments", node.line)
            self.asm.builtin(Builtin.BEGIN_FRAME)
            return

        if name == 'endFrame':
            if len(node.args) != 0:
                raise CompileError("endFrame() takes no arguments", node.line)
            self.asm.builtin(Builtin.END_FRAME)
            return

        if name == 'sendUsart':
            # sendUsart(array_or_str) — send bytes via USART
            if len(node.args) != 1:
                raise CompileError("sendUsart() takes 1 argument: array or str_id", node.line)
            arg = node.args[0]
            if isinstance(arg, VarRef) and arg.name in self.array_vars:
                # Array variable — load arr_id, use array builtin
                slot = self._var_slot(arg.name, node.line)
                self.asm.load(slot)
                self.asm.builtin(Builtin.SEND_USART)
            else:
                # String expression (str_id) — use string builtin
                self._gen_expr(arg)
                self.asm.builtin(Builtin.SEND_USART_STR)
            return

        if name == 'millis':
            # millis() → u32 millisecond tick count
            if len(node.args) != 0:
                raise CompileError("millis() takes no arguments", node.line)
            self.asm.builtin(Builtin.MILLIS)
            return

        if name == 'fpgaCmd':
            # fpgaCmd(cmd, data) — raw FPGA command + data word
            if len(node.args) != 2:
                raise CompileError("fpgaCmd() takes 2 arguments: cmd, data", node.line)
            self._gen_expr(node.args[0])
            self._gen_expr(node.args[1])
            self.asm.builtin(Builtin.FPGA_CMD)
            return

        if name == 'fpgaData':
            # fpgaData(data) — raw FPGA data word (after a fpgaCmd)
            if len(node.args) != 1:
                raise CompileError("fpgaData() takes 1 argument: data", node.line)
            self._gen_expr(node.args[0])
            self.asm.builtin(Builtin.FPGA_DAT)
            return

        if name == 'critical':
            # critical() — enter critical section, VM runs until next yield
            if len(node.args) != 0:
                raise CompileError("critical() takes no arguments", node.line)
            self.asm.builtin(Builtin.CRITICAL)
            return

        if name == 'setBrightness':
            # setBrightness(percent) — set LCD backlight 0..100
            if len(node.args) != 1:
                raise CompileError("setBrightness() takes 1 argument: percent (0-100)", node.line)
            self._gen_expr(node.args[0])
            self.asm.builtin(Builtin.SET_BRIGHTNESS)
            return

        if name == 'brightness':
            # brightness() → current backlight percent (0..100)
            if len(node.args) != 0:
                raise CompileError("brightness() takes no arguments", node.line)
            self.asm.builtin(Builtin.BRIGHTNESS)
            return

        if name == 'rtcRead':
            # rtcRead() → arr_id [sec, min, hour, day, weekday, month, year]
            if len(node.args) != 0:
                raise CompileError("rtcRead() takes no arguments", node.line)
            self.asm.builtin(Builtin.RTC_READ)
            return

        if name == 'rtcWrite':
            # rtcWrite(arr_id) — set RTC from [sec, min, hour, day, weekday, month, year]
            if len(node.args) != 1:
                raise CompileError("rtcWrite() takes 1 argument: array", node.line)
            self._gen_expr(node.args[0])
            self.asm.builtin(Builtin.RTC_WRITE)
            return

        if name == 'fileOpen':
            # fileOpen(name) → handle (1 or 2), or 0xFF on error.
            # Caller MUST check handle != 0xFF before use.
            if len(node.args) != 1:
                raise CompileError("fileOpen() takes 1 argument: name", node.line)
            self._gen_expr(node.args[0])
            self.asm.builtin(Builtin.FILE_OPEN)
            return

        if name == 'fileRead':
            # fileRead(handle) → byte (0..255) or -1 on EOF.
            # Passing 0xFF or an unopened handle → VM error.
            if len(node.args) != 1:
                raise CompileError("fileRead() takes 1 argument: handle", node.line)
            self._gen_expr(node.args[0])
            self.asm.builtin(Builtin.FILE_READ)
            return

        if name == 'fileSize':
            # fileSize(handle) → total byte count.
            # Passing 0xFF or an unopened handle → VM error.
            if len(node.args) != 1:
                raise CompileError("fileSize() takes 1 argument: handle", node.line)
            self._gen_expr(node.args[0])
            self.asm.builtin(Builtin.FILE_SIZE)
            return

        if name == 'fileClose':
            # fileClose(handle) — release one of the two slots.
            # Passing 0xFF or an unopened handle → VM error.
            if len(node.args) != 1:
                raise CompileError("fileClose() takes 1 argument: handle", node.line)
            self._gen_expr(node.args[0])
            self.asm.builtin(Builtin.FILE_CLOSE)
            return

        if name == 'showModal':
            # showModal(builderFn, [overlayClickFn]) → result
            # Suspends the VM until setDialogResult fires (typically from a
            # callback on a widget inside the dialog). The builder runs as a
            # normal function; its return value (the dialog overlay widget id)
            # is captured by the runtime. The optional second arg is the
            # overlay's on_click handler — if omitted, the compiler injects
            # @modalDefaultOverlayClick from lib/modal.fl.
            if len(node.args) not in (1, 2):
                raise CompileError(
                    "showModal() takes 1 or 2 arguments: builderFn, [overlayClickFn]",
                    node.line)
            self._gen_expr(node.args[0])
            if len(node.args) == 2:
                self._gen_expr(node.args[1])
            else:
                # Synthesize @modalDefaultOverlayClick. Resolves now (will fail
                # with a clear error if the user did not #include "modal.fl").
                if 'modalDefaultOverlayClick' not in self.func_ids:
                    raise CompileError(
                        'showModal() without explicit handler requires '
                        '#include "modal.fl"',
                        node.line)
                fn_id = self.func_ids['modalDefaultOverlayClick']
                self.asm.push(fn_id)
            self.asm.builtin(Builtin.SHOW_MODAL)
            return

        if name == 'setDialogResult':
            # setDialogResult(result) — record on the innermost open modal
            # frame. Subsequent calls within the same dialog are ignored
            # (first-write-wins). No-op when no dialog is open.
            if len(node.args) != 1:
                raise CompileError("setDialogResult() takes 1 argument: result", node.line)
            self._gen_expr(node.args[0])
            self.asm.builtin(Builtin.SET_DIALOG_RESULT)
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
            self.asm._emit(Op.CALL)
            info['patches'].append(self.asm.pos)
            self.asm._emit(b'\x00\x00')
        # User function may change target
        self._current_target = None

    def _gen_set(self, node):
        if len(node.args) < 2:
            raise CompileError("set() needs at least 2 arguments: set(prop, value...)", node.line)

        prop_name = self._prop_name_from_arg(node.args[0], node.line)
        prop_id, is_compound = _resolve_prop(prop_name)
        value_args = node.args[1:]

        # Special: set(text, "literal") or set(text, str_id_expr)
        if prop_id == Prop.TEXT:
            if len(value_args) != 1:
                raise CompileError("set(text) takes 1 value", node.line)
            arg = value_args[0]
            if isinstance(arg, StrLit):
                # String literal → W_SET PROP_TEXT LEN (allocates in StringPool)
                self.asm.w_set_text(arg.value)
            else:
                # Expression (str_id) → BUILTIN SET_TEXT
                self._gen_expr(arg)
                self.asm.str_set_text()
            return

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
            # set(graph_arr, samples) — `samples` is an array variable; load its
            # slot (which holds the runtime arr_id) instead of expanding it as
            # an indexed read. Same pattern as arrFree/arrToStr.
            arg = value_args[0]
            if (prop_id == Prop.GRAPH_ARR and isinstance(arg, VarRef)
                    and arg.name in self.array_vars):
                slot = self._var_slot(arg.name, node.line)
                self.asm.load(slot)
            else:
                self._gen_expr(arg)
            self.asm.w_set(prop_id)

    def _emit_target(self, var_name, line):
        """Emit dynamic target for a widget variable: LOAD slot + W_TARGET_S.
        The variable's slot holds the runtime widget id (set by W_ALLTAR), so
        this stays correct even when widgets are destroyed and reallocated —
        which static W_TARGET <id> would not, since the slot can be reused for
        a different widget (e.g. dialog overlay → next dialog's overlay).
        Skips emission when this widget is already the current target."""
        if var_name not in self.widget_ids:
            raise CompileError(
                f"'{var_name}' is not a widget variable (use var {var_name} = alloc())",
                line)
        if self._current_target != var_name:
            slot = self._var_slot(var_name, line)
            self.asm.load(slot)
            self.asm.w_target_s()
            self._current_target = var_name

    def _gen_dot_assign(self, node):
        """Compile widget.prop = value → W_TARGET + W_SET."""
        self._emit_target(node.obj, node.line)

        prop_id, is_compound = _resolve_prop(node.prop)

        # Special: text property
        if prop_id == Prop.TEXT:
            val = node.value
            # x.text = "literal" → W_SET_TEXT (no heap alloc)
            if isinstance(val, StrLit):
                self.asm.w_set_text(val.value)
            # x.text = str("literal") → optimize to W_SET_TEXT
            elif (isinstance(val, CallExpr) and val.name == 'str'
                    and len(val.args) == 1 and isinstance(val.args[0], StrLit)):
                self.asm.w_set_text(val.args[0].value)
            else:
                # Expression (str_id) → STR_SET_TEXT builtin
                self._gen_expr(val)
                self.asm.str_set_text()
            return

        if is_compound:
            if not isinstance(node.value, ArrayLit):
                raise CompileError(
                    f"compound property '{node.prop}' requires [...] value", node.line)
            elements = node.value.elements
            all_const = all(isinstance(e, NumLit) for e in elements)
            if all_const:
                self.asm.w_set_compound(prop_id, [e.value for e in elements])
            else:
                scalars = COMPOUND_SCALARS.get(prop_id)
                if scalars is None or len(elements) != len(scalars):
                    raise CompileError(
                        f"'{node.prop}' expects {len(scalars) if scalars else '?'} values",
                        node.line)
                for scalar_id, val_expr in zip(scalars, elements):
                    self._gen_expr(val_expr)
                    self.asm.w_set(scalar_id)
        else:
            self._gen_expr(node.value)
            self.asm.w_set(prop_id)

    def _gen_dot_read(self, node):
        """Compile widget.prop read → W_TARGET + W_GET."""
        self._emit_target(node.obj, node.line)
        prop_id, is_compound = _resolve_prop(node.prop)
        if is_compound:
            raise CompileError(
                f"cannot read compound property '{node.prop}' directly", node.line)
        self.asm.w_get(prop_id)

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


def compile(source, filename="<input>", include_dirs=None):
    """Compile source code to bytecode. Returns bytes."""
    try:
        source = preprocess(source, filename, include_dirs)
        tokens = tokenize(source)
        parser = Parser(tokens)
        program = parser.parse()
        codegen = CodeGen()
        bytecode = codegen.generate(program)
        return _ImageBytes(bytecode, debug_symbols=codegen.debug_info(filename))
    except CompileError:
        raise
    except Exception as e:
        raise CompileError(f"internal error: {e}") from e


# System callback name → expected_arg_count
SYSTEM_CALLBACKS = {
    'setup':            0,
    'loop':             0,
    'on_program_start': 0,
    'on_page_changing': 2,
    'on_page_changed':  1,
    'on_user_message':  1,
    'on_touch_down':    2,
    'on_touch_up':      0,
    'on_touch_move':    2,
}


def compile_with_meta(source, filename="<input>", include_dirs=None):
    """Compile source code to (bytecode, metadata_or_None).

    Legacy API — wraps build_image internally.
    Returns (image_bytes, None); CLI symbol debug is written to a .dbg sidecar.
    """
    return build_image(source, filename, include_dirs), None


def build_image(source, filename="<input>", include_dirs=None, render_mode="dirty"):
    """Compile source to VM image format (v4 header + opcodes).

    Image format (v4):
      version(u8=4) + function_count(u16 LE) + global_count(u16 LE) + flags(u16 LE)
      + widget_count(u8) + ext_count(u8)
      + [func_id(u16) + kind(u8) + pad(u8) + offset(u32) + length(u32)] × N
      + opcodes...

    Flags bit 0: render_mode (0=dirty, 1=buffered, 2=epaper)
    widget_count: number of alloc() call sites for firmware Vec pre-allocation.

    Requires fn setup() and fn loop() in the source.
    Global variables (top-level var) are supported.
    """
    try:
        # Extract #hint_widgets from raw source before preprocessing strips the directive.
        hint_wc, hint_ec = _extract_hint_widgets(source)

        source = preprocess(source, filename, include_dirs)
        tokens = tokenize(source)
        parser = Parser(tokens)
        program = parser.parse()

        # Validate system callback arg counts
        for fn in program.functions:
            if fn.name in SYSTEM_CALLBACKS:
                expected = SYSTEM_CALLBACKS[fn.name]
                if len(fn.params) != expected:
                    raise CompileError(
                        f"{fn.name}() expects {expected} parameter(s), got {len(fn.params)}",
                        fn.line if hasattr(fn, 'line') else 0)

        codegen = CodeGen()
        bytecode = codegen.generate_image(program)
        debug_symbols = codegen.debug_info(filename)

        # Build the image header with function table
        cc = CcCompiler()
        func_names = {}  # offset -> name
        for fn_name, info in codegen.functions.items():
            if info['addr'] is not None:
                func_id = cc.define_func(fn_name)
                fid, kind, _, _ = cc._funcs[fn_name]
                offset = info['addr']
                length = (info['end_addr'] or offset) - offset
                cc._funcs[fn_name] = (fid, kind, offset, length)
                func_names[offset] = fn_name
        if render_mode not in ("dirty", "buffered", "epaper"):
            raise CompileError(f"invalid render mode: {render_mode}")        
        flags = 0x01 if render_mode == "buffered" else 0x02 if render_mode == "epaper" else 0x00
        # Use #hint_widgets if provided; fall back to static alloc()-site count.
        auto_wc = codegen.next_widget_id - 1
        widget_count = hint_wc if hint_wc > 0 else auto_wc
        ext_count    = hint_ec if hint_wc > 0 else widget_count
        header = cc.build_image_header(
            global_count=len(codegen.global_vars),
            flags=flags,
            widget_count=widget_count,
            ext_count=ext_count,
        )
        image = header + bytecode

        # Stash function name map on the bytes object for CLI use
        image = _ImageBytes(image, func_names, debug_symbols)
        return image

    except CompileError:
        raise
    except Exception as e:
        raise CompileError(f"internal error: {e}") from e


class _ImageBytes(bytes):
    """bytes subclass that carries function name metadata for disassembly."""
    def __new__(cls, data, func_names=None, debug_symbols=None):
        obj = super().__new__(cls, data)
        obj.func_names = func_names or {}
        obj.debug_symbols = debug_symbols or {}
        return obj


def compile_page(source, bg_color=0x0000, filename="<input>", include_dirs=None):
    """Compile source to page format: bg_color(u16 LE) + bytecode."""
    bytecode = compile(source, filename, include_dirs)
    return _ImageBytes(
        struct.pack('<H', bg_color) + bytecode,
        debug_symbols=getattr(bytecode, 'debug_symbols', {}))


# ============================================================
# CLI
# ============================================================


_KIND_NAMES = {
    0: 'setup', 1: 'loop', 2: 'func', 3: 'on_program_start',
    4: 'on_page_changing', 5: 'on_page_changed', 6: 'on_user_message',
    7: 'on_touch_down', 8: 'on_touch_up', 9: 'on_touch_move',
}


def _debug_path_for(path):
    root, _ = os.path.splitext(path)
    return root + '.dbg'


def _write_debug_file(output_path, output):
    symbols = getattr(output, 'debug_symbols', None)
    if not symbols:
        return None
    dbg_path = _debug_path_for(output_path)
    with open(dbg_path, 'w', encoding='utf-8') as f:
        json.dump(symbols, f, indent=2, sort_keys=True)
        f.write('\n')
    return dbg_path


def _load_debug_file(binary_path):
    dbg_path = _debug_path_for(binary_path)
    if not os.path.isfile(dbg_path):
        return {}
    with open(dbg_path, 'r', encoding='utf-8') as f:
        return json.load(f)


def _debug_func_names(debug_symbols):
    names = {}
    for fn in (debug_symbols or {}).get('functions', []):
        offset = fn.get('offset')
        name = fn.get('name')
        if offset is not None and name:
            names[offset] = name
    return names


def _looks_like_image(data):
    if len(data) < 5:
        return False
    version = data[0]
    if version < 1 or version > 4:
        return False
    try:
        func_count = struct.unpack_from('<H', data, 1)[0]
        return _image_header_size(version, func_count) <= len(data)
    except Exception:
        return False


def _extract_labels(image, debug_symbols=None):
    """Build {offset: "fn_name()"} labels dict from image header."""
    # Use real function names if available (from _ImageBytes)
    real_names = dict(_debug_func_names(debug_symbols))
    real_names.update(getattr(image, 'func_names', {}))
    version = image[0]
    func_count = struct.unpack_from('<H', image, 1)[0]
    hdr_base = _image_header_size(version, 0)
    labels = {}
    for i in range(func_count):
        base = hdr_base + i * 12
        kind = image[base + 2]
        offset = struct.unpack_from('<I', image, base + 4)[0]
        if offset in real_names:
            name = real_names[offset]
        else:
            name = _KIND_NAMES.get(kind, f'func_{kind}')
        labels[offset] = f'{name}()'
    return labels


def _image_header_size(version, func_count):
    """Return header size in bytes for the given version."""
    if version >= 4:
        base = 9
    elif version >= 3:
        base = 7
    else:
        base = 5
    return base + func_count * 12


def _print_image_header(output, debug_symbols=None):
    """Print image header summary to stdout."""
    real_names = dict(_debug_func_names(debug_symbols))
    real_names.update(getattr(output, 'func_names', {}))
    version = output[0]
    func_count = struct.unpack_from('<H', output, 1)[0]
    global_count = struct.unpack_from('<H', output, 3)[0]
    hdr_base = _image_header_size(version, 0)  # fixed part only
    opcode_start = hdr_base + func_count * 12

    extra = ''
    if version >= 3:
        flags = struct.unpack_from('<H', output, 5)[0]
        rm = 'buffered' if flags & 0x01 else 'dirty'
        extra = f', render={rm}'
    if version >= 4:
        wc, ec = output[7], output[8]
        extra += f', widgets={wc}, exts={ec}'

    print(f'; image v{version}: {func_count} functions, {global_count} globals, opcodes={len(output) - opcode_start}B{extra}')
    for i in range(func_count):
        base = hdr_base + i * 12
        fid = struct.unpack_from('<H', output, base)[0]
        kind = output[base + 2]
        offset = struct.unpack_from('<I', output, base + 4)[0]
        length = struct.unpack_from('<I', output, base + 8)[0]
        if offset in real_names:
            name = real_names[offset]
        else:
            name = _KIND_NAMES.get(kind, f'kind={kind}')
        print(f';   #{fid} {name}  offset={offset} length={length}')


def main():
    import argparse

    parser = argparse.ArgumentParser(description='ferrite-ui language compiler')
    parser.add_argument('source', help='Source file (.fl), or binary when using --disasm/--hexdump')
    parser.add_argument('-o', '--output', help='Output binary file')
    parser.add_argument('--page', type=str, default=None,
                        help='Page mode: bg_color as hex (e.g. 0x0000)')
    parser.add_argument('--image', action='store_true',
                        help='Output execute image (header + bytecode)')
    parser.add_argument('--disasm', action='store_true', help='Print disassembly')
    parser.add_argument('--hexdump', action='store_true', help='Print hex dump')
    parser.add_argument('-I', '--include', action='append', default=[],
                        metavar='DIR', help='Add include search directory (repeatable)')
    parser.add_argument('--render-mode', choices=['dirty', 'buffered'], default='dirty',
                        help='Render mode: dirty (partial update) or buffered (full redraw)')
    args = parser.parse_args()

    include_dirs = [os.path.abspath(d) for d in args.include]
    source_ext = os.path.splitext(args.source)[1].lower()
    binary_input = (
        source_ext != '.fl'
        and args.page is None
        and (args.disasm or args.hexdump)
    )
    debug_symbols = {}

    if binary_input:
        try:
            with open(args.source, 'rb') as f:
                output = f.read()
            debug_symbols = _load_debug_file(args.source)
        except FileNotFoundError:
            print(f"error: file not found: {args.source}", file=sys.stderr)
            sys.exit(1)
        except OSError as e:
            print(f"error: cannot read {args.source}: {e}", file=sys.stderr)
            sys.exit(1)

        use_image = _looks_like_image(output)
        if use_image:
            version = output[0]
            func_count = struct.unpack_from('<H', output, 1)[0]
            opcode_start = _image_header_size(version, func_count)
            raw_code = output[opcode_start:]
        else:
            raw_code = output
    else:
        try:
            with open(args.source, 'r', encoding='utf-8') as f:
                source = f.read()
        except FileNotFoundError:
            print(f"error: file not found: {args.source}", file=sys.stderr)
            sys.exit(1)
        except OSError as e:
            print(f"error: cannot read {args.source}: {e}", file=sys.stderr)
            sys.exit(1)

        try:
            # Expand #include directives before auto-detection.
            detected_source = preprocess(source, args.source, include_dirs)

            # Auto-detect image mode: if source has fn setup() and fn loop(), use --image
            has_setup_loop = ('fn setup()' in detected_source and 'fn loop()' in detected_source)
            use_image = args.image or (has_setup_loop and args.page is None)

            if use_image:
                output = build_image(source, args.source, include_dirs=include_dirs,
                                     render_mode=args.render_mode)
                # New image format: header + opcodes
                if len(output) >= 5:
                    version = output[0]
                    func_count = struct.unpack_from('<H', output, 1)[0]
                    opcode_start = _image_header_size(version, func_count)
                    raw_code = output[opcode_start:]
                else:
                    raw_code = output
            elif args.page is not None:
                bg = int(args.page, 0)
                output = compile_page(source, bg, args.source, include_dirs=include_dirs)
                raw_code = output[2:]
            else:
                output = compile(source, args.source, include_dirs=include_dirs)
                raw_code = output
            debug_symbols = getattr(output, 'debug_symbols', {})
        except CompileError as e:
            print(f"error: {e}", file=sys.stderr)
            sys.exit(1)

    # Build function labels for disassembly
    labels = {}
    if use_image and len(output) >= 5:
        labels = _extract_labels(output, debug_symbols)
    elif debug_symbols:
        labels = {
            fn['offset']: f"{fn['name']}()"
            for fn in debug_symbols.get('functions', [])
            if 'offset' in fn and 'name' in fn
        }

    if args.output:
        with open(args.output, 'wb') as f:
            f.write(output)
        msg = f"{args.source} -> {args.output} ({len(output)} bytes)"
        if use_image:
            version = output[0]
            func_count = struct.unpack_from('<H', output, 1)[0]
            opcode_start = _image_header_size(version, func_count)
            msg += f" (header: {opcode_start}B, opcodes: {len(output) - opcode_start}B, {func_count} functions)"
        dbg_path = _write_debug_file(args.output, output)
        if dbg_path:
            msg += f", debug: {dbg_path}"
        print(msg)

    if args.disasm:
        if args.page is not None:
            bg = struct.unpack_from('<H', output)[0]
            print(f'; page bg_color: 0x{bg:04X}')
        if use_image:
            _print_image_header(output, debug_symbols)
        print(disassemble(raw_code, labels=labels, symbols=debug_symbols))

    if args.hexdump:
        a = Asm()
        a._buf = bytearray(output)
        print(a.hexdump())

    if not args.output and not args.disasm and not args.hexdump:
        if use_image:
            _print_image_header(output, debug_symbols)
        print(disassemble(raw_code, labels=labels, symbols=debug_symbols))


if __name__ == '__main__':
    main()
