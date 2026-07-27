# Praxis

A small, statically typed, garbage-collected programming language designed for
Advent of Code-style puzzle solving. It favors rapid iteration, concise data
manipulation, practical parsing, and strong diagnostics over systems-programming
concerns.

> **Status:** Milestone 10 complete — the crash debugger (§9). A fault renders
> a numbered backtrace + locals and (when attached to a terminal, or with
> `--debug=always`) drops into an interactive crash REPL with all fifteen §9.4
> commands: `bt`/`frame`/`up`/`down`/`locals` for navigation, `p EXPR`/`type
> EXPR`/`heap EXPR` for read-only evaluation through the JIT, `source`/`input`/
> `parser` for context, and `restart`/`reload` to rerun. The §9.6 noninteractive
> fallback covers piped/non-TTY runs. All five §19.10 acceptance criteria pass.
> See [`praxis_technical_design.md`](./praxis_technical_design.md) for the full
> language and the milestone roadmap (§19); the next milestone (M11) is the
> LSP / IDE integration.

## Command surface

```text
praxis run day05.px < input.txt      # JIT-compile and run, reading stdin
praxis run day05.px --input in.txt   # same, but read input from a file
praxis check day05.px                # front-end only (lex + parse + type-check)
```

`run` works end-to-end: lex → parse → infer → lower → MIR → Cranelift JIT →
execute, then print the result. `watch`, `repl`, and `lsp` are wired but
implemented in later milestones.

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
- `crates/praxis-cli` — the `praxis` command (`run`, `check`).
