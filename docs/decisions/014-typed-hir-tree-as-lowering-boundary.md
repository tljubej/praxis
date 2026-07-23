# ADR-014: A typed-HIR tree as the MIR lowering boundary

**Date:** 2026-07-23 · **Status:** accepted

## Context

The contract pipeline (§10.1) is `… → Typed HIR → Monomorphization → MIR → …`.
M2's front end produces no separate typed-tree module: it walks the lossless AST
and attaches inferred types to **name-reference ranges only** (the `ref_types`
map on `Analysis`), not to every subexpression. The JIT backend needs the type
of *every* node — literals, binops, calls, blocks — inline, without re-running
unification.

M4 therefore needs a lowering source that carries a `Type` on every node.

## Decision

Add a `lower` module to `praxis-hir` that consumes `Analysis` + the AST and
produces a typed tree (`TypedModule` / `TypedItem` / `TypedStmt` / `TypedExpr`),
each node carrying its inferred `Type` handle. `analyze` / `analyze_root` are
unchanged — `lower` is a pure consumer.

The lowering re-derives each node's type in "read mode" against the finalized
`TypeDb` (mirroring the inference rules but never unifying), looking up symbol
schemes via `decls` (now surfaced on `Analysis`) for declarations and `refs` for
references. Generic functions are rejected with a `Y100` diagnostic ("not
supported yet; M4 is monomorphic"), deferring §13.6 monomorphization honestly.

`praxis-mir`'s builder consumes `TypedModule` and emits one MIR `Function` per
source `fn`.

## Reason

- A typed tree decouples the JIT from the AST's lossless shape and from
  inference internals; the backend never re-derives a type.
- Threading the declaration map (`decls`) onto `Analysis` is the honest fix for
  any downstream pass that must map a `let`/`var`/`fn`/param declaration site to
  its `SymbolId` unambiguously under shadowing.
- Rejecting generics at this boundary keeps M4's scope monomorphic (matching the
  §19 acceptance corpus) without a silent correctness gap.

## Consequences

- `Analysis` gains a public `decls` field; this is additive and M2's tests are
  unaffected.
- `lower` takes `&mut Analysis` because instantiating schemes to read concrete
  shapes allocates fresh `TypeDb` slots. Prior results are preserved; only the
  arena grows.
- The call node carries a resolved `callee_name: String` so the MIR/JIT can name
  the target without a `NameTable`.
