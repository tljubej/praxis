# ADR-106: The debug values are the collector's one weak arm

**Date:** 2026-08-03
**Status:** accepted — implemented
**Milestone:** post-M11 correctness (handover 23 defect D-2)
**Amends:** ADR-104's Consequences bullet "a latent soundness question is
unchanged and is *not* this change's to fix" — this is that change. ADR-044's
two-set *contract* is untouched and this ADR exists to keep it that way; what
changes is that the second set is no longer invisible to the collector. ADR-033
decision 1's argument for the eager snapshot copy is strengthened, not replaced.
ADR-039 decision 3's poison ordering and ADR-103 decision 3's "no page is ever
unmapped while its heap lives" are both used as delivered, and ADR-109's Q-1
answer is what keeps the predicate available.

## Context

`RootSlots::dead` nulls a shadow slot the moment its local dies (ADR-044
decision 2, MIR-01). The debug slot for the same local keeps the value, because
MIR-16 says a value that has been produced stays renderable and because the
whole point of ADR-044 was that making the root set exact must not make the
debugger render `<uninit>` for a binding the user can still see in their source.

Those two sentences describe a window. Between a local's last use and the
program's fault, the crash debugger names an object that **no arm of
`RuntimeRoots` reaches**. A collection in that window is entitled to reclaim it,
and does.

What happens next is worth spelling out, because the two outcomes are not
equally bad and only one of them is the reason this could not be fixed at the
edge.

`Heap::sweep` finalizes the block, calls `GcHeader::poison` — which nulls the
descriptor and the heap id — and clears the block's `allocated` bit so the block
can be reissued (RT-01). If the program faults *before* anything allocates,
`praxis_snapshot_debug_chain` copies a `GcRef` to a poisoned header, and the
first thing the debugger does with it panics: `GcHeader::descriptor` asserts
`"descriptor read from a poisoned (swept) GcHeader"`. Loud, and across the ABI
boundary, but not silent.

If anything allocates first — and a program that just collected is a program
that is allocating — `PageHeader::claim_free_block` hands that exact block to
the next object of its layout and writes a fresh header over the poison.
`a_reclaimed_block_is_reused_for_the_next_object_of_its_layout` shows the reuse
is deterministic: the next allocation of a class takes the *lowest* free block.
The debug slot now names a **live object of a different type**. The snapshot
copies it; `render_local_line` prints the dead local's name and its *static*
type string — from `DebugLocal.type_id`, which is the compiler's — beside the
reissued object formatted through its *own* descriptor; and `impl RootSet for
CrashSnapshot` makes it a strong GC root. Every layer behaves correctly on the
data it was handed, so nothing anywhere reports a problem, and the user reads a
line that is well-formed and false.

It is not hypothetical, and it does not need a contrived program.
`a_local_the_collector_reclaimed_renders_as_an_absence_not_as_a_dangling_ref` is
seventeen lines of ordinary Praxis: fill a `Vec` named `xs` with two hundred
elements, stop reading it, run a loop that allocates past the pacing threshold,
then fault. Run against the tree *before* this change, the crash debugger reports
`xs` as a one-element `Vec` holding `[40748]` — the block came back as one of the
loop's own `junk` vectors, and the user is shown a value that was never theirs
under a name that was.

That second outcome is the whole shape of the fix. **A predicate applied after
the block comes back cannot tell the two cases apart**, because at that point
there is nothing to tell: the block is an ordinary live object, exactly as
indistinguishable from a legitimate one as any other. The only interval in which
"did this die?" has an answer is between the sweep that reclaimed the block and
the allocation that reissues it.

This is true on `main` before any of the performance work, for exactly the same
values. Handover 21 §3.2 and ADR-104 altered neither the values nor their
lifetimes; ADR-104 recorded the hole and deferred it here because closing it
needed a concept the collector did not have.

## Decision 1: the arm is weak, and "weak" is a different trait

`RuntimeRoots` gains a sixth arm, `debug`, carrying the crash debugger's two
stack headers. `RootSet::push_roots` destructures it and **does not push it**.

A `RootSet` answers "what must survive". The new trait `WeakSet` answers the
question the collector had no way to ask: "what names storage but has no say in
whether that storage survives".

```rust
pub trait WeakSet {
    fn clear_reclaimed(&self) -> usize;
}
```

`RuntimeRoots` implements both, and `Heap::collect` passes it as both, so the
strong set and the weak set are read out of one sealed value and cannot describe
different runtimes.

**The trait split is the enforcement, not the documentation.** ADR-104 decision
3 made "hand the debug values to the collector" unspellable by typing the two
stacks differently: `impl RootSet` lives on `SlotStackHeader<*mut GcHeader>`, and
`debug_values` is a `SlotStackHeader<Option<GcRef>>`, so it simply does not have
one. That refusal is retained verbatim. `WeakSet::clear_reclaimed` cannot
undermine it, because it is not handed the mark worklist — it takes no
arguments and returns a count. There is still no expression in this runtime that
roots a debug value; what there is now is an expression that *tells* a debug
value it was not rooted.

### Why a sixth strong arm is rejected

It is a one-line change, and it is ADR-044's set-merge arriving by the back door.
Rooting the debug slots makes the collector's root set the over-approximate one
again, which is the coupling MIR-01 and MIR-02 were written to remove. The
retention is not a constant factor either: `DebugSlots::visible()` grows
monotonically within a block, and a top-level Praxis script lowers to **one**
block, so a script's every intermediate stays live for the length of the run.

`a_dead_local_stops_being_reachable_from_its_frame` is the end-to-end gate. It
runs two programs differing only in whether a three-thousand-element `Vec` is
read after the loop that fills it, and asserts their heaps come out different
sizes. The strong arm fails it by construction. It passes unchanged here, and it
is the test that says the arm stayed weak.

`the_debug_arm_contributes_no_strong_roots` is the same statement one layer
down, so a future attempt fails on the line that would do it rather than on a
heap size three layers away.

## Decision 2: the clear runs inside the collection, after the sweep, and the predicate is the poison

`Heap::collect_inner` becomes mark → sweep → **clear the weak set** → reset the
pacing counter. Both bounds of that position are load-bearing.

**After the sweep**, because `GcHeader::poison` is what marks a block reclaimed
and the sweep is what calls it — before clearing the `allocated` bit, so a stale
`GcRef` reaching a released block is always rejected rather than traced
(ADR-039 decision 3). Nothing else in the runtime nulls a descriptor. So at that
instant `is_poisoned()` is exactly "reclaimed", and it costs one load and a
compare against zero.

**Before any allocation**, for the reason the Context gives: `claim_free_block`
writes a live header over the poison, and after that the predicate answers a
different question. `the_weak_scan_runs_after_the_sweep_and_before_the_block_is_reissued`
pins both bounds as observations rather than as this paragraph — the slot's
header was already poisoned when the scan looked at it, and the next allocation
of that layout took the same block back.

**`is_poisoned()`, not the mark phase's `heap_id() != Some(self.id)`.** Those
differ on exactly one case and it matters: a debug slot naming an object of
*another* heap is not dangling — that heap owns it, and it is alive. The mark
phase conflates the two deliberately, because it must reject both. The weak scan
must reject only the first, or a multi-heap host's debugger loses values that are
perfectly fine.

### Why not a filter at snapshot time, or at render time

`praxis_snapshot_debug_chain` could skip a local whose header is poisoned, and
`praxis-debugger`'s `render_local_line` could do the same. Both are strictly
weaker than the clear and both are rejected for the same reason: they catch the
loud case and miss the quiet one. A block that died and came back is not
poisoned, so no such filter fires, and the wrong-typed value is rendered and
rooted exactly as before. `a_reissued_block_is_not_rendered_under_the_dead_locals_name`
is that case, written as a test, and it is the one that fails against every
edge-applied filter.

The render-time version is worse again: it moves a soundness property into a
crate that has no collector, no heap and no way to know when a sweep happened.

### Why not a pre-sweep mark-bit predicate

Scanning between the mark and the sweep, and nulling every slot whose block is
unmarked, reaches the same conclusion one phase earlier and without depending on
the poison at all. It is rejected because "unmarked" and "reclaimed" are not the
same set: an **immortal** is never reclaimed whatever its mark bit says
(`Heap::sweep` skips immortal pages entirely, ADR-103 decision 4), so an
unrooted interned small `Int` — which after ADR-100 is most `Int` locals a
program has — is unmarked and alive at that point. The pre-sweep predicate nulls
its debug slot and the local renders `<uninit>` while the object it names is in
perfect health.

This matters beyond the rejection. Q-1 in handover 23 asked whether pages should
be segregated by descriptor, which would have taken `GcHeader` to nothing and
`is_poisoned()` with it. **ADR-109 answers it: they stay segregated by size
class**, and one of its reasons is this very predicate — a per-page descriptor
is shared by every block on the page, so there is no per-block word left to null
and "swept" stops being a thing a block can say about itself. So the predicate
this decision rests on is not going away, and the two decisions hold each other
up rather than merely coexisting.

If it ever does change, it must become the **`allocated` bit read after the
sweep**, never the mark bit read before it, for the immortal reason above.
Recorded here because the next person to shrink the header will be reading
ADR-109, not this.

## Decision 3: the scan is driven from the frame entries, so it clears exactly what a snapshot would copy

The obvious implementation walks the value stack's `[base, top)`, which is one
linear pass and is how the shadow stack is scanned. The scan walks
`DebugFrameStackHeader::claimed()` instead, and for each entry the run of
`meta.local_count` slots its `values` pointer names.

That is the identical walk `crash_snapshot::copy_stack` performs. Driving the
clear from it makes "every value a snapshot could copy has been checked" true by
construction, rather than true by an argument about the frame runs partitioning
the reservation.

It is also what gives the write a pointer that may legally be written through.
`entry.values` is the `*mut Option<GcRef>` a prologue and `DebugFrameGuard::set`
already store through, carrying the reservation's own provenance. A scan over
`SlotStackHeader::claimed()` would have to re-derive a `*mut` from the `&[T]` it
hands back, which is undefined regardless of whether anything else is reading —
and the page allocator has never been run under a sanitizer (handover 23 §5), so
this is not a place to be clever.

The partition premise the first implementation would have relied on is kept as a
check rather than dropped: `clear_reclaimed` takes the value stack's header too
and `debug_assert_eq!`s the slots it scanned against its `len()`. A prologue that
ever claims value slots without a frame entry to name them fires that in every
debug build, instead of silently leaving a run of slots the scan cannot see.

## Decision 4: a reclaimed local renders `<uninit>`

The slot becomes `None`, which `render_local_line` already prints as `<uninit>`
and which is the same value a fresh claim's zero carries (F18). No debugger
source file changes.

A distinct `<collected>` would be more honest to the user, and it is rejected on
cost. A debug slot is one machine word with two states, and ADR-044 decision 3
deleted `null_sentinel_ref` precisely because an in-band invalid `GcRef` was UB
whether or not anything dereferenced it. A third state costs either a second
word per slot — doubling the reservation and adding a store per definition to
generated code, which is the cost ADR-104 just removed — or a tagged pointer,
which is the in-band encoding ADR-044 removed. Neither is worth a nicer string.

**What is given up is small and bounded.** The local was already dead at the
source level; that is *why* nothing rooted it. The user sees `<uninit>` for it
where they previously saw a value. What is gained is that the value they see is
never wrong: showing a reissued block under the dead local's name and declared
type is a worse answer than showing no value, and it is the answer nothing
downstream can detect.

The tests that pin renderability are the check that this trade stays bounded.
`a_local_the_root_set_dropped_is_still_renderable`,
`a_fault_between_a_definition_and_the_next_safepoint_shows_the_value`,
`a_temp_that_never_reached_a_shadow_slot_is_still_renderable` and the REPL-level
`m11_locals_split_users_and_temps_with_types` all run programs that allocate far
below `INITIAL_COLLECT_THRESHOLD`, so no collection runs inside them and none of
them moves. **If one of them ever does move, the predicate is wrong and the scan
is nulling live values** — that is what those tests mean here, and it is why
none of them was weakened to accommodate this change. The sixteen REPL
transcript tests are unchanged for the same reason, and were checked rather than
assumed.

`a_local_that_survives_the_collection_is_renderable_with_its_real_contents` is
the positive form, and it is the one the four above cannot give: the same
program shape as the defect's regression test, with the local *read* after the
allocating loop, asserting its elements come back intact through several
collections. Without it, a scan that nulled every slot unconditionally would
satisfy every other test in this ADR.

## Measurements

The scan is one load and a compare against zero per claimed debug slot per
collection, plus one `meta` indirection per live frame. It is the same order as
the shadow-stack scan the mark phase already performs, over a set of the same
size, and unlike the mark phase it neither traces nor allocates. It is also
*per collection*, and the pacer doubles its threshold after each paced one, so
the number of scans over a run is logarithmic in the bytes allocated.

Apple M2 Pro, release build, `/usr/bin/time -p`, minimum of three (four for the
last row), the two binaries run interleaved (A,B,A,B,…) and their stdout diffed
every round. The A binary is this change; the B binary is this change with the
single line `weak.clear_reclaimed();` removed from `collect_inner`, which
isolates the scan from everything else the change touches. Both were built in a
detached worktree at `54d2d9a` with only this change applied, because the
working tree had an unrelated change in flight.

| | with the scan | without | delta |
|---|---:|---:|---:|
| `collatz` @ 340,000 | 1.33 s | 1.33 s | — |
| `tree` @ 60 | 0.65 s | 0.66 s | — |
| 700-deep × 6,000 | 2.24 s | 2.23 s | — |

`collatz` and `tree` are the two ends of the *suite's* range on purpose.
`collatz.px` is a file of top-level statements, which `praxis_hir::lower` folds
into one synthetic entry function, so the whole run performs exactly one
prologue and `top - base` is one frame's locals — a handful of slots, however
many collections run. `tree` recurses, but only to depth 16, so its claimed set
is still under a hundred slots.

Neither is the worst case, so the third row is one built for it: a function that
recurses **700 deep** with four `Vec` locals in every frame, called 6,000 times,
so every collection scans several thousand claimed slots and the allocation rate
keeps the collector busy throughout. That is two orders of magnitude more slots
per scan than anything in the suite reaches, and it is still not measurable
against a laptop that drifts several percent over a few minutes. The number this
ADR owes is the worst case rather than the mean, and the worst case is noise.

**Retention is unchanged, which is what a weak arm retaining nothing looks like
when it is observed rather than asserted.** Peak RSS, `/usr/bin/time -l`, same
interleaving:

| | with the scan | without |
|---|---:|---:|
| `tree` @ 60 | 518.51 MiB | 518.42 MiB |
| `collatz` @ 340,000 | 506.92 MiB | 507.02 MiB |
| 700-deep × 6,000 | 1257.87 MiB | 1257.85 MiB |

The deltas are ±0.02%, in both directions, which is the page allocator's own
run-to-run variation. A strong sixth arm would not look like this: it would hold
every intermediate of every live frame, and on the third row that is the entire
history of a 700-frame stack.

**No ABI bump.** Nothing in `RuntimeContext` moves, no `praxis_*` symbol is added
or removed, and generated code is byte-identical — the whole change is on the
runtime side of a boundary generated code does not cross. `RUNTIME_ABI_VERSION`
stays where it is. If a version bump ever looks necessary for this, the design
has drifted into generated code and something is wrong.

## Consequences

- **The debugger's fidelity contract gains a clause, and it is a narrowing.**
  ADR-044 said a value that has been produced stays renderable. It now stays
  renderable *until the collector has proved nothing can reach it*, after which
  it is an absence. That is the only honest terminal state for a value whose
  storage is gone, and it is the one the `Option<GcRef>` niche already spelled.
- **`a_dead_local_stops_being_reachable_from_its_frame` is now load-bearing in
  two directions.** It was the gate that ADR-104's reconstruction-from-the-shadow-frame
  could not work; it is now also the gate that this arm stayed weak. Do not
  relax it, and do not "fix" it if a future strong-arm proposal makes it fail —
  the failure is the test working.
- **The collector has two kinds of set, and adding a seventh owner to
  `RuntimeContext` now forces a strength decision.** `RootSet::push_roots`
  destructures all six arms, so a new field does not compile until someone has
  decided whether it keeps objects alive or merely names them.
- **`GcHeader::is_poisoned` has a shelf life, and now has a caller that depends
  on it.** The predicate answers "reclaimed" only between the sweep and the next
  allocation. That is recorded on the method itself, because the next reader of
  it will not have read this.
- **ADR-109 and this decision hold each other up.** Descriptor-segregated pages
  would have deleted the null-descriptor poison, and ADR-109 rejects them partly
  for that. If a future shrink ever takes the descriptor out of the header
  anyway, this scan's predicate must become the page's `allocated` bit read
  *after* the sweep; decision 2 says why it must not become the mark bit read
  before it.
- **The `is_poisoned()` read is a read of mapped memory, and that is a premise,
  not an accident.** `Heap::release_pages` is the only place a page is ever
  unmapped and it runs after `finalize_all` at teardown (ADR-103 decision 3).
  The weak scan relies on that exactly as `Heap::mark`'s provenance check does.
- **`Heap::collect_with` and `maybe_collect_with` pass `()` as the weak set**, so
  all twenty-seven in-crate call sites that collect against a bare `RootScope`
  are unchanged. `collect_with_weak` is the door for the tests that do have one,
  and `impl WeakSet for ()` is what makes "no weak set" a value rather than an
  `Option` every caller has to spell.
- **One doc comment this change falsifies was not corrected here.**
  `RuntimeContext.debug_values` still says "**The collector never reads this.**
  It is not an arm of `RuntimeRoots`". Both halves are now false: it is the sixth
  arm, and the collector reads and writes it once per collection. It should read
  that the collector never *traces* it. The field lives in `context.rs`, which
  was owned by another change in flight when this one landed.
