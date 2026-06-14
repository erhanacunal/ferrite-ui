"use strict";

const vscode = require("vscode");
const { LanguageClient, TransportKind, ServerOptions } = require("vscode-languageclient/node");
const path = require("path");
const fs = require("fs");

let client;

/**
 * Resolve the path to ferrite_lsp.py.
 * Checks:
 *   1. User setting ferrite.server.path
 *   2. tools/ferrite_lsp.py relative to workspace root(s)
 *   3. tools/ferrite_lsp.py relative to this extension
 */
function resolveServerPath() {
    const config = vscode.workspace.getConfiguration("ferrite.server");
    const configured = config.get("path", "");
    if (configured && fs.existsSync(configured)) {
        return configured;
    }

    // Look for ferrite_lsp.py in workspace tools/ directories
    const workspaces = vscode.workspace.workspaceFolders || [];
    for (const ws of workspaces) {
        const candidate = path.join(ws.uri.fsPath, "tools", "ferrite_lsp.py");
        if (fs.existsSync(candidate)) {
            return candidate;
        }
    }

    // Fallback: try relative to this extension
    const extDir = path.dirname(__filename);
    // extension is in vscode-ferrite/; tools/ is the parent
    const sibling = path.join(extDir, "..", "ferrite_lsp.py");
    if (fs.existsSync(sibling)) {
        return sibling;
    }

    // Last resort: just use the filename and hope it's in PATH
    return "ferrite_lsp.py";
}

function activate(context) {
    const pythonCommand = vscode.workspace.getConfiguration("ferrite.server").get("python", "python");
    const serverPath = resolveServerPath();

    const serverOptions = {
        command: pythonCommand,
        args: [serverPath],
        options: {
            env: Object.assign({}, process.env),
        },
    };

    const clientOptions = {
        documentSelector: [{ scheme: "file", language: "ferrite" }],
        synchronize: {
            fileEvents: vscode.workspace.createFileSystemWatcher("**/*.fl"),
        },
        outputChannelName: "Ferrite Language Server",
        traceOutputChannel: vscode.window.createOutputChannel("Ferrite LSP Trace"),
    };

    client = new LanguageClient(
        "ferrite-lsp",
        "Ferrite Language Server",
        serverOptions,
        clientOptions
    );

    client.start().then(() => {
        console.log("Ferrite LSP client started");
    });

    // Register restart command
    context.subscriptions.push(
        vscode.commands.registerCommand("ferrite.restartServer", async () => {
            if (client) {
                await client.restart();
                vscode.window.showInformationMessage("Ferrite LSP server restarted.");
            }
        })
    );
}

function deactivate() {
    if (client) {
        return client.stop();
    }
}

module.exports = { activate, deactivate };
