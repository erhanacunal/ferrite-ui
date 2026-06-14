#!/usr/bin/env python3
"""Ferrite Language Server Protocol implementation.

Zero-dependency LSP server for the ferrite-lang (.fl) C-like language.
Communicates via JSON-RPC 2.0 over stdin/stdout.

Usage:
    python ferrite_lsp.py

Features:
    - Diagnostics (compile errors on open/save)
    - Completions (keywords, builtins, widget properties, types)
    - Hover info (builtin signatures, property descriptions)
    - Go-to-definition (functions, @callback references)
    - Document symbols (function outline)
    - Semantic tokens (syntax highlighting)
"""

import sys
import os
import json
import re
import traceback

# ---- Ensure cwd is the tools directory so ferrite_lang imports work ----
_TOOLS_DIR = os.path.dirname(os.path.abspath(__file__))
if _TOOLS_DIR not in sys.path:
    sys.path.insert(0, _TOOLS_DIR)


# ============================================================
# LSP message framing helpers
# ============================================================

def _read_message():
    """Read one JSON-RPC message from stdin (Content-Length header + body)."""
    headers = {}
    while True:
        line = sys.stdin.buffer.readline().decode('utf-8', errors='replace')
        if not line:
            return None
        line = line.rstrip('\r\n')
        if line == '':
            break
        if ':' in line:
            key, val = line.split(':', 1)
            headers[key.strip().lower()] = val.strip()
    length = int(headers.get('content-length', 0))
    if length == 0:
        return None
    body = sys.stdin.buffer.read(length).decode('utf-8', errors='replace')
    return json.loads(body)


def _send_message(msg):
    """Send one JSON-RPC message to stdout."""
    body = json.dumps(msg, ensure_ascii=False)
    sys.stdout.buffer.write(
        f'Content-Length: {len(body.encode("utf-8"))}\r\n\r\n{body}'.encode('utf-8')
    )
    sys.stdout.buffer.flush()


# ============================================================
# Server state
# ============================================================

class Server:
    def __init__(self):
        self.documents = {}  # uri -> {"text": str, "version": int}

    def handle(self, msg):
        method = msg.get('method', '')
        msg_id = msg.get('id')

        if method == 'initialize':
            return self._initialize(msg_id, msg.get('params', {}))
        elif method == 'initialized':
            return None  # no response needed
        elif method == 'shutdown':
            return self._respond(msg_id, None)
        elif method == 'exit':
            sys.exit(0)
        elif method == 'textDocument/didOpen':
            self._did_open(msg.get('params', {}))
            return None
        elif method == 'textDocument/didChange':
            self._did_change(msg.get('params', {}))
            return None
        elif method == 'textDocument/didClose':
            self._did_close(msg.get('params', {}))
            return None
        elif method == 'textDocument/didSave':
            self._did_save(msg.get('params', {}))
            return None
        elif method == 'textDocument/completion':
            return self._completion(msg_id, msg.get('params', {}))
        elif method == 'textDocument/hover':
            return self._hover(msg_id, msg.get('params', {}))
        elif method == 'textDocument/definition':
            return self._definition(msg_id, msg.get('params', {}))
        elif method == 'textDocument/documentSymbol':
            return self._document_symbol(msg_id, msg.get('params', {}))
        elif method == 'textDocument/semanticTokens/full':
            return self._semantic_tokens(msg_id, msg.get('params', {}))
        elif msg_id is not None:
            return self._respond(msg_id, None)
        return None

    def _respond(self, msg_id, result):
        return {'jsonrpc': '2.0', 'id': msg_id, 'result': result}

    def _notify(self, method, params):
        _send_message({'jsonrpc': '2.0', 'method': method, 'params': params})

    # ---- initialize ----

    def _initialize(self, msg_id, params):
        caps = params.get('capabilities', {})
        return self._respond(msg_id, {
            'capabilities': {
                'textDocumentSync': {
                    'openClose': True,
                    'change': 1,  # full sync
                    'save': True,
                },
                'completionProvider': {
                    'triggerCharacters': ['.', '@', '"'],
                },
                'hoverProvider': True,
                'definitionProvider': True,
                'documentSymbolProvider': True,
                'semanticTokensProvider': {
                    'legend': {
                        'tokenTypes': [
                            'keyword', 'function', 'variable', 'parameter',
                            'type', 'number', 'string', 'comment',
                            'operator', 'property', 'macro', 'decorator',
                        ],
                        'tokenModifiers': ['declaration', 'definition', 'readonly', 'defaultLibrary'],
                    },
                    'full': True,
                },
            },
            'serverInfo': {
                'name': 'ferrite-lsp',
                'version': '1.0.0',
            },
        })

    # ---- document sync ----

    def _did_open(self, params):
        uri = params['textDocument']['uri']
        self.documents[uri] = {
            'text': params['textDocument']['text'],
            'version': params['textDocument']['version'],
        }
        self._publish_diagnostics(uri)

    def _did_change(self, params):
        uri = params['textDocument']['uri']
        # full sync — contentChanges[0].text is the full document
        changes = params.get('contentChanges', [])
        if changes:
            self.documents[uri] = {
                'text': changes[0]['text'],
                'version': params.get('textDocument', {}).get('version', 0),
            }
        self._publish_diagnostics(uri)

    def _did_close(self, params):
        uri = params['textDocument']['uri']
        self.documents.pop(uri, None)

    def _did_save(self, params):
        uri = params['textDocument']['uri']
        if uri in self.documents:
            self._publish_diagnostics(uri)

    # ---- diagnostics ----

    def _publish_diagnostics(self, uri):
        doc = self.documents.get(uri)
        if not doc:
            return
        diagnostics = []
        try:
            from ferrite_lang import compile
            compile(doc['text'], filename=_uri_to_path(uri))
        except Exception as e:
            line = getattr(e, 'line', 0)
            msg = str(e)
            # Parse "line N: message" format from CompileError
            if not line and msg.startswith('line '):
                try:
                    parts = msg.split(': ', 1)
                    line = int(parts[0].split(' ')[1])
                    msg = parts[1] if len(parts) > 1 else msg
                except (ValueError, IndexError):
                    pass
            diagnostics.append({
                'range': {
                    'start': {'line': max(0, line - 1), 'character': 0},
                    'end': {'line': max(0, line - 1), 'character': 0},
                },
                'severity': 1,  # Error
                'source': 'ferrite',
                'message': msg,
            })
        self._notify('textDocument/publishDiagnostics', {
            'uri': uri,
            'diagnostics': diagnostics,
        })

    # ---- completions ----

    def _completion(self, msg_id, params):
        uri = params['textDocument']['uri']
        position = params['position']
        doc = self.documents.get(uri)
        if not doc:
            return self._respond(msg_id, [])

        text = doc['text']
        line = position['line']
        char = position['character']
        lines = text.split('\n')
        if line >= len(lines):
            return self._respond(msg_id, [])

        current_line = lines[line]
        prefix = current_line[:char]

        # Determine completion context
        items = []

        # After '.' → widget property completions
        dot_match = re.search(r'(\w+)\.(\w*)$', prefix)
        if dot_match:
            prop_prefix = dot_match.group(2).lower()
            items = _property_completions(prop_prefix)
            return self._respond(msg_id, {'isIncomplete': False, 'items': items})

        # After '@' → function reference completions
        at_match = re.search(r'@(\w*)$', prefix)
        if at_match:
            func_prefix = at_match.group(1)
            items = _func_ref_completions(func_prefix, doc['text'])
            return self._respond(msg_id, {'isIncomplete': False, 'items': items})

        # Inside #include "..." → file path completion
        inc_match = re.search(r'#include\s+"([^"]*)$', prefix)
        if inc_match:
            file_prefix = inc_match.group(1)
            items = _include_completions(file_prefix, uri)
            return self._respond(msg_id, {'isIncomplete': False, 'items': items})

        # General context: keywords + builtins + types
        word_match = re.search(r'(\w*)$', prefix)
        word_prefix = word_match.group(1) if word_match else ''
        items = _keyword_completions(word_prefix)
        items += _builtin_completions(word_prefix)
        items += _type_completions(word_prefix)

        return self._respond(msg_id, {'isIncomplete': False, 'items': items})

    # ---- hover ----

    def _hover(self, msg_id, params):
        uri = params['textDocument']['uri']
        position = params['position']
        doc = self.documents.get(uri)
        if not doc:
            return self._respond(msg_id, None)

        text = doc['text']
        line = position['line']
        char = position['character']
        lines = text.split('\n')
        if line >= len(lines):
            return self._respond(msg_id, None)

        current_line = lines[line]

        # Find the word at the cursor position
        word, word_start, word_end = _word_at(current_line, char)
        if not word:
            return self._respond(msg_id, None)

        # Check if it's a builtin
        builtin_info = BUILTIN_SIGNATURES.get(word)
        if builtin_info:
            markdown = f"### `{word}` — built-in function\n\n```ferrite\n{builtin_info['sig']}\n```\n\n{builtin_info['desc']}"
            return self._respond(msg_id, {
                'contents': {'kind': 'markdown', 'value': markdown},
            })

        # Check if it's a keyword
        keyword_info = KEYWORD_DOCS.get(word)
        if keyword_info:
            return self._respond(msg_id, {
                'contents': {'kind': 'markdown', 'value': f'### `{word}` — keyword\n\n{keyword_info}'},
            })

        # Check if it's a property (inside dot access context)
        prop_info = PROP_DOCS.get(word.lower())
        if prop_info:
            return self._respond(msg_id, {
                'contents': {'kind': 'markdown', 'value': f'### `.`{word} — widget property\n\n{prop_info}'},
            })

        # Check if it's a callback
        callback_info = CALLBACK_DOCS.get(word)
        if callback_info:
            return self._respond(msg_id, {
                'contents': {'kind': 'markdown', 'value': f'### `{word}` — system callback\n\n```ferrite\n{callback_info["sig"]}\n```\n\n{callback_info["desc"]}'},
            })

        return self._respond(msg_id, None)

    # ---- go-to-definition ----

    def _definition(self, msg_id, params):
        uri = params['textDocument']['uri']
        position = params['position']
        doc = self.documents.get(uri)
        if not doc:
            return self._respond(msg_id, None)

        text = doc['text']
        line = position['line']
        char = position['character']
        lines = text.split('\n')
        if line >= len(lines):
            return self._respond(msg_id, None)

        current_line = lines[line]
        word, _, _ = _word_at(current_line, char)
        if not word:
            return self._respond(msg_id, None)

        # Look for function definition: fn word(
        fn_pattern = re.compile(r'\bfn\s+' + re.escape(word) + r'\s*\(', re.MULTILINE)
        m = fn_pattern.search(text)
        if m:
            before = text[:m.start()]
            def_line = before.count('\n')
            return self._respond(msg_id, {
                'uri': uri,
                'range': {
                    'start': {'line': def_line, 'character': 0},
                    'end': {'line': def_line, 'character': 0},
                },
            })

        # Look for @word function reference → jump to fn word
        at_fn_pattern = re.compile(r'\bfn\s+' + re.escape(word) + r'\s*\(', re.MULTILINE)
        m = at_fn_pattern.search(text)
        if m:
            before = text[:m.start()]
            def_line = before.count('\n')
            return self._respond(msg_id, {
                'uri': uri,
                'range': {
                    'start': {'line': def_line, 'character': 0},
                    'end': {'line': def_line, 'character': 0},
                },
            })

        return self._respond(msg_id, None)

    # ---- document symbols ----

    def _document_symbol(self, msg_id, params):
        uri = params['textDocument']['uri']
        doc = self.documents.get(uri)
        if not doc:
            return self._respond(msg_id, [])

        text = doc['text']
        symbols = []
        # Match fn name(params) { ... }
        pattern = re.compile(
            r'^\s*fn\s+(\w+)\s*\(([^)]*)\)\s*(?:->\s*(\w+)\s*)?',
            re.MULTILINE
        )
        for m in pattern.finditer(text):
            name = m.group(1)
            params_str = m.group(2).strip()
            return_type = m.group(3) or 'int'
            before = text[:m.start()]
            fn_line = before.count('\n')
            symbols.append({
                'name': name,
                'kind': 12,  # Function
                'range': {
                    'start': {'line': fn_line, 'character': 0},
                    'end': {'line': fn_line, 'character': 0},
                },
                'selectionRange': {
                    'start': {'line': fn_line, 'character': m.start() - before.rfind('\n') - 1 if '\n' in before else m.start()},
                    'end': {'line': fn_line, 'character': 0},
                },
                'detail': f'fn {name}({params_str}) -> {return_type}',
            })
        return self._respond(msg_id, symbols)

    # ---- semantic tokens ----

    _TOKEN_TYPES = {
        'keyword': 0,
        'function': 1,
        'variable': 2,
        'parameter': 3,
        'type': 4,
        'number': 5,
        'string': 6,
        'comment': 7,
        'operator': 8,
        'property': 9,
        'macro': 10,
        'decorator': 11,
    }
    _TOKEN_MODIFIERS = {
        'declaration': 0,
        'definition': 1,
        'readonly': 2,
        'defaultLibrary': 3,
    }

    def _semantic_tokens(self, msg_id, params):
        uri = params['textDocument']['uri']
        doc = self.documents.get(uri)
        if not doc:
            return self._respond(msg_id, {'data': []})

        data = _tokenize_semantic(doc['text'], self._TOKEN_TYPES, self._TOKEN_MODIFIERS)
        return self._respond(msg_id, {'data': data})


# ============================================================
# Keyword / Builtin / Property data
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
    'showModal':   {'sig': 'showModal(builderFn, [overlayClickFn]) -> int', 'desc': 'Show a modal dialog. Suspends VM until setDialogResult.'},
    'setDialogResult': {'sig': 'setDialogResult(result)', 'desc': 'Set result value on innermost modal dialog frame.'},
}

PROP_DOCS = {
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
    'margin': 'Margin compound: {top, right, bottom, left}',
    'border': 'Border widths compound: {top, right, bottom, left}',
    'padding': 'Padding compound: {top, right, bottom, left}',
    'margin_top': 'Top margin',
    'margin_right': 'Right margin',
    'margin_bottom': 'Bottom margin',
    'margin_left': 'Left margin',
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
    'alpha': 'Background opacity (0-255)',
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
}

# ============================================================
# Completion helpers
# ============================================================

_COMPLETION_ITEM = 1  # Text
_COMPLETION_FUNCTION = 3  # Function
_COMPLETION_PROPERTY = 10  # Property
_COMPLETION_KEYWORD = 14  # Keyword
_COMPLETION_SNIPPET = 15  # Snippet
_COMPLETION_TYPE = 13  # Type parameter


def _keyword_completions(prefix):
    items = []
    for kw in ['var', 'const', 'fn', 'if', 'else', 'while', 'for', 'return',
               'true', 'false', 'break', 'continue']:
        if kw.startswith(prefix):
            items.append({
                'label': kw,
                'kind': _COMPLETION_KEYWORD,
                'detail': 'keyword',
            })
    # Snippets for common constructs
    if 'fn'.startswith(prefix):
        items.append({
            'label': 'fn',
            'kind': _COMPLETION_SNIPPET,
            'detail': 'function definition',
            'insertText': 'fn ${1:name}(${2:params}) {\n    ${0}\n}',
            'insertTextFormat': 2,  # Snippet
        })
    if 'if'.startswith(prefix):
        items.append({
            'label': 'if',
            'kind': _COMPLETION_SNIPPET,
            'detail': 'if statement',
            'insertText': 'if (${1:cond}) {\n    ${0}\n}',
            'insertTextFormat': 2,
        })
    if 'while'.startswith(prefix):
        items.append({
            'label': 'while',
            'kind': _COMPLETION_SNIPPET,
            'detail': 'while loop',
            'insertText': 'while (${1:cond}) {\n    ${0}\n}',
            'insertTextFormat': 2,
        })
    if 'for'.startswith(prefix):
        items.append({
            'label': 'for',
            'kind': _COMPLETION_SNIPPET,
            'detail': 'for loop',
            'insertText': 'for (${1:var i = 0}; ${2:i < n}; ${3:i++}) {\n    ${0}\n}',
            'insertTextFormat': 2,
        })
    return items


def _builtin_completions(prefix):
    items = []
    for name, info in BUILTIN_SIGNATURES.items():
        if name.startswith(prefix):
            items.append({
                'label': name,
                'kind': _COMPLETION_FUNCTION,
                'detail': info['sig'],
                'documentation': info['desc'],
            })
    return items


def _type_completions(prefix):
    items = []
    for t in ['int', 'float', 'widget', 'string', 'array']:
        if t.startswith(prefix):
            items.append({
                'label': t,
                'kind': _COMPLETION_TYPE,
                'detail': 'type',
            })
    return items


def _property_completions(prefix):
    items = []
    for name, doc in PROP_DOCS.items():
        if name.startswith(prefix):
            items.append({
                'label': name,
                'kind': _COMPLETION_PROPERTY,
                'detail': 'widget property',
                'documentation': doc,
            })
    return items


def _func_ref_completions(prefix, text):
    """Complete @function references with functions defined in the document."""
    items = []
    # Find all fn definitions in the document
    fn_pattern = re.compile(r'^\s*fn\s+(\w+)\s*\(', re.MULTILINE)
    seen = set()
    for m in fn_pattern.finditer(text):
        name = m.group(1)
        if name.startswith(prefix) and name not in seen:
            seen.add(name)
            items.append({
                'label': name,
                'kind': _COMPLETION_FUNCTION,
                'detail': 'fn',
            })
    return items


def _include_completions(prefix, uri):
    """Complete #include paths with .fl files in the workspace."""
    items = []
    base_path = _uri_to_path(uri) if uri.startswith('file://') else ''
    base_dir = os.path.dirname(base_path) if base_path else _TOOLS_DIR
    search_dir = base_dir

    # If prefix has a directory component, resolve it
    if '/' in prefix or '\\' in prefix:
        candidate = os.path.normpath(os.path.join(search_dir, prefix))
        search_dir = os.path.dirname(candidate) if os.path.dirname(candidate) else search_dir
        file_prefix = os.path.basename(prefix)
    else:
        file_prefix = prefix

    try:
        for entry in os.listdir(search_dir):
            if entry.endswith('.fl') and entry.startswith(file_prefix):
                # Compute relative path
                full = os.path.join(search_dir, entry)
                try:
                    rel = os.path.relpath(full, base_dir) if base_dir else entry
                except ValueError:
                    rel = entry
                items.append({
                    'label': rel.replace('\\', '/'),
                    'kind': 17,  # File
                    'detail': 'include',
                })
    except OSError:
        pass
    return items


# ============================================================
# Helpers
# ============================================================

def _uri_to_path(uri):
    """Convert file:// URI to a local filesystem path."""
    if uri.startswith('file:///'):
        # Windows: file:///C%3A/path → C:/path
        path = uri[8:]
        if '%3A' in path.lower() or '%3a' in path.lower():
            # Drive letter is encoded
            path = path.replace('%3A', ':').replace('%3a', ':')
            path = path.lstrip('/')
            path = path[0].upper() + ':' + path[1:]
        elif len(path) >= 3 and path[0] == '/' and path[2] == ':':
            path = path[1:]  # /C:/ → C:/
        return path
    elif uri.startswith('file://'):
        return uri[7:]
    return uri


def _word_at(line, column):
    """Extract the identifier word at the given column position in a line.
    Returns (word, start_col, end_col) or (None, 0, 0).
    """
    if column < 0 or column > len(line):
        return None, 0, 0
    # Find word boundaries
    start = column
    while start > 0 and _is_ident_char(line[start - 1]):
        start -= 1
    end = column
    while end < len(line) and _is_ident_char(line[end]):
        end += 1
    if start < end:
        return line[start:end], start, end
    # Check if cursor is directly on a dot (for property hover)
    if column > 0 and line[column - 1] == '.' and column > 1 and _is_ident_char(line[column - 2]):
        pstart = column - 2
        while pstart > 0 and _is_ident_char(line[pstart - 1]):
            pstart -= 1
        return line[pstart:column - 1], pstart, column - 1
    return None, 0, 0


def _is_ident_char(ch):
    return ch.isalnum() or ch == '_'


# ============================================================
# Semantic tokenization
# ============================================================

# Token patterns for the ferrite language
_TOKEN_SPEC = [
    # Preprocessor directives
    ('COMMENT_LINE',  r'//[^\n]*'),
    ('COMMENT_BLOCK', r'/\*[\s\S]*?\*/'),
    # Strings
    ('STRING', r'"(?:\\.|[^"\\])*"'),
    # Numbers (float first to avoid ambiguity with int.operator)
    ('FLOAT',  r'\b\d+\.\d+(?:[eE][+-]?\d+)?\b'),
    ('HEX',    r'\b0[xX][0-9a-fA-F_]+'),
    ('BIN',    r'\b0[bB][01_]+'),
    ('NUMBER', r'\b\d[\d_]*\b'),
    # Keywords + types (checked after matching)
    ('IDENT',  r'[a-zA-Z_]\w*'),
    # Operators / punctuation
    ('OPERATOR', r'==|!=|<=|>=|&&|\|\||<<|>>|->|\+\+|--|\+=|-=|\*=|/=|%=|[+\-*/%&|!<>=]=?'),
    # Single-char punctuation
    ('PUNCT', r'[(){}\[\],;.:?@]'),
    # Whitespace / newlines
    ('NEWLINE', r'\n'),
    ('SKIP', r'[ \t\r]+'),
    ('PREPROC', r'#[a-zA-Z_]\w*'),
]

_TOKEN_RE = re.compile(
    '|'.join(f'(?P<{name}>{pattern})' for name, pattern in _TOKEN_SPEC)
)

_KEYWORD_SET = {'var', 'const', 'fn', 'if', 'else', 'while', 'for',
                'return', 'true', 'false', 'break', 'continue', 'not'}

_TYPE_SET = {'int', 'float', 'widget', 'string', 'array'}

_BUILTIN_SET = set(BUILTIN_SIGNATURES.keys())

_CALLBACK_SET = set(CALLBACK_DOCS.keys())

_PROP_SET = set(PROP_DOCS.keys())


def _tokenize_semantic(text, token_types, token_modifiers):
    """Produce semantic token delta-encoded data for the full document.

    Returns a list of integers in the standard LSP semantic tokens format:
    [deltaLine, deltaStart, length, tokenType, tokenModifiers] repeated.
    """
    data = []
    prev_line = 0
    prev_col = 0
    lines = text.split('\n')
    last_was_dot = False  # track if previous significant token was a dot

    for line_no, line_text in enumerate(lines):
        # Track position within the line
        col = 0
        # Reset per-line state
        prev_in_line = False
        while col < len(line_text):
            m = _TOKEN_RE.match(line_text, col)
            if not m:
                col += 1
                continue
            kind = m.lastgroup
            value = m.group()
            start_col = col
            length = len(value)
            col += length

            delta_line = line_no - prev_line
            delta_col = start_col - (prev_col if prev_line == line_no else 0)

            if kind == 'COMMENT_LINE' or kind == 'COMMENT_BLOCK':
                _push_token(data, delta_line, delta_col, length,
                            token_types['comment'], 0)
                prev_line = line_no
                prev_col = start_col
                last_was_dot = False
            elif kind == 'STRING':
                _push_token(data, delta_line, delta_col, length,
                            token_types['string'], 0)
                prev_line = line_no
                prev_col = start_col
                last_was_dot = False
            elif kind in ('FLOAT', 'HEX', 'BIN', 'NUMBER'):
                _push_token(data, delta_line, delta_col, length,
                            token_types['number'], 0)
                prev_line = line_no
                prev_col = start_col
                last_was_dot = False
            elif kind == 'IDENT':
                if value in _KEYWORD_SET:
                    _push_token(data, delta_line, delta_col, length,
                                token_types['keyword'], 0)
                elif value in _TYPE_SET:
                    _push_token(data, delta_line, delta_col, length,
                                token_types['type'], 0)
                elif last_was_dot and value.lower() in _PROP_SET:
                    # Property access after dot
                    _push_token(data, delta_line, delta_col, length,
                                token_types['property'], 0)
                elif value in _BUILTIN_SET:
                    _push_token(data, delta_line, delta_col, length,
                                token_types['function'],
                                token_modifiers['defaultLibrary'])
                else:
                    # Could be variable or user function — use variable as default
                    _push_token(data, delta_line, delta_col, length,
                                token_types['variable'], 0)
                prev_line = line_no
                prev_col = start_col
                last_was_dot = False
            elif kind == 'OPERATOR':
                _push_token(data, delta_line, delta_col, length,
                            token_types['operator'], 0)
                prev_line = line_no
                prev_col = start_col
                last_was_dot = False
            elif kind == 'PUNCT':
                if value == '@':
                    _push_token(data, delta_line, delta_col, length,
                                token_types['decorator'], 0)
                else:
                    _push_token(data, delta_line, delta_col, length,
                                token_types['operator'], 0)
                last_was_dot = (value == '.')
                prev_line = line_no
                prev_col = start_col
            elif kind == 'PREPROC':
                _push_token(data, delta_line, delta_col, length,
                            token_types['macro'], 0)
                prev_line = line_no
                prev_col = start_col
                last_was_dot = False
            elif kind == 'NEWLINE':
                last_was_dot = False
                # Don't update prev_col here; newline resets column tracking
            # SKIP: just consume, no token emitted

    return data


def _push_token(data, delta_line, delta_start, length, token_type, token_modifiers):
    data.extend([delta_line, delta_start, length, token_type, token_modifiers])


# ============================================================
# Main loop
# ============================================================

def main():
    server = Server()

    # Log to a file for debugging (only if env var is set)
    log_file = os.environ.get('FERRITE_LSP_LOG')
    if log_file:
        sys.stderr = open(log_file, 'w', encoding='utf-8')
        sys.stderr.write('ferrite-lsp started\n')
        sys.stderr.flush()

    try:
        while True:
            msg = _read_message()
            if msg is None:
                break
            try:
                response = server.handle(msg)
                if response is not None:
                    _send_message(response)
            except Exception:
                if log_file:
                    traceback.print_exc(file=sys.stderr)
                    sys.stderr.flush()
    except KeyboardInterrupt:
        pass
    except EOFError:
        pass


if __name__ == '__main__':
    main()
