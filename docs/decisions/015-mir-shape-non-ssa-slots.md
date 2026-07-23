# ADR-015: MIR shape — non-SSA slots, transient scalars, Cranelift makes SSA

**Date:** 2026-07-23 · **Status:** accepted

## Context

§13.5 specifies what MIR contains (basic blocks, explicit branches, local
`GcRef` slots, transient scalar temporaries, calls, allocations, checks, fault
edges, safepoints, debug metadata) and explicitly says **"MIR does not need to
be SSA initially. The Cranelift lowering layer creates SSA values and block
parameters."** §10.3 forbids any source-language scalar from having a separate
ABI representation across safepoints: payloads may live in a register transiently
but must be re-materialized as a `GcRef` before any safepoint/call/store/return.

## Decision

MIR is **slot-based, not SSA**. The unit of storage is a `Local`, of two kinds:

- `LocalKind::Gc` — holds a uniform `GcRef`. These are the only locals the GC
  roots at a safepoint.
- `LocalKind::Scalar(ScalarKind)` — a transient `i64`/`u8`/`u32`/`bool` payload
  extracted from a `GcRef` for a local computation. It must not survive a
  safepoint: the builder materializes a fresh `GcRef` (`Materialize`) before any
  call/store/return.

A `Function` is `locals + blocks`; each `Block` is `insts + terminator`. The
Cranelift backend maps each `Local` to a Cranelift `Variable`; Cranelift's SSA
builder turns the slot CFG into SSA automatically (`seal_all_blocks` resolves
loop backedges).

`Inst` includes `ConstInt`, `Alloc`, `ExtractScalar`, `Materialize`, `IntBinOp`,
`IntCmp`, `Call`, `CheckFault`, `MoveGc`. `Terminator` is `Branch`/`Jump`/
`Return`/`Fault`.

## Reason

- Non-SSA MIR is far simpler to emit from the typed tree (no phi placement); the
  complexity is pushed into Cranelift, which exists precisely to construct SSA.
- The `Gc`/`Scalar` split makes the §10.3 re-materialization rule a structural
  property the builder upholds, not a convention.
- `ConstInt` lets literals be folded by the backend (a Cranelift `iconst`) rather
  than round-tripping through an allocation.

## Consequences

- Arithmetic lowers as `Extract` → `IntBinOp` → re-`Alloc`/`Materialize` around
  each faultable op. A future pass can elide the re-allocation between an extract
  and the next materialize (the values are already scalars).
- Loops require sealing all blocks together after the CFG is built (per-block
  sealing fails on backedges).
