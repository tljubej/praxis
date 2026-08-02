# ADR-103: A page owns the storage and the liveness, and the registry is gone

**Date:** 2026-08-02
**Status:** Accepted
**Milestone:** Performance (handover 21, finding §3.6)
**Amends:** ADR-011's storage decision (the `bumpalo::Bump` arena and the side
`live` registry are both replaced); ADR-039's Decision 3 (poisoning now precedes
clearing a bitmap bit rather than dropping a registry entry — the guarantee is
unchanged, the mechanism is not)

## Context

ADR-011 chose a `bumpalo::Bump` arena plus a side `live: Vec<NonNull<GcHeader>>`
registry, and said why: bump allocation needs no `unsafe` allocator of our own,
and the registry gives precise liveness "without a heap-walking scan (we never
need to recover object boundaries, which the design does not specify)".

Three costs accumulated behind that choice.

1. **Every allocation wrote to a side structure.** `alloc_raw` pushed a header
   pointer onto a `Vec` that, under the doubling pacer, grew to tens of millions
   of entries.
2. **Every sweep walked it in full**, touching every *surviving* object twice —
   once to test its mark colour and once to reset it to white — and doing a
   `swap_remove` per dead one.
3. **Nothing was ever given back.** A `Bump` reclaims everything or nothing.
   §3.1 fixed the *lookup* (a SipHash-keyed `HashMap` became an array indexed by
   size class) but not the shape: a free bucket keyed by layout is dead capital
   for every other layout, and the registry's eight bytes per object were pure
   overhead on top.

The measured consequence was memory. On `mandelbrot` the process peaked at
1.03 GiB; on `tree`, 787 MiB. Handover 21 §3.6 predicted the registry was also
the bulk of the remaining *time* — 36% in `alloc_raw`, 24% in `collect_inner`.
That profile was taken before §3.1, §3.3 and §3.5 landed. It no longer held (see
Consequences).

## Decision 1: storage is a size-class page, and the page's base is a mask

Every object's `[GcHeader | payload]` block lives on a **page**: one 32 KiB
allocation, aligned to 32 KiB, whose first 424 bytes are a `PageHeader` and
whose remainder is an array of equal-sized blocks. Because the base is aligned
to the page size, `page_of(p)` is `p & !(PAGE_SIZE - 1)` — no side table, which
would have reintroduced exactly the per-object hash lookup §3.1 removed.

The ladder is **8-byte granular**, from a bare header (24 bytes) to 128 bytes:
14 rungs. Not powers of two — a power-of-two ladder would round a `Vec`'s
56-byte block to 64 and a `Map`'s 88 to 128, making composites *bigger* than the
arena gave them. Every one of the 22 descriptors in `BUILTINS` lands on a rung
with under 8 bytes of waste, and `the_ladder_covers_every_builtin_descriptor`
makes that a checked invariant rather than a hope: a twenty-third built-in with
a bigger payload fails that test instead of quietly costing a page each.

A descriptor whose payload is larger than the ladder, or aligned more strictly
than a `GcHeader`, gets a page of its own holding one block. No production
descriptor takes that path; only `heap::tests::OVERALIGNED` does, and it costs a
whole page, which is right for a fixture and would be wrong for anything a
program allocates in a loop.

Recovering an object's index within its page is a multiply-shift: subtract
`first_block`, multiply by a Lemire reciprocal, shift 32. ADR-011 declined to
recover object boundaries because the design did not specify them; size-class
segregation is what makes recovery arithmetic rather than a scan, so that reason
expired. The reciprocal is a derivation, and this codebase pins derivations —
`the_reciprocal_divides_exactly_for_every_stride_and_offset` checks it against
`/` exhaustively, for every rung and every offset a page can address.

## Decision 2: the allocated bitmap is the registry, and it is a bitmap on purpose

A page carries two bitmaps: `allocated` (1 = this block holds an initialized
object) and `mark` (1 = the mark phase reached it this cycle).

`allocated` replaces the `live` registry *and* the free list. Allocation claims
the lowest clear bit at or above a per-page cursor; sweep computes `allocated &
!mark` a word at a time; `finalize_all` enumerates every live object by walking
`allocated`, which is the answer to "without a registry, how do you find
everything at teardown" (RT-02).

**It is a bitmap rather than a linked list through free blocks, and that is
soundness rather than taste.** Threading a next-pointer through a dead block's
first eight bytes would overwrite its `descriptor`, so `is_poisoned()` would
report filed storage as a typed object — hazard H7 exactly. Nothing is ever
written into a free block: it keeps the null descriptor `poison` left there
until the allocator re-heads it.

The mark bit moving out of the header is what deletes sweep's per-survivor
store. `WHITE`/`GREY`/`BLACK` go with it: there are two colours in a bitmap, and
the grey set *is* the mark worklist, which the old code's own comment already
said.

## Decision 3: an emptied page is re-classed, and no page is ever unmapped while its heap lives

A page that sweep empties goes to the heap's own pool, and the next class that
needs a page takes it and re-classes it. That is where the memory win comes
from: storage is reusable across *layouts*, where a per-layout free list left an
emptied bucket useless to everything else.

**Pages are never returned to the global allocator while the heap lives.** That
is a soundness rule, not a policy. The whole stale-reference story — a `GcRef`
naming swept storage is *rejected*, not followed — requires the storage to still
be readable, which is what `bumpalo` gave for free by never releasing a chunk.
`Heap::drop` releases them all, in the same window hazard H8 already documents,
after `finalize_all`.

`Heap::reset` keeps every page for the same reason, clears every bitmap, and
re-stamps every page with the freshly minted `HeapId` — so a reference minted
before the reset now fails *both* the provenance check and the allocated-bit
check.

## Decision 4: an immortal is a page flag, not a registry omission

`alloc_immortal` used to allocate through `alloc_raw` and then linear-scan the
registry to un-register what it had just registered. With ~1281 interned small
`Int`s (ADR-100) that scan was quadratic in the table.

An immortal now comes from a page flagged `IMMORTAL`, which sweep and
`finalize_all` skip: no bit of its `allocated` bitmap is ever cleared and
nothing on it is ever finalized. That is a stronger statement than the omission
was — there is no window in which an immortal is momentarily collectable — and
it is why `Immortals::new` no longer pre-colours the three singletons black. The
pre-colouring was transient protection against a mark phase that happened to
visit them; the flag is permanent.

Sweep does clear an immortal page's *mark* bits, which costs three pages' worth
of word stores per collection. Leaving them set would make the next cycle stop
at an immortal instead of tracing through it. Every immortal payload is a scalar
with no children today, so it would be harmless — but "harmless because of what
the payload happens to be" is not an invariant.

Immortal pages carry the heap's own `HeapId`, not zero, because
`context::tests::a_runtimes_immortals_belong_to_its_own_live_heap` requires
`rt.heap().owns(cached)` to answer true.

## Decision 5: the header keeps `heap_id`, and that is what makes the mask sound

`page_of` is arithmetic. Applied to an address that is not inside a page, it
yields a wild pointer, and the mark phase starts from a bare `GcRef` it did not
choose.

The header therefore **keeps** its `heap_id`, and `Heap::mark` reads it *first*:

1. `header.heap_id() != Some(self.id)` — skip. This is ADR-039 Decision 2
   verbatim, still before the first dereference of anything the header points
   at.
2. Only then mask to the page and set the block's mark bit.

Only this heap's allocator writes this heap's id into a header, and it only ever
writes one into a block on one of this heap's pages. So a header that passes (1)
is inside a page, and the mask is arithmetic on an address whose provenance is
already established. A `GcHeader` that lives anywhere else — `GcHeader::detached`
in the root-set tests, `abi::tests::dirty_padded_false`'s hand-laid `Bool` —
carries either zero or a freshly minted id no live heap holds, and is rejected
at (1).

The alternative the plan for this change proposed was to move `heap_id` out of
the header entirely and answer provenance from the page, adding a
`DetachedObject` guard so that "a `GcHeader` outside a page" had no spelling.
That is a larger change (it rewrites five fixtures across four files, one of them
byte-exact padding) for a strictly weaker guarantee: it makes the mask
unconditional, so its soundness rests on having found every fixture, where
keeping the header's copy makes the mask conditional on a check the collector
was already doing. The `heap_id` field costs nothing — removing it would not
shrink the header, which is 20 bytes of fields in 24 bytes of `#[repr(C)]`
padding either way.

## Consequences

- **No ABI bump, no codegen change.** `GcHeader` is still 24 bytes and
  `payload_offset_for(8)` still returns 24, so the immediate `Inst::EnumTag`
  folds into an `iadd_imm` is unchanged.
  `gc::tests::removing_the_mark_byte_did_not_move_the_payload_offset` pins it.
  ADR-039's three Decisions all remain literally true: `payload_offset` is still
  recorded in the header by the one function that computes it, `heap_id` is still
  per-allocation provenance the mark phase enforces before any dereference, and
  sweep still poisons before the storage becomes reclaimable.
- **`praxis-runtime` no longer depends on `bumpalo`.** The workspace still does
  (`praxis-codegen-cranelift`, `praxis-input-parser`).
- **Peak RSS falls by 1.3× to 1.8×** across the suite: `vm` 253 → 138 MiB,
  `mandelbrot` 1032 → 573 MiB, `tree` 787 → 518 MiB, `bfs` 764 → 577 MiB,
  `collatz` 111 → 72 MiB. That is the registry's eight bytes per object plus
  cross-class page reuse; it is *not* the header shrink, which is not in this
  change.
- **Time is roughly neutral, and the finding's headline profile no longer held.**
  Handover 21 §3.6 measured `alloc_raw` at 36% and `collect_inner` at 24% of
  `collatz`. On the tree this landed on — with §3.1, §3.3 and §3.5 already in —
  the same profile reads 6.1% and 6.6%. This change takes `collect_inner` to
  1.1% and puts allocation up to 7.7%, for a net 31% reduction in collector time
  on `collatz` and a 0–16% wall-clock improvement across the suite
  (`bfs` 1.16×, `hashwork` 1.10×, `mandelbrot` 1.10×, `collatz` and `tree`
  unchanged). The bitmap claim is a little dearer than popping a `Vec`; the
  sweep is six times cheaper.
- **`Heap` is `Cell`-only.** The two `RefCell`s are gone, so allocation pays no
  borrow-flag traffic.
- **`Heap::committed_bytes()` and `Heap::page_count()`** replace
  `Bump::allocated_bytes()` for RT-01's test.
- **A `Heap` that never allocates creates no page**, which matters because the
  debugger mints a second heap (ADR-032).
- **A page is 32 KiB, so a program that allocates one object of a rare class pays
  a whole page for it.** With 14 rungs the worst case is 448 KiB of slack, and an
  emptied page is re-classed rather than stranded.
- **The natural end state is not this.** Segregating pages by *descriptor*
  instead of by size class would make `descriptor`, `payload_offset` and `size`
  all page constants and take `GcHeader` to nothing — the descriptor set is
  bounded at 22 built-ins. It is out of scope here because it changes what a
  `GcRef` points at and touches all 122 `payload::<T>()` sites. Anyone who shrinks
  the header to 8 bytes first should know that work is wasted if that end state
  is ever taken.
