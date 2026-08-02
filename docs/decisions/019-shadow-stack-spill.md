# ADR-019: Compiler-managed shadow-stack spill

**Date:** 2026-07-23 · **Status:** accepted

> **Amended by ADR-100 (2026-08-02): the frame is no longer an allocation, and
> the chain is no longer a chain.** Decision 1's per-frame `Box` and `parent`
> pointer, and decision 3's two extern helpers, are retired for one contiguous
> runtime-owned region that generated code bump-allocates inline. What survives
> is everything else: `#[repr(C)]` with a compile-time-derived offset the
> backend emits directly, raw nullable slots rather than `GcRef`, a `RootSet`
> impl as the collector's only door, and the obligation that every function
> claims exactly one frame and every return path gives it back. Decisions 2, 4
> and 5 are unchanged in substance — `RuntimeContext`'s field keeps its position
> and width (it is now `shadow`), the spill still writes `LocalId → slot_index`
> before the safepoint, and pacing is untouched. The body below is kept as
> written, because the reasoning for a per-frame allocation is what a future
> reader will be tempted by, and the Consequences bullet on prologue overhead is
> the sentence ADR-100 exists to answer.

## Context

§12.3 requires compiler-managed root tracking so the non-moving mark-and-sweep
collector (ADR-011) knows which `GcRef`s are live across GC safepoints. ADR-016
implemented the backward-dataflow liveness pass that computes the minimal
`live_roots` set per safepoint (`Alloc`/`Materialize`/`Call`), and ADR-012
shipped host-side explicit root frames (`RootScope`) for M3.

But M4's Cranelift backend **never spilled** those `live_roots` into a structure
the collector could see. This was harmless in M4 because `run.rs` never triggered
a collection during JIT execution — only the host could call `Runtime::collect`.
M5 makes the spill load-bearing: `Vec.push` allocates, and the §19.5 acceptance
criterion "vector growth and nested vectors survive collection" requires a GC
that actually runs during execution with correct roots.

## Decision

Implement a compiler-managed shadow stack (the §12.3 "shadow-stack frames"
option, not the explicit-root-frames option):

1. **`ShadowFrame`** (`praxis-runtime/src/shadow_frame.rs`): a `#[repr(C)]`
   struct with a fixed-capacity `[*mut GcHeader; 64]` slot array + a `parent`
   pointer chaining to the caller's frame. Implements `RootSet` so the collector
   walks the whole chain. Slots are raw nullable pointers (not `GcRef`) because
   a slot is null until the backend writes a value — `GcRef` is `NonNull` by
   construction and cannot represent that state.

2. **`RuntimeContext.roots: *mut ShadowFrame`** — the current top of the root
   chain. ABI version bumped to v2 (the `RUNTIME_ABI_VERSION` check catches any
   stale codegen).

3. **Cranelift prologue/epilogue:** every generated function calls
   `praxis_push_shadow_frame(ctx, gc_local_count)` in the prologue (chains onto
   `ctx.roots`, returns the frame pointer) and `praxis_pop_shadow_frame(ctx,
   frame)` in the epilogue (including the fault epilogue).

4. **Spill at safepoints:** at each `Alloc`/`Materialize`/`Call`, the backend
   stores each `LocalId` in `live_roots` into its frame slot
   (`frame_ptr + SLOTS_OFFSET + slot_index * 8`) *before* the safepointing call.
   The Gc-local → slot-index map is built once per function.

5. **Automatic GC:** `Heap::maybe_collect(roots)` runs when allocation pressure
   crosses a geometrically-growing threshold (64 KiB initial, doubles after each
   collection). The `praxis_alloc_*` / `praxis_vec_push` wrappers call it,
   rooting from `ctx.roots`. This is what makes "survives collection" testable.

## Reason

- The non-moving collector (ADR-011) means spilled roots never need updating —
  the `GcRef` addresses are stable for the object's lifetime.
- A fixed slot array (not a `Vec`) keeps each frame a single allocation with no
  indirection; the slot offset is a compile-time constant the backend emits
  directly. The 64-slot cap is checked at compile time.
- Automatic GC on allocation pressure is the only way the §19.5 acceptance
  criterion ("survives collection") is meaningfully testable from JIT'd code;
  host-only collection would not exercise the spill.

## Consequences

- The ABI version bump (v1 → v2) means any stale compiled artifact is caught at
  startup by `assert_abi_version`.
- Every function now has a prologue/epilogue overhead (two extern calls + frame
  allocation). This is acceptable for a puzzle-solving language; a future
  optimization can elide frames for functions with no safepoints.
- The spill's correctness is proven by `fib(20)` (6765, ~20k allocations,
  multiple GC cycles) and a 10k-iteration allocation loop both producing correct
  results.
