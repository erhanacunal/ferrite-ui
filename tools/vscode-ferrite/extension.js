"use strict";

const vscode = require("vscode");
const { LanguageClient } = require("vscode-languageclient/node");
const path = require("path");
const fs = require("fs");

const SERVER = path.join("tools", "ferrite_lsp.py");

let client = null;
let output = null;
let trace = null;

/**
 * Resolve the path to ferrite_lsp.py. Checks, first hit wins:
 *   1. setting ferrite.server.path
 *   2. <workspace folder>/tools/ferrite_lsp.py (the ferrite-ui repo is open)
 *   3. <workspace folder>/../ferrite-ui/tools/ferrite_lsp.py (a sibling checkout,
 *      e.g. ferrite-ui-xl, which uses ferrite-ui's tools/)
 *   4. $FERRITE_UI/tools/ferrite_lsp.py
 *   5. any parent dir of an open .fl file that has tools/ferrite_lsp.py
 *   6. ../ferrite_lsp.py relative to this extension (run from the repo)
 * Returns { path, tried } with path = null when none exists.
 */
function resolveServerPath() {
    const tried = [];
    const check = (p) => {
        if (!p) return null;
        tried.push(p);
        return fs.existsSync(p) ? p : null;
    };

    const configured = vscode.workspace.getConfiguration("ferrite.server").get("path", "");
    let hit = check(configured);
    if (hit) return { path: hit, tried };

    const folders = (vscode.workspace.workspaceFolders || []).map((ws) => ws.uri.fsPath);
    for (const dir of folders) {
        if ((hit = check(path.join(dir, SERVER)))) return { path: hit, tried };
    }
    for (const dir of folders) {
        if ((hit = check(path.join(dir, "..", "ferrite-ui", SERVER)))) return { path: hit, tried };
    }
    if (process.env.FERRITE_UI && (hit = check(path.join(process.env.FERRITE_UI, SERVER)))) {
        return { path: hit, tried };
    }
    for (const doc of vscode.workspace.textDocuments) {
        if (doc.uri.scheme !== "file" || !doc.fileName.toLowerCase().endsWith(".fl")) continue;
        let dir = path.dirname(doc.fileName);
        for (;;) {
            if ((hit = check(path.join(dir, SERVER)))) return { path: hit, tried };
            const parent = path.dirname(dir);
            if (parent === dir) break;
            dir = parent;
        }
    }
    if ((hit = check(path.join(__dirname, "..", "ferrite_lsp.py")))) return { path: hit, tried };
    return { path: null, tried };
}

async function startClient() {
    const config = vscode.workspace.getConfiguration("ferrite.server");
    const { path: serverPath, tried } = resolveServerPath();
    if (!serverPath) {
        output.appendLine("ferrite_lsp.py not found. Tried:");
        tried.forEach((p) => output.appendLine("  " + p));
        const pick = await vscode.window.showErrorMessage(
            "Ferrite: ferrite_lsp.py not found. Set 'ferrite.server.path' (or FERRITE_UI) to the ferrite-ui " +
            "checkout, then run 'Ferrite: Restart Language Server'. Highlighting and snippets still work.",
            "Open Settings", "Show Log");
        if (pick === "Open Settings") {
            vscode.commands.executeCommand("workbench.action.openSettings", "ferrite.server.path");
        } else if (pick === "Show Log") {
            output.show();
        }
        return;
    }

    const pythonCommand = config.get("python", "python");
    output.appendLine(`Starting: ${pythonCommand} ${serverPath}`);
    client = new LanguageClient(
        "ferrite-lsp",
        "Ferrite Language Server",
        {
            command: pythonCommand,
            args: [serverPath],
            options: { cwd: path.dirname(serverPath), env: Object.assign({}, process.env) },
        },
        {
            documentSelector: [{ scheme: "file", language: "ferrite" }],
            synchronize: { fileEvents: vscode.workspace.createFileSystemWatcher("**/*.fl") },
            initializationOptions: { includeDirs: config.get("includeDirs", []) },
            outputChannel: output,
            traceOutputChannel: trace,
        }
    );
    try {
        await client.start();
        output.appendLine("Server started.");
    } catch (err) {
        client = null;
        output.appendLine("Server failed to start: " + err);
        output.show(true);
        vscode.window.showErrorMessage(
            `Ferrite: language server failed to start (${pythonCommand} ${serverPath}). See the output channel; ` +
            "check 'ferrite.server.python'.");
    }
}

async function stopClient() {
    if (client) {
        const c = client;
        client = null;
        try {
            await c.stop();
        } catch (_) {
            // already dead
        }
    }
}

function activate(context) {
    output = vscode.window.createOutputChannel("Ferrite Language Server");
    trace = vscode.window.createOutputChannel("Ferrite LSP Trace");
    context.subscriptions.push(output, trace);

    // Registered before the server is looked up, so it exists even when the
    // server is missing (restart = look it up again).
    context.subscriptions.push(
        vscode.commands.registerCommand("ferrite.restartServer", async () => {
            await stopClient();
            await startClient();
            if (client) {
                vscode.window.showInformationMessage("Ferrite language server restarted.");
            }
        })
    );
    context.subscriptions.push(
        vscode.workspace.onDidChangeConfiguration(async (e) => {
            if (e.affectsConfiguration("ferrite.server")) {
                await stopClient();
                await startClient();
            }
        })
    );

    return startClient();
}

function deactivate() {
    return stopClient();
}

module.exports = { activate, deactivate };
