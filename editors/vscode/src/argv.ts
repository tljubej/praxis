// The argv the extension hands the local Praxis binary.
//
// **This file is the whole of §19.11 criterion 5 that can be tested without a
// VS Code host**, so it is deliberately free of `vscode` imports: it is pure
// data in, `string[]` out. `extension.ts` builds a command line only by calling
// these functions, so what a command actually runs is what a test can assert.
//
// It is checked twice:
//
//   - `npm test` runs `argv.test.ts` under `node --test`, at the extension's own
//     toolchain (packaging, M14);
//   - `crates/praxis-cli/tests/grammar.rs` reads this file as **text** in
//     `just ci` and checks that every subcommand named below is one the CLI
//     actually has. That is the drift that matters — the extension invoking a
//     subcommand or flag the CLI renamed — and it is caught with no Node
//     toolchain in CI (ADR-002).

/** The `praxis` subcommands the extension's four commands invoke. */
export type PraxisSubcommand = "run" | "check" | "watch";

/** The `--input FILE` option `praxis run` takes (§7.1): read the process input
 *  from a file instead of stdin. */
export interface RunOptions {
  /** Absolute path to the input file, when the user has one selected. */
  inputPath?: string;
}

/**
 * The argv for a file command.
 *
 * The binary itself is **not** part of this: it is the configured
 * `praxis.binaryPath`, and mixing it in here would make the array untestable
 * against a fixed expectation.
 */
export function fileCommandArgv(
  subcommand: PraxisSubcommand,
  filePath: string,
  options: RunOptions = {},
): string[] {
  const argv: string[] = [subcommand, filePath];
  // `--input` is `run`'s alone. `check` never reads input and `watch` takes the
  // source file only; appending it to either would be an argument the CLI
  // rejects rather than one it ignores.
  if (subcommand === "run" && options.inputPath) {
    argv.push("--input", options.inputPath);
  }
  return argv;
}

/** The argv for the language server. `praxis lsp` speaks JSON-RPC on stdio and
 *  takes no arguments. */
export function serverArgv(): string[] {
  return ["lsp"];
}

/**
 * The shell command line for an integrated terminal.
 *
 * §15.4 asks for debugger output in an integrated terminal rather than an
 * output channel, and the reason is that the crash REPL is **interactive** —
 * an output channel is write-only, so a program that faults would print its
 * prompt into a pane that cannot answer it.
 *
 * Quoting is minimal and deliberate: a path containing a double quote is
 * rejected rather than escaped, because the escaping rules differ between
 * `cmd.exe`, PowerShell and POSIX shells and a wrong guess silently runs a
 * different command.
 */
export function terminalCommand(binaryPath: string, argv: string[]): string {
  return [binaryPath, ...argv].map(quoteArgument).join(" ");
}

function quoteArgument(argument: string): string {
  if (argument.includes('"')) {
    throw new Error(
      `refusing to build a command line for an argument containing a double quote: ${argument}`,
    );
  }
  return /[\s]/.test(argument) ? `"${argument}"` : argument;
}
