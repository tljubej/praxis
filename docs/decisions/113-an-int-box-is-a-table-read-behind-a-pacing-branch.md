# ADR-113: An `Int` box is a table read behind a pacing branch, and the token is permission to collect

**Date:** 2026-08-03
**Status:** accepted — implemented
**Milestone:** post-M11 performance (handover 23 item P-1, the half ADR-110 left
owing — "P-1a")
**Amends:** ADR-040's `Safepoint` doc, which said "allocate on the paced path
without pacing has no spelling" and now has to say what generated code does
instead. Cashes in ADR-102's "What was deliberately *not* done" §2, which wrote
the argument in one paragraph and said it deserved its own record. ADR-100
decision 3 (`int_ref` paces even when it answers from the table) is preserved
exactly, and the preservation is the whole point. ADR-112's pacer is untouched;
what changes is that its predicate now has a second reader that cannot call it.

## Context

ADR-110 landed the free half of P-1. `praxis_alloc_bool` and `praxis_alloc_unit`
have not allocated since ADR-040 decision 4, so their call sites became a
`select` and a load with no argument to make. Its Consequences named what was
left: *"the interned-`Int` probe and the bitmap claim both remain, and both need
something this change did not: ADR-040's `Safepoint` token has to acquire a
spelling in generated code."*

This is the interned-`Int` probe.

**`Inst::Materialize` is the hot allocating instruction in the language.** Not
`Inst::Alloc` — `lower_lit_gc` has routed every in-range `Int` *literal* to
`Inst::ConstGc` since ADR-100, so `AllocKind::Int` in the wild is the
out-of-range literal and nothing else. What is left is `Materialize`: a loop
counter's `i = i + 1`, an accumulator's running total, the sink of a fused
pipeline. Every one of them was:

- a `bl` to `praxis_alloc_int`, with the full caller-saved-register clobber that
  at `opt_level = "none"` spills and reloads every live value in the loop;
- an `abi_guard!` `catch_unwind` region around the callee;
- `RuntimeRoots::from_context` — six arms built from seven pointer loads and
  about a dozen null tests, assembled so that a collection *that does not happen*
  would have had the whole root set;
- `Heap::pace` → `Heap::maybe_collect`, two loads and a compare;
- and then, for the overwhelmingly common case, `int_ref` answered from
  `small_int`'s table: **two loads, and no allocation at all.**

Everything in the first four bullets stands in front of the fifth. The
measurement below says removing it is worth 18.7% of `collatz` and 13.9% of
`primes`.

**The reason it had been deferred twice is not codegen.** ADR-102 deferred it
because §3.6 was in flight and would move every offset an inline allocator
baked; ADR-109 answered that (pages stay segregated by size class), and P-1a
bakes no page offset anyway. What remains is the second reason, which would have
outlived any re-measurement:

> ADR-040's `Safepoint` token exists so that "allocate on the paced path without
> pacing" *has no spelling*: obtaining the token is the pacing. An inline
> allocator spells it.

## Decision 1: the fast path is taken only where `maybe_collect` would have returned `false`, and that is why it forges no token

The emitted sequence, at `opt_level = "none"`, on aarch64:

```text
hot:   heap  = load.i64 [ctx  + InlineInternSite::heap_offset()]
       since = load.i64 [heap + Heap::BYTES_SINCE_COLLECT_OFFSET]
       thr   = load.i64 [heap + Heap::COLLECT_THRESHOLD_OFFSET]
       due   = icmp uge since, thr
               brif due, slow, probe
probe: index = iadd_imm value, -SMALL_INT_MIN
       ok    = icmp_imm ule index, span
               brif ok, fast, slow
fast:  off   = ishl_imm index, log2(SMALL_INT_STRIDE)
       base  = load.i64 [ctx + offset_of!(RuntimeContext, small_ints)]
       r     = load.i64 [base + off]
slow:  (cold) r = call praxis_alloc_int(ctx, value)
```

Four loads, two compares, two never-taken branches, and a shift-scaled index.
`praxis_alloc_int` keeps its `#[no_mangle]`, its `abi_guard!`, its
`Effect::Allocates` manifest row and its address arm; it simply stops being
called on the branch where it would have done nothing.

**The token is permission to *collect*, not permission to allocate.** That
sentence is what the whole decision turns on, and it is a claim about `Safepoint`
rather than a convenient reading of it: `Heap::pace` is the only producer
*because* `pace` is where `maybe_collect` runs, and `Heap::alloc` demands one
*because* an allocation is the event that must have offered the collector a
turn. The inline fast path offers the collector a turn — it evaluates exactly
the predicate the turn consists of — and then allocates nothing whatsoever. It
hands back an immortal the runtime minted before `main` ran, on a page sweep does
not walk.

So the equivalence is not approximate, and it is worth writing out per branch:

| state | before | after |
|---|---|---|
| due | `pace` collects, then table or `alloc` | branch to `praxis_alloc_int`, which paces, collects, and does the same |
| not due, in range | `pace` does nothing; return `small_ints[i]` | return `small_ints[i]` |
| not due, out of range | `pace` does nothing; `Heap::alloc` | branch to `praxis_alloc_int`, same |

The middle row is the only one that changed, and what it dropped is
`RuntimeRoots::from_context` (which has no effects — it reads six pointers out
of the context and builds a struct) and a `maybe_collect` that returned `false`
without touching a byte. **The collection schedule is not changed, delayed or
reordered:** a collection that used to happen at a given `Materialize` still
happens at that `Materialize`, because the branch that reaches the wrapper is the
same predicate on the same two words.

That preserves ADR-100 decision 3 rather than trading it away. That decision's
argument is that returning before `Heap::pace` would let a loop reach the
threshold and never act on it, because in such a loop nothing else touches the
counter. The inline path acts on it: `due` is the *first* thing it tests.

**What was rejected.** Testing the range first and the counter only on a miss is
one instruction cheaper on the hot path and is the defect this record exists to
forbid — a program whose pressure came from `Text` or `Vec` would reach its
threshold and then have every collection deferred at every loop counter in
between, for as long as the loop's `Int`s stayed small. It is forbidden by an
assertion about the instruction stream, not by a comment: see decision 4.

## Decision 2: `Heap::collection_is_due` is the one statement of the predicate, and the export surface is two offsets

`maybe_collect` and `maybe_collect_with` each carried their own copy of
`self.bytes_since_collect.get() >= self.collect_threshold.get()`. They now call

```rust
pub fn collection_is_due(&self) -> bool
```

and its doc carries the obligation at the definition, in the imperative:

> a term added here must be added to `emit_inline_intern` in
> `crates/praxis-codegen-cranelift/src/lower.rs`, or generated code allocates on
> a branch where the collector was due.

`Heap::BYTES_SINCE_COLLECT_OFFSET` and `COLLECT_THRESHOLD_OFFSET` are minted with
`offset_of!` beside the struct, fields staying private —
`GcHeader::DESCRIPTOR_OFFSET`'s pattern exactly, for ADR-039 decision 1's reason.
No numeric offset is written anywhere in the backend.

**The narrowness of that surface is the mechanism, and it is a weak one on its
own.** Two offsets is what a two-term predicate needs; a pacer wanting a third
term finds nothing to hand the backend, which is the moment its author reads the
doc above. That is a speed bump, not a proof, so the pairing is also asserted:
`the_pacing_predicate_is_one_unsigned_compare_of_the_two_exported_words` reads a
live `Heap` **through the two exported displacements**, over a state table
straddling the threshold, and asserts the two words it finds compare exactly as
`collection_is_due` answers. It fails three separate ways — an offset naming the
wrong field, a `#[repr(C)]` that stopped being one, and a predicate that grew a
term — and the third is the one it is for.

This is P-2's problem in advance and it was checked against P-2 as shipped:
ADR-112 grew the *threshold rule* three terms and left the *predicate* alone, and
that is the right shape. `next_threshold` may become arbitrarily clever; what
generated code reproduces is one compare of two words, and only a change to
*that* owes the backend anything.

## Decision 3: `InlineInternSite` is one value with private fields, and there is exactly one of it

```rust
pub struct InlineInternSite { /* seven private numbers */ }
impl InlineInternSite {
    pub(crate) const fn new(table_offset: usize, min: i64, max: i64, stride: usize) -> Self;
}
// small_int.rs, beside SMALL_INT_MIN/MAX:
pub const INLINE_INTERN_SITE: InlineInternSite = InlineInternSite::new(…);
```

The sequence above needs four displacements and three immediates, six of which
name a private field of a `#[repr(C)]` struct in `praxis-runtime`. Handed over as
loose constants they are six independent chances to pair one table's base with
another's bounds — and there *is* another table: ADR-107's interned ASCII `Char`s
live at `ctx.small_chars` with a different range and a different length. A
`Char` arm written as a copy of this one with `SMALL_INT_MIN`/`SMALL_INT_MAX`
left in place reads past the end of a 128-entry table, silently, for any code
point above 127.

So they are one value, its fields are private, its constructor is `pub(crate)`,
and the one instance is minted in the module whose own doc already calls itself
"the one statement of the range". The backend cannot assemble a site; it can only
name one this crate wrote, and it takes it **by value** as an argument to
`emit_inline_intern`. "Inline-probe a table generated code has no right to probe"
has no spelling, because there is no second value to name. P-4a's `Char` arm will
mint its own in `small_char.rs`, next to *its* bounds, which is where that
decision belongs.

`new` fills the two pacing offsets from `Heap` itself rather than taking them as
arguments. A site describing a table but not the pacing words would be exactly
"permission without the obligation", and this is the one place the type system
can refuse to express that.

**This is `SlotCount` / `ImmortalWitness` again, and it is deliberately smaller
than the `InlineAllocSite` the mapping for this item proposed.** That sketch
carried a class index, a block stride, a payload offset and a payload size, and
refused to exist for a descriptor whose `owned_bytes` charge or size class the
inline sequence could not reproduce. Every one of those fields is for the *claim*
path — P-1b, the bitmap allocation — and P-1a allocates nothing at all. A type
named `InlineAllocSite` carrying only intern-table metadata would be a promise it
does not keep, and four fields nothing reads are four fields that go stale before
their first reader arrives. When the claim path lands it wants its own value with
its own refusals; the name is left free for it.

**What the type cannot do, stated rather than implied.** It cannot force the
backend to *emit* the pacing compare. That claim is about an instruction stream,
and no value passed into a function constrains what the function emits. Decision
4 is where it is carried, and it is carried by a test that reads the emitted
Cranelift.

## Decision 4: the IR-shape tests are the gate for the half nothing else can see

ADR-102's consequence — "the instruction is the fact" — applies here with more
force than it did there. A behavioural test cannot distinguish an inline load
from a call to a wrapper that performs the same load; that was true of
`ExtractScalar`. Here it is stronger still: **in the state the fast path runs in,
"the collector was offered a turn and declined" and "the collector was not
offered a turn" are the same observable program.** No test that runs a program
can see the pacing compare, or see it being deleted.

Three tests read the emitted IR:

- `an_inline_int_box_tests_the_pacing_counter_before_it_reads_the_table` — both
  pacing words are loaded in the entry block, compared with `icmp uge`, and the
  branch on the result is the entry block's **terminator**; the table base is
  *not* read before it. This is decision 1's rejected alternative, written as an
  assertion.
- `an_inline_int_box_probes_the_intern_table_before_it_allocates` — the index
  arithmetic and the span compare are present, every immediate equals the one the
  site carries, and the hot path contains no `call`.
- `the_only_block_that_calls_praxis_alloc_int_is_the_cold_one` — exactly one
  block is cold, it is the one that calls, no hot block calls, and both bail-outs
  branch into that one block rather than each growing a callee.

Two behavioural tests cover what IR shape cannot: that the arithmetic is right,
and that the collector still runs.
`an_inline_interned_box_is_the_object_the_wrapper_would_have_answered` computes
each boundary of the table through a loop (so nothing can fold it back into an
`Inst::ConstGc`) and asserts pointer identity with `Immortals::small_int` —
an off-by-one in the index is a *wrong number*, silently, which is the only wrong
answer this change can produce. `a_loop_that_boxes_only_large_ints_still_collects`
allocates ~40,000 objects it retains none of and asserts the live count ends
below the iteration count, which cannot hold without a collection having run.
It is deliberately not the `after < before + 1` shape handover 22 §6 found four
tests using wrongly.

## Decision 5: the range test is one unsigned compare, and `small_int` proves it

`small_int::index_of` is `v >= MIN && v <= MAX` — two signed compares and two
branches. The emitted form is the two's-complement identity for the same
predicate, `(v - MIN) as u64 <= (MAX - MIN) as u64`, which is one compare, one
branch, and reuses the subtraction the index needs anyway.

That identity is exact and standard, which is precisely why it is the kind of
thing that gets believed rather than checked until a range change makes it false.
`the_unsigned_range_test_generated_code_emits_answers_index_of` checks it, in the
module that owns the range, against the function that *is* the range: over both
extremes of `i64` (where the signed subtraction overflows and the unsigned result
must land above the span rather than wrapping back into it), over every boundary,
and densely across the range with 64 values of margin either side.

The span and the floor come off the site, so neither `SMALL_INT_MIN` nor
`SMALL_INT_MAX` appears in the backend.

## `RUNTIME_ABI_VERSION` stays at 19 and gains a paragraph

No layout, calling convention or wrapper signature changed. What changed is the
*set of things generated code depends on*, which is the v12 and v17 precedent:
generated code now reads `Heap.bytes_since_collect` and `Heap.collect_threshold`
at fixed displacements through `RuntimeContext.heap`, which it had never done —
ADR-112's Consequences could still say "nothing in generated code reads a `Heap`
field offset" and that sentence is now false. Repacking `Heap` is a
generated-code change from here on.

The number does not move because v19 has not shipped: it is the open window
ADR-105, ADR-107, ADR-109 and ADR-111 are already in, and a version is a
statement about a build. This is one more paragraph in that window.

## Measurements

Apple M2 Pro, 16 GiB, macOS 26.5.2, release build, `benchmarks/sizes.json`
sizes, run directly through `./praxis run` with the size on stdin. The two
binaries are **saved aside and run interleaved**, with which one goes first
alternating on successive reps, because the laptop drifts by several percent over
a few minutes (handover 23 §5). Minimum per arm. Every run's stdout was compared
against the other arm's; all matched, on all seven benchmarks.

**The two binaries differ in two lowering arms and nothing else.** The baseline
is this tree with `Inst::Materialize { Int }` and `Inst::Alloc { AllocKind::Int }`
restored to `call_symbol` — not the previous commit, which would have folded in
ADR-109's 16-byte header, ADR-110's inline `Bool`, ADR-111 and ADR-112's bounded
pacer and reported them as this change's win. (It was measured that way first,
and the difference is large: `mandelbrot` reads −14.4% against the previous
commit and −0.8% against the right baseline. The wrong number is recorded here
because a comparison that cannot say what it held constant is not a measurement.)

| | before | after | | reps |
|---|---:|---:|---|---:|
| `collatz` @ 340,000 | 1.233 s | **1.003 s** | **−18.7%** | 5 |
| `primes` @ 1,600,000 | 1.430 s | **1.231 s** | **−13.9%** | 5 |
| `mandelbrot` @ 430 | 2.550 s | 2.530 s | −0.8% | 3 |
| `hashwork` @ 9,400,000 | 3.986 s | 3.986 s | — | 3 |
| `vm` @ 2,800,000 | 7.116 s | 7.166 s | +0.7% | 3 |
| `pipeline` @ 1,000,000 | 3.795 s | 3.849 s | +1.4% | 5 |
| `tree` @ 330 | 2.943 s | 3.002 s | +2.0% | 5 |

`collatz` and `primes` are the two benchmarks whose inner loops are `Int`
arithmetic feeding an `Int` accumulator, and they are where the change is. The
`collatz` figure reproduced to the tenth of a percent across two independent
five-rep passes.

**`tree` and `pipeline` pay, reproducibly, and it is the honest cost of the
shape.** Both came out at +1.4%/+2.0% in a three-rep pass and again at
+1.4%/+2.0% in a five-rep pass, so it is outside this machine's drift. Their
`Materialize`s box values that mostly leave `small_int`'s range, so what they get
is the two loads, the compare and the branch **in front of** the call they were
already making, plus three extra basic blocks per site for the register allocator
to carry at `opt_level = "none"`. A 2% cost on the two allocation-bound
benchmarks buys 19% and 14% on the two arithmetic-bound ones; the trade is
recorded rather than hidden, and the obvious way to narrow it was tried and does
not work — see below.

### The single-branch variant, measured and rejected

Folding the two tests into one branch — `bail = due | (index > span)`, one
`brif`, one fewer block per site — is the obvious response to `tree`'s and
`pipeline`'s +2%, and it is what a reader will propose. It was built (same tree,
same harness, five reps, interleaved) and it is **worse on every benchmark**:

| | two branches | one branch | |
|---|---:|---:|---|
| `collatz` | 0.954 s | 1.015 s | **+6.3%** |
| `primes` | 1.172 s | 1.195 s | +2.0% |
| `tree` | 2.957 s | 3.027 s | +2.3% |
| `pipeline` | 3.836 s | 3.864 s | +0.7% |

The mechanism is the register file, not the branch predictor. Two conditional
branches read the condition codes the two compares already set — on aarch64 that
is `cmp; b.hs; cmp; b.hi`, and both are predicted not-taken on the hot path, so
they cost essentially nothing. The `bor` cannot read flags: it needs both
compare results *materialized*, so `cset`, `cset`, `orr`, `cbnz` replaces two
branches with three ALU ops and a branch. Trading two free branches for three
real instructions is the wrong direction, and it is the wrong direction on the
allocation-bound benchmarks too, which is what says the +2% above is not the
branch count.

(The variant was measured with the now-unreachable middle block left in as a
stub, which Cranelift drops; the 6.3% on `collatz` is an order of magnitude
larger than any layout artifact could account for, and the instruction-count
argument above predicts its sign and rough size.)

### `opt_level = "speed"`, the third measurement

Handover 21 §3.7 and handover 22 §4 both record `"speed"` as a negative result,
and the standing explanation for it is that allocation is an opaque call and a
memory clobber, so Cranelift's mid-end cannot move anything across a loop body.
This change removes exactly that, for the commonest allocation in the suite —
which is why handover 23 nominated the moment after P-1a as the point to try
again. Same harness, this tree, `"none"` versus `"speed"`:

| | none | speed | |
|---|---:|---:|---|
| `collatz` | 1.003 s | **0.939 s** | **−6.3%** |
| `primes` | 1.229 s | 1.209 s | −1.6% |
| `mandelbrot`, `vm`, `tree`, `hashwork`, `pipeline` | | | within ±0.5% |

The compile-time floor (size 0, seven reps, minimum) costs **+0.2 to +0.9 ms**
per program — `tree` +0.9 ms on a 35 ms floor, `bfs` +0.8 ms on 92 ms — against
the "up to 4.7 ms" handover 21 measured, because there is less code to optimize
now.

**The result has changed and the standing comment is stale.** It is no longer
"every workload within ±3% both directions": one benchmark moves 5–6%
reproducibly, six do not move at all, and the compile-time cost is under a
millisecond. That is not yet a reason to flip the default — one benchmark is one
benchmark, and `collatz` is the one whose loop this change most directly opened —
but it is the first non-null result the flag has produced, and the third
measurement is the one the comment at `Jit::in_generation` asked for. **The
comment and handover 21 §3.7 need updating with these numbers**, and the default
is left at `"none"` here because flipping it is a change to a file this record's
work did not otherwise touch and deserves its own decision.

## What was deliberately *not* done

**P-1b, the bitmap claim, is not here.** The mapping for this item splits P-1 at
exactly this seam, and the split holds: P-1a bakes `RuntimeContext` and `Heap`
scalar offsets and nothing else, while the claim path bakes eight `PageHeader`
offsets and three `GcHeader` ones, writes a header generated code must get
byte-for-byte right, and bumps two live counters whose disagreement is a
use-after-free. It needs the `mark`/`sweep`/`finalize_all` debug audit the mapping
describes; P-1a needs none of it, because it writes nothing at all. The audit is
not added speculatively — there is no header for it to audit until there is a
header generated code wrote.

**`Char` keeps its call**, although `small_char` is the same shape of table.
`AllocChar`'s manifest row is `AllocatesAndFaults`, so an inline path must also
route an invalid code point to the wrapper that raises `InvalidChar` with the
same message and the same `CheckFault` diversion (RT-18's regression), and P-4a
may move that validation into the table's own bounds and change the shape of the
arm. Doing it now means writing it twice.

**The `spill.spill_roots` above these arms stays**, verbatim, for ADR-110's
reason: `Inst::Materialize` and `Inst::Alloc` are unconditional safepoints in
MIR, and that is a MIR-level property about which instructions the collector may
run at, not a backend arm's to narrow from what it happens to emit. Narrowing it
here would put the manifest's answer and the backend's answer in two places
(MIR-10). It also happens to be what makes the cold arm correct without further
thought: the roots are in the shadow frame before the branch, so the wrapper may
collect.

**`praxis_alloc_int` is not deleted, narrowed or split.** It is the cold block's
callee, it is what the debugger's throwaway modules and any future non-inlining
path call, and keeping it as the callee is what makes "the answer is what it
always was" a property of the code rather than of this document.

## Consequences

- **The collection schedule is bit-for-bit unchanged**, which is a stronger claim
  than "safe" and is what decision 1's table is for. Every existing pacing test
  is therefore still testing what it was; none needed editing, which was checked
  rather than assumed.
- **Generated code now depends on `Heap`'s field layout.** ADR-112's Consequences
  say "nothing in generated code reads a `Heap` field offset — `lower.rs` takes
  offsets only from `RuntimeContext` and `EnumPayload`". That sentence is now
  false, and it was true for the whole life of the project until this change.
  Repacking `Heap` is a generated-code change.
- **The type system carries the *which table* half of the obligation and the
  tests carry the *which instructions* half**, and the boundary between them is
  stated in `InlineInternSite`'s doc rather than left for a reader to discover.
  An enforcement mechanism that is claimed to prove more than it does is worse
  than none, because the next person trusts it.
- **A `Materialize { Int }` site is now three basic blocks and a join.** ADR-102's
  consequence — `blocks[blk_idx]` is the MIR block's *entry* only, and an
  instruction lowered later in the same MIR block may land in a different
  Cranelift block — now applies to the commonest instruction in the language
  rather than to checked arithmetic and scalar reads. `emit_inline_intern` leaves
  the builder switched to the merge block for the same reason `emit_scalar_load`
  does: `spill.store_debug_defs` runs immediately after `lower_inst` and must land
  where both arms are visible.
- **`opt_level = "speed"` has a real result for the first time**, and the comment
  telling people not to try it a third time is stale. See above.
- **Two benchmarks are ~2% slower**, and the cost falls on exactly the programs
  whose integers leave the interned range. Raising `SMALL_INT_MAX` is now a
  sharper trade than it was: it is still ADR-100's resident-memory question, but
  it now also decides which side of this branch a program's arithmetic lands on.
- **`InlineInternSite` is a public type with no public constructor**, so the
  workspace has one more value that can be named and not made. That is the third
  (`SlotCount` bounds a frame, `ImmortalWitness` confines minting, this confines
  probing), and the shape is now a house idiom rather than three coincidences.

## Open questions

- **Where does `tree`'s +2.0% actually go?** It is not the branch count — the
  single-branch variant above is worse still — so it is either the three extra
  basic blocks per site at `opt_level = "none"` or the cost of the pacing loads in
  front of a call that was going to happen anyway. The two have different
  remedies (the first is a codegen-quality question, the second says raise
  `SMALL_INT_MAX`), and telling them apart needs a profile rather than another
  A/B.
- **Should `bytes_since_collect`/`collect_threshold` become one signed countdown?**
  One load, one sign test and one branch instead of two loads and a compare — and,
  more to the point, an export surface of a *single* offset, which is the
  strongest form of decision 2's argument, because a pacer needing a third term
  would then have nothing at all to export. It is a `Heap` change with a `Pacer`
  interaction and it wants ADR-112's author.
- **Should `Inst::EnumTag` acquire the descriptor check?** Unchanged from
  ADR-102's list; noted only because this record adds a second inline site that
  proves its precondition and a third that still does not.
- **Is `collatz`'s remaining cost the debug frame?** ADR-102 asked whether it was
  `praxis_alloc_int` or handover 21's §3.2. It was `praxis_alloc_int`, and that
  question is now answered and closed. What is left of `collatz` should be
  re-profiled against this tree before anything else is attributed to it.
