# Ferrite Language Support (VS Code)

Editor support for ferrite-lang (`.fl`) files:

- syntax highlighting (TextMate grammar + semantic tokens), snippets, bracket/comment config
- diagnostics as you type: the file is compiled the way a build compiles it
  (include dirs and device caps from the nearest `project.json`)
- completion, hover, signature help, go to definition, document outline

The language features come from `tools/ferrite_lsp.py` in the ferrite-ui repo, which wraps
`tools/ferrite_analysis.py` (the same code help the designer's editor uses). The extension
is only the client; it needs Python 3 and a ferrite-ui checkout.

## Install

```
cd tools/vscode-ferrite
npm install
npm run package                    # -> ferrite-lang-<version>.vsix
code --install-extension ferrite-lang-1.0.1.vsix
```

For development, open `tools/vscode-ferrite` in VS Code and press F5 (Extension Development Host).

## Finding the server

`ferrite_lsp.py` is looked up in this order:

1. setting `ferrite.server.path`
2. `<workspace folder>/tools/ferrite_lsp.py` (the ferrite-ui repo is open)
3. `<workspace folder>/../ferrite-ui/tools/ferrite_lsp.py` (a sibling checkout, e.g. ferrite-ui-xl is open)
4. `$FERRITE_UI/tools/ferrite_lsp.py`
5. a parent dir of an open `.fl` file that has `tools/ferrite_lsp.py`
6. next to the extension (when it runs from the repo)

If none is found, the paths tried go to the *Ferrite Language Server* output channel; set
`ferrite.server.path` (the server restarts on settings changes) or `FERRITE_UI`.

## Settings

| Setting | Default | |
|---|---|---|
| `ferrite.server.python` | `python` | interpreter that runs the server |
| `ferrite.server.path` | | path to `ferrite_lsp.py` |
| `ferrite.server.includeDirs` | `[]` | extra include dirs, after the `project.json` ones |

Command: **Ferrite: Restart Language Server** (after changing settings or the compiler).
Server output: the *Ferrite Language Server* output channel.
