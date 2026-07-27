# ADR-032: Debugger expressions allocate on the main GC heap (closes §21.8)

**Date:** 2026-07-27
**Status:** Accepted
**Closes:** §21.8 open decision ("Whether debugger expression functions can
allocate freely or use a separate temporary heap generation")
**Milestone:** M10 (Crash debugger REPL, §19.10) — recorded in M10a; exercised
in M10b (`p EXPR`).

## Context

§9.5 specifies that `p EXPR` in the crash REPL JIT-compiles a synthetic
read-only function and executes it against the selected frame's snapshot slots.
That function may allocate (e.g. `p xs.map(|x| x + 1)` builds a new `Vec`).
§21.8 left open whether those allocations go on the main GC heap or a separate
"temporary heap generation" that is discarded on REPL exit.

A separate generation would bound debugger-heap growth by construction, at the
cost of a second arena, a second root-scoping mechanism, and careful handling
of values that escape a `p` expression into the REPL's display.

## Decision

**Debugger expressions allocate freely on the main GC heap.** No separate
generation.

## Reason

1. **GC safety already holds.** The crash snapshot implements `RootSet`
   (ADR-033 / M10-WS3), so every object reachable from a snapshot — including
   any object a `p EXPR` allocation subsequently links off a snapshot local — is
   retained by the collector. The §19.10 acceptance criterion ("GC retains all
   objects reachable from snapshots") is satisfied without a second heap.

2. **Accumulation is a non-acceptance performance concern.** A debugging
   session is short-lived and human-paced; the heap growth from a handful of
   `p` evaluations is negligible. Per the design priority order (§2.3) and rule
   11 ("Do not optimize representation before a benchmark identifies a
   bottleneck"), the second-generation machinery is premature.

3. **Simplicity.** One heap, one root mechanism, one collector. A second
   generation would duplicate rooting logic and complicate value escape (a
   `p` result printed by the REPL must outlive the expression that built it).

## Consequences

- M10b's `p EXPR` evaluator allocates through the existing `praxis_alloc_*`
  wrappers with no special path.
- If a future long-lived debugger session or watch mode (§13 watch-loop
  acceptance) shows unbounded growth from repeated `p` allocations, revisit with
  a benchmark — a per-REPL-command collection or a generation is the likely
  remedy, gated behind the same snapshot root set.
