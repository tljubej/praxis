# praxis-hir

Name resolution and the high-level intermediate representation for
[Praxis](https://github.com/tljubej/praxis).

Two passes layered over the lossless syntax tree:

1. **Name resolution** — walk the typed AST, build a lexical scope tree, mint a
   `SymbolId` per declaration, resolve every reference, and emit `N0xx`
   diagnostics. Shadowing is settled here: each `var` gets a distinct id, and an
   initializer resolves names in the *preceding* environment.
2. **Type inference** — consume the resolved names, infer a type for every
   expression and binding through `praxis-typeck`, and emit `Y0xx` diagnostics.

`analyze` runs both and returns an `Analysis` carrying the symbol table, the
scope tree, the resolved references, the inferred types and every diagnostic.
That one value is what the CLI, the language server and the rest of the compiler
all ask their questions of — including the language server's parser-sublanguage
features, which read an index inference retains rather than re-scanning template
interiors.

A name's identity is its `SymbolId`, never its spelling, which is what tells two
shadowed bindings apart when the editor is asked to rename one of them.

## What it provides

- `analyze`, `Analysis` — the entry point and its result.
- `Symbol`, `SymbolId`, `ScopeTree`, `NameResolution` — resolution's output.
- `TypedModule`, `TypedFn`, `TypedExpr`, `TypedStmt`, `TypedPattern` — the typed
  HIR that `praxis-mir` lowers.
- `exhaustive` — the check that a `match` covers every case.
- `hover` — what the language server shows for a node.

## Part of Praxis

Praxis is a small, statically typed, garbage-collected language for Advent of
Code-style puzzles: the input parser is part of the language, types are inferred
rather than written, and a program that falls over hands you its state instead
of a stack trace.

To *use* the language, install [`praxis-cli`](https://crates.io/crates/praxis-cli)
— it provides the `praxis` binary. The
[repository](https://github.com/tljubej/praxis) has the book, the design
document and the decision records.

This crate is one stage of that compiler, published so the pipeline is
inspectable and so `praxis-cli` can be built from the registry. Its API tracks
what the compiler needs and is not a stable platform for outside consumers.

Praxis was written with large language models against a human design. The
repository's README says what that means for the license.

Licensed under either of Apache License 2.0 or the MIT license, at your option.
