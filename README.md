# Praxis

A small, statically typed, garbage-collected programming language designed for
Advent of Code-style puzzle solving. It favors rapid iteration, concise data
manipulation, practical parsing, and strong diagnostics over systems-programming
concerns.

> **Status:** Milestone 10 Part 1 complete — the crash debugger substrate
> (§9): §7.11 rich parse diagnostics, debug-frame codegen wiring (ADR-021),
> crash snapshots with GC rooting (the §19.10 "GC retains snapshot-reachable
> objects" criterion), the §9.6 noninteractive fallback, and the interactive
> crash REPL (`bt`/`frame`/`up`/`down`/`locals`/`help`/`quit`). A fault now
> renders a numbered backtrace + locals; `--debug=auto|always|never` controls
> REPL entry. Part 2 adds the read-only `p EXPR`/`type EXPR` evaluator and
> `restart`/`reload`. See
> [`praxis_technical_design.md`](./praxis_technical_design.md) for the full
> language and the milestone roadmap (§19).

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
