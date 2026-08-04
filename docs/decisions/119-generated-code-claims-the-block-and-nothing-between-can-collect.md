# ADR-119: Generated code claims the block, and what makes that safe is that nothing between the pacing branch and the last store can collect

**Date:** 2026-08-04
**Status:** accepted — implemented
**Milestone:** post-M11 performance (handover 26's W10, re-scoped by handover 27
§4 from "the `Float` path" to "the inline scalar claim"; the half ADR-113 called
P-1b and deliberately did not build)

**Amends:** `Heap::Safepoint`'s doc, whose ADR-113 paragraph says *"The inline
path allocates nothing — it hands back an immortal the runtime minted before
`main` ran"*. That sentence is now false, and decision 1 is what replaces it.
**Amends ADR-113 decision 2**, whose two exported offsets it calls *"the whole
export surface, and its narrowness is deliberate"* — see the export-surface
section, which is an argument and not an ABI paragraph. **Cashes in ADR-113
decision 3**, which declined to write an `InlineAllocSite` on the grounds that
"four fields nothing reads are four fields that go stale before their first
reader arrives", and left the name free. ADR-040's token is preserved in the
same sense ADR-113 preserved it, and by a different argument. ADR-109's
size-class segregation is what makes the stride a compile-time fact; ADR-039
decision 1 is still the one layout authority, and generated code now transcribes
its answer.

## Context

**Handover 27 §4 made this package conditional, and the gate ran.** The rule was:
read `claim_block` + `alloc_raw` + `praxis_alloc_int` + `praxis_alloc_float` off
the profile, net out composite allocations, and *under 10% on every benchmark
W10 is not built this round*. Netted scalar-allocation share, measured on the
tree after all eight other packages landed:

| benchmark | netted | | benchmark | netted |
|---|---:|---|---|---:|
| `mandelbrot` | **32.5%** | | `tree` | 18.7% |
| `vm` | **28.8%** | | `primes` | 10.0% |
| `pipeline` | **27.2%** | | `bfs` | 8.0% |
| `collatz` | **24.5%** | | | |

Three things about that measurement shape this record rather than merely
authorizing it.

**The netting barely bit.** Composites are 2.9% on `pipeline` and round to zero
everywhere else. After two rounds of work these loops do not allocate composites
in steady state, so the concern the gate existed to test is measured and false.

**The eligible set is exactly what was counted.** ADR-113 keeps every in-range
`Int` out of `praxis_alloc_int` entirely — it is an inline table read — so every
sample inside that symbol is an *out-of-range* `Int`, which is one of the two
arms below. No discount applies. And `mandelbrot`'s share is entirely
`praxis_alloc_float` where every other row is entirely `praxis_alloc_int`;
`praxis_alloc_char` is 0.0% everywhere.

**Generated code inlines zero actual allocations.** ADR-113 inlined a table
*read*. Out-of-range `Int` takes `emit_inline_intern`'s cold call; `Float` and
`Char` take an unconditional `call_symbol` with no pacing test and no inline arm
at all. The eligible set is provably exactly two arms — out-of-range `Int` and
`Float` — because `Heap::occupy` charges
`stride + descriptor.owned_bytes_of(payload)` and no scalar descriptor carries an
`owned_bytes` callback. `Char` is excluded for ADR-113's reason, unchanged:
`AllocChar`'s manifest row is `AllocatesAndFaults`, so an inline arm would have
to reproduce a *fault*, not just a claim.

## Decision 1: the `Safepoint` obligation is three claims, and the second one is the one that does the work

The clause this falsifies is in `crates/praxis-runtime/src/heap.rs`, verbatim:

> the token is permission to collect, not permission to allocate. The inline
> path allocates nothing — it hands back an immortal the runtime minted before
> `main` ran.

The first sentence survives. The second does not: this path allocates, and the
object it hands back is one it built. What replaces it is three parts, and **all
three are claims about the emitted instruction stream**, so all three are pinned
by tests that read the emitted Cranelift rather than by this document.

### Part 1 — entry

Every store the claim sequence performs is dominated, **in the emitted Cranelift
CFG**, by the branch on `Heap::collection_is_due`.

`the_inline_claim_is_dominated_by_the_pacing_branch` asserts it with wave 0's
`assert_dominates`, over a function with **two** claim sites so that the second
one's guard is not the entry block's terminator. That is deliberate and it is
what makes this strictly stronger than ADR-113's assertion, which asks "is the
branch the entry block's terminator" — a question that is true of a lowering with
one guarded site in it and says nothing about a real function, where the
hundredth `Materialize` is nowhere near the entry.

Identifying the guard block took one clause worth recording: `icmp uge` alone is
not enough, because the claim emits a *second* unsigned `>=`, the
`cursor >= last_word` bail-out. The two are told apart by the width of what they
compare — the pacing words are `usize` and are loaded whole, where `cursor` and
`last_word` are `u32` and arrive through `uload32`. A test that had matched the
scan block instead would have asserted a dominance that holds for a reason other
than the one this part claims.

### Part 2 — duration. This is the clause that replaces "it allocates nothing"

Between the pacing branch and the last store there is **no call**, and therefore
no point at which a collection can begin. A collection runs only inside
`Heap::collect_inner`, which generated code reaches only through a `praxis_*`
wrapper, which is a call. So *not due on entry* implies *not due throughout*.

That is what makes the claimed block unsweepable before its reference reaches a
root slot, and it is what stops a re-entrant claim being handed the same free
bit: there is no re-entry, because there is nothing to re-enter through.

`nothing_between_the_pacing_branch_and_the_last_store_can_collect` asserts that
no hot block calls, that exactly one cold block per site does, and — separately —
that the store block does not branch at all, so no bail-out can leave a header
written and no `allocated` bit set.

**Everything else in this record depends on part 2 and nothing depends on the
store order.** That is stated here rather than implied, because the store order
below is the kind of thing a reader will mistake for the argument.

### Part 3 — state

The sequence leaves the heap field-for-field as
`alloc_raw → claim_block → occupy` would, **including both live counters**.

Both counters matter and neither is a statistic. `Heap::live_count` is only ever
*decremented* — `sweep` takes it down by the blocks it reclaimed and never
recomputes it — so a claim that skipped the bump does not leave the number low,
it underflows a `usize` on the first collection. `PageHeader::live_count` is
worse: `relink_pages` reads it to decide which availability list a page joins, so
an understated page joins the **empty pool**, and `reclass` then hands its
storage — blocks with live objects in them — to another layout.

`the_inline_claim_writes_every_word_the_wrapper_would` asserts the whole ordered
list of eight stores as one list, which makes it simultaneously a completeness
claim, an order claim and a displacement claim. It has to be one list: two of the
eight displacements collide numerically — a `GcHeader`'s payload and a `Heap`'s
`bytes_since_collect` are both `+16` — so no single line identifies itself.

`PageHeader::cursor` is **not** stored, and that is not an omission.
`claim_free_block` sets `cursor = w` on success; `w` is read from `cursor` here
and never advanced, because the inline form scans one word where the wrapper
loops. The store would write back the value it just read.

That the eight displacements name the fields they claim to is a separate
assertion in a separate crate:
`the_claim_site_displacements_name_the_fields_they_claim_to` reads a live `Heap`,
a live `PageHeader` and a live `GcHeader` through every one of them and compares
against the accessors. The IR test says *which words*; that one says *which
fields*; neither can say the other.

### The store order is a severity ranking, not the safety argument

Order: header → payload → `allocated` bit → counters.

**The order is not what makes the sequence safe — part 2 is.** It is a ranking
against a collection part 2 says cannot occur. Writing the descriptor first
removes the one *unrecoverable* failure: a sweep reaching an `allocated` bit
whose header holds uninitialized bytes would read them as a
`*const TypeDescriptor` and make an indirect call through `drop_value`. Setting
the bit after the payload leaves only bookkeeping failures — a block counted and
not readable, a counter high by one. Claiming more for the order than that is
exactly what ADR-113's Consequences forbid: *"an enforcement mechanism that is
claimed to prove more than it does is worse than none, because the next person
trusts it."*

## Decision 2: `InlineClaimSite` refuses a descriptor, and the refusal is `const`

```rust
pub struct InlineClaimSite { /* sixteen private numbers */ }
impl InlineClaimSite {
    pub(crate) const fn of(descriptor: &'static TypeDescriptor) -> Option<InlineClaimSite>;
}
// scalars.rs, beside the descriptors:
pub const INT_CLAIM_SITE: InlineClaimSite = /* … or the build fails */;
pub const FLOAT_CLAIM_SITE: InlineClaimSite = /* … */;
```

`InlineInternSite` confines *which table* the backend may probe. This confines
something stronger: **which descriptors have a claim sequence at all.** `of`
answers `None` for two classes, and both refusals are the reason this type exists
rather than a tidiness:

- a descriptor carrying an `owned_bytes` callback, because `occupy` charges
  `stride + owned_bytes_of(payload)` and the second term is an indirect call on a
  payload that does not exist yet. Every scalar answers `None` to `owned_bytes`;
  `Text` and `Vec` answer `Some`. A `Text` claimed inline would under-charge the
  pacer by its entire buffer, which is RT-04 re-introduced;
- a descriptor whose block `SizeClass::of` rejects, because a large page is
  claimed by a linear scan of `empty_large` keyed on the whole layout, which is
  not a bitmap claim in any sense.

Both are `const`, so a backend arm naming a descriptor with an `owned_bytes`
charge **fails the build**. `only_a_descriptor_with_no_owned_bytes_charge_has_a_claim_site`
walks every one of `descriptor::BUILTINS` and asserts the claimable set is
exactly `{no owned_bytes} ∩ {on the ladder}`, rather than a list kept in step by
hand.

`BlockLayout::of` and `SizeClass::of` became `const fn` for this, which is the
one-experiment question handover 27 §9 registered — see decision 3.

**What the type cannot do**, stated rather than implied, as ADR-113 decision 3
insists: it cannot force the backend to emit the pacing compare, to emit the
stores in an order, or to emit all of them. Decision 1's three tests carry those,
and the boundary is written in the type's own doc.

## Decision 3: the three open questions, answered by building

Handover 27 §9 listed these as one-experiment questions. Each was run.

### Does `stride` fold to a compile-time immediate?

**Yes — `iconst`, and so do `first_block` and the payload displacement.** The
CLIF at an `Alloc { Float }` site:

```text
    v188 = iconst.i64 24            ; the stride
    v189 = imul v187, v188
    v191 = iconst.i64 600           ; first_block, folded
    v194 = iconst.i64 16            ; the recorded payload displacement
```

**But not for the reason the question assumed.** Const-evaluation does not
"reach the backend"; `BlockLayout::of` and `SizeClass::of` are `pub(crate)` to
`praxis-runtime` and the backend cannot call them at all. What reaches the
backend is a `const` value the runtime evaluated — which is the better answer,
because it puts the derivation in the crate that owns the geometry. It did need
the new accessor the question suspected: `PageHeader::first_block_of`, stated
once in `page.rs` and used by `new_small`, `reclass` and the site.
`the_block_geometry_is_folded_rather_than_read_off_the_page` pins all three as
immediates, and `the_folded_first_block_is_the_one_every_page_of_the_class_has`
pins the fold itself against a live page of every rung, before and after a
re-class.

### Tail word: bail, or reproduce `tail_mask`?

**Bail — and it is free, which is not what the question predicted.** Handover 27
says bailing "costs one more branch". It costs none. The loop bound
`claim_free_block` applies is `w <= last`, so the sequence already had to compare
`cursor` against `last_word`; widening the bail from `w > last` to `w >= last`
changes a condition code and nothing else. The two arms were built and counted at
the same site with `PRAXIS_DUMP_VCODE`:

| | scan+word blocks | per-iteration, executed cycle |
|---|---:|---:|
| bail at `w >= last` | 8 | **141** |
| reproduce `tail_mask` | 12 | 145 |

(Both rows are release builds of this tree at the `Float` site below, differing
in that one emitter and in the one extra `PageHeader` displacement it needs. The
experiment was then reverted and the binary rebuilt: it reproduced the
pre-experiment release binary's sha256 exactly, which is what says nothing of it
was left behind.)

Reproducing the mask costs a `tail_mask` load, an `icmp eq`, a `csel` and an
`and`: **+4 machine instructions per claim site**, and one more exported
`PageHeader` displacement.

What bailing cedes is the last bitmap word of every page. For the stride the two
claimable descriptors share, that is **60 blocks of 1340** — 4.5% of claims on a
full page take the wrapper, which does the loop the inline form does not. It is a
throughput cost on a path that is correct either way, against 4 instructions on
every claim. The trade is recorded rather than assumed, which is what handover 27
asked for.

One consequence worth writing down: on a page with a **single** bitmap word this
bail would fire on every claim and the inline arm would be dead code. That is a
statement about reach and not about correctness, and it is asserted rather than
reasoned about — see the `read_last >= 1` assertion in
`the_claim_site_displacements_name_the_fields_they_claim_to`.

### How many instructions does the claim cost?

**51 aarch64 instructions for a `Float`, 46 for an out-of-range `Int`, and no
call.** Reading-estimate was 20–25 with 3–4 conditional bails; the bail count was
right (three: no page, word exhausted, word full) and the instruction count was
low by a factor of two. See Measurements.

## Decision 4: the export surface widens, and here is the argument for it

ADR-113 decision 2 says of `BYTES_SINCE_COLLECT_OFFSET` and
`COLLECT_THRESHOLD_OFFSET`:

> **This pair is the whole export surface, and its narrowness is deliberate.** A
> pacer whose predicate needed a third term would have nothing to hand the
> backend, which is the point at which whoever writes it has to read
> `Heap::collection_is_due`'s doc.

That sentence is now false, and it is owed an argument rather than a paragraph in
the ABI changelog. Three parts.

**First, the count is smaller than the two previous records predicted.** ADR-113
priced P-1b at "eight `PageHeader` offsets and three `GcHeader` ones", and
handover 27 §4 repeated the figure. Built, it is **four** `PageHeader`
displacements — `cursor`, `last_word`, `allocated`, `live_count` — and three
`GcHeader` ones, of which one (`DESCRIPTOR_OFFSET`) already existed. Three
`PageHeader` fields the estimate assumed would be read are not: `first_block` and
the stride are folded (decision 3), `tail_mask` is not read because the tail word
is ceded (decision 3), and `block_size` is the folded stride. The estimate was
made by listing the fields `claim_block`/`occupy` touch; four of them turn out to
be compile-time facts about a size class rather than run-time facts about a page,
and that is a property of ADR-109's segregation, not a coincidence.

**Second, the mechanism ADR-113's narrowness was protecting is not the count.**
Its argument is that a pacer wanting a third term "finds nothing to hand the
backend". That mechanism is untouched: `InlineClaimSite::of` fills the two pacing
offsets from `Heap` itself rather than taking them as arguments, exactly as
`InlineInternSite::new` does, so a claim site still cannot exist that describes a
block to take without also carrying the predicate's operands. What widened is a
*different* surface — the object layout — and it has its own, stronger mechanism:
the numbers are private fields reached only through one `const` value that
refuses to exist for a descriptor whose bookkeeping the sequence cannot
reproduce. ADR-113 had no such refusal to offer because there was nothing to
refuse.

**Third, the cost is real and it is permanent.** Repacking `PageHeader` is a
generated-code change from here on, and so is reordering `GcHeader`'s three
fields. That is genuinely a new constraint on two structs that were free to move
for the whole life of the project, and it is not paid for by an "it's only four
numbers" argument. What pays for it is that these four are the *narrowest*
description of a bitmap claim that exists: a size class, a cursor, a bitmap and
two counters. Any inline allocator for a segregated-fit heap reads exactly this
set. Widening it further — a second word of the bitmap, `tail_mask`, a free-list
head — would each be a new decision, and decision 3 declined two of them on
measurement rather than on taste.

The offsets are minted with `offset_of!` beside the structs, fields staying
private, which is `GcHeader::DESCRIPTOR_OFFSET`'s pattern for ADR-039 decision
1's reason. **No numeric offset is written anywhere in the backend.**

## `RUNTIME_ABI_VERSION` stays at 20, and this fills in the line W6 pre-stubbed

v20's numeral is ADR-116's for the round; this appends prose under the line
already reserved for it and touches no digit. What the line records:

Generated code now reads and **writes** `PageHeader` and `GcHeader` field
layouts, which it had never done. It reads `cursor`, `last_word`, `allocated` and
`live_count` off a page; it writes a whole `GcHeader` — descriptor,
`payload_offset`, `heap_id`; it reads and writes `Heap.live_count` and
`Heap.bytes_since_collect`; and it folds a size class's stride, `first_block` and
payload displacement as immediates.

This is the v12/v17 class — the meaning of something changed where the layout did
not — and the v15 class at the same time, because generated code reads fields it
never read. A v20-compiled program run against a v19 runtime with a different
`PageHeader` layout would claim bits out of the wrong words and write headers at
wrong addresses inside a live page. That is silent and it is a wild write, which
is the worst entry in this changelog and the reason the sentence is here rather
than in a commit message.

## Measurements

**No timing.** This package was built during a phase in which the machine is
shared and timing was forbidden, so **the two arms are staged and the wall-clock
sweep is owed.** Everything below is deterministic: instruction counts out of the
real compile path via `PRAXIS_DUMP_CLIF`/`PRAXIS_DUMP_VCODE`, walked with
`benchmarks/periter.py` — the in-tree walker, because two hand-written walkers of
that rule were wrong this round in two different ways and one reached a published
record.

Arms, both from this tree, differing in one `const` pair (`INLINE_SCALAR_CLAIM`)
and nothing else:

| arm | path | sha256 |
|---|---|---|
| A (`--features praxis-codegen-cranelift/adr119-arm-a`) | `/tmp/praxis-arms/W10-a` | `7a1cec50ca18ff8b071e04c6158b998b963dffcf28617effacb165bd63604c5d` |
| B (this branch) | `/tmp/praxis-arms/W10-b` | `292099802f77452ab81fae1c866be545975ecbe56f7b34e3c17a4a09ea256814` |

(A binary's hash moves when *any* source byte in a crate does, comments
included — `-C metadata` is derived from the crate's contents — so these two are
comparable to each other and to nothing else. What says the arms differ in the
emitted code rather than only in a symbol hash is the instruction census below.)

`praxis-runtime` is byte-for-byte identical in both: the displacements,
`InlineClaimSite` and the two site constants compile either way and simply have
no reader in arm A. So this is an A/B of the emitted code and of nothing else —
including the export surface decision 4 argues the cost of.

### The `Float` site

`acc = acc + 1.5` in a `while` loop, `<entry>`, `opt_level = "none"`. Both boxes
in the loop (`acc` and the counter `i`) are on the executed cycle; `i` stays
inside `small_int`'s range, so its box is ADR-113's table read in both arms.

| | per iteration (vcode) | calls on the path |
|---|---:|---:|
| arm A | 91 | 1 |
| arm B | **141** | **0** |

Broken out at the box itself:

| | arm A | arm B |
|---|---|---|
| root spill into the shadow frame | 6 | 6 |
| pacing test | — (the wrapper's) | 5 |
| the box | `load_ext_name_far`, `mov`, `blr` — **3** | claim: 2 + 4 + 4 + 36 = **46** |
| callee | `praxis_alloc_float` **88** static instructions, which calls a `Heap::alloc_raw` specialization of **82** | none |

**The inline sequence is bigger, and saying so is the point.** ADR-118 part 2 put
it exactly right: *"an inline sequence is bigger than the `bl` it replaces,
because the wrapper's body was never in the count."* What the 50 extra
instructions buy is the deletion of a call whose real cost is `abi_guard!`'s
`catch_unwind` region, `RuntimeRoots::from_context`, and the caller-saved-register
clobber that at `opt_level = "none"` spills and reloads every live value in the
loop. None of that is in either column.

### The out-of-range `Int` site

`acc = acc + 100000` from a starting value outside the table, same shape.

The claim is **46** instructions in four blocks (2 + 4 + 4 + 36) behind ADR-113's
existing range probe, against `praxis_alloc_int` at 92 static instructions plus
the same 82-instruction `alloc_raw`.

**`periter.py` cannot report arm A's executed cycle here, and that is worth
recording as a limitation of the tool rather than glossed.** In arm A the
out-of-range edge branches to a *cold* block, and the walker's rule — from
`dump.rs`'s module doc — excludes cold blocks from the loop body. So arm A's
reported 101 is the cycle in which `acc` is *in* range, which this program never
takes. The comparison above is therefore made block-for-block at the site rather
than per-iteration. A per-iteration number for arm A would need a walker that
follows a cold edge, which is a different rule and not one to invent in a
footnote.

### The calibration this record is entitled to, and the one it is not

ADR-113's own inline `Int` table read was re-priced on this tree with a
purpose-built toggle at **+26.1% on `primes` and +16.1% on `collatz`**, against
netted profile shares of 10.0% and 24.5%. An inline scalar path has already been
worth *more* than its profile share once on this codebase, for the reason that
applies here too: the call's `abi_guard!`/`catch_unwind` region and
`RuntimeRoots::from_context` prologue are not in the callee's profile bucket.
That is a reason to expect a result; it is not a result.

**No number is quoted on `mandelbrot`.** W8-S0 already took it 3.29× and W8-S1
would take its remaining two float boxes to zero, so a `mandelbrot` number
expires twice. Nothing is carried from handover 25, and the "63% allocator" share
does not appear.

### The acceptance test, half of which expired

Handover 27 §4 named ADR-113's reproducible regression as this package's
acceptance test: `tree` +2.0% and `pipeline` +1.4%. Re-measured on the current
tree with an ADR-113 toggle, two independent passes:

- **`tree` −1.6% ± 1.3% and −1.6% ± 0.9%** — still there, reproducing to the
  tenth of a percent, about four fifths of its original size. **This is the
  acceptance test and it is not yet run.** `tree`'s `Materialize`s box values
  that mostly leave `small_int`'s range, so they pay ADR-113's pacing test in
  front of a call they were making anyway — and it is exactly that call this
  record deletes. If the sweep does not retire it, this did not land.
- **`pipeline` −0.3% and −0.6%, inside its own spread both times** — **gone.**
  W8-S0 is 1.645× on `pipeline` and took the `Materialize` sites that were paying
  it. Its row **expired**; it is not carried forward as half of a pair, and this
  sentence exists because carrying it silently is what four corrected numbers in
  this round have in common.

### What was verified, since the clock was not

- `just ci` green.
- `./scripts/asan.sh`: **32 executables produced, all instrumented; 2071 passed,
  0 failed, 0 AddressSanitizer reports.** Against the stated baseline of 2061
  passed / 0 reports / 32 instrumented, the +10 is exactly this package's ten new
  tests.
- All eight benchmarks, both arms, at `benchmarks/sizes.json` sizes: **stdout
  byte-identical between the arms and equal to `benchmarks/results.json`'s
  recorded checksums**, on every one. Untimed.

## Why a green ASan run is necessary and not sufficient

`scripts/asan.sh` instruments the Rust workspace. **It does not instrument
JIT-generated code** — Cranelift emits that raw and no `-Z` flag reaches it — and
this is the package that puts generated code to work writing `GcHeader`s into
pages. So the run above says the *runtime* is clean while the new unsafe
behaviour executes beside it, and says nothing about the stores themselves. What
covers those is four things, in the shape W4b's record used:

1. **The addresses are not computed by generated code from scratch.** The page
   comes off `Heap.partial[c]`, which only `relink_pages` and `grow_class` write
   and which holds only small pages of class `c`. The block index is
   `w * 64 + ctz(free)` for a word the page's own `last_word` admits, which is
   `claim_free_block`'s arithmetic transcribed. The byte offset is
   `first_block + index * stride`, both immediates that
   `the_folded_first_block_is_the_one_every_page_of_the_class_has` and
   `the_claim_site_displacements_name_the_fields_they_claim_to` check against a
   live page. Nothing here is a pointer the compiler invented.
2. **The bound the tail word needs is enforced by refusing the tail word.**
   `w >= last` bails, so the sequence never claims a bit in the only word whose
   top names blocks the page does not have. This is the failure ASan would be
   least likely to catch — the storage past the last block is *inside* the page's
   own 32 KiB allocation, so writing it is not a red-zone hit — and it is
   structurally impossible rather than checked.
3. **Every displacement is `offset_of!` of the field it names**, asserted through
   a live object of each of the three kinds, so a `#[repr(C)]` that stopped being
   one fails a test rather than moving a store.
4. **The one failure ASan genuinely could catch is the one the store order is
   ranked against** — a sweep dereferencing an unwritten header — and the whole of
   decision 1 part 2 is that it cannot occur, asserted over the emitted CFG. The
   behavioural tests exercise the residual anyway:
   `an_inline_claimed_object_is_what_the_wrapper_would_have_answered` runs 40,000
   claims across many collections with every object rooted and reads all of them
   back, and `a_loop_that_boxes_only_floats_still_collects` runs 20,000 that are
   rooted by nothing.

The honest summary: a green run is evidence about everything *except* the
sequence, and the sequence is covered by construction and by IR shape. If a
future package wants stronger, the mechanism is the `mark`/`sweep`/`finalize_all`
debug audit ADR-113's "What was deliberately not done" describes — an audit that
walks every allocated bit and asserts its header is well-formed. It is not added
here, speculatively, for the reason ADR-113 gave for not adding it then: it wants
a failure to have been seen first.

## What was deliberately *not* done

**`Char` keeps its call**, for ADR-113's reason verbatim: `AllocChar`'s row is
`AllocatesAndFaults`, an inline arm must route an invalid code point to the
wrapper that raises `InvalidChar` with the same message and the same `CheckFault`
diversion (RT-18), and handover 23's P-4a may move that validation into
`small_char`'s bounds and change the arm's shape. Its profile share is 0.0% on
every benchmark, so this costs nothing measurable and P-4a inherits an unforced
hand.

**No composite is claimed inline.** `Vec`, `Text`, records, tuples and closures
all either carry an `owned_bytes` charge or are built in two phases by a filler
wrapper. `InlineClaimSite::of` refuses the first class in `const`; the second is
not a claim problem at all.

**The cursor is not advanced past a full word**, and the page is not dropped off
its availability list when it fills. Both are `claim_block`'s loop, and the
inline form bails to it instead. A sequence that unlinked a full page would be
writing a list the collector rebuilds, which is `relink_pages`'s "rebuilding
rather than unlinking is what makes membership structural" — the one invariant in
that module a backend arm has no business touching.

**`spill.spill_roots` above these arms stays**, verbatim, for ADR-110's and
ADR-113's reason: `Inst::Materialize` and `Inst::Alloc` are unconditional
safepoints in MIR, and that is a MIR-level property about which instructions the
collector may run at, not a backend arm's to narrow from what it happens to emit.
It is also what keeps the cold arm correct without further thought.

**`praxis_alloc_int` and `praxis_alloc_float` are not deleted, narrowed or
split.** They are the cold blocks' callees, the debugger's throwaway modules call
them, and keeping them is what makes "the answer is what it always was" a
property of the code rather than of this document — which the byte-identical
stdout on all eight benchmarks is the evidence for.

## Consequences

- **`Heap::Safepoint`'s doc is no longer the whole story about generated code**,
  and the sentence that was is now marked as ADR-113's and superseded. The type
  still means what it meant; what changed is that "the inline path allocates
  nothing" has become "the inline path allocates on a branch where the collector
  provably cannot run".
- **Generated code depends on `PageHeader`'s and `GcHeader`'s field layout.**
  ADR-113 made `Heap`'s layout a generated-code dependency and called that
  surface complete. It was complete for a table read. Repacking either of these
  two structs is now a generated-code change, and the ABI changelog says so.
- **A `Materialize { Float }` site is now six basic blocks and a join**, where it
  was one call. ADR-102's consequence — `blocks[blk_idx]` is the MIR block's
  *entry* only — applies to it, and `emit_inline_claim_box` leaves the builder at
  its merge for `emit_scalar_load`'s reason.
- **The out-of-range `Int` edge is no longer a bail-out.** ADR-113's cold block
  had two predecessors — the pacing branch and the range branch — and its test
  asserted that they shared a callee rather than each growing one. It now has
  four: the pacing branch, and the claim's three refusals (no page for the class,
  the cursor word past the end, the cursor word full). The range branch is no
  longer one of them, because being out of range is no longer a reason to call
  anything. `the_only_block_that_calls_praxis_alloc_int_is_the_cold_one` passes
  unchanged, which is the property it was really asserting.
- **4.5% of claims on a full page still take the wrapper**, by decision 3's
  choice. If a later package wants them, the change is four instructions and one
  more exported displacement, and this record is where the price is written down.
- **`praxis-runtime` gained a fourth "value that can be named and not made"** —
  `SlotCount` bounds a frame, `ImmortalWitness` confines minting,
  `InlineInternSite` confines probing, `InlineClaimSite` confines claiming. It is
  the first of the four whose refusal is a `const fn` returning `Option`, so the
  refusal is a build failure rather than a runtime absence.
- **The wall-clock result is owed and the arms are staged for it.** Anyone
  running `benchmarks/ab.py` against the two paths above is running this record's
  acceptance test, and `tree` is the row it turns on.

## Open questions

- **Does `tree`'s remaining 1.6% actually go?** ADR-113 left it, this is its
  repair, and the sweep has not been run. If it does not, the second candidate is
  the one ADR-113's own open question named — the extra basic blocks per site at
  `opt_level = "none"` rather than the call — and this package adds four more of
  them per site, so a *worse* `tree` would be an informative result rather than a
  puzzling one.
- **Should the claim sequence scan more than one word?** It bails when the cursor
  word is full, which on a page filling front-to-back happens once per 64 blocks
  and hands the wrapper a call it makes for one allocation and then not again. A
  two-word scan is another load, an `orr` and a select of which word to claim
  from. Nobody has counted the frequency; `periter.py` cannot see it because it is
  a run-time distribution, not a shape.
- **Is `opt_level = "speed"` worth a fourth look?** ADR-113 recorded the first
  non-null result the flag ever produced (`collatz` −6.3%) and attributed it to
  removing an opaque call from a loop. This removes the *other* opaque call from
  the same loops. The measurement is cheap and it is not this record's to make.
- **Should `Heap::live_count` and `PageHeader::live_count` be derivable rather
  than maintained?** Both are now incremented in two places — the runtime and
  generated code — and decremented in one. A count recomputed from the bitmaps at
  sweep time would make the increments unnecessary and this record's part 3 much
  shorter. It is a `Heap` change with a cost on every sweep, and it wants
  measuring, not arguing.
