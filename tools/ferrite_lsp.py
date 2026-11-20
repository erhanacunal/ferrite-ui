#!/usr/bin/env python3
"""Ferrite Language Server Protocol implementation.

Zero-dependency LSP server for the ferrite-lang (.fl) C-like language.
Communicates via JSON-RPC 2.0 over stdin/stdout.

Usage:
    python ferrite_lsp.py

Features (the analysis is ferrite_analysis.Document, shared with the
designer's code editor):
    - Diagnostics (compile errors as you type, with the project's include
      dirs and device caps from the nearest project.json)
    - Completions (locals, globals, functions, classes and their members,
      widget constructors and their named arguments, properties, builtins)
    - Hover info (declarations with their comments, builtins, properties)
    - Signature help
    - Go-to-definition (functions, classes, members, widgets, globals, locals,
      also into included files)
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
        self.analyzers = {}  # uri -> ferrite_analysis.Document
        self.extra_includes = []  # initializationOptions.includeDirs (client setting)

    def _analyzer(self, uri):
        a = self.analyzers.get(uri)
        if a is None:
            path = _uri_to_path(uri)
            dirs, superops, native_flex = project_settings(path)
            dirs = list(dirs) + [d for d in self.extra_includes if d not in dirs]
            a = self.analyzers[uri] = Document(path, dirs, superops, native_flex)
        return a

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
        elif method == 'textDocument/signatureHelp':
            return self._signature_help(msg_id, msg.get('params', {}))
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
        opts = params.get('initializationOptions') or {}
        self.extra_includes = [d for d in opts.get('includeDirs', []) if isinstance(d, str) and d]
        return self._respond(msg_id, {
            'capabilities': {
                'textDocumentSync': {
                    'openClose': True,
                    'change': 1,  # full sync
                    'save': True,
                },
                'completionProvider': {
                    'triggerCharacters': ['.', '@', '"', '(', ','],
                },
                'signatureHelpProvider': {
                    'triggerCharacters': ['(', ','],
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
        self.analyzers.pop(uri, None)

    def _did_save(self, params):
        uri = params['textDocument']['uri']
        if uri in self.documents:
            self._publish_diagnostics(uri)

    # ---- diagnostics ----

    def _publish_diagnostics(self, uri):
        doc = self.documents.get(uri)
        if not doc:
            return
        diagnostics = [{
            'range': {
                'start': {'line': d.line, 'character': 0},
                'end': {'line': d.line, 'character': 1000},
            },
            'severity': 1,  # Error
            'source': 'ferrite',
            'message': d.message,
        } for d in self._analyzer(uri).check(doc['text'])]
        self._notify('textDocument/publishDiagnostics', {
            'uri': uri,
            'diagnostics': diagnostics,
        })

    def _position(self, params):
        """(analyzer, text, line, character) of a request, or None."""
        uri = params['textDocument']['uri']
        doc = self.documents.get(uri)
        if not doc:
            return None
        pos = params['position']
        return self._analyzer(uri), doc['text'], pos['line'], pos['character']

    # ---- completions ----

    def _completion(self, msg_id, params):
        at = self._position(params)
        if at is None:
            return self._respond(msg_id, [])
        a, text, line, char = at
        items = []
        for c in a.complete(text, line, char):
            item = {'label': c.label, 'kind': _COMPLETION_KINDS.get(c.kind, 1)}
            if c.detail:
                item['detail'] = c.detail
            if c.doc:
                item['documentation'] = c.doc
            if c.insert is not None:
                item['insertText'] = c.insert
            items.append(item)
        return self._respond(msg_id, {'isIncomplete': False, 'items': items})

    # ---- hover ----

    def _hover(self, msg_id, params):
        at = self._position(params)
        text = at and at[0].hover(*at[1:])
        if not text:
            return self._respond(msg_id, None)
        head, _, rest = text.partition('\n\n')
        value = f"```ferrite\n{head}\n```" + (f"\n\n{rest}" if rest else '')
        return self._respond(msg_id, {'contents': {'kind': 'markdown', 'value': value}})

    # ---- signature help ----

    def _signature_help(self, msg_id, params):
        at = self._position(params)
        sig = at and at[0].signature(*at[1:])
        if not sig:
            return self._respond(msg_id, None)
        return self._respond(msg_id, {
            'signatures': [{
                'label': sig.label,
                'documentation': sig.doc,
                'parameters': [{'label': p} for p in sig.params],
            }],
            'activeSignature': 0,
            'activeParameter': max(sig.active, 0),
        })

    # ---- go-to-definition ----

    def _definition(self, msg_id, params):
        at = self._position(params)
        loc = at and at[0].definition(*at[1:])
        if not loc:
            return self._respond(msg_id, None)
        return self._respond(msg_id, {
            'uri': _path_to_uri(loc.path),
            'range': {
                'start': {'line': loc.line, 'character': 0},
                'end': {'line': loc.line, 'character': 0},
            },
        })

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


# Keyword / builtin / property data lives in ferrite_analysis (shared with
# the designer's editor).
from ferrite_analysis import (  # noqa: E402
    KEYWORD_DOCS, BUILTIN_SIGNATURES, PROP_DOCS, CALLBACK_DOCS, Document, project_settings)


_COMPLETION_KINDS = {
    'method': 2, 'function': 3, 'widget': 4, 'field': 5, 'variable': 6, 'class': 7,
    'property': 10, 'keyword': 14, 'snippet': 15, 'file': 17, 'constant': 21, 'type': 22,
}


# ============================================================
# Helpers
# ============================================================

def _path_to_uri(path):
    """Local path → file:// URI."""
    path = os.path.abspath(path).replace('\\', '/')
    if not path.startswith('/'):
        path = '/' + path  # C:/x → /C:/x
    return 'file://' + path


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
