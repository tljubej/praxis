# ADR-003: Lossless syntax tree uses the `rowan` crate

**Date:** 2026-07-23 · **Status:** accepted

## Context

The design (§13.1) requires the parser to produce a lossless tree that retains
trivia (whitespace + comments), because the formatter, the language server's
incremental edits, accurate diagnostics, and code actions all need it. The
design names the shape — "immutable green nodes plus lightweight red wrappers,
or an equivalent rowan-style design" — but leaves the implementation open.

There are two realistic options: adopt the [`rowan`](https://crates.io/crates/rowan)
crate (the library rust-analyzer is built on), or hand-roll an equivalent
green/red tree.

## Decision

Adopt `rowan` (v0.16) for the lossless green/red tree. Praxis owns the
`SyntaxKind` enum, the `rowan::Language` implementation, and the typed AST
wrappers (§13.2, in `praxis-ast`); `rowan` owns the immutable green tree, the
lazy red cursor layer with parent pointers, the `GreenNodeBuilder` checkpoint
mechanism the Pratt parser needs, exact source round-trip, and trivia
retention.

## Reason

- The design explicitly names "a rowan-style design," so this is the default
  reading of the contract rather than a deviation.
- The lazy red layer (parent pointers, O(1) cursors into one immutable tree)
  is the single most subtle, bug-prone piece of a lossless tree to hand-roll;
  `rowan` has already debugged it.
- `GreenNodeBuilder::checkpoint` / `start_node_at` maps directly onto
  recursive-descent + Pratt parsing: wrap an already-emitted operand sequence
  into a node after the fact when a binary operator is seen.
- `node.to_string()` reproduces the source exactly (trivia retained), which is
  the formatter-idempotency acceptance criterion for free.
- Typed `AstNode` wrappers (§13.2) interoperate natively with `rowan`'s `ast`
  module, so the layering in `praxis-ast` is idiomatic.

## Consequences

- One new dependency (`rowan`) on `praxis-syntax`, `praxis-ast`, and
  `praxis-test-support`. No dependency is added to `praxis-source`, so the
  front-end leaf stays dependency-free and the DAG stays clean.
- `rowan` uses its own `TextRange`/`TextSize` types internally. Praxis `Span` /
  `BytePos` remain the source of truth for diagnostics; a small `From` bridge
  translates at the tree boundary. `rowan` types do not leak into diagnostics.
- Praxis invariants (AGENTS.md: "make illegal states unrepresentable") are
  encoded in the `SyntaxKind` enum and the typed `praxis-ast` wrappers; `rowan`
  is a generic carrier and does not weaken that discipline.
