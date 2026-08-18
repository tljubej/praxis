# praxis-ast

Typed syntax wrappers over [Praxis](https://github.com/tljubej/praxis) syntax
nodes.

Each wrapper — `SourceFile`, `VarStmt`, `PathExpr` and the rest — is a
strongly-typed view over a `SyntaxNode` whose `SyntaxKind` is fixed by
construction, so a wrongly-typed wrapper cannot exist: `cast` answers `None`
when the kind does not match.

Accessors borrow into the underlying green tree rather than copying source
strings, so walking a file costs no allocations.

The set of wrappers is deliberately minimal. A node is wrapped when a consumer
needs it, not ahead of one.

## What it provides

- `AstNode` — the trait every wrapper implements: a `const KIND`, `cast`,
  `syntax` and `span`.
- `nodes` — the wrappers themselves, re-exported at the crate root.

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
