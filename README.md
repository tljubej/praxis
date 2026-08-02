# Praxis

A small, statically typed, garbage-collected programming language designed for
Advent of Code-style puzzle solving. It favors rapid iteration, concise data
manipulation, practical parsing, and strong diagnostics over systems-programming
concerns.

> **Status:** Milestone 11 complete — the **language server MVP**, the VS Code
> extension, and its syntax highlighting.
>
> `praxis lsp` is a real JSON-RPC process over stdio (ADR-095: one synchronous
> loop, no async runtime). It serves document synchronization with incremental
> revisions, diagnostics, hover, completion including receiver methods,
> signature help, go-to-definition, document symbols, and semantic tokens.
> All five §19.11 acceptance criteria pass, each gated on the assertion that
> would fail a plausible-but-wrong implementation rather than on "something was
> returned" — the inner constructor's own type, a `Map` method's absence from
> `grid.`, the active parameter after a comma, the second of two shadowed
> declarations, four distinct parser token ranges.
>
> Two things are worth knowing about the shape of it. **`praxis check` now
> routes through the language server's query layer** (ADR-097), so a divergence
> between what the CLI prints and what the editor underlines is unrepresentable
> rather than merely unlikely. And **inference retains the parser AST**
> (ADR-098): hover on an inner constructor, capture-type completion and the four
> parser token classes read spans the compiler computed, rather than a second
> scanner in the editor that could disagree with it.
>
> `editors/vscode/` is the thin extension: a launcher, four commands, and a
> TextMate grammar that highlights a `.px` file before the server attaches and
> while it is down. Both layers emit the **same** TextMate scopes, and four Rust
> tests read the grammar at test time to keep its word lists from drifting from
> the lexer's — with no Node toolchain in `just ci` (ADR-002).
>
> Milestone 10, the crash debugger (§9), and the implementation repair before it
> both remain closed; see
> [`docs/handovers/18-every-row-closed-handover.md`](./docs/handovers/18-every-row-closed-handover.md).
> See [`praxis_technical_design.md`](./praxis_technical_design.md) for the full
> language and the milestone roadmap (§19); the next milestone (M12) is LSP
> completeness — find references, rename, workspace symbols, inlay hints, the
> formatter, and code actions — and `praxis watch` / `praxis repl` remain
> unimplemented.

## Command surface

```text
praxis run day05.px < input.txt      # JIT-compile and run, reading stdin
praxis run day05.px --input in.txt   # same, but read input from a file
praxis check day05.px                # front-end only (lex + parse + type-check)
praxis lsp                           # the language server, JSON-RPC over stdio
```

`run` works end-to-end: lex → parse → infer → lower → MIR → Cranelift JIT →
execute, then print the result. `check` runs the front end and routes through
the same query layer the language server uses (ADR-097). `lsp` speaks the
Language Server Protocol on stdin/stdout and is not meant to be run by hand —
the VS Code extension in `editors/vscode/` launches it. `watch` and `repl` are
wired but implemented in later milestones.

## Development

Requires a stable Rust toolchain and the [`just`](https://github.com/casey/just)
command runner:

```sh
cargo install just
```

Run the full quality gate — this is exactly what CI runs:

```sh
just ci
```

Individual checks (useful during iteration):

```text
just fmt       # reformat the code (modifies files)
just fmt-check # verify formatting without touching files
just clippy    # lint the whole workspace
just test      # run the whole test suite
just build     # build every crate
```

**Doctests are disabled and CI does not run them** (`doctest = false` in every
library crate). A doctest in a `///` comment will still be *compiled* by
`cargo doc`, but it is never executed — so do not rely on one to check anything.
Put the assertion in a unit test instead.

The reason is cost, not principle: `cargo test` runs `rustdoc --test` per crate,
and rustdoc has to analyze the whole crate before it can discover how many
doctests are in it. Finding none takes as long as finding some, the work is not
shared with the compilation cargo just did, and nothing about it is cached — so
it re-ran on every invocation. That was **95 seconds of the suite** to run the
one doctest the workspace had.

## Layout

See `praxis_technical_design.md` §14 for the crate responsibilities. The
short version:

- `crates/praxis-source` — files, spans, line maps, diagnostics.
- `crates/praxis-parser` — lexer + lossless rowan syntax tree.
- `crates/praxis-ast` — typed AST wrappers over the rowan tree.
- `crates/praxis-types` — interned type arena, unification, generalization.
- `crates/praxis-hir` — name resolution, type inference, typed-HIR lowering.
- `crates/praxis-mir` — slot-based IR with GC liveness analysis.
- `crates/praxis-codegen-cranelift` — MIR → Cranelift JIT backend.
- `crates/praxis-runtime` — GC heap, type descriptors, ABI wrappers, input parser.
- `crates/praxis-input-parser` — the input-parser DSL (ParserAst, plans, synthesis).
- `crates/praxis-stdlib` — method catalog schema and the prelude.
- `crates/praxis-cli` — the `praxis` command (`run`, `check`, `lsp`).
- `crates/praxis-lsp` — the shared front-end query layer (§14.2) **and** the LSP
  transport. `praxis check` calls the first half; the server is the second.
- `editors/vscode` — the thin VS Code extension and the TextMate grammar. Its
  drift gates are Rust tests in `crates/praxis-cli/tests/grammar.rs`, so `just
  ci` stays the whole gate and CI needs no Node toolchain.
