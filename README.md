# Praxis

A small, statically typed, garbage-collected programming language designed for
Advent of Code-style puzzle solving. It favors rapid iteration, concise data
manipulation, practical parsing, and strong diagnostics over systems-programming
concerns.

> **Status:** everything through Milestone 12 of the roadmap in
> [`praxis_technical_design.md`](./praxis_technical_design.md) §19 is
> implemented **except the formatter**, which M12 lists as a deliverable and
> which is out of scope by decision (below). `praxis run` compiles and executes
> end to end, `praxis check` reports diagnostics without generating code,
> `praxis lsp` is a full language server, and a runtime fault drops into a crash
> debugger with a full-screen TUI. `var` is the one binding form; a pipeline
> materializes eagerly, so a chain that ends on a stage already answers a `Vec`.
>
> Milestone 13 — corpus validation and performance hardening — is in progress.
> The [benchmark suite](./benchmarks/) (8 programs, each written in Praxis, Rust
> and Python, with byte-identical output enforced) measures Praxis at 7× Rust,
> 0.2× CPython 3.14 and 1.09× CPython's peak resident set; see
> [`benchmarks/REPORT.md`](./benchmarks/REPORT.md). The AoC corpus under
> [`tests/aoc-corpus`](./tests/aoc-corpus) and
> [`tests/input-parsers`](./tests/input-parsers) is a `cargo test` gate: every
> `.px` file in the `tests/` tree must run clean and match its `.out`
> (`crates/praxis-cli/tests/corpus.rs`). What M13 still owes is a corpus drawn
> from more than one puzzle year, and the watch-mode criterion, which cannot be
> met before `praxis watch` exists.
>
> **Not implemented:** a stable formatter (§19.12) — there is no `praxis fmt`
> and the server does not advertise `documentFormattingProvider`, deliberately
> ([`docs/handovers/29-milestone-12-handover.md`](./docs/handovers/29-milestone-12-handover.md)
> §4). The M1 skeleton in `crates/praxis-parser/src/fmt.rs` is still there and
> is kept working as the syntax grows — its own unit tests hold it to
> idempotency, and one covers interpolation — but nothing outside the crate
> calls it, so it is a library function rather than a capability.
>
> `praxis watch` is scheduled for a later milestone and `praxis repl` is
> scheduled nowhere; both are dispatchable subcommands that print an explicit
> not-implemented message and exit 2.
>
> Design decisions are recorded in [`docs/decisions/`](./docs/decisions) and
> milestone handovers in [`docs/handovers/`](./docs/handovers).

## Command surface

```text
praxis run day05.px < input.txt        # JIT-compile and run, reading stdin
praxis run day05.px --input in.txt     # same, but read input from a file
praxis run day05.px --debug=never      # never enter the crash debugger
praxis check day05.px                  # front-end only (lex + parse + infer)
praxis lsp --stdio                     # the language server, JSON-RPC on stdio
praxis --color=always check day05.px   # force ANSI diagnostics
```

`run` works end-to-end: lex → parse → infer → lower → monomorphize → MIR →
Cranelift JIT → execute. It prints a result line only when the entry point
returns a non-`Unit` value; a file of top-level statements compiles to a
`Unit`-returning entry whose only output is what `out(...)` wrote.

`check` runs the front end and routes through the same query layer the language
server uses (ADR-097), so what the CLI prints and what the editor underlines
cannot diverge.

`lsp` speaks the Language Server Protocol on stdin/stdout and is not meant to be
run by hand — the VS Code extension in `editors/vscode/` launches it. `--stdio`
is accepted and ignored: stdio is the only transport this server has, and
several clients append the flag to the server's argv.

`--debug <auto|always|never>` (on `run`) decides what a runtime fault does.
`auto`, the default, enters the crash debugger iff both stdin and stdout are a
terminal; `always` forces it; `never` prints the noninteractive fault report and
exits nonzero. When the debugger is entered, a real terminal gets the
full-screen TUI — backtrace, source, locals and transcript at once — and
everything else gets the line REPL.

`--color <auto|always|never>` is global and valid on every subcommand. `auto`
styles output iff stderr is a terminal; `always` forces ANSI even when piped;
`never` emits plain text. Both the diagnostic renderer and the crash debugger's
fault report honor it.

`watch <file>` and `repl` are declared subcommands that print a not-implemented
message and exit 2.

Exit codes are a closed set of three: `0` — no errors; `1` — a language error or
a runtime fault (an internal compiler error included); `2` — the CLI could not
start the job (an unreadable source or `--input` file, an unimplemented
command). `lsp` returns the LSP protocol's own `0`/`1` instead.

## The language server

`praxis lsp` is a JSON-RPC process over stdio — one synchronous loop on the main
thread, no async runtime (ADR-095). It serves document synchronization with
incremental revisions, diagnostics, hover with documentation, completion
including receiver methods, signature help, go-to-definition, find references,
rename, workspace symbols, document symbols, inlay hints, code actions, and
semantic tokens.

Inlay hints show what the compiler inferred: `fn foo(a, b)` reads as
`fn foo(a: Int, b: Int)`, and a type inference has not pinned shows as `?T`
rather than as nothing. Accepting a hint writes the annotation into the file
where the annotation is both legal and spellable — a `?T` or an anonymous-record
hint is shown but cannot be accepted.

A quick fix *is* a diagnostic's machine-applicable suggestion (ADR-132), so
every fix the editor offers is one `praxis check` also prints; each is gated by
applying it and re-analyzing. Rename applies the edit to a copy and accepts it
only if name resolution comes out unchanged (ADR-131), which catches capture in
both directions.

Inference retains the parser AST (ADR-098), so hover on an inner constructor,
capture-type completion and the four parser token classes read spans the
compiler computed.

`editors/vscode/` is the thin extension: a launcher, four commands, and a
TextMate grammar that highlights a `.px` file before the server attaches and
while it is down. Both layers emit the **same** TextMate scopes. Four gates in
`crates/praxis-cli/tests/grammar.rs` read the grammar at test time — three
sweeping the compiler's own closed tables (`SyntaxKind`'s keywords,
`AtomicKind::ALL`, `Constructor::ALL`) and one requiring every server token
scope to be a scope the grammar emits — so `just ci` stays the whole gate and
needs no Node toolchain (ADR-002).

## The book

[`docs/book/`](./docs/book/) is an mdBook: 46 chapters in seven parts, plus two
appendices. It covers getting started and the CLI, the language itself
(bindings, scalars, text, control flow, functions, records, enums, pattern
matching, collections, pipelines, grids and graphs), the input-parser DSL, type
inference, the crash debugger, editor support and the diagnostic index, and the
compiler internals — the pipeline, the object heap and collector, and a map of
the ADRs. The prelude and method catalog are reference tables; the appendices
are complete programs and the grammar.

```sh
just book         # render to docs/book/book
just book-serve   # live reload on localhost:3000
just book-binary  # build the praxis binary book-verify runs the examples with
just book-verify  # re-run every example and diff it against the chapters
just book-bless   # rewrite expectations from what the compiler prints
```

Its examples are checked, not illustrative. Every code block that shows a
program *and* its output is one of 402 real files under `docs/book/examples/`,
each with exactly one expectation file, and `book-verify` runs all of them —
including the ones that are supposed to fail, whose diagnostics, fault reports
and debugger transcripts are captured the same way. A `.px` with no expectation
fails the check rather than being skipped.

`book-verify` needs a built `praxis` binary — `just book-binary`, or set
`PRAXIS` to one you have — and it **is** part of `just ci`, which builds that
binary first. `book-bless` is the one book recipe deliberately outside `ci`:
review its diff, because it is how a real regression gets papered over.

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

`ci` is `fmt-check`, `clippy`, `test`, then `book-binary` and `book-verify`: it
builds the `praxis` binary and verifies every book example against it.

Individual checks (useful during iteration):

```text
just fmt       # reformat the code (modifies files)
just fmt-check # verify formatting without touching files
just clippy    # lint the whole workspace
just test      # run the whole test suite
just build     # build every crate
just asan      # the whole suite under AddressSanitizer
```

`asan` is deliberately not part of `ci` — the instrumented build is a second
full compile of a gate that is already slow, so it runs nightly instead via
`.github/workflows/asan.yml`, calling the same recipe. It needs a nightly
toolchain (`rustup toolchain install nightly`; `rust-toolchain.toml` pins stable
and `scripts/asan.sh` overrides it). It does **not** cover Cranelift-generated
code, so a green run is necessary and not sufficient for any change that puts
new unsafe behaviour in generated code.

**Doctests are disabled and CI does not run them** (`doctest = false` in every
library crate; `praxis-cli` is binary-only and has no lib target). A doctest in
a `///` comment will still be *compiled* by `cargo doc`, but it is never
executed — so do not rely on one to check anything. Put the assertion in a unit
test instead. The reason is cost, not principle: `cargo test` runs
`rustdoc --test` per crate, rustdoc must analyze the whole crate before it can
find out how many doctests are in it, and the result is never cached, so
finding zero costs as much as finding some.

## Layout

See `praxis_technical_design.md` §14 for the crate responsibilities. The
short version:

- `crates/praxis-source` — files, spans, line maps, diagnostics.
- `crates/praxis-syntax` — the `SyntaxKind` vocabulary, the rowan language tag,
  and the one rule each for identifiers, literals, interpolation and template
  ends.
- `crates/praxis-parser` — lexer + lossless rowan syntax tree.
- `crates/praxis-ast` — typed AST wrappers over the rowan tree.
- `crates/praxis-types` — interned type arena, unification, generalization.
- `crates/praxis-hir` — name resolution, type inference, match coverage,
  typed-HIR lowering and monomorphization.
- `crates/praxis-mir` — slot-based IR with GC liveness analysis.
- `crates/praxis-runtime` — GC heap, type descriptors, ABI wrappers, input
  parser.
- `crates/praxis-repr` — the total, bidirectional `Type` ↔ `TypeDescriptor`
  bridge; a type with no runtime representation is an error, never a wrong
  descriptor.
- `crates/praxis-codegen-cranelift` — MIR → Cranelift JIT backend.
- `crates/praxis-debugger` — crash snapshots, the noninteractive fault report,
  and the interactive crash REPL and its full-screen TUI.
- `crates/praxis-input-parser` — the input-parser DSL (ParserAst, plans, synthesis).
- `crates/praxis-stdlib` — method catalog schema and the prelude.
- `crates/praxis-cli` — the `praxis` command (`run`, `check`, `lsp`; `watch` and
  `repl` are honest stubs).
- `crates/praxis-lsp` — the shared front-end query layer (§14.2) **and** the LSP
  transport. `praxis check` calls the first half; the server is the second.
- `crates/praxis-test-support` — one-file source-map fixtures and stable golden
  syntax-tree dumps for §17.2's snapshot tests.
- `editors/vscode` — the thin VS Code extension and the TextMate grammar. Its
  drift gates are Rust tests in `crates/praxis-cli/tests/grammar.rs`, so `just
  ci` stays the whole gate and CI needs no Node toolchain.
