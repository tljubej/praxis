# Praxis

A small, statically typed, garbage-collected programming language designed for
Advent of Code-style puzzle solving. It favors rapid iteration, concise data
manipulation, practical parsing, and strong diagnostics over systems-programming
concerns.

> **Status:** early implementation. This repository currently contains Milestone 0
> (workspace, source/diagnostic data structures, runtime ABI contracts, method
> catalog schema, and a CLI that can load and diagnose a `.px` file). The full
> language is described in [`praxis_technical_design.md`](./praxis_technical_design.md).

## Command surface

```text
praxis run day05.px < input.txt
praxis check day05.px
praxis watch day05.px --input input.txt
praxis repl
praxis lsp
```

Today only `check` does real work (load + lex + render diagnostics). The other
commands are wired and will be implemented in later milestones.

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
- `crates/praxis-runtime` — GC ABI types, type descriptors, runtime context.
- `crates/praxis-stdlib` — method catalog schema and the prelude.
- `crates/praxis-cli` — the `praxis` command.
- Other crates are skeletons that later milestones fill in.
