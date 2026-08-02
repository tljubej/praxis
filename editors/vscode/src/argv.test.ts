// Unit tests for the argv construction (§19.11 criterion 5).
//
// Run with `npm test` after `npm run compile`. **Not part of `just ci`**: ADR-002
// makes `just ci` the whole gate, and a second toolchain in it is a cost this
// milestone does not need to pay. The drift that a Rust test *can* catch — a
// subcommand the CLI no longer has — is caught in
// `crates/praxis-cli/tests/grammar.rs`, which reads `argv.ts` as text.

import { strict as assert } from "node:assert";
import { test } from "node:test";

import { fileCommandArgv, serverArgv, terminalCommand } from "./argv";

test("run passes the file and nothing else", () => {
  assert.deepEqual(fileCommandArgv("run", "/w/day01.px"), ["run", "/w/day01.px"]);
});

test("run passes --input when one is configured", () => {
  assert.deepEqual(fileCommandArgv("run", "/w/day01.px", { inputPath: "/w/input.txt" }), [
    "run",
    "/w/day01.px",
    "--input",
    "/w/input.txt",
  ]);
});

test("check and watch never take --input", () => {
  // `--input` is `run`'s alone: the CLI rejects it on the others, so appending
  // it would fail the command rather than be ignored.
  assert.deepEqual(fileCommandArgv("check", "/w/day01.px", { inputPath: "/w/input.txt" }), [
    "check",
    "/w/day01.px",
  ]);
  assert.deepEqual(fileCommandArgv("watch", "/w/day01.px", { inputPath: "/w/input.txt" }), [
    "watch",
    "/w/day01.px",
  ]);
});

test("the server takes no arguments beyond the subcommand", () => {
  assert.deepEqual(serverArgv(), ["lsp"]);
});

test("the terminal command invokes the configured binary", () => {
  assert.equal(
    terminalCommand("/opt/praxis/bin/praxis", fileCommandArgv("run", "/w/day01.px")),
    "/opt/praxis/bin/praxis run /w/day01.px",
  );
});

test("a path with spaces is quoted", () => {
  assert.equal(
    terminalCommand("praxis", fileCommandArgv("check", "/My Puzzles/day01.px")),
    'praxis check "/My Puzzles/day01.px"',
  );
});

test("a path with a double quote is refused rather than guessed at", () => {
  assert.throws(() => terminalCommand("praxis", ["run", 'a"b.px']), /double quote/);
});
