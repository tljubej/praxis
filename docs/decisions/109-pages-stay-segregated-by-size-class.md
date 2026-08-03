# ADR-109: Pages stay segregated by size class, and the header shrinks instead

**Date:** 2026-08-03
**Status:** Accepted
**Milestone:** post-M11 (handover 23, question Q-1 and item P-3)
**Amends:** ADR-103's final Consequence ("the natural end state is not this") —
descriptor segregation is now **rejected**, not deferred; ADR-039's Consequences
(the header's field list and its byte count).

## Context

ADR-103 chose size-class pages — 14 rungs from 24 to 128 bytes, `GcHeader`
retained at 24 bytes, no ABI bump, no codegen change — and its own Consequences
named the alternative and called it the natural end state:

> Segregating pages by *descriptor* instead of by size class would make
> `descriptor`, `payload_offset` and `size` all page constants and take
> `GcHeader` to nothing […] Anyone who shrinks the header to 8 bytes first should
> know that work is wasted if that end state is ever taken.

That sentence has been blocking two items. Handover 23 recorded P-3 (shrink the
header) as "do not start this before answering Q-1", and P-1 (the inline
allocation fast path) as depending on Q-1 for which page-metadata offsets it
would bake into generated code. So the question had to be answered before either
could start, and this ADR answers it rather than deferring it a third time.

## Decision 1: pages stay segregated by size class

The argument everyone reaches for first is memory stranding, and it does **not**
carry. Counted against this tree: the 22 built-in descriptors reach only **eight
distinct rungs** (24/32/40/48/56/64/80/88), so descriptor-segregated pages cost
22 × 32 KiB = 704 KiB in the worst case against the 8 × 32 KiB = 256 KiB that is
reachable today — a 448 KiB delta, against a suite whose benchmarks peak between
507 MiB and 3.19 GiB. That is 0.09%. Anyone who rejects descriptor segregation
on stranding grounds has rejected it for a reason that is not true, and would be
right for the wrong reason; the number is recorded here so it does not have to be
re-derived.

**What rejects it is provenance ordering.** `Heap::mark` reads `header.heap_id()`
and refuses a reference this heap did not allocate *before* it applies `page_of`:

1. `header.heap_id() != Some(self.id)` — skip. ADR-039 Decision 2 verbatim,
   still before the first dereference of anything the header points at.
2. Only then mask to the page and set the block's mark bit.

ADR-103 Decision 5 is explicit that this ordering is what makes the mask
"arithmetic on an address whose provenance is already established" rather than a
wild pointer. Descriptor segregation deletes the header. With no per-object word,
there is **nothing left to read first** — `page_of` must be applied to an
unvalidated `GcRef` and the result dereferenced to find out whether the reference
was ours. That is precisely the unconditional mask ADR-103 already weighed and
called "a strictly weaker guarantee", arriving as a consequence of the layout
rather than as a proposal, which makes it harder to notice rather than easier.

Two further costs, both on paths that were recently made fast on purpose:

- **ADR-102's inline type proof gets slower.** The check is one load of the
  descriptor from the object's own cache line — the line the payload read is
  about to touch anyway. Read from a page header it becomes an `and` plus a load
  from a *different* line, on a path whose whole point was that it is a branch
  and not a call.
- **The poisoned-descriptor refusal loses its subject.** Sweep poisons a block by
  nulling its descriptor, and `is_poisoned()` is what `emit_scalar_load`'s cold
  path relies on for REP-56. A per-page descriptor is shared by every block on
  the page, so it cannot record that one block is filed storage. ADR-103
  Decision 2 already rejected a free list threaded through dead blocks for
  exactly this reason (hazard H7); descriptor segregation reintroduces the hazard
  from the other side.

So the answer to Q-1 is **size class**, and it is settled rather than postponed.
An 8-byte `{heap_id}`-only header on descriptor-segregated pages would keep the
ordering and is the only version of the idea worth revisiting; it costs 4–6 days
and is not on the table until something asks for it.

## Decision 2: the header goes to 16 bytes by deleting a field with no readers

Because the header survives, P-3 is not wasted work — and it is much smaller than
handover 23 describes.

`GcHeader.size: u32` has **zero readers in the workspace.** Every `.size()` call
site is `TypeDescriptor::size`. Deleting the field and its accessor takes
`#[repr(C)] GcHeader` from 24 bytes to 16 — `descriptor` at 0..8,
`payload_offset` at 8..10, padding, `heap_id` at 12..16 — and every allocated
block in the heap loses exactly eight bytes. An `Int` goes 32 → 24; a `Map`,
88 → 80.

The change is a one-field deletion because `page::MIN_BLOCK` and `BLOCK_GRANULE`
are already *derived* from the header, so the ladder, the block count, the bitmap
width and the page header's size all follow without an edit. That derivation was
never checked by anything; it is now, because it is what made this cheap.

**It stops at 16 bytes rather than the 8 handover 23 asks for**, and the trade is
deliberate. Reaching 8 means deleting `heap_id`, which forfeits ADR-039
Decision 2's read-before-mask ordering — the same property Decision 1 above just
refused to give up for a much larger prize. It also means deleting
`payload_offset` and deriving it from `descriptor().align()`, which adds a
dependent load to every one of the 187 `payload::<T>()` sites. One further rung
of shrink is not worth either.

All three of ADR-039's Decisions stay literally true: `payload_offset` is still
recorded in the header by the one function that computes it, `heap_id` is still
per-allocation provenance the mark phase enforces before any dereference, and
sweep still poisons before storage becomes reclaimable.

## Decision 3: the two tuning questions under Q-1 are measurements, and only one is

Handover 23 files `PAGE_SIZE` and page-return-to-the-OS as "two smaller tuning
questions, both one constant and both measurable rather than arguable." One of
those descriptions is right and one is not.

**`PAGE_SIZE` is genuinely one constant.** Everything downstream — `PAGE_MASK`,
`MAX_BLOCKS`, `BITMAP_WORDS`, both `const` assertions — is derived, and both
derivation-pinning tests re-derive automatically. So it is exactly the experiment
the handover says it is. The handover's metadata figures are wrong, and so were
the corrections first written here: they were computed against the 24-byte
header that Decision 2 above then deleted, which halves `MIN_BLOCK`'s divisor and
doubles every bitmap. Measured on the tree Decision 2 actually landed on, where
`MIN_BLOCK` is 16 and a `PageHeader` is 72 fixed bytes plus two bitmaps:

| `PAGE_SIZE` | `MAX_BLOCKS` | `BITMAP_WORDS` | `PageHeader` | share of a page |
| --- | --- | --- | --- | --- |
| 16 KiB | 1024 | 16 | 328 B | 2.00% |
| **32 KiB (today)** | **2048** | **32** | **584 B** | **1.78%** |
| 64 KiB | 4096 | 64 | 1096 B | 1.67% |

So the metadata share falls as the page grows, and 16 KiB is the only one of the
three that costs more than today rather than less — which is the opposite of the
direction the handover's "3.2%" implies for it. That figure is not the 16 KiB
number at all: 3.34% is what an *8-byte* `MIN_BLOCK` would cost at 32 KiB, where
the bitmaps go from 32 words to 64. The two appear to have been transposed.

Whoever runs this experiment should note that the win is smaller than the table
suggests, because the metadata is not the interesting axis — a bigger page
strands more slack on a rare class, and that is what the measurement is for.
Both `const` assertions in `page.rs` hold at all three sizes (the tightest is
64 KiB, at 4027 blocks against a 4096-bit bitmap).

**Returning emptied pages to the OS is not one constant, and the blocker is not
the one the handover names.** It asks for a soundness argument for
`madvise(MADV_FREE)` on the block region while the header stays mapped. That
argument is available and clean: `MADV_FREE` leaves the mapping valid, and the
bytes read back as either their previous contents or zeros — and *both* encode
poisoned, because the previous contents are the null descriptor sweep wrote and
zeros are a null descriptor with a zero `heap_id`. So `page_of` stays sound,
ADR-039 Decision 2 stays sound, and `is_poisoned()` answers true either way.

The real blocker is provenance of the memory rather than of the references:
pages come from `std::alloc::alloc`, and madvising memory the system allocator
owns is outside its contract. The honest version moves page allocation to
`mmap`/`munmap` — which also makes the page-aligned request natural rather than
over-allocated — and adds a `decommit` on the emptied-page path. That is half a
day and its own decision record, and it is not this one.

## Consequences

- **Q-1 is closed.** P-1 may bake page-metadata offsets knowing the ladder is
  what it will keep baking against, and P-3 is unblocked.
- **Every derived constant moved, and one of them moved the wrong way.**
  `MIN_BLOCK` 24 → 16 and `BLOCK_GRANULE` unchanged at 8, so `NUM_CLASSES` goes
  14 → 15, `MAX_BLOCKS` 1365 → 2048, `BITMAP_WORDS` 22 → 32 and `PageHeader`
  424 → 584 bytes — 1.29% of a page to 1.78%. That last one is a **cost**: a
  finer floor means more blocks to track, so 160 more bytes of metadata per page.
  It is bought back many times over — an `Int` page held 1010 blocks of 32 bytes
  and now holds **1340 of 24**, a third more objects in the same 32 KiB — but it
  is a real counter-entry, and it is why `page.rs` changed size without needing
  an edit. Not one line of it was touched: `MIN_BLOCK` and `BLOCK_GRANULE` are
  written as `size_of::<GcHeader>()` and `align_of::<GcHeader>()`, and everything
  above follows from those two. `gc::tests::the_ladder_floor_follows_the_header`
  now pins that derivation, which nothing did before.
- **The 22 built-ins still reach exactly eight rungs**, each eight bytes below
  where it was: 16/24/32/40/48/56/72/80, against 24/32/40/48/56/64/80/88. Every
  descriptor keeps the *class index* it had, because the floor and every rung
  above it fell by the same eight bytes; `Unit` is still alone on index 0 and
  `Map` is still the largest at index 8. The fifteenth rung is the new **floor**
  — a 16-byte block, which is a bare `GcHeader` and nothing else, and is exactly
  what a `Unit` is — while `MAX_BLOCK` stayed at 128, so the ladder simply got
  one rung longer at the bottom. The stranding number in Decision 1 is therefore
  unchanged at 8 × 32 KiB reachable, and the worst per-rung waste is still under
  a granule;
  `page::tests::the_ladder_covers_every_builtin_descriptor` re-derives and
  passes.
- **`RUNTIME_ABI_VERSION` is bumped to 19.** No source moves — `payload_offset_for`
  is the single `const` authority and `Inst::EnumTag` and `emit_scalar_load`
  still call it — but the immediates they fold change from 24 to 16, so a
  runtime and a compiler from different sides of this change disagree about where
  every payload is. This shares the v19 bump with ADR-105's `stack_left` and the
  interned-`Char` table pointer, all of which land together; the changelog entry
  names all three, because a version is a statement about a build and these are
  one build.
- **The memory win is a density win, and how it reaches peak RSS changed while
  this ADR was being written.** `Heap::occupy` charges the *stride* against
  `bytes_since_collect`, so a 25% narrower `Int` is 25% fewer bytes charged per
  `Int`. Under the doubling pacer that meant "one fewer doubling of
  `collect_threshold`" — a step function. ADR-112 landed first and bounded the
  pacer at `max(min(previous × 2, 64 MiB), live × 2)`, so the mechanism is now a
  directly smaller *live set* under a `3 × live` bound. That makes the win
  cleaner and more attributable than the step function was, but it only appears
  for programs whose peak is live-set-bound rather than pinned at the 64 MiB
  speculative ceiling — which, on this suite, is `tree`, `pipeline`, `bfs` and
  `hashwork`, not `primes` or `collatz`.
- **This measurement is dominated by the pacer and had to be taken after P-2**,
  which it was. Before ADR-112, `benchmarks/REPORT.md` recorded `primes` peaking
  at 967 MiB while holding nothing at all, because peak RSS under the doubling
  pacer was about half of everything the program ever allocated. Measuring an
  8-byte-per-object change against that would have been measuring one noisy
  number against another.
- **Every block boundary in the heap moves by eight bytes**, and handover 23 §5
  records that the page allocator has never been run under a sanitizer — no
  nightly toolchain on the machine — and calls it the single largest untested
  surface in the tree. If anything mysterious shows up after this change, that is
  where to look first.
