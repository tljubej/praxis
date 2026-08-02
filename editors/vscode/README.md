# The Praxis VS Code extension

Thin by design (§15.4, §20 rule 3): it registers `.px`, launches `praxis lsp`,
exposes four commands, and ships a TextMate grammar. **No parsing and no type
logic in TypeScript** — everything the editor knows about a Praxis program comes
from the compiler over the protocol, so the two cannot disagree.

## What it contributes

| Thing | Where |
|---|---|
| The `praxis` language and `.px` extension | `package.json` |
| Comments, brackets, autoclosing, indentation | `language-configuration.json` |
| Fallback syntax highlighting | `syntaxes/praxis.tmLanguage.json` |
| Semantic-token → TextMate scope mapping | `package.json`'s `semanticTokenScopes` |
| `Praxis: Run File` / `Check File` / `Watch File` / `Restart Language Server` | `src/extension.ts` |
| The argv every command builds | `src/argv.ts` |

`praxis.binaryPath` (default `praxis`) points at the local binary; every command
and the language server invoke that one path. `praxis.trace.server` turns on
JSON-RPC tracing.

`Praxis: Watch File` runs `praxis watch`, which is **not implemented yet** — the
command exists so the surface is complete and the binary's own "not implemented"
message is what the user sees. Hiding the command would be a quieter lie.

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

This adds **no Node toolchain to CI** — `just ci` is the whole gate (ADR-002).
A `vscode-tmgrammar-test` snapshot suite is worth revisiting at M14, when the
extension is packaged and a Node step exists anyway.

## Building it

```bash
npm install
npm run compile
npm test
```

`npm test` runs `src/argv.test.ts` under `node --test`. It is not part of
`just ci`, for the reason above; the drift it would catch that matters — a
subcommand the CLI no longer has — is caught by the Rust gate instead.

## The manual check

The last mile genuinely needs a host, so it is written down rather than
automated:

1. `cargo build --release` (or `cargo build`), and set `praxis.binaryPath` to
   `target/debug/praxis` in the extension development host's settings.
2. `F5` in this directory to launch the Extension Development Host.
3. Open `tests/aoc-corpus/day02_grid_of_char.px`.
4. Expect: the file is coloured before anything connects; `grid` and `char`
   inside the `read` are coloured as a parser constructor and a capture type;
   hovering `read`'s body shows `Grid[Char]`; typing `map.` offers the grid
   methods.
5. Run `Praxis: Check File` — the integrated terminal shows
   `<binaryPath> check <file>` and its exit status.
6. Run `Praxis: Run File` — the same terminal runs the program, with
   `--input input.txt` appended when that file sits beside the source.
7. Run `Praxis: Restart Language Server` and confirm diagnostics come back.
