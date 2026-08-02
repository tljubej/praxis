# ADR-033: Crash snapshots root through `DebugLocal.value` copies

**Date:** 2026-07-27
**Status:** Accepted
**Milestone:** M10-WS3 (Crash snapshot + GC rooting, §9.3 / §19.10)
**Builds on:** ADR-011 (non-moving collector), ADR-019 (shadow-stack spill),
ADR-021 (debug-frame metadata)
**Amended by:** ADR-104 (decision 4 only)

## Context

§9.3 requires that on a fault, each generated function copies or links its
debug frame into a persistent crash snapshot **before** returning, and that "GC
references in snapshots become roots." The §19.10 acceptance criterion makes
this explicit: "GC retains all objects reachable from snapshots."

The challenge: the fault unwind is multi-frame. Each function's fault epilogue
pops its shadow + debug frames as it returns to its caller. By the time control
reaches the host, `ctx.debug_top` is null again — the live chain is gone. The
snapshot must be captured **while the chain is intact**, and the captured
`GcRef`s must remain valid across subsequent collections.

## Decision

1. **Deep-copy the chain at the first fault epilogue.** The generated fault
   epilogue calls `praxis_snapshot_debug_chain(ctx)` *before* popping the debug
   frame. The helper is **idempotent**: a `taken` guard on the runtime's
   `SnapshotSlot` means only the first (innermost) call captures — the innermost
   frame's epilogue runs first, while the full parent chain is still linked.
   Outer frames unwinding later hit the guard and skip.

2. **Copy `DebugLocal.value` by value; root the copies.** `CrashSnapshot`
   implements `RootSet` by yielding every copied `DebugLocal.value`. The copy is
   a shallow `Vec<DebugLocal>` clone — correct because `DebugLocal` is plain
   data (a `GcRef` + raw pointers to `'static` descriptor/name data); the
   `GcRef`s keep pointing at the same objects.

3. **Stability relies on the non-moving collector (ADR-011).** Because the heap
   is precise and non-moving, a `GcRef` copied into a snapshot keeps its
   address; the collector never relocates the object. Transitive reachability
   (a snapshot Vec's elements, a record's fields) is the collector's job — the
   snapshot pins only the entry points.

4. **The spill keeps snapshot values fresh (ADR-019/021).** WS2's extended
   spill mirrors each live-root write into both the shadow-stack slot *and* the
   matching `DebugLocal.value`, so the snapshot reflects the live state at the
   safepoint before the fault, not a stale prologue-time value.

   > **Amended by ADR-104.** The *def-store* keeps them fresh, at the same or
   > earlier program points: the backend writes each `Gc` local's debug slot at
   > the instruction that defines it, and emits nothing at safepoints. Decisions
   > 1–3 are untouched, and decision 1 in particular is: the innermost fault
   > epilogue still captures, still before any pop, still guarded by
   > `SnapshotSlot::is_set()`. A slot stack does not destroy the words a pop
   > releases, so capturing lazily at the host became *possible* under ADR-104
   > and is rejected — values above `top` are in no arm of `RuntimeRoots`, which
   > is decision 2's rooting story.

## Alternatives considered

- **Snapshot via `longjmp` / suspended native stack.** Rejected (§9.3): the
  design explicitly avoids platform tricks and suspended JIT stacks. The
  snapshot is a plain data copy; native frames unwind normally.
- **Capture in every epilogue (no idempotency guard).** Would overwrite the
  intact-chain snapshot with progressively shorter chains as outer frames
  unwind. The guard ensures the richest (full-chain) snapshot wins.
- **A separate root-scan over the live `debug_top` chain.** Impossible: the
  chain is null by the time the host regains control. The copy is necessary.

## Consequences

- The host reads the snapshot via `Runtime::crash_snapshot()` /
  `take_crash_snapshot()` after a fault; it is `None` on a clean run or a
  host-side fault before any debug frame was pushed.
- The noninteractive fallback (WS4) and the REPL (WS5) root the snapshot when
  collecting during their lifetime (the GC-retention test in WS3 exercises
  this directly).
- `DebugLocal` is `Clone + Copy` (plain data); `CrashSnapshot: RootSet` is the
  single integration point with the collector.
