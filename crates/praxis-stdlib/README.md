# praxis-stdlib

The method catalog schema and the prelude for [Praxis](https://github.com/tljubej/praxis).

One source of truth for what built-in methods exist, what their types are, and
how they lower — to a runtime symbol or to an intrinsic. The type checker, HIR
lowering, the code generator, the documentation generator and the language
server's completion all read *this* catalog, so none of them can come to hold a
second opinion about what `Vec.windows` means.

That is a hard rule of the design rather than a convention, and it is why the
schema carries everything all five consumers need in one entry: signature,
purity, capability requirements, lowering, and the documentation text an editor
shows on hover.

## What it provides

- `MethodCatalog`, `MethodEntry`, `MethodLowering`, `Purity` — the schema.
- `builtin_catalog` — the built-in methods themselves.
- `PRELUDE`, `BUILTIN_TYPES`, `NUMERIC_HELPERS`, `GRAPH_HELPERS`, `SIZED_CTORS`
  — the names in scope before a program declares anything.
- `TypePattern`, `ScalarType`, `CollectionCtor`, `Bound` — the type vocabulary
  the catalog's signatures are written in, shared with `praxis-typeck`.
- `completion_data` — the catalog projected into what a language server offers.

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
