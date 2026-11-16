"""Editor support for ferrite-lang (.fl): diagnostics, completion, hover,
signature help and go-to-definition.

Pure Python, no UI and no protocol: ferrite_designer's code editor calls it in
process, ferrite_lsp wraps it in JSON-RPC. One `Document` per edited file:

    doc = Document("proj/.build/main.fl", include_dirs=[...], superops=True)
    doc.check(text)                 # compile: [Diagnostic]; refreshes the index
    doc.complete(text, line, col)   # [Completion]
    doc.hover(text, line, col)      # str or None
    doc.signature(text, line, col)  # Signature or None
    doc.definition(text, line, col) # Location or None

Lines and columns are 0-based, in `text` (the editor's current, possibly
unsaved, contents of `path`). Symbols come from the last text that parsed (the
index), so completion keeps working while the code is half typed; locals and
the cursor's context come from the current text.
"""

import os
import re
import json
from dataclasses import dataclass, field

_TOOLS_DIR = os.path.dirname(os.path.abspath(__file__))

from ferrite_lang import (  # noqa: E402
    CompileError, Parser, tokenize, preprocess_map, build_image,
    FnDef, ClassDef, VarDecl, CallExpr, NumLit, FloatLit, StrLit, BoolLit,
    KEYWORDS, _display_path,
)
from ferrite_cc import PROP_MAP  # noqa: E402


@dataclass
class Diagnostic:
    line: int               # 0-based line in the edited text
    message: str
    severity: str = 'error'


@dataclass
class Completion:
    label: str
    kind: str               # keyword type function method field property variable
                            # constant class widget file snippet
    detail: str = ''
    doc: str = ''
    insert: str = None      # text to insert (default: label)


@dataclass
class Location:
    path: str               # absolute path
    line: int               # 0-based


@dataclass
class Signature:
    label: str              # "drawText(x, y, font_id, fg, bg, text)"
    params: list = field(default_factory=list)   # parameter labels, as in label
    active: int = 0
    doc: str = ''


@dataclass
class _Sym:
    kind: str               # function class widget global const field method local param
    name: str
    path: str = None
    line: int = 0           # 1-based in path
    type: str = None
    detail: str = ''
    owner: str = None       # class of a field/method


class _Index:
    """Symbols of one parse: functions, classes, globals, widget constructors."""

    def __init__(self):
        self.functions = {}  # name -> (FnDef, path, line)   (no methods, no __ names)
        self.classes = {}    # name -> (ClassDef, path, line)
        self.globals = {}    # name -> (VarDecl, path, line)
        self.widgets = {}    # name -> (defaults, path, line)
        self.line_map = []   # preprocessed line - 1 -> (path, line)


# ============================================================
# Context of the cursor
# ============================================================

_FN_HEAD_RE = re.compile(r'\bfn\s+(\w+)\s*\(([^)]*)\)\s*(?:->\s*(\w+))?\s*$')
_CLASS_HEAD_RE = re.compile(r'\bclass\s+(\w+)(?:\s*:\s*(\w+))?\s*$')
_LOCAL_RE = re.compile(
    r'\bvar\s+(\w+)\s*(?::\s*(\w+))?\s*(?:\[\s*\w+\s*\])?\s*(?:=\s*([^;]*))?')
_LAMBDA_PARAMS_RE = re.compile(r'\|([\w\s,:]*)\|\s*$')


def _scan(text):
    """Walk `text` (everything before the cursor) skipping strings and
    comments. Returns (open brackets [(char, pos)], open blocks [(kind, name,
    extra, pos)], state) where state is 'code', 'string' or 'comment'. A block
    is ('fn', name, (params, return type)), ('class', name, parent) or
    ('lambda', None, params) or ('block', ...)."""
    brackets, blocks = [], []
    i, n = 0, len(text)
    stmt_start = 0
    while i < n:
        ch = text[i]
        if ch == '/' and i + 1 < n and text[i + 1] == '/':
            j = text.find('\n', i)
            if j < 0:
                return brackets, blocks, 'comment'
            i = j
            continue
        if ch == '/' and i + 1 < n and text[i + 1] == '*':
            j = text.find('*/', i + 2)
            if j < 0:
                return brackets, blocks, 'comment'
            i = j + 2
            continue
        if ch == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == '\\' else 1
            if j >= n:
                return brackets, blocks, 'string'
            i = j + 1
            continue
        if ch in '([':
            brackets.append((ch, i))
        elif ch in ')]':
            if brackets:
                brackets.pop()
        elif ch == '{':
            head = text[stmt_start:i]
            m = _FN_HEAD_RE.search(head)
            c = _CLASS_HEAD_RE.search(head)
            lam = _LAMBDA_PARAMS_RE.search(head)
            if m:
                blocks.append(('fn', m.group(1), (m.group(2), m.group(3)), i))
            elif c:
                blocks.append(('class', c.group(1), c.group(2), i))
            elif lam or head.rstrip().endswith('||'):
                blocks.append(('lambda', None, lam.group(1) if lam else '', i))
            else:
                blocks.append(('block', None, None, i))
            stmt_start = i + 1
        elif ch == '}':
            if blocks:
                blocks.pop()
            stmt_start = i + 1
        elif ch == ';':
            stmt_start = i + 1
        i += 1
    return brackets, blocks, 'code'


def _params(text):
    """'a: int, b' -> [('a', 'int'), ('b', None)]"""
    out = []
    for p in text.split(','):
        p = p.strip()
        if not p:
            continue
        name, _, t = p.partition(':')
        out.append((name.strip(), t.strip() or None))
    return out


def _offset(text, line, col):
    lines = text.split('\n')
    line = max(0, min(line, len(lines) - 1))
    return sum(len(x) + 1 for x in lines[:line]) + min(col, len(lines[line]))


def _word_at(text, off):
    """(word, start, end) of the identifier at offset `off` (or ending there)."""
    s = off
    while s > 0 and (text[s - 1].isalnum() or text[s - 1] == '_'):
        s -= 1
    e = off
    while e < len(text) and (text[e].isalnum() or text[e] == '_'):
        e += 1
    return text[s:e], s, e


def _split_top(segment):
    """Split a call's argument text at top-level commas."""
    parts, depth, cur = [], 0, []
    i = 0
    while i < len(segment):
        ch = segment[i]
        if ch == '"':
            j = i + 1
            while j < len(segment) and segment[j] != '"':
                j += 2 if segment[j] == '\\' else 1
            cur.append(segment[i:j + 1])
            i = j + 1
            continue
        if ch in '([{':
            depth += 1
        elif ch in ')]}':
            depth -= 1
        if ch == ',' and depth == 0:
            parts.append(''.join(cur))
            cur = []
        else:
            cur.append(ch)
        i += 1
    parts.append(''.join(cur))
    return parts


def _type_name(t):
    return t if t and t != 'any' else None


def _vardecl_type(v, classes, widgets):
    """Type of a VarDecl from its annotation or initializer."""
    if v.array_size is not None:
        return 'array'
    t = _type_name(v.var_type)
    if t:
        return t
    init = v.init
    if isinstance(init, CallExpr):
        if init.name in classes:
            return init.name
        if init.name == 'alloc' or init.name in widgets:
            return 'widget'
    if isinstance(init, FloatLit):
        return 'float'
    if isinstance(init, StrLit):
        return 'string'
    if type(init).__name__ == 'WidgetExpr':
        return 'widget'
    return None


def _const_text(v):
    if isinstance(v, (NumLit, FloatLit)):
        return str(v.value)
    if isinstance(v, BoolLit):
        return 'true' if v.value else 'false'
    if isinstance(v, StrLit):
        return json.dumps(v.value)
    return None


# ============================================================
# Projects
# ============================================================

def project_settings(path):
    """(include_dirs, superops, native_flex) for a .fl file: from the nearest
    project.json in its directory or above (as ferrite_build resolves them),
    else its own directory plus ferrite-ui's lib/."""
    lib = os.path.normpath(os.path.join(_TOOLS_DIR, '..', 'lib'))
    d = os.path.dirname(os.path.abspath(path))
    while True:
        pj = os.path.join(d, 'project.json')
        if os.path.isfile(pj):
            try:
                with open(pj, encoding='utf-8') as f:
                    proj = json.load(f)
            except (OSError, ValueError):
                break
            dirs = [os.path.normpath(os.path.join(d, x)) for x in proj.get('include_dirs', [])]
            caps = {}
            try:
                from ferrite_devices import device_spec
                caps = device_spec(proj.get('device'))['caps'] if proj.get('device') else {}
            except Exception:
                caps = {}
            dirs = list(dict.fromkeys(dirs + [lib]))
            return dirs, bool(caps.get('superops')), bool(caps.get('native_flex'))
        parent = os.path.dirname(d)
        if parent == d:
            break
        d = parent
    return [os.path.dirname(os.path.abspath(path)), lib], False, False


# ============================================================
# Document
# ============================================================

class Document:
    def __init__(self, path, include_dirs=(), superops=False, native_flex=False):
        self.path = os.path.abspath(path)
        self.include_dirs = list(include_dirs)
        self.superops = superops
        self.native_flex = native_flex
        self.index = _Index()
        self._ctors = {}   # class name -> its constructor FnDef
        self._methods = {} # 'Class.method' -> FnDef (with self)
        self._files = {}   # path -> (mtime, lines) for declaration text

    # --- diagnostics and the index ---

    def check(self, text):
        """Compile `text` as the file's contents. Returns the errors (at most
        one: the compiler stops at the first) and refreshes the index if the
        text parses."""
        self.reindex(text)
        try:
            build_image(text, filename=self.path, include_dirs=self.include_dirs,
                        superops=self.superops, native_flex=self.native_flex)
        except CompileError as e:
            return [self._diagnostic(text, e)]
        except Exception as e:  # noqa: BLE001 - an editor must not die on a compiler bug
            return [Diagnostic(0, f"internal compiler error: {e}")]
        return []

    def _diagnostic(self, text, e):
        msg = e.msg if hasattr(e, 'msg') else str(e)
        if not e.line:
            return Diagnostic(0, msg)
        if e.file is None:
            return Diagnostic(e.line - 1, msg)
        # In an included file: report it at the #include that pulls it in.
        where = f"{e.file} line {e.line}: {msg}"
        try:
            _, line_map = preprocess_map(text, self.path, self.include_dirs)
        except CompileError:
            return Diagnostic(0, where)
        target = os.path.normcase(os.path.normpath(
            os.path.join(os.path.dirname(self.path), e.file)))
        me = os.path.normcase(self.path)
        for i, (p, ln) in enumerate(line_map):
            if os.path.normcase(p) == target and ln == e.line:
                for p2, ln2 in line_map[i + 1:]:
                    if os.path.normcase(p2) == me:
                        return Diagnostic(ln2 - 1, where)
        return Diagnostic(0, where)

    def reindex(self, text):
        """Collect the symbols of `text` and its includes; keeps the old index
        if it doesn't parse. Returns True if it did."""
        try:
            source, line_map = preprocess_map(text, self.path, self.include_dirs)
            parser = Parser(tokenize(source))
            program = parser.parse()
        except Exception:  # noqa: BLE001 - half-typed code
            return False

        def at(line):
            if 1 <= line <= len(line_map):
                return line_map[line - 1]
            return (None, 0)

        idx = _Index()
        idx.line_map = line_map
        for c in program.classes:
            idx.classes[c.name] = (c, *at(c.line))
        for f in program.functions:
            if '.' in f.name or f.name.startswith('__') or f.name in idx.classes:
                continue
            idx.functions[f.name] = (f, *at(f.line))
        for s in program.statements:
            if isinstance(s, VarDecl):
                idx.globals[s.name] = (s, *at(s.line))
        for name, defaults in parser.widgets.items():
            idx.widgets[name] = (defaults, *at(parser.widget_lines.get(name, 0)))
        # Constructors: the function named after the class, for its params.
        self._ctors = {f.name: f for f in program.functions if f.name in idx.classes}
        self._methods = {f.name: f for f in program.functions if '.' in f.name}
        self.index = idx
        return True

    # --- symbols ---

    def _class_chain(self, cls):
        """The class and its bases, nearest first."""
        out, seen = [], set()
        while cls in self.index.classes and cls not in seen:
            seen.add(cls)
            c = self.index.classes[cls][0]
            out.append(c)
            cls = c.parent
        return out

    def _members(self, cls):
        """{name: _Sym} of a class's fields and methods, inherited included."""
        out = {}
        for c in reversed(self._class_chain(cls)):
            for f in c.fields:
                t = _vardecl_type(f, self.index.classes, self.index.widgets)
                out[f.name] = _Sym('field', f.name, *self._loc(f.line), type=t,
                                   detail=f"{c.name}.{f.name}" + (f": {t}" if t else ''),
                                   owner=c.name)
            for m in c.methods:
                sig = self._fn_sig(m.name, m.params, m.param_types, m.return_type)
                out[m.name] = _Sym('method', m.name, *self._loc(m.line),
                                   type=_type_name(m.return_type), detail=f"{c.name}.{sig}",
                                   owner=c.name)
        return out

    def _loc(self, line):
        """(path, line) of a line of the indexed (preprocessed) source."""
        lm = self.index.line_map
        return lm[line - 1] if 1 <= line <= len(lm) else (None, 0)

    @staticmethod
    def _fn_sig(name, params, types, ret):
        ps = []
        for i, p in enumerate(params):
            t = _type_name(types[i]) if types and i < len(types) else None
            ps.append(f"{p}: {t}" if t else p)
        r = _type_name(ret)
        return f"{name}({', '.join(ps)})" + (f" -> {r}" if r and r != 'int' else '')

    def _locals(self, text, off):
        """{name: (type, offset)} of the params and locals of the function
        around `off` (plus the class it is a method of, as 'self')."""
        _, blocks, _ = _scan(text[:off])
        out = {}
        fn_block = next((b for b in reversed(blocks) if b[0] == 'fn'), None)
        cls = next((b for b in reversed(blocks) if b[0] == 'class'), None)
        if cls is not None and fn_block is not None and fn_block[3] > cls[3]:
            out['self'] = (cls[1], cls[3])
        start = 0
        if fn_block is not None:
            start = fn_block[3]
            for name, t in _params(fn_block[2][0]):
                out[name] = (_type_name(t), start)
        for b in blocks:
            if b[0] == 'lambda' and b[3] >= start:
                for name, t in _params(b[2]):
                    out[name] = (_type_name(t), b[3])
        if fn_block is None:
            return out
        body = text[start:off]
        for m in _LOCAL_RE.finditer(body):
            name, ann, init = m.groups()
            t = _type_name(ann)
            if t is None and init:
                t = self._init_type(init.strip())
            out[name] = (t, start + m.start())
        return out

    def _init_type(self, init):
        """Type of an initializer's source text, for the common shapes."""
        m = re.search(r'\bas\s+(\w+)\s*$', init)
        if m and m.group(1) in self.index.classes:
            return m.group(1)
        m = re.match(r'(\w+)\s*\(', init)
        if m:
            name = m.group(1)
            if name in self.index.classes:
                return name
            if name == 'alloc' or name in self.index.widgets:
                return 'widget'
            f = self.index.functions.get(name)
            return _type_name(f[0].return_type) if f else None
        if init.startswith('"'):
            return 'string'
        if re.fullmatch(r'\d+\.\d*', init):
            return 'float'
        return None

    def _type_of(self, text, off, expr):
        """Type of a chain like `a`, `a.b`, `a.m().c` at `off`: a class name,
        'widget', a builtin type, or None."""
        parts = [p.strip() for p in re.split(r'\.(?![^()]*\))', expr)]
        if not parts or not parts[0]:
            return None
        head = parts[0]
        t = None
        if head.endswith(')'):
            name = head[:head.index('(')].strip()
            f = self.index.functions.get(name)
            if name in self.index.classes:
                t = name
            elif name in self.index.widgets:
                t = 'widget'
            elif f:
                t = _type_name(f[0].return_type)
        else:
            loc = self._locals(text, off)
            if head in loc:
                t = loc[head][0]
            elif head in self.index.globals:
                t = _vardecl_type(self.index.globals[head][0], self.index.classes,
                                  self.index.widgets)
        for p in parts[1:]:
            if t not in self.index.classes:
                return None
            name = p[:p.index('(')].strip() if p.endswith(')') else p
            mem = self._members(t).get(name)
            t = mem.type if mem else None
        return t

    def _symbol(self, text, off):
        """The symbol at `off` as a _Sym, or a (category, name) for builtins
        and the like, or None."""
        word, s, e = _word_at(text, off)
        if not word:
            return None
        before = text[:s].rstrip()
        if before.endswith('@'):
            f = self.index.functions.get(word)
            return _Sym('function', word, f[1], f[2]) if f else None
        if before.endswith('.'):
            m = re.search(r'((?:\w+(?:\([^()]*\))?\s*\.\s*)*\w+(?:\([^()]*\))?)\s*\.$', before)
            base = self._type_of(text, s, m.group(1)) if m else None
            if base in self.index.classes:
                return self._members(base).get(word)
            return ('property', word) if word in PROP_MAP else None
        loc = self._locals(text, off)
        if word in loc and word != 'self':
            t, pos = loc[word]
            return _Sym('local', word, self.path, text.count('\n', 0, pos) + 1, type=t)
        idx = self.index
        if word in idx.classes:
            return _Sym('class', word, idx.classes[word][1], idx.classes[word][2])
        if word in idx.widgets:
            return _Sym('widget', word, idx.widgets[word][1], idx.widgets[word][2])
        if word in idx.functions:
            f, p, ln = idx.functions[word]
            return _Sym('function', word, p, ln,
                        detail=self._fn_sig(f.name, f.params, f.param_types, f.return_type))
        if word in idx.globals:
            v, p, ln = idx.globals[word]
            return _Sym('const' if v.is_const else 'global', word, p, ln,
                        type=_vardecl_type(v, idx.classes, idx.widgets))
        if word in BUILTIN_SIGNATURES:
            return ('builtin', word)
        if word in KEYWORD_DOCS:
            return ('keyword', word)
        if word in CALLBACK_DOCS:
            return ('callback', word)
        if word in PROP_MAP:
            return ('property', word)
        return None

    def _decl_text(self, path, line, text):
        """The declaration's line, with the comment lines just above it."""
        if path is None or line < 1:
            return ''
        if os.path.normcase(path) == os.path.normcase(self.path):
            lines = text.split('\n')
        else:
            try:
                mtime = os.path.getmtime(path)
                cached = self._files.get(path)
                if cached is None or cached[0] != mtime:
                    with open(path, encoding='utf-8') as f:
                        cached = (mtime, f.read().split('\n'))
                    self._files[path] = cached
                lines = cached[1]
            except OSError:
                return ''
        if line > len(lines):
            return ''
        out = [lines[line - 1].strip().rstrip('{').rstrip()]
        i = line - 2
        while i >= 0 and lines[i].strip().startswith('//'):
            out.insert(0, lines[i].strip())
            i -= 1
        return '\n'.join(out)

    def short_path(self, path):
        """`path` for display: relative to this file's directory, or for a
        library file `<include dir name>/<path in it>` (lib/flex.fl)."""
        here = os.path.dirname(self.path)
        for d in [here] + self.include_dirs:
            d = os.path.abspath(d)
            try:
                rel = os.path.relpath(path, d)
            except ValueError:
                continue
            if not rel.startswith('..'):
                rel = rel.replace('\\', '/')
                return rel if d == here else os.path.basename(d) + '/' + rel
        return path

    # --- hover / definition ---

    def hover(self, text, line, col):
        off = _offset(text, line, col)
        sym = self._symbol(text, off)
        if sym is None:
            return None
        if isinstance(sym, tuple):
            cat, word = sym
            if cat == 'builtin':
                b = BUILTIN_SIGNATURES[word]
                return f"{b['sig']}\n\n{b['desc']}"
            if cat == 'keyword':
                return f"{word} (keyword)\n\n{KEYWORD_DOCS[word]}"
            if cat == 'callback':
                c = CALLBACK_DOCS[word]
                return f"{c['sig']}\n\n{c['desc']}"
            if cat == 'property':
                return f"{word} (widget property)\n\n{PROP_DOCS.get(word, '')}".rstrip()
            return None
        decl = self._decl_text(sym.path, sym.line, text)
        where = ''
        if sym.path and os.path.normcase(sym.path) != os.path.normcase(self.path):
            where = f"\n\n{self.short_path(sym.path)}:{sym.line}"
        if sym.kind == 'local':
            return f"{sym.name}: {sym.type or 'any'} (local)\n\n{decl}"
        return f"{decl}{where}" if decl else (sym.detail or sym.name)

    def definition(self, text, line, col):
        sym = self._symbol(text, _offset(text, line, col))
        if not isinstance(sym, _Sym) or sym.path is None:
            return None
        return Location(sym.path, max(0, sym.line - 1))

    # --- signature help ---

    def signature(self, text, line, col):
        off = _offset(text, line, col)
        brackets, _, state = _scan(text[:off])
        if state != 'code':
            return None
        opens = [b for b in brackets if b[0] == '(']
        if not opens:
            return None
        pos = opens[-1][1]
        if brackets[-1][1] != pos:      # inside a [...] in the call: no help
            return None
        head = text[:pos].rstrip()
        m = re.search(r'((?:\w+(?:\([^()]*\))?\s*\.\s*)*)(\w+)$', head)
        if not m:
            return None
        name, chain = m.group(2), m.group(1).rstrip().rstrip('.').strip()
        active = len(_split_top(text[pos + 1:off])) - 1
        if chain:
            base = self._type_of(text, pos, chain)
            if base not in self.index.classes:
                return None
            mem = self._members(base).get(name)
            if mem is None or mem.kind != 'method':
                return None
            fn = self._methods.get(f"{mem.owner}.{name}")
            if fn is None:
                return None
            return self._signature(name, fn.params[1:], fn.param_types[1:], fn.return_type,
                                   active, self._decl_doc(mem, text))
        if name in self.index.widgets:
            props = [p for p, _, _ in self.index.widgets[name][0]]
            label = f"{name}(prop: value, ..., children: [...], parent: w)"
            doc = "Defaults: " + (", ".join(props) if props else "none")
            return Signature(label, [], -1, doc)
        if name in self.index.classes:
            ctor = self._ctors.get(name)
            if ctor is None:
                return None
            return self._signature(name, ctor.params, ctor.param_types, None, active,
                                   self._decl_text(*self.index.classes[name][1:], text))
        f = self.index.functions.get(name)
        if f is not None:
            fn = f[0]
            return self._signature(name, fn.params, fn.param_types, fn.return_type, active,
                                   self._decl_text(f[1], f[2], text))
        b = BUILTIN_SIGNATURES.get(name)
        if b is not None:
            sig = b['sig']
            inner = sig[sig.find('(') + 1:sig.rfind(')')]
            params = [p.strip() for p in inner.split(',')] if inner.strip() else []
            return Signature(sig, params, min(active, len(params) - 1) if params else -1,
                             b['desc'])
        return None

    def _decl_doc(self, sym, text):
        return self._decl_text(sym.path, sym.line, text)

    def _signature(self, name, params, types, ret, active, doc):
        labels = []
        for i, p in enumerate(params):
            t = _type_name(types[i]) if types and i < len(types) else None
            labels.append(f"{p}: {t}" if t else p)
        r = _type_name(ret)
        label = f"{name}({', '.join(labels)})" + (f" -> {r}" if r and r != 'int' else '')
        act = min(active, len(labels) - 1) if labels else -1
        return Signature(label, labels, act, doc)

    # --- completion ---

    def complete(self, text, line, col):
        off = _offset(text, line, col)
        before = text[:off]
        cur_line = before[before.rfind('\n') + 1:]
        inc = re.search(r'#include\s+"([^"]*)$', cur_line)
        if inc:
            return self._include_items(inc.group(1))
        brackets, _, state = _scan(before)
        if state != 'code':
            return []
        prefix = re.search(r'(\w*)$', before).group(1)
        head = before[:len(before) - len(prefix)]
        items = None
        if head.rstrip().endswith('@') and head.endswith('@'):
            items = [Completion(n, 'function', self._fn_sig(f.name, f.params, f.param_types,
                                                            f.return_type))
                     for n, (f, _, _) in self.index.functions.items()]
        elif head.endswith('.'):
            m = re.search(r'((?:\w+(?:\([^()]*\))?\s*\.\s*)*\w+(?:\([^()]*\))?)\s*\.$', head)
            base = self._type_of(text, off, m.group(1)) if m else None
            if base in self.index.classes:
                items = [Completion(s.name, s.kind, s.detail) for s in
                         self._members(base).values() if not s.name.startswith('__')]
            elif base in (None, 'widget'):
                items = self._property_items()
            else:
                items = []
        else:
            items = self._widget_arg_items(text, brackets, head)
            if items is None:
                items = self._general_items(text, off)
        p = prefix.lower()
        out = [c for c in items if c.label.lower().startswith(p)]
        seen, uniq = set(), []
        for c in sorted(out, key=lambda c: (c.label.lower() != p and not c.label.startswith(prefix),
                                            _KIND_ORDER.get(c.kind, 9), c.label.lower())):
            if c.label not in seen:
                seen.add(c.label)
                uniq.append(c)
        return uniq

    def _widget_arg_items(self, text, brackets, head):
        """Named arguments at an argument start inside `Widget(`."""
        if not brackets or brackets[-1][0] != '(':
            return None
        pos = brackets[-1][1]
        m = re.search(r'(\w+)\s*$', text[:pos])
        if not m or m.group(1) not in self.index.widgets:
            return None
        args = _split_top(head[pos + 1:])
        if args[-1].strip():          # in a value: general completion
            return None
        given = {a.split(':', 1)[0].strip() for a in args[:-1] if ':' in a}
        items = [Completion(c.label, 'property', c.detail, c.doc, c.label + ': ')
                 for c in self._property_items() if c.label not in given]
        for word, doc in (('children', 'Child widgets, in order: [Label(...), name = Button(...), w]'),
                          ('parent', 'Attach the widget to this one (0 = the root)')):
            if word not in given:
                items.append(Completion(word, 'keyword', doc, '', word + ': '))
        return items

    def _property_items(self):
        return [Completion(n, 'property', 'widget property', PROP_DOCS.get(n, ''))
                for n in sorted(PROP_MAP)]

    def _general_items(self, text, off):
        idx = self.index
        items = [Completion(k, 'keyword', 'keyword', KEYWORD_DOCS.get(k, ''))
                 for k in sorted(KEYWORDS - {'not'})]
        items += [Completion(t, 'type', 'type') for t in ('int', 'float', 'widget', 'string', 'array')]
        items += [Completion(n, 'function', b['sig'], b['desc'])
                  for n, b in BUILTIN_SIGNATURES.items()]
        for n, (f, _, _) in idx.functions.items():
            items.append(Completion(n, 'function',
                                    self._fn_sig(f.name, f.params, f.param_types, f.return_type)))
        for n, (c, _, _) in idx.classes.items():
            ctor = self._ctors.get(n)
            sig = self._fn_sig(n, ctor.params, ctor.param_types, None) if ctor else n
            items.append(Completion(n, 'class', f"class {n}" + (f" : {c.parent}" if c.parent else '')
                                    + f" — {sig}"))
        for n, (d, _, _) in idx.widgets.items():
            items.append(Completion(n, 'widget', f"widget {n}(...)"))
        for n, (v, _, _) in idx.globals.items():
            if v.is_const:
                val = _const_text(v.init)
                items.append(Completion(n, 'constant', f"const {n}" + (f" = {val}" if val else '')))
            else:
                t = _vardecl_type(v, idx.classes, idx.widgets)
                items.append(Completion(n, 'variable', f"var {n}" + (f": {t}" if t else '')))
        for n, (t, _) in self._locals(text, off).items():
            items.append(Completion(n, 'variable', f"{n}: {t}" if t else n))
        return items

    def _include_items(self, prefix):
        out = []
        dirs = [os.path.dirname(self.path)] + self.include_dirs
        sub, _, file_prefix = prefix.replace('\\', '/').rpartition('/')
        seen = set()
        for d in dirs:
            d2 = os.path.join(d, sub) if sub else d
            try:
                entries = os.listdir(d2)
            except OSError:
                continue
            for e in sorted(entries):
                full = os.path.join(d2, e)
                if not e.startswith(file_prefix) or e in seen:
                    continue
                if e.endswith('.fl'):
                    seen.add(e)
                    out.append(Completion((sub + '/' if sub else '') + e, 'file', d))
                elif os.path.isdir(full) and not e.startswith('.'):
                    seen.add(e)
                    out.append(Completion((sub + '/' if sub else '') + e + '/', 'file', 'directory'))
        return out


_KIND_ORDER = {'variable': 0, 'field': 0, 'property': 1, 'method': 1, 'function': 2,
               'widget': 3, 'class': 3, 'constant': 4, 'keyword': 5, 'type': 6, 'file': 7}


# ============================================================
# Builtin / keyword / property data (also used by ferrite_lsp)
# ============================================================


KEYWORD_DOCS = {
    'var': 'Declare a mutable local variable: `var name: type = expr;`',
    'const': 'Declare a compile-time constant: `const NAME = expr;`',
    'fn': 'Define a function: `fn name(params) -> type { ... }`',
    'if': 'Conditional branch: `if (cond) { ... } else { ... }`',
    'else': 'Alternative branch for `if`',
    'while': 'Loop while condition is true: `while (cond) { ... }`',
    'for': 'C-style for loop: `for (init; cond; update) { ... }`',
    'return': 'Return from function, optionally with a value',
    'true': 'Boolean true (integer 1)',
    'false': 'Boolean false (integer 0)',
    'break': 'Exit the innermost loop immediately',
    'continue': 'Skip to next iteration of the innermost loop',
    'not': 'Logical NOT prefix operator: `not expr` (alias for `!`)',
    'int': '32-bit signed integer type',
    'float': '32-bit IEEE-754 floating point type',
    'widget': 'Widget reference type',
    'string': 'Heap-allocated string ID type',
    'array': 'Heap-allocated dynamic array ID type',
    'class': 'Define a class: `class Name : Base { var field; fn method() { ... } }`',
}

BUILTIN_SIGNATURES = {
    'alloc':       {'sig': 'alloc() -> widget', 'desc': 'Allocate a new widget. Returns the widget ID.'},
    'target':      {'sig': 'target(widget)', 'desc': 'Set the current target widget for subsequent property access.'},
    'set':         {'sig': 'set(prop, value...)', 'desc': 'Set a widget property on the current target. Compound props accept multiple values.'},
    'get':         {'sig': 'get(prop) -> int', 'desc': 'Read a scalar property from the current target widget.'},
    'parent':      {'sig': 'parent(widget)', 'desc': 'Set the parent of the current target widget.'},
    'dirty':       {'sig': 'dirty()', 'desc': 'Mark the current target widget as dirty (needs repaint).'},
    'render':      {'sig': 'render()', 'desc': 'Trigger a full render pass.'},
    'halt':        {'sig': 'halt()', 'desc': 'Halt the VM (stop execution).'},
    'yield_op':    {'sig': 'yield_op()', 'desc': 'Yield execution to the VM scheduler.'},
    'fillRect':    {'sig': 'fillRect(x, y, w, h, color)', 'desc': 'Draw a filled rectangle.'},
    'rect':        {'sig': 'rect(x, y, w, h, color)', 'desc': 'Draw a rectangle outline.'},
    'line':        {'sig': 'line(x0, y0, x1, y1, color)', 'desc': 'Draw a line between two points.'},
    'circle':      {'sig': 'circle(cx, cy, r, color)', 'desc': 'Draw a circle outline.'},
    'fillCircle':  {'sig': 'fillCircle(cx, cy, r, color)', 'desc': 'Draw a filled circle.'},
    'drawImage':   {'sig': 'drawImage(x, y, image_id)', 'desc': 'Draw an image at position.'},
    'drawText':    {'sig': 'drawText(x, y, font_id, fg, bg, "text")', 'desc': 'Draw static text at position.'},
    'delay':       {'sig': 'delay(ms)', 'desc': 'Pause execution for N milliseconds.'},
    'itof':        {'sig': 'itof(int) -> float', 'desc': 'Convert int to float.'},
    'ftoi':        {'sig': 'ftoi(float) -> int', 'desc': 'Convert float to int (truncate).'},
    'fneg':        {'sig': 'fneg(float) -> float', 'desc': 'Negate a float value.'},
    'fadd':        {'sig': 'fadd(a, b) -> float', 'desc': 'Float addition.'},
    'fsub':        {'sig': 'fsub(a, b) -> float', 'desc': 'Float subtraction.'},
    'fmul':        {'sig': 'fmul(a, b) -> float', 'desc': 'Float multiplication.'},
    'fdiv':        {'sig': 'fdiv(a, b) -> float', 'desc': 'Float division.'},
    'feq':         {'sig': 'feq(a, b) -> int', 'desc': 'Float equality comparison (returns 0 or 1).'},
    'fne':         {'sig': 'fne(a, b) -> int', 'desc': 'Float not-equal comparison.'},
    'flt':         {'sig': 'flt(a, b) -> int', 'desc': 'Float less-than comparison.'},
    'fle':         {'sig': 'fle(a, b) -> int', 'desc': 'Float less-or-equal comparison.'},
    'fgt':         {'sig': 'fgt(a, b) -> int', 'desc': 'Float greater-than comparison.'},
    'fge':         {'sig': 'fge(a, b) -> int', 'desc': 'Float greater-or-equal comparison.'},
    'sin':         {'sig': 'sin(x) -> float', 'desc': 'Sine of x (radians).'},
    'cos':         {'sig': 'cos(x) -> float', 'desc': 'Cosine of x (radians).'},
    'sqrt':        {'sig': 'sqrt(x) -> float', 'desc': 'Square root of x.'},
    'abs':         {'sig': 'abs(x) -> float', 'desc': 'Absolute value of x.'},
    'atan2':       {'sig': 'atan2(y, x) -> float', 'desc': 'Arc tangent of y/x (radians).'},
    'floor':       {'sig': 'floor(x) -> float', 'desc': 'Floor (round down).'},
    'ceil':        {'sig': 'ceil(x) -> float', 'desc': 'Ceil (round up).'},
    'str':         {'sig': 'str("literal") -> str_id', 'desc': 'Allocate a string from a literal.'},
    'itos':        {'sig': 'itos(int) -> str_id', 'desc': 'Convert int to string.'},
    'ftos':        {'sig': 'ftos(float) -> str_id', 'desc': 'Convert float to string.'},
    'sprintf':     {'sig': 'sprintf(fmt, args...) -> str_id', 'desc': 'Format a string (C-style, up to 8 args).'},
    'concat':      {'sig': 'concat(a, b) -> str_id', 'desc': 'Concatenate two strings.'},
    'strcat':      {'sig': 'strcat(a, b) -> str_id', 'desc': 'Alias for concat().'},
    'parseInt':    {'sig': 'parseInt(str_id) -> int', 'desc': 'Parse string as integer.'},
    'parseFloat':  {'sig': 'parseFloat(str_id) -> float', 'desc': 'Parse string as float.'},
    'strLen':      {'sig': 'strLen(str_id) -> int', 'desc': 'Get string length in bytes.'},
    'strlen':      {'sig': 'strlen(str_id) -> int', 'desc': 'Alias for strLen().'},
    'strcmp':      {'sig': 'strcmp(a, b) -> int', 'desc': 'Compare two strings (-1/0/1).'},
    'setText':     {'sig': 'setText(str_id)', 'desc': 'Set text on the current target widget.'},
    'drawStr':     {'sig': 'drawStr(x, y, font_id, fg, bg, str_id)', 'desc': 'Draw a dynamic string at position.'},
    'strClear':    {'sig': 'strClear()', 'desc': 'Clear all allocated strings.'},
    'strFree':     {'sig': 'strFree(str_id)', 'desc': 'Free a specific string.'},
    'arrFree':     {'sig': 'arrFree(arr_id)', 'desc': 'Free an array.'},
    'arrLen':      {'sig': 'arrLen(arr_id) -> int', 'desc': 'Number of elements in an array.'},
    'arrToStr':    {'sig': 'arrToStr(arr, [len]) -> str_id', 'desc': 'Convert array bytes to string.'},
    'roundedRect': {'sig': 'roundedRect(x, y, w, h, r, color)', 'desc': 'Draw a rectangle with rounded corners.'},
    'fillRoundedRect': {'sig': 'fillRoundedRect(x, y, w, h, r, color)', 'desc': 'Draw a filled rounded rectangle.'},
    'arc':         {'sig': 'arc(cx, cy, r, start, end, color)', 'desc': 'Draw an arc.'},
    'beginFrame':  {'sig': 'beginFrame()', 'desc': 'Begin a buffered render frame.'},
    'endFrame':    {'sig': 'endFrame()', 'desc': 'Commit and display the current render frame.'},
    'sendUsart':   {'sig': 'sendUsart(data)', 'desc': 'Send data via USART (array or string).'},
    'millis':      {'sig': 'millis() -> int', 'desc': 'Get elapsed milliseconds since boot.'},
    'fpgaCmd':     {'sig': 'fpgaCmd(cmd, data)', 'desc': 'Send raw FPGA command.'},
    'fpgaData':    {'sig': 'fpgaData(data)', 'desc': 'Send raw FPGA data (after fpgaCmd).'},
    'critical':    {'sig': 'critical()', 'desc': 'Enter critical section (VM runs without yielding).'},
    'setBrightness': {'sig': 'setBrightness(pct)', 'desc': 'Set LCD backlight brightness (0-100).'},
    'brightness':  {'sig': 'brightness() -> int', 'desc': 'Get current backlight brightness (0-100).'},
    'rtcRead':     {'sig': 'rtcRead() -> arr_id', 'desc': 'Read RTC: returns array [sec, min, hour, day, weekday, month, year].'},
    'rtcWrite':    {'sig': 'rtcWrite(arr_id)', 'desc': 'Write array to RTC.'},
    'fileOpen':    {'sig': 'fileOpen(name) -> handle', 'desc': 'Open a file. Returns handle (1 or 2) or 0xFF on error.'},
    'fileRead':    {'sig': 'fileRead(handle) -> int', 'desc': 'Read next byte (0-255) or -1 on EOF.'},
    'fileSize':    {'sig': 'fileSize(handle) -> int', 'desc': 'Get file size in bytes.'},
    'fileClose':   {'sig': 'fileClose(handle)', 'desc': 'Close a file handle.'},
    'fileSeek':    {'sig': 'fileSeek(handle, pos)', 'desc': 'Seek to byte position in file.'},
    'syscall':     {'sig': 'syscall(id, args...) -> int', 'desc': 'Invoke a system call (up to 16 args).'},
    'syscallStr':  {'sig': 'syscallStr(id, args...) -> string', 'desc': 'Invoke a system call whose result is a string id (up to 16 args).'},
    'showModal':   {'sig': 'showModal(builderFn, [overlayClickFn]) -> int', 'desc': 'Show a modal dialog. Suspends VM until setDialogResult.'},
    'setDialogResult': {'sig': 'setDialogResult(result)', 'desc': 'Set result value on innermost modal dialog frame.'},
    'animate':     {'sig': 'animate(widget, prop, to, durationMs, easing, [@onEnd])', 'desc': 'Tween a widget property from its current value to `to`. easing = EASE_* | REPEAT_LOOP/REPEAT_YOYO. Optional @onEnd fires when a one-shot tween finishes.'},
    'stopAnim':    {'sig': 'stopAnim(widget)', 'desc': 'Cancel all running tweens on a widget.'},
    'stopAnimProp': {'sig': 'stopAnimProp(widget, prop)', 'desc': 'Cancel the tween on one property of a widget.'},
    'pinMode':     {'sig': 'pinMode(pin, mode)', 'desc': 'Configure a user GPIO pin: GPIO_INPUT, GPIO_OUTPUT, GPIO_INPUT_PULLUP, GPIO_INPUT_PULLDOWN (gpio.fl).'},
    'pinWrite':    {'sig': 'pinWrite(pin, value)', 'desc': 'Drive an output pin: non-zero = high, 0 = low.'},
    'pinRead':     {'sig': 'pinRead(pin) -> int', 'desc': 'Read a GPIO pin level (0 or 1).'},
    'strByte':     {'sig': 'strByte(str, index) -> int', 'desc': 'Byte `index` of a string (0..255), or -1 past the end. UTF-8 characters outside ASCII take 2-3 bytes.'},
    'playSound':   {'sig': 'playSound(name [, loop]) -> int', 'desc': 'Play a sound resource ("sounds" in project.json), replacing the playing one. loop != 0 repeats it until stopSound(). Returns 1 if it started, 0 if the sound is missing. Boards with audio (caps.audio) only: elsewhere the build fails.'},
    'stopSound':   {'sig': 'stopSound()', 'desc': 'Stop the playing sound.'},
    'setVolume':   {'sig': 'setVolume(percent)', 'desc': 'Audio output volume, 0..100.'},
    'runProgram':  {'sig': 'runProgram(name, arg = 0) -> int', 'desc': 'Ends this program and starts program `name` from scratch (widgets, strings, arrays, callbacks and dialogs are dropped; EEPROM and files stay). Nothing after a successful call runs. Returns 0 only if there is no such program.'},
    'programArg':  {'sig': 'programArg() -> int', 'desc': 'The `arg` the program that started this one passed to `runProgram`; 0 for `main` at boot.'},
    'programName': {'sig': 'programName(index) -> string', 'desc': 'Name of program `index` (0-based, image order), "" past the last one. `programName(-1)` is the name of this program.'},
    'spawnProgram': {'sig': 'spawnProgram(name, arg = 0) -> int', 'desc': 'Start program `name` as a new task next to this one, on the same widget tree (it starts after this burst; `programArg()` gives it `arg`). Returns its task id, or -1 if there is no such program or all 16 task slots are taken. Its objects are freed when it ends. Boards with caps.multi_program only: elsewhere the build fails.'},
    'killProgram': {'sig': 'killProgram(task) -> int', 'desc': 'End task `task` after this burst (its widgets, strings and arrays are freed); `killProgram(taskId())` ends this task at once. Returns 0 for main (task 0) or a task that is not running. Boards with caps.multi_program only: elsewhere the build fails.'},
    'taskId':      {'sig': 'taskId() -> int', 'desc': 'This task\'s id (0 = main). Boards with caps.multi_program only: elsewhere the build fails.'},
    'taskAlive':   {'sig': 'taskAlive(task) -> int', 'desc': '1 while task `task` is starting or running, else 0. Boards with caps.multi_program only: elsewhere the build fails.'},
    'sendMessage': {'sig': 'sendMessage(task, a, b = 0) -> int', 'desc': 'Queue `on_task_message(from, a, b)` on task `task` (delivered after this burst). Returns 0 if it is not running or 64 messages are waiting. Boards with caps.multi_program only: elsewhere the build fails.'},
    'setForeground': {'sig': 'setForeground(task) -> int', 'desc': 'Task `task` now gets the global input events (on_touch_down/move/up, on_swipe, on_long_press); widget events always go to the task that set the handler. 0 if it is not running. Boards with caps.multi_program only: elsewhere the build fails.'},
    'foregroundTask': {'sig': 'foregroundTask() -> int', 'desc': 'The task that gets the global input events (0 = main). Boards with caps.multi_program only: elsewhere the build fails.'},
    'soundPlaying': {'sig': 'soundPlaying() -> int', 'desc': '1 while a sound is playing (until its last sample is output), else 0.'},
    'eepromSize':  {'sig': 'eepromSize() -> int', 'desc': 'EEPROM size in bytes (0 if the board has none).'},
    'eepromRead':  {'sig': 'eepromRead(addr) -> int', 'desc': 'Read one EEPROM byte (0..255). Bad address or bus error → VM fault 16.'},
    'eepromWrite': {'sig': 'eepromWrite(addr, value)', 'desc': 'Store the low byte of value. Unchanged bytes are not rewritten. Blocks up to ~5 ms per write.'},
    'eepromReadArr': {'sig': 'eepromReadArr(addr, arr, count)', 'desc': 'Read count EEPROM bytes into arr[0..count].'},
    'eepromWriteArr': {'sig': 'eepromWriteArr(addr, arr, count)', 'desc': 'Store the low byte of arr[0..count] at addr. Only changed 16-byte chunks are written (~5 ms each).'},
    'diskOpen':    {'sig': 'diskOpen(path, mode) -> int', 'desc': 'Open a file on the board storage (caps.storage): mode 0 read, 1 write (create/truncate), 2 append. A handle (16+) for fileRead/fileWrite/fileClose, or DISK_ERR_* (< 0). Paths: 8.3 names separated by "/".'},
    'diskDelete':  {'sig': 'diskDelete(path) -> int', 'desc': 'Delete a file that is not open. 0 or DISK_ERR_*.'},
    'diskMkdir':   {'sig': 'diskMkdir(path) -> int', 'desc': 'Make a directory. 0 or DISK_ERR_* (DISK_ERR_EXISTS if it is there).'},
    'diskRmdir':   {'sig': 'diskRmdir(path) -> int', 'desc': 'Delete an empty directory. 0 or DISK_ERR_* (DISK_ERR_NOT_EMPTY).'},
    'diskExists':  {'sig': 'diskExists(path) -> int', 'desc': '0 nothing there, 1 a file, 2 a directory, or DISK_ERR_*.'},
    'diskList':    {'sig': 'diskList(dir, index) -> string', 'desc': 'Name of entry `index` of `dir` (a directory name ends in "/"); "" past the last.'},
    'fileWrite':   {'sig': 'fileWrite(handle, byte) -> int', 'desc': 'Write the low byte to a diskOpen file. 1 or DISK_ERR_*.'},
    'fileWriteStr': {'sig': 'fileWriteStr(handle, str) -> int', 'desc': 'Write the bytes of a string to a diskOpen file. Bytes written or DISK_ERR_*.'},
    'fileWriteArr': {'sig': 'fileWriteArr(handle, arr, count) -> int', 'desc': 'Write the low byte of each of arr[0..count]. Bytes written or DISK_ERR_*.'},
    'fileReadArr': {'sig': 'fileReadArr(handle, arr, count) -> int', 'desc': 'Read up to `count` bytes into arr[0..] (diskOpen or fileOpen handles). Bytes read, 0 at the end.'},
    'onGesture':   {'sig': 'onGesture(widget, mask, @handler)', 'desc': 'Flutter-style gesture recognizers (caps.gestures, lib/gesture.fl): the widget takes part in the GESTURE_* recognizers of `mask`, which compete in an arena per touch and report to handler(widget, event) with GE_* events. mask 0 removes them.'},
    'gestureInfo': {'sig': 'gestureInfo(field) -> int', 'desc': 'Inside a gesture handler: a detail (GI_*) of the event it was called for (position, deltas, velocity, scale x1000, rotation in milliradians...). 0 elsewhere.'},
}

PROP_DOCS = {
    'layer': 'Own layer (composited surface) for the widget and its subtree: 0/1',
    'layer_alpha': "Opacity of the widget's layer, 0-255",
    'loc_x': 'X position (scalar)',
    'loc_y': 'Y position (scalar)',
    'size_w': 'Width (scalar)',
    'width': 'Width (alias for size_w)',
    'size_h': 'Height (scalar)',
    'height': 'Height (alias for size_h)',
    'visible': 'Visibility flag: 0 = hidden, 1 = visible',
    'enabled': 'Enabled flag: 0 = disabled, 1 = enabled',
    'clickable': 'Clickable flag: 0 = not clickable, 1 = clickable',
    'bg_color': 'Background color (RGB565)',
    'background_color': 'Background color (alias for bg_color)',
    'border_color': 'Border color (RGB565)',
    'flags': 'Widget flags bitmask',
    'location': 'Location compound: {x, y}',
    'pos': 'Position (alias for location)',
    'size': 'Size compound: {w, h}',
    'border': 'Border widths compound: {top, right, bottom, left}',
    'padding': 'Padding compound: {top, right, bottom, left}',
    'border_top': 'Top border width',
    'border_right': 'Right border width',
    'border_bottom': 'Bottom border width',
    'border_left': 'Left border width',
    'padding_top': 'Top padding',
    'padding_right': 'Right padding',
    'padding_bottom': 'Bottom padding',
    'padding_left': 'Left padding',
    'kind': 'Widget kind ID',
    'text_color': 'Text/foreground color (RGB565)',
    'font_id': 'Font resource ID',
    'text_align': 'Text alignment (bitmask: H=0-2, V=0-8)',
    'press_color': 'Press highlight color (RGB565)',
    'image_id': 'Image resource ID',
    'on_click': 'Click event handler (assigned with @handler)',
    'on_paint': 'Paint event handler',
    'on_tap': 'Tap/change event handler',
    'border_radius': 'Border corner radius in pixels',
    'radius': 'Border radius (alias for border_radius)',
    'value': 'Current value (sliders, etc.)',
    'checked': 'Checkbox checked state',
    'max_length': 'Maximum text length for text input',
    'cursor_pos': 'Cursor position in text input',
    'on_change': 'Change event handler (alias for on_tap)',
    'scroll_y': 'Vertical scroll offset',
    'clip_children': 'Clip children to widget bounds',
    'gradient_color': 'End color for background gradient',
    'gradient_dir': 'Gradient direction: 0=none, 1=horizontal, 2=vertical',
    'graph_arr': 'Graph data array ID',
    'graph_array': 'Graph data array (alias)',
    'graph_count': 'Max graph samples to draw',
    'graph_flags': 'Graph render flags (bit 0: linear/spline, bit 1: fill)',
    'multi_line': 'Multi-line label text flag',
    'rich_text': 'Text as HTML: <b> <i> <u> <font color="#RRGGBB"> <br> &lt; ... (caps.rich_text)',
    'alpha': 'Background opacity (0-255)',
    'group_id': 'Exclusive group (button group, radio): one checked per group_id; radio 0 = siblings',
    'flex': 'Flex container mode for lib/flex.fl: FLEX_ROW/FLEX_COLUMN | JUSTIFY_* | ALIGN_*',
    'flex_gap': 'Flex container: gap between children in px (lib/flex.fl)',
    'flex_grow': 'Flex child: share of the leftover main-axis space, 0 = fixed size (lib/flex.fl)',
    'parent': 'Parent widget id, read-only (-1 = none)',
    'first_child': 'First child widget id, read-only (-1 = none)',
    'next_sibling': 'Next sibling widget id, read-only (-1 = none)',
    'text': 'Text content (compound: LEN-encoded)',
    'text_id': 'Text content (alias for text)',
}

CALLBACK_DOCS = {
    'setup': {'sig': 'fn setup()', 'desc': 'Called once at program start.'},
    'loop': {'sig': 'fn loop()', 'desc': 'Called repeatedly in the main event loop.'},
    'on_program_start': {'sig': 'fn on_program_start()', 'desc': 'Called when the program begins execution.'},
    'on_page_changing': {'sig': 'fn on_page_changing(from_page, to_page)', 'desc': 'Called before a page transition.'},
    'on_page_changed': {'sig': 'fn on_page_changed(new_page)', 'desc': 'Called after a page transition completes.'},
    'on_user_message': {'sig': 'fn on_user_message(data)', 'desc': 'Called when a USART user message is received.'},
    'on_touch_down': {'sig': 'fn on_touch_down(x, y)', 'desc': 'Called on touch press. Coordinates are packed.'},
    'on_touch_up': {'sig': 'fn on_touch_up()', 'desc': 'Called on touch release.'},
    'on_touch_move': {'sig': 'fn on_touch_move(x, y)', 'desc': 'Called on touch drag. Coordinates are packed.'},
    'on_task_message': {'sig': 'fn on_task_message(from, a, b)', 'desc': 'Task `from` called sendMessage(this task, a, b).'},
    'on_task_exit': {'sig': 'fn on_task_exit(task, code)', 'desc': 'A task this one spawned ended: code 0 = exited or killed, > 0 = VM fault code, -1 = could not start. Without this handler a fault opens the built-in "Program stopped" dialog.'},
}
