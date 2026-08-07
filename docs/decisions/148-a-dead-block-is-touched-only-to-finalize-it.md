# ADR-148: A dead block is touched only to finalize it, and the page knows whether any of its blocks needs it

**Date:** 2026-08-07
**Status:** Accepted — **not yet implemented.** The Measurements below are a
prototype, measured and then reverted; Consequences lists what implementing it
still owes. A reader looking for the "— implemented" marker should not find one.
**Milestone:** post-M11 performance
**Amends:** ADR-103's sweep. The page still owns the storage and the liveness and
the sweep is still one pass over the bitmaps; what changes is that reclaiming a
block no longer implies *reading* it. It also narrows the poison contract
ADR-039 decision 2 introduced — see Decision 4, which does not delete poison but
stops it being the only thing standing between a stale root and a traced dead
object.

## Context

### The complaint

"Can we make the GC faster." The collector had never been profiled as a whole
since ADR-103 rebuilt it; the per-collection *fixed* cost had been cut twice
(ADR-114, ADR-128) and ADR-129 had priced the ceiling against the result, but
nobody had asked where the time inside a collection goes.

### Where the time is

`/usr/bin/sample` at 1 ms over each benchmark at its `sizes.json` size,
inclusive share of `Heap::collect_inner`:

| benchmark | GC share of wall clock |
| --- | --- |
| `pipeline` | **37.0%** |
| `tree` | **25.3%** |
| `hashwork` | **12.4%** |
| `vm` | 2.1% |
| `bfs` | 1.4% |
| `primes` | 0.7% |
| `collatz`, `mandelbrot` | below the sampler's floor |

Three benchmarks carry all of it, and they are the three that hold a live set.
Splitting the phases with timers inside `collect_inner` (`mark` and `sweep` are
private and inline into it, so the sampler cannot separate them):

| benchmark | collections | mark | sweep | objects marked | blocks freed |
| --- | ---: | ---: | ---: | ---: | ---: |
| `pipeline` | 71 | 264.2 ms | 247.1 ms | 64,785,748 | 106,501,023 |
| `tree` | 49 | 109.0 ms | 57.8 ms | 16,905,560 | 43,258,410 |
| `hashwork` | 183 | 126.7 ms | 52.6 ms | 23,459,600 | 46,078,904 |
| `vm` | 117 | 0.1 ms | 21.1 ms | 5,413 | 19,570,628 |
| `bfs` | 16 | 19.4 ms | 5.4 ms | 4,635,611 | 195,948 |
| `primes` | 14 | 0.0 ms | 1.7 ms | 28 | 1,570,136 |

Roughly 55% mark, 45% sweep. **`vm` and `primes` are the shape that gives the
sweep away**: they mark essentially nothing and still spend all their collection
time in the sweep, because what they do is die in quantity.

### What the sweep does to a block it is about to reclaim

For each bit in `allocated & !marked`, `Heap::sweep` reads the block's
`GcHeader` to get its descriptor, calls `descriptor.drop_value` through a
function pointer, and poisons the header. That is a **read and two stores to a
cache line that has not been touched since the object was allocated** — on
`pipeline`, 1.5 M cold lines per collection.

Fitting the table above as `sweep ≈ a × freed + b × words` puts `b` at
approximately zero and `a` between 1.1 ns (`primes`, `vm`) and 2.3 ns
(`pipeline`, whose heap is larger and colder). The bitmap scan the design was
built around costs nothing. **The per-dead-object touch is the sweep.**

### What is actually dead

`drop_value` is a no-op for every scalar — `scalars.rs` says so in a comment
already. Deriving `needs_drop::<P>()` at descriptor construction and counting
the dead by it:

| benchmark | blocks freed | need finalizing | trivial |
| --- | ---: | ---: | ---: |
| `tree` | 43,258,410 | **0** | 100% |
| `hashwork` | 46,078,904 | **0** | 100% |
| `vm` | 19,570,628 | **0** | 100% |
| `primes` | 1,570,136 | **0** | 100% |
| `pipeline` | 106,501,023 | 11,762,429 | **89.0%** |
| `bfs` | 195,948 | 94,361 | 51.8% |

Four of the six free nothing that needs finalizing *at all*, and the collector
reads and poisons every one of those blocks to discover it.

### The alignment that makes this cheap to exploit

To skip a dead block you must know it needs no finalization — and the descriptor
that answers is exactly the cold read the change exists to avoid. So the answer
has to live somewhere already hot: the page.

A per-page flag is only sound if every path that places an object on a page can
maintain it, and generated code claims blocks **inline** since ADR-119 without
calling `Heap::claim_block`. That would be fatal, except for this, across all 22
builtins:

| | `needs_drop` | `owned_bytes` | inline claim site |
| --- | --- | --- | --- |
| `Unit` `Bool` `Int` `Byte` `Char` `Float` `VarCell` `Range` | false | none | **yes** |
| `Text` `Vec` `Deque` `Grid` `Map` `Set` `Counter` `MinHeap` `MaxHeap` `BitSet` `Tuple` `Record` `Enum` `Closure` | true | some | no |

The three columns agree on all 22 rows, and one of the two implications is not a
coincidence but a construction: `InlineClaimSite::of` **already** refuses any
descriptor with an `owned_bytes` charge, because the inline sequence cannot
reproduce a charge that is an indirect call on a payload that does not exist yet
(ADR-119). The other implication — `owned_bytes.is_none() ⟹ !needs_drop` — is
today only an observation, and Decision 3 is what stops it being one.

## Decision 1: a page records whether it has ever held a finalizable object

`PageHeader` gains `FLAG_FINALIZABLE` in the existing `flags: Cell<u8>` beside
`FLAG_IMMORTAL` — no growth in the header, no movement in the size-class ladder,
no ABI bump. It is set in `Heap::claim_block` and `Heap::claim_large_block` when
the claiming descriptor's `needs_drop` is true, and cleared where a page is
emptied and re-purposed: `PageHeader::reclass` and `PageHeader::clear_bitmaps`.

**Sticky until the page empties, and set behind a test.** The store is guarded by
`!page.has_finalizable()` so it happens at most once per page per epoch rather
than once per allocation, which keeps the page header's line clean on the
allocation path. The flag is conservative in the safe direction: a page that once
held a finalizable object keeps the old per-block path until it empties, which
costs time and can never skip a finalization.

## Decision 2: a page with nothing to finalize is swept by bitmap arithmetic

When `!page.has_finalizable()`, the per-word body becomes

```rust
let dead = alive & !marked;
if dead != 0 {
    freed += dead.count_ones();
    page.set_allocated_word(word, alive & marked);
}
if marked != 0 {
    page.clear_mark_word(word);
}
```

and **no block on the page is read or written**. `freed`, `release_blocks`,
`live_count` and the `live_count × block_size` live-set measurement are
unchanged, so ADR-112's pacing input is the same number it was. Immortal pages
still `continue` above this, unchanged.

`Heap::finalize_all` is untouched and remains correct: it finalizes whatever is
still `allocated` at teardown, and the fast path clears the `allocated` bit of
everything it reclaims. Nothing is finalized twice and nothing that owns bytes is
skipped.

## Decision 3: `needs_drop` is derived, and the inline claim site refuses it

`TypeDescriptor` gains `needs_drop: bool`, set in `builtin::<P>` and
`for_test::<P>` from `std::mem::needs_drop::<P>()` and carried through
`with_owned_bytes`. It is **derived, never declared** — the same discipline as
`size` and `align`, and for the reason ADR-039 decision 1 gives: a hand-written
answer is a second authority that can disagree with the first. Every
`drop_value` in the runtime is `drop_in_place::<P>` for the descriptor's own
payload or an empty function, so `needs_drop::<P>()` is exactly the predicate.

And `InlineClaimSite::of` gains a second refusal: `descriptor.needs_drop`
returns `None`. **This is what turns the table above from an observation into a
construction.** Today the new clause refuses nothing that the `owned_bytes`
clause does not already refuse, so it costs no inline claim site in the
language as it stands. What it buys is that a future descriptor that owns
nothing measurable but still has a `Drop` — the one shape that would let
generated code place a finalizable object on a page whose flag nobody set —
loses its inline claim site instead of silently leaking. The alternative was a
test asserting the two columns agree, and a test says *that they agree today*
where a refusal says *that they must*.

## Decision 4: the mark phase tests the `allocated` bit, and poison keeps only its finalization role

Poison has two jobs. It marks storage as finalized, and it is what makes a stale
`GcRef` reaching `Heap::mark` fail the provenance check (`heap_id` reads 0) —
hazard H7. Decision 2 stops poisoning blocks on clean pages, so the second job
needs its own answer.

The mark loop already knows the block index and already computes it, and the
`allocated` bitmap is in the page header it just touched. So the loop's

```rust
debug_assert!(page.is_allocated(index), "a live header on a free block");
```

becomes a real check that `continue`s. This is not weaker than poison: both
reject a reference to a reclaimed block and neither rejects one to a block that
has since been reissued, which is why ADR-106 puts the weak scan between the
sweep and the next allocation.

**It is not free and it is priced here rather than netted away.** On an
unbalanced five-repetition sweep it cost about a point of the geometric mean —
1.0215× with the check against 1.0319× without — and it should be re-measured
against the balanced harness when this lands. It is kept because in release
builds today the `debug_assert` compiles out and poison is the *only* thing
between a rooting bug and a traced dead object, and removing the last one to buy
a point is not a trade this collector should make.

## Decision 5: the one production reader of `is_poisoned` moves to the `allocated` bit

`GcHeader::is_poisoned` has exactly one non-test production caller:
`DebugFrameStackHeader::clear_reclaimed` (`debug.rs:719`, ADR-106's weak arm),
which asks "did this die in the collection that just ran". It moves to the same
question the mark phase now asks — is this block still allocated — through a
`Heap` helper that keeps the provenance check in front of the page derivation,
because a weak slot may name another heap's object.

Nine tests in `praxis-runtime` assert poison semantics directly and are rewritten
to assert reclamation instead; the prototype run left 446 passing and exactly
those 9 failing, which is the whole blast radius and is listed in Consequences.

## Decision 6: the childless-`trace` skip is **rejected**

After Decision 2 the mark phase is 95% of GC time on `tree` and `hashwork`, so
the obvious next move was measured and is recorded here so it is not measured
again. Counting marked objects by type:

| benchmark | composition of the marked set |
| --- | --- |
| `pipeline` | `Int` **100%** (64,785,462 of 64,785,748) |
| `hashwork` | `Int` **100%** |
| `tree` | `Int` 67%, `Record` 33% |
| `bfs` | `Int` 76%, `Vec` 24% |

`Int`'s `trace` is an empty function, so `pipeline` and `hashwork` make tens of
millions of indirect calls per run that do nothing. Marking the eight childless
descriptors (`Unit` `Bool` `Int` `Byte` `Char` `Float` `BitSet` `Range`) and
skipping the call measured **1.0041× on the geometric mean** — inside the noise
floor of every benchmark it could affect.

The reason is worth keeping: the descriptor pointer is at offset 0 of the header,
the *same cache line* the mark loop has already read `heap_id` from, so the load
that finds the callback is already paid; and a call site that sees one target for
64 M consecutive iterations is predicted perfectly. There is no win to take. **A
flag on every descriptor in the language is not worth 0.4%**, and mark's cost is
its pointer chase, not its calls.

## Measurements

Both arms ship behind environment toggles in one binary, so the A/B is one
executable run twice — `PRAXIS_GC_PACER`'s precedent (ADR-112 decision 4) and
handover 26 §6's rule. Twelve repetitions, four arms, **each arm occupying each
position within a repetition exactly three times** so no arm is favoured by a
systematically fast slot; every arm's stdout compared byte-for-byte against the
baseline's before any timing is reported.

The `noise` column is the base arm's own median-over-minimum spread, and it is
printed because two of these rows cannot resolve the effect they are being asked
about.

| benchmark | noise | sweep (D1–D5) | mark (D6) | both |
| --- | ---: | ---: | ---: | ---: |
| `pipeline` | 1.8% | **1.1031 / 1.1043** | 1.0013 / 1.0109 | 1.1091 / 1.1132 |
| `tree` | 2.9% | **1.1199 / 1.0766** | 1.0145 / 1.0045 | 1.1008 / 1.0725 |
| `vm` | 0.7% | **1.0446 / 1.0359** | 1.0085 / 1.0044 | 1.0425 / 1.0396 |
| `hashwork` | 1.8% | **1.0327 / 1.0294** | 0.9950 / 0.9768 | 1.0362 / 1.0407 |
| `primes` | 0.4% | 1.0070 / 1.0060 | 0.9994 / 0.9988 | 1.0103 / 1.0076 |
| `mandelbrot` | 0.3% | 1.0023 / 1.0022 | 1.0002 / 0.9998 | 1.0025 / 1.0018 |
| `collatz` | 1.7% | 1.0011 / 0.9987 | 1.0045 / 1.0014 | 0.9973 / 0.9997 |
| `bfs` | **8.6%** | 1.0021 / 0.9942 | 1.0098 / 1.0272 | 1.0076 / 1.0187 |
| **geometric mean** | | **1.0382 / 1.0302** | 1.0041 / 1.0029 | 1.0375 / 1.0361 |

Each cell is *minimum ratio / median ratio*; above 1.00 the arm is faster.

### What the sweep phase itself does

Measured with the same in-collector timers, sweep milliseconds for the whole run:

| benchmark | before | after |
| --- | ---: | ---: |
| `vm` | 26.2 | **0.5** |
| `hashwork` | 64.6 | **1.5** |
| `tree` | 68.6 | **5.4** |
| `pipeline` | 274.4 | **137.4** |
| `bfs` | 5.3 | 4.8 |

`pipeline` only halves, and that is the design working rather than falling
short: 11% of its dead objects are `Tuple`s, which own a `Vec`, so their pages
keep `FLAG_FINALIZABLE` and take the per-block path exactly as they must.

### Why `bfs` is not evidence either way

`bfs` spends **1.4%** of its wall clock in the collector, so no arm here can move
it by more than about a point — and its own base arm has an **8.6%**
median-over-minimum spread. Every `bfs` figure above is noise. An earlier,
unbalanced five-repetition run showed it "regressing" 7–8% in three arms whose
mechanisms cannot interact, which is what prompted the balanced harness; it is
recorded because the next person to measure this suite will see the same thing.

### Peak resident set

Unchanged on all eight, checked with `/usr/bin/time -l`. This removes collector
work rather than allocations, so
[the pacer's dependency on allocation volume](./121-a-slot-that-holds-a-scalar-is-not-a-box.md)
(decision 13) is not touched: `bytes_since_collect` is charged at allocation and
nothing here allocates less. An earlier *unsound* probe that skipped finalization
entirely took `pipeline` from 106 to 286 MiB, which is the leak that decision 1's
flag exists to prevent, and it is the reason the flag is per page rather than a
global "this program has no finalizable types".

## Consequences

**This decision is recorded before its implementation, per this directory's
README.** What it still owes:

1. Decisions 1–5 implemented against the tree, not behind an environment toggle.
   The toggles existed to make the A/B one binary and have no place in the
   shipped collector — there is no second pacing rule here worth keeping around.
2. The nine `praxis-runtime` tests that assert poison semantics rewritten to
   assert reclamation: `sweeping_poisons_the_reclaimed_header`,
   `a_swept_reference_is_not_traced_again`,
   `a_reclaimed_block_is_reused_for_the_next_object_of_its_layout`,
   `the_weak_scan_nulls_only_what_this_collection_reclaimed`,
   `the_weak_scan_runs_after_the_sweep_and_before_the_block_is_reissued`,
   `a_weak_slot_whose_object_died_becomes_an_absence`,
   `the_same_word_in_a_reference_slot_is_still_cleared_by_the_scan`,
   `a_zero_local_frame_neither_holds_nor_clears_anything`, and
   `a_reissued_block_is_not_rendered_under_the_dead_locals_name`.
3. A test that a page's `FLAG_FINALIZABLE` is cleared by both `reclass` and
   `clear_bitmaps` — the flag is sticky, so a page that kept it across a
   re-class would quietly never take the fast path again, which is a *slow*
   failure and therefore an invisible one.
4. A test that `InlineClaimSite::of` refuses a `needs_drop` descriptor
   (Decision 3), which is the clause holding up Decision 1's soundness.
5. Decision 4's cost re-measured on the balanced harness rather than carried at
   the approximate figure this record quotes.
6. The ASan job matters more than usual here: this changes when a block's header
   is written and stops writing it in one case, and `just asan` is the gate that
   would catch a stale read the mark phase's new check does not.

**What this does not change.** The collector is still precise, non-moving,
single-threaded and without a write barrier (§12.1). Sweep is still one pass over
the page list, still measures the live set in block bytes for ADR-112, and still
does not touch a *survivor* — ADR-103's property is preserved, and this record
extends it to most of the dead. No header field moves, `RUNTIME_ABI_VERSION` does
not change, and generated code is not re-emitted.

**What is left after this.** Mark becomes roughly 95% of GC time on `tree` and
`hashwork` and about two thirds on `pipeline`, at 4.1–6.4 ns per object visited,
and Decision 6 establishes there is no cheap constant factor left in it. The
remaining structural cost is that a stable live set is re-marked from scratch on
every collection — `pipeline` marks 913 k objects 71 times over — which is what a
generational collector exists to avoid and what §12.1's "no write barrier" rules
out today. That is the next question about this collector, and it is a much
larger one than this record.
