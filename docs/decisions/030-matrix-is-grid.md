# ADR-030: `matrix(P)` is `Grid[T]` (close §21.1)

**Date:** 2026-07-27
**Status:** Accepted
**Supersedes:** §21.1 open decision ("Whether `Matrix[T]` exists separately from `Grid[T]`")
**Milestone:** M9 (Input parser v2, §19.9)

## Context

The technical design (`praxis_technical_design.md` §7.5 `matrix`, §21.1) left open
whether the parser DSL's `matrix(element_parser)` constructor produces a distinct
`Matrix[T]` type or reuses the existing `Grid[T]`. The M9 deliverable is
"`matrix` or finalized grid-matrix design."

`Grid[T]` (TypeId 7) already provides a rectangular 2-D backing with
`.at(row, col)` indexing, row-major storage, equality, hashing, and iteration —
landed in M6 and completed in M8-WS5. A separate `Matrix[T]` would duplicate
that representation, its descriptor, its method surface, and its GC tracing,
with the only hypothesized payoff being matrix-specific algebra (multiplication,
determinants) that no Advent-of-Code fixture in the corpus requires.

## Decision

**`matrix(P)` parses whitespace-separated elements per line into a rectangular
`Grid[result(P)]`.** There is no new type, no new TypeId, and no new runtime
descriptor. The difference between `grid(P)` and `matrix(P)` is purely how each
*tokenizes a row*:

- `grid(cell_parser)` — every character of the row is a cell (`grid(char)`,
  `grid(digit)`). Rows are uniform-width by character count.
- `matrix(element_parser)` — whitespace-separated tokens per row
  (`matrix(int)`). Rows are uniform-width by *token* count.

Both produce `Grid[T]`.

## Consequences

- One 2-D type to maintain, document, and teach. `Grid[T]`'s `.at`,
  `.width`, `.height`, equality/hashing, and (future) iteration serve both
  shapes.
- The M9 `matrix` constructor is a single new `PlanNode::Matrix` arm +
  `walk_matrix` interpreter case that splits each row on whitespace and parses
  `width` tokens — strictly less code than a new collection type.
- §21.1 is closed. Should genuine matrix-algebra demand appear later (post-M13
  corpus profiling), a distinct `Matrix[T]` type can be introduced at that
  point without disturbing `Grid[T]`.
- No `TypeDb` or `CollectionCtor` change. The `Grid` ctor already exists; the
  result type is `db.collection(CollectionCtor::Grid, vec![elem])`, identical to
  `grid(P)`.

## Revisit trigger

A corpus fixture that (a) needs matrix multiplication / transposition as a
first-class operation AND (b) cannot be expressed cleanly with `.at` indexing
over `Grid[T]`. Absent that, `Grid[T]` remains the sole 2-D type.
