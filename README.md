# Praxis

A small, statically typed, garbage-collected programming language designed for
Advent of Code-style puzzle solving. It favors rapid iteration, concise data
manipulation, practical parsing, and strong diagnostics over systems-programming
concerns.

> **Status:** Milestone 10 complete, **and the implementation repair with it.**
> The adversarial audit of 2026-07-28 raised 139 findings and shipped 149
> `#[ignore]`d regressions as its acceptance gate. Every stage of
> [`docs/handovers/implementation-repair-plan-2026-07-28.md`](./docs/handovers/implementation-repair-plan-2026-07-28.md)
> is closed and **`cargo test --workspace` reports zero ignored tests**, against
> a baseline of 149.
>
> **138 of the 139 findings are addressed.** The one that is not is `MIR-10`,
> which the register carries as `PARTIAL — part owed`: its verifier landed and
> the *rule* the finding is about — "a faulting instruction is followed by a
> `CheckFault`" — did not. Three things are open, and they are lists rather than
> a single number because that is what does not go stale: **MIR-10's owed rule**,
> **ten open rows in that plan's §4.1** (two of them, `REP-52` and `REP-53`, are
> that rule's two ends; §4.1 names them all and is the authority on the count),
> and **two open language decisions**. See
> [`docs/handovers/16-repair-s18-s21-and-the-second-register-handover.md`](./docs/handovers/16-repair-s18-s21-and-the-second-register-handover.md).
> (The suite's pass count lives in `implementation-repair-progress.md` §1 and
> nowhere else, so it can only be stale in one place — which is why it is not
> repeated here.)
>
> Milestone 10 is the crash debugger (§9). A fault renders
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
- `crates/praxis-cli` — the `praxis` command (`run`, `check`).
