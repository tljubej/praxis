# Architectural decisions

Per the Praxis technical design (§20, rule 1), the design document is the
current contract and deliberate deviations are recorded here before
implementing them. Each entry is short: context, decision, and the reason.

Entries are numbered and dated. Add new decisions at the bottom.

- [ADR-001: Snapshot testing library is `insta`](./001-snapshot-testing-insta.md)
- [ADR-002: CI runs `just ci`, with a minimal GitHub Actions wrapper](./002-ci-via-just.md)
- [ADR-003: Lossless syntax tree uses the `rowan` crate](./003-lossless-tree-uses-rowan.md)
- [ADR-004: Hand-written recursive-descent parser with Pratt climbing](./004-parser-technique.md)
- [ADR-005: Formatter skeleton lives in `praxis-parser`](./005-formatter-lives-in-praxis-parser.md)
- [ADR-006: Milestone 1 fuzz gate is a `proptest` property test](./006-m1-fuzz-via-proptest.md)
- [ADR-007: Type representation is an interned arena](./007-type-representation-interning.md)
- [ADR-008: let-generalization uses Pottier-style binding levels](./008-let-generalization-levels.md)
- [ADR-009: Minimal typed AST wrappers over rowan nodes](./009-m2-typed-ast-wrappers.md)
- [ADR-010: Method catalog bridge in M2; `.method()` dispatch deferred to M5](./010-method-catalog-bridge-and-m5-deferral.md)
- [ADR-011: Precise non-moving mark-and-sweep over a Bumpalo arena + live-set registry](./011-gc-bumpalo-mark-sweep.md)
- [ADR-012: Explicit root frames for M3](./012-root-tracking-explicit-frames.md)
- [ADR-013: Scalar + Vec[T] descriptors in M3; other collection descriptors in M5](./013-m3-descriptors-scalars-and-vec.md)
