# ADR-039: The `GcHeader` owns the object layout, and every allocation carries its heap

**Date:** 2026-07-28
**Status:** Accepted
**Milestone:** Repair (foundation F6, stage S4)
**Amends:** ADR-011's header description (`GcHeader` is 24 bytes, not 16);
ADR-012's root-set contract (a root from another heap is now ignored, not traced)

## Context

Two defects shared one `#[repr(C)]` struct, so they had to be fixed in one
repack (§6 H6 of the repair plan — the header's size is read by JIT-emitted
code, and two separate layout changes mean two chances to desynchronize
generated code from the runtime).

1. **The object layout was calculated in three places, and they disagreed.**
   `Heap::alloc_raw` placed the payload at `round_up(size_of::<GcHeader>(),
   payload_align)`, `GcHeader::payload` read it back at exactly
   `size_of::<GcHeader>()`, and codegen inlined `size_of::<GcHeader>()` a third
   time to reach an enum's tag. For a payload aligned more strictly than the
   header, initialization and access used different addresses — so tracing,
   field access and finalization all ran against the wrong bytes. Latent rather
   than live (no in-tree payload over-aligns), but the three copies are the
   defect, and the address mismatch is only its most visible symptom.

2. **A `GcRef` carried no provenance.** `RootSet::push_roots` can yield a
   reference from any heap, and `Heap::mark` dereferenced whatever it was
   handed. Collecting heap B marked heap A's object black, delaying A's
   reclamation of it; a reference to already-swept storage was traced through a
   finalized payload's descriptor. Neither is expressible as a check the caller
   could have written — the information simply was not in the object.

## Decision 1: `payload_offset` is in the header, and one function computes it

`GcHeader::payload_offset_for(payload_align)` is the object-layout calculation.
`Heap::alloc_raw` calls it to place the payload, stores what it returned in the
header's new `payload_offset: u16`, and `GcHeader::payload` adds that recorded
offset. Generated code calls the same `const fn` and folds it into an
immediate. The three independent copies are gone; the remaining relationship —
"the offset in the header is the offset the allocator used" — is established at
the single point of construction.

The header's fields are now **private to the `gc` module**. The allocator is
the only constructor, so a header whose `payload_offset` disagrees with the
address its payload was initialized at cannot be built.

## Decision 2: `HeapId` is allocation provenance, and the mark phase enforces it

Every `Heap` mints a process-unique `HeapId` (`NonZeroU32`) at construction,
stamps it into every header it allocates, and mints a **fresh** one at `reset`.
`Heap::mark` compares it against `self.id` **before the first dereference of
anything the header points at** and skips the reference otherwise. This is the
O(1) test that closes the foreign-root hole without the `GcRef<'h>` lifetime
branding the audit considered and rejected as XL.

A foreign root is *skipped*, not a panic. Each heap marking only its own
objects is total and needs no caller discipline; a debug panic would make the
same situation a crash in exactly the configuration (the debugger's second
heap, ADR-032) where it is most likely to be hit.

`reset` minting a new id is what closes the dangling-immortal window: the
immortal singletons handed out before a reset name storage the arena is free to
reissue, and they now fail the provenance check instead of being traced.

## Decision 3: sweep poisons before it unregisters

`Heap::sweep` finalizes the payload, then writes `descriptor = null; heap_id =
0` into the header, and only then drops it from the live registry.
`GcHeader::is_poisoned` reads that state and `descriptor()` panics on it rather
than dereferencing a null.

This is the precondition for reusing swept arena storage (RT-01, stage S6):
without poisoning, a free list would upgrade a stale `GcRef` from "points at
dead-but-typed memory" to "points at a live object of a different type", which
is strictly worse than today's leak. Landing it with the repack means the
free-list work inherits the guarantee rather than having to add it.

## Consequences

- `GcHeader` is 24 bytes (8 descriptor + 4 size + 2 payload_offset + 1 mark +
  1 pad + 4 heap_id), align 8. `RUNTIME_ABI_VERSION` and
  `COMPILER_EXPECTED_ABI_VERSION` go to **8**, once, for this change.
- `heap::tests::overaligned_payload_accessor_matches_initialized_address` and
  `heap::tests::foreign_heap_root_cannot_delay_reclamation` are un-ignored and
  passing.
- `Heap::owns(GcRef)` is public: hosts that hold references across heaps can
  ask, instead of assuming.
- An alignment above 32 KiB would not fit `payload_offset`; `alloc_raw` panics
  naming the descriptor rather than truncating. No descriptor comes close.
- All 122 `payload::<T>()` call sites are unchanged — they route through
  `GcHeader::payload`, which is why the offset could move at all.
