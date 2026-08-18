# The Praxis VS Code extension

Thin by design: it registers `.px`, launches `praxis lsp`, exposes three
commands, and ships a TextMate grammar. **No parsing and no type logic in
TypeScript** — everything the editor knows about a Praxis program comes from the
compiler over the protocol, so the two cannot disagree.

## What it contributes

| Thing | Where |
|---|---|
| The `praxis` language and `.px` extension | `package.json` |
| Comments, brackets, autoclosing, indentation | `language-configuration.json` |
| Fallback syntax highlighting | `syntaxes/praxis.tmLanguage.json` |
| Semantic-token → TextMate scope mapping | `package.json`'s `semanticTokenScopes` |
| `Praxis: Run File` / `Check File` / `Restart Language Server` | `src/extension.ts` |
| The argv every command builds | `src/argv.ts` |

`praxis.binaryPath` (default `praxis`) points at the local binary; every command
and the language server invoke that one path. `praxis.trace.server` turns on
JSON-RPC tracing.

## What arrives without the extension contributing anything

Diagnostics, hover, completion, signature help, go-to-definition, document
symbols, semantic tokens, find references, rename, workspace symbols, inlay
hints and quick fixes. All of them are server capabilities: the extension
registers nothing for them, which is what "thin by design" means in practice.

Two things worth knowing as a user:

- **Inlay hints are on.** An unannotated binding or parameter shows the type the
  compiler inferred — `fn foo(a: Int, b: Int)` — and `?T` where inference has
  not pinned one. Accepting a hint (double-click, or *Accept Inlay Hint*) writes
  the annotation into the file where that is legal. `editor.inlayHints.enabled`
  turns them off; the server has no setting of its own.
- **Praxis has no formatter.** The server does not advertise formatting, so
  VS Code keeps whatever it would do by itself on `Format Document`.

## The two highlighting layers, and why they agree

Highlighting arrives twice: the TextMate grammar paints a `.px` file instantly —
before the server attaches, while it is restarting, and in any renderer that
does not speak LSP — and the server's semantic tokens refine it once analysis
has run.

They must not *fight*. If the grammar scoped a parser constructor as a plain
function and the semantic layer scoped it as something a theme colours
differently, the token would change colour as the server connected. So both
layers emit the **same TextMate scopes**: the grammar directly, and the semantic
tokens through `contributes.semanticTokenScopes`.

| Construct | Scope |
|---|---|
| Parser constructor (`lines`, `grid`, …) | `entity.name.function.parser.praxis` |
| Template literal text | `string.quoted.other.template.praxis` |
| Capture name (`n` in `{n:int}`) | `variable.other.capture.praxis` |
| Capture type (`int` in `{n:int}`) | `support.type.capture.praxis` |

The one place the layers are *allowed* to disagree is which identifiers are
parser constructors: `lines` is a constructor inside a parser expression and an
ordinary name outside one, and a regex grammar approximates the region. The
disagreement resolves toward the compiler, because semantic tokens win.

## The drift gates are Rust tests

A grammar's keyword list is a copy of the lexer's that no compiler checks, and
the failure is invisible — a word quietly stops being coloured, and nobody files
it. So `crates/praxis-cli/tests/grammar.rs` **reads these files at test time**
and checks four things in `just ci`:

1. every keyword in `SyntaxKind`'s table is in the grammar's keyword pattern;
2. every `AtomicKind::keyword()` is in the capture-type pattern;
3. every `Constructor::keyword()` is in the constructor pattern;
4. every custom semantic token type in the server's legend has a
   `semanticTokenScopes` entry, and that entry's scope is one the grammar emits.

This adds **no Node toolchain to CI** — `just ci` is the whole gate.

## Building it

```bash
npm install
npm run compile
npm test
```

`npm test` runs `src/argv.test.ts` under `node --test`. It is not part of
`just ci`, for the reason above; the drift it would catch that matters — a
subcommand the CLI no longer has — is caught by the Rust gate instead.

## Sideloading it

`F5` runs the extension from *source*, which is not the same thing a user
installs — and the difference has already hidden one bug (see the `.vscodeignore`
comment). So the packaged form is worth checking too:

```bash
npm install && npm run compile && npx @vscode/vsce package
```

That writes `praxis-0.1.0.vsix`. Install it from the Extensions view's `…` menu
→ **Install from VSIX…**, or with the CLI:

```bash
code --install-extension editors/vscode/praxis-0.1.0.vsix
```

Then set `praxis.binaryPath` to an absolute path to the built binary, or put
`praxis` on `PATH` with `cargo install --path crates/praxis-cli`. The `.vsix` is
gitignored; reinstalling the same version replaces the previous one.

## The manual check

The last mile genuinely needs a host, so it is written down rather than
automated. Steps 1–8 run from source (`F5`); **step 9 is the packaged form**,
because the two differ:

1. `cargo build --release` (or `cargo build`), and set `praxis.binaryPath` to
   `target/debug/praxis` in the extension development host's settings.
2. `F5` in this directory to launch the Extension Development Host.
3. Open `tests/aoc-corpus/day02_grid_of_char.px`.
4. Expect: the file is coloured before anything connects; `grid` and `char`
   inside the `read` are coloured as a parser constructor and a capture type;
   hovering `read`'s body shows `Grid[Char]`; hovering `grid` itself shows its
   signature and what it does; typing `map.` offers the grid methods; every
   unannotated binding carries an inferred-type hint.
5. Put the caret on a binding: `Shift+F12` lists its references, and `F2`
   renames it — try renaming it to `out` and expect a refusal that names the
   collision rather than a silent no-op.
6. Run `Praxis: Check File` — the integrated terminal shows
   `<binaryPath> check <file>` and its exit status.
7. Run `Praxis: Run File` — the same terminal runs the program, with
   `--input input.txt` appended when that file sits beside the source.
8. Run `Praxis: Restart Language Server` and confirm diagnostics come back.
9. Package and install per "Sideloading it" above, reload, and confirm the
   extension **activates** — a missing runtime dependency shows up only here,
   as "Cannot find module" in the Extension Host log, and never under `F5`.
