# ADR-011: Precise non-moving mark-and-sweep over a Bumpalo arena + live-set registry

**Date:** 2026-07-23 · **Status:** accepted

## Context

Milestone 3 (§19) requires "a precise non-moving mark-and-sweep collector."
§12.1 fixes the collector's character: precise, non-moving, single-threaded,
no write barrier. §12.2 gives only a *conceptual* header layout and leaves the
allocator and free-list strategy implementation-defined. The acceptance
criterion "GC stress tests preserve nested references" demands that sweep
actually reclaims unreachable objects while preserving everything reachable
through nested `GcRef`s.

## Decision

Allocate every object from a **`bumpalo::Bump` arena** and maintain a
**side `live` registry** (`Vec<NonNull<GcHeader>>`) of every live allocation's
header. The collector is a classical tri-color mark-and-sweep driven off the
registry:

- **Mark** — start from the root set, grey-worklist through each reached
  object's descriptor `trace` callback, coloring headers via a `Cell<u8>` mark
  in `GcHeader`. No write barrier (§12.1).
- **Sweep** — iterate the `live` registry; any header still white gets its
  descriptor `drop_value` called (§12.5) and is dropped from the registry.

This is precise (the registry knows every allocation exactly) and non-moving
(object addresses never change, satisfying §12.1's "stable object addresses").

## Reason

- Bumpalo gives fast, simple bump allocation with no `unsafe` allocator
  boilerplate of our own; the registry gives precise liveness without a
  heap-walking scan (we never need to recover object boundaries, which the
  design does not specify).
- Non-moving addresses keep future Rust wrapper types (§11.2) simple and let
  debugger snapshots retain references safely (§12.1).
- It is the simplest correct design — the workspace prefers closed simple
  implementations (rule 20.10) and defers optimization until a benchmark
  identifies a bottleneck (rule 20.11).

## Consequences

- Memory freed by sweep is **not returned to the arena** until the arena is
  reset; only the liveness registration is removed. For AoC-scale short-lived
  workloads (§12.1) this is acceptable. A free-list / arena-reset policy can
  land later without changing the descriptor or `GcRef` ABI.
- The `Heap` owns the arena + registry and stays `#[repr(C)]` so the
  `RuntimeContext.heap` pointer offset is stable (Appendix B).
- `praxis-runtime` gains a dependency on `bumpalo`.
