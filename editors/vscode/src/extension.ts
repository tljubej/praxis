// The Praxis VS Code extension (§15.4, WS9).
//
// **Intentionally thin.** It registers `.px`, launches `praxis lsp`, exposes the
// four commands, and restarts the server. There is **no parsing and no type
// logic in TypeScript** (§15.4, §20 rule 3): everything the editor knows about
// a Praxis program comes from the compiler over the protocol, so the extension
// and the compiler cannot disagree about what a file means.
//
// The argv construction lives in `argv.ts`, which imports nothing from `vscode`
// so it can be tested without a host.

import * as fs from "node:fs";
import * as path from "node:path";
import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";

import type { PraxisSubcommand } from "./argv";
import { fileCommandArgv, serverArgv, terminalCommand } from "./argv";

/** The one language id, matching `package.json`'s `contributes.languages`. */
const LANGUAGE_ID = "praxis";

/** The terminal the run/check/watch commands share. §15.4 asks for an
 *  integrated terminal rather than an output channel because the crash REPL is
 *  interactive — an output channel cannot answer a prompt. */
const TERMINAL_NAME = "Praxis";

let client: LanguageClient | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  context.subscriptions.push(
    vscode.commands.registerCommand("praxis.runFile", () => runFileCommand("run")),
    vscode.commands.registerCommand("praxis.checkFile", () => runFileCommand("check")),
    vscode.commands.registerCommand("praxis.watchFile", () => runFileCommand("watch")),
    vscode.commands.registerCommand("praxis.restartServer", () => restartServer(context)),
  );

  // Restart when the configured binary changes: the server is a *process*, and
  // pointing at a different build has to relaunch it rather than reconfigure it.
  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration("praxis.binaryPath")) {
        void restartServer(context);
      }
    }),
  );

  await startServer(context);
}

export async function deactivate(): Promise<void> {
  await stopServer();
}

/** The configured path to the `praxis` binary. A bare name resolves on PATH. */
function binaryPath(): string {
  return vscode.workspace.getConfiguration("praxis").get<string>("binaryPath") || "praxis";
}

async function startServer(context: vscode.ExtensionContext): Promise<void> {
  const command = binaryPath();
  const serverOptions: ServerOptions = {
    run: { command, args: serverArgv(), transport: TransportKind.stdio },
    debug: { command, args: serverArgv(), transport: TransportKind.stdio },
  };
  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: LANGUAGE_ID }],
    synchronize: {
      fileEvents: vscode.workspace.createFileSystemWatcher("**/*.px"),
    },
  };

  client = new LanguageClient("praxis", "Praxis Language Server", serverOptions, clientOptions);
  try {
    await client.start();
    context.subscriptions.push(client);
  } catch (error) {
    // A missing binary is the common case and deserves the fix, not a stack
    // trace: the server is the local toolchain, and the setting that points at
    // it is the thing to change.
    void vscode.window.showErrorMessage(
      `Praxis: could not start \`${command} lsp\`. Set \`praxis.binaryPath\` to the Praxis binary. (${String(
        error,
      )})`,
    );
    client = undefined;
  }
}

async function stopServer(): Promise<void> {
  if (client) {
    await client.stop();
    client = undefined;
  }
}

async function restartServer(context: vscode.ExtensionContext): Promise<void> {
  await stopServer();
  await startServer(context);
}

/**
 * Run one of the file commands against the active editor's document.
 *
 * The document is saved first: `praxis` reads the file from disk, so running an
 * unsaved buffer would check the previous version and report about code the
 * user is no longer looking at.
 */
async function runFileCommand(subcommand: PraxisSubcommand): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (!editor || editor.document.languageId !== LANGUAGE_ID) {
    void vscode.window.showWarningMessage("Praxis: open a `.px` file first.");
    return;
  }
  await editor.document.save();

  const filePath = editor.document.uri.fsPath;
  const argv = fileCommandArgv(subcommand, filePath, {
    inputPath: subcommand === "run" ? defaultInputPath(filePath) : undefined,
  });

  const terminal = findOrCreateTerminal();
  terminal.show(true);
  terminal.sendText(terminalCommand(binaryPath(), argv));
}

/**
 * The input file a `run` should use, or `undefined` for stdin.
 *
 * The convention is `input.txt` beside the source, which is what the corpus
 * uses. Nothing is invented when it is absent: `praxis run` without `--input`
 * reads stdin, and the terminal is where the user can type it.
 */
function defaultInputPath(filePath: string): string | undefined {
  const candidate = path.join(path.dirname(filePath), "input.txt");
  return fs.existsSync(candidate) ? candidate : undefined;
}

function findOrCreateTerminal(): vscode.Terminal {
  const existing = vscode.window.terminals.find((t) => t.name === TERMINAL_NAME);
  return existing ?? vscode.window.createTerminal(TERMINAL_NAME);
}
