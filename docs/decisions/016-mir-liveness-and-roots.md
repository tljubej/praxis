# ADR-016: MIR liveness and the per-safepoint root set

**Date:** 2026-07-23 · **Status:** accepted

## Context

§12.3 requires compiler-managed root tracking: "A function roots references live
across allocation safepoints; **MIR liveness computes the minimal root set.**"
§12.4 names the safepoints: every GC allocation, every allocating runtime call,
and (optionally) loop backedges. The M3 `RootScope` roots everything pushed into
it — over-conservative. The JIT needs the minimal set per safepoint.

## Decision

Implement a backward-dataflow liveness pass (`praxis_mir::liveness::annotate`)
that computes, per safepoint (`Alloc`/`Materialize`/`Call`), the set of live
`LocalKind::Gc` locals and stores it in the instruction's `live_roots` field.

The pass is classic backward dataflow to a fixpoint (`live_out` = union of
successors' `live_in`), then a forward walk that snapshots the live set at each
safepoint — excluding the safepoint's own `dst` (defined at the safepoint, so not
live across it). `BTreeSet<LocalId>` gives deterministic, stable root sets.

The Cranelift backend consumes `live_roots` to spill exactly those `GcRef`s into
the shadow-stack frame at each safepoint (the §12.3 seam).

## Reason

- Per-safepoint minimal roots keep short-lived garbage collectable, matching the
  contract's intent rather than the M3 over-approximation.
- Storing `live_roots` on the instruction (rather than a side table) keeps the
  root set co-located with the safepoint the backend emits.
- Deterministic ordering eases snapshot tests and stable frame layouts.

## Consequences

- M4's GC rooting via the generated shadow-stack frame is *specified* by this
  pass; the actual stack-slot spill in Cranelift is wired in the backend. Until
  that spill is emitted, collection is triggered only by the host's explicit
  `Runtime::collect` (the non-moving heap keeps referenced objects alive
  regardless), so M4's acceptance tests pass without a moving collector.
- A future pass may shrink the set further (e.g. drop locals dead before the next
  allocation); the current set is already minimal at the safepoint.
