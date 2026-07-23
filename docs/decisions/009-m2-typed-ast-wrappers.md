# ADR-009: Minimal typed AST wrappers over rowan nodes

**Date:** 2026-07-23 · **Status:** accepted

## Context

§13.2 calls for "typed wrappers over syntax nodes" so the HIR (and later the
LSP) consume the lossless tree through strongly-typed accessors rather than by
re-matching `SyntaxKind` everywhere. The M1 handover flagged this as an open
decision: build the `AstNode` foundation up front, or incrementally as each
milestone needs wrappers — "either is fine; don't over-build."

## Decision

Lay a minimal `AstNode` trait foundation in `praxis-ast` and add **only the
wrappers M2 walks**: `SourceFile`, `LetStmt`, `VarStmt`, `AssignStmt`, `FnItem`,
`ParamList`, `Param`, `ExprStmt`, `TypeRef`, and the expression nodes
(`Literal`, `PathExpr`, `BinExpr`, `UnaryExpr`, `ParenExpr`, `TupleExpr`,
`BlockExpr`, `IfExpr`, `ElseBranch`, `WhileExpr`, `CallExpr`, `ArgList`). Each
is a thin newtype over `SyntaxNode` with a `const KIND` and a `cast` that
rejects mismatched kinds — so a wrongly-typed wrapper is unrepresentable.

## Reason

- Matches §13.2 ("typed wrappers over syntax nodes; avoid copying source
  strings") and the rowan/rust-analyzer idiom natively.
- Incremental: every wrapper added is consumed by name resolution or inference
  this milestone. No speculative wrappers to maintain or later delete.
- Names stay **bare `Ident` tokens** (M1's tree has no `NAME`/`NAME_REF` wrapper
  nodes); the wrappers expose the `Ident` token directly. This avoids touching
  any M1 snapshot and is recorded as a refinement point, not a limitation.

## Consequences

- `praxis-ast` now depends on `praxis-parser` (dev-dep, for tests) and
  `praxis-test-support` (dev-dep). The DAG stays clean.
- Later milestones add wrappers as they need them (e.g. `MatchExpr`, `RecordLit`
  in M7). The pattern is fixed: newtype + `KIND` + `cast` + accessors.
- An `Expr` enum lets HIR dispatch over expression kinds once without re-casting;
  it grows a variant per new expression form.
