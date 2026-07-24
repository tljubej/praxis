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
- [ADR-014: A typed-HIR tree as the MIR lowering boundary](./014-typed-hir-tree-as-lowering-boundary.md)
- [ADR-015: MIR shape — non-SSA slots, transient scalars, Cranelift makes SSA](./015-mir-shape-non-ssa-slots.md)
- [ADR-016: MIR liveness and the per-safepoint root set](./016-mir-liveness-and-roots.md)
- [ADR-017: Runtime ABI wrappers and the no-panic fault protocol](./017-runtime-abi-wrappers-and-fault-protocol.md)
- [ADR-018: Monomorphization deferred — M4 is monomorphic](./018-monomorphization-deferred.md)
- [ADR-019: Compiler-managed shadow-stack spill](./019-shadow-stack-spill.md)
- [ADR-020: Method-call dispatch through the built-in catalog](./020-method-dispatch-and-collections.md)
- [ADR-021: Debug frame metadata and shadowed-symbol registration](./021-debug-frame-metadata.md)
- [ADR-022: Source-slice `Text` representation in M6](./022-source-slice-text.md)
- [ADR-023: Input-parser DSL architecture](./023-input-parser-dsl.md)
- [ADR-024: Provisional structural records ahead of M7](./024-provisional-structural-records.md)
- [ADR-025: TypeData record/enum via def-id indirection](./025-typedata-record-enum-defid.md)
- [ADR-026: Structural equality & hashing via descriptors + internal capability check](./026-structural-equality-hashing.md)
