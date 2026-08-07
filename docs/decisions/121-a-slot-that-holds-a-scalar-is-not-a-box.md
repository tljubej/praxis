# ADR-121: A slot that provably holds a scalar is not a box, and changing what a slot holds is not substitution

**Date:** 2026-08-07
**Status:** accepted — implemented
**Milestone:** post-M11 performance
([handover 26](../handovers/26-ten-packages-six-waves-and-the-five-things-25-got-wrong.md)
§4 W8-S1,
[handover 28](../handovers/28-five-numbers-only-a-later-package-caught.md) §11 item 1,
wave 5 — the decision gate, opened)
**Amends:** `praxis-mir`'s `ir.rs` header, which said a `Scalar` local "must not"
survive a GC safepoint. That sentence was never the rule the tree enforced and
was contradicted by `verify.rs`'s own header and by the `lower_seq_*`
accumulators; decision 3 states what the rule actually is. Everything else —
ADR-015's non-SSA slot model, ADR-088's positional fault rule, ADR-108's refusal
of a pass framework, ADR-120's forwarding, ADR-122's provable descriptors — is a
constraint this fits inside, and each is where a decision below comes from.

## Context

ADR-120 deletes a box whose only reader is in the same block. What it provably
cannot reach is the shape [handover 28](../handovers/28-five-numbers-only-a-later-package-caught.md)
§11 ranked first on what was left: a **loop-carried assignment**, which
`crate::build` lowers to a `MoveGc` into the binding's existing slot. ADR-120's
own census named the survivors — `mandelbrot`'s `x` and `y`, `collatz`'s `c` and
`steps` — and said in as many words that they "are W8-S1's".

The cost of one such slot is not one box. Measured on `dump.rs`'s canonical loop
before this package, per iteration:

* one `Materialize` per assignment — ADR-113's pacing branch, unsigned range
  test, table read, and an out-of-line call on the miss;
* one **guarded** `ExtractScalar` per *use*, and a use is not once per binding:
  `collatz`'s `c` is read three times per iteration, in three different blocks,
  and ADR-102 made each read a descriptor load, a second load of the context's
  descriptor, a compare and a branch before the payload load;
* and — the part no instruction count shows — a *doubling of the per-iteration
  paths*. Each inline box has a hot fast path and a hot slow path, so a loop with
  two of them has **four** distinct per-iteration cycles and `periter.py` refuses
  to name one number for it.

Handover 28 budgeted this as "a new analysis, not W8-S0 with a wider gate", on
ADR-120 decision 1's statement that loop-carried substitution needs dominance and
reaching definitions — two of the four analyses ADR-108 declined to build. That
is true of substitution. It is not what this needed.

## Decision 1: change what the **slot** holds, which asks no dominance question

MIR is not SSA. A `LocalId` names a *slot*, not a value, which is exactly why
ADR-120 decision 1 refuses whole-function `LocalId` substitution: replacing local
`e` with local `s` at "the uses `s` reaches" requires knowing which definition
reaches which use.

This pass never asks. It rewrites **every** definition and **every** use of one
slot, uniformly, and changes the slot's `LocalKind` from `Gc` to `Scalar(k)`. A
total rewrite has no reaching-definition question because the answer is the same
at every point. That is the same argument `provable.rs`'s header makes for its
own flow-insensitivity — *"the answer is a property of the slot… so it holds at
every point that reads the slot without any dominance or reaching-definitions
question, which is exactly the four analyses ADR-108 declined to build"* — and
it is why `promote.rs` is a scan and not a dataflow.

**The consequence for the budget is the headline.** The pass is 400 lines, it
adds one `Inst` variant, and it needed no new analysis at all.

## Decision 2: the kind comes from `ProvableDescriptors`, unchanged

ADR-122 already answers "which `DescriptorClass` does *every* definition of this
slot produce", over exactly the four shapes where MIR wrote the descriptor down.
A slot every definition of which produces `Int` **is** an `Int` slot. Three of
its properties are load-bearing here rather than convenient:

* **A parameter is `Bottom`.** Its trap 1 seeds a definition-less local `Bottom`
  rather than letting a universal quantifier bless it vacuously — so parameters
  are excluded *by construction*, not by a check here that could be forgotten.
  Promoting one is an ABI change and is not this package; see Open questions.
* **A runtime result is `Bottom`.** A `Vec[Int]` element read through
  `praxis_vec_get` has no proof, so no `MirType` is consulted and no front-end
  guarantee is believed — the premise handover 26 §4 read the repair log and
  refuted.
* **It is a greatest fixpoint**, so a loop variable whose back edge assigns it
  from another loop variable resolves to its class instead of collapsing to
  `Bottom`. That is precisely the shape this pass exists for. A pessimistic
  analysis would find nothing.

**No new analysis, and no second answer to "what does this slot hold".** The
alternative — reading `Local::ty` — is the one handover 26 §4 refuted, and it
would also have gone blind exactly where it mattered: 35 sites in `build.rs`
lower to `MirType::Opaque`, including `cur` in every compound assignment.
Definition-kind inference does not care.

## Decision 3: a `Scalar` local may cross a safepoint, and it always could

`ir.rs`'s header said a `Scalar` local "**must not** survive a GC safepoint — the
lowering materializes a fresh `GcRef` from them before any safepoint, call,
store, or return". Read as a rule, this package violates it on every line.

It was not the rule. `verify.rs`'s header says so directly, and has since F17:

> **`ScalarLiveAcrossSafepoint` is not implemented, and that is a decision.** …
> A scalar is a *copy* of a payload, so it cannot dangle when the object it came
> from is collected; the invariant that actually matters is "no raw word in a
> slot the collector reads", and `RootIsNotGc` plus `MoveGcFromScalar` are that
> invariant stated directly.

And the tree already relied on it: a `sum`'s running `i64` is live across every
`praxis_vec_get` in a fused pipeline, by construction, and has been since M6.

So the `Gc`-side rule (no raw word in a rootable slot, P0-03) is real and
enforced, and its converse was a sentence in a header that no rule implemented.
The header now states the real rule. **This is the only thing this ADR amends,
and it is a correction to prose, not a relaxation of an invariant.**

## Decision 4: one new instruction, `Inst::MoveScalar`

A promoted slot is *assigned* — `acc = acc + i` — so a word must be able to move
between two scalar slots. ADR-120 refused this variant, correctly, because
operand-rewriting never needs it; handover 26 §3 framed it as a dilemma between
adding `MoveScalar` and doing whole-function substitution. Decision 1 is the
third option, and it needs the move.

`verify` gets the mirror of `MoveGcFromScalar`: **`MoveScalarKindMismatch`**,
which requires both slots to be `Scalar` **of the named kind**. The width half is
not decoration — `MoveGcFromScalar` checks membership only, and a rule here that
did the same would admit moving a `Bool` word into an `Int` slot, which re-boxes
one byte as eight. The failure the *kind* half guards is worse than the one it
mirrors: that rule stops a raw word entering a slot the collector dereferences;
this one stops a `GcRef` entering a slot the collector **ignores**, which is an
object swept while live.

The backend arm is `MoveGc`'s, verbatim — every MIR local is one Cranelift
`Variable` of type `I64` regardless of kind — and the duplication is deliberate:
merging them would say the two instructions are interchangeable, which is true in
the backend and false in MIR. Cranelift's copy propagation removes the move
outright, which is what makes a promoted slot cost *nothing* rather than less.

## Decision 5: profitability is decided per **copy group**, weighted by loop membership

Promotion removes boxes at definitions and adds them at uses that genuinely want
an object. Two things make the naive rule wrong, and both were caught as failing
tests rather than reasoned out:

**Static counts decide it backwards.** `var acc` summed in a loop and printed
once removes one box per iteration and adds one at the `out(acc)` — one against
one. Every block on a cycle therefore weighs `LOOP_WEIGHT` (10). "On a cycle" is
plain reachability from a block to itself, not a loop-finder: nesting is
invisible, so `mandelbrot`'s three `while`s are one region and an inner block
weighs the same as an outer one. That under-values and never over-values, and no
shape in the suite is close enough for it to change an answer.

**A value does not live in one slot.** `var m = if c > 3 { … } else { … }` lowers
to a result temp assigned by a `MoveGc` in each arm and a second `MoveGc` into
`m`. Scored one local at a time that chain unravels from the far end: `m` alone
sees one box added and none removed and is declined; the temp feeding it then
sees a copy into a non-promoted slot and is declined; and so on backwards until
nothing is promoted and the boxes have merely moved. So `MoveGc` edges between
two candidates union them, and the group is scored as one.

The groups are then **independent** — no `MoveGc` has a candidate on each side of
a group boundary, by construction — so one pass is a fixed point and no
iteration is needed.

## Decision 6: `Char` is excluded by the fault gate, not by name

No definition of a promoted slot may fault. `praxis_alloc_char` validates its
Unicode scalar, so ADR-088 puts a `CheckFault` after every `Alloc { Char }` and
`Materialize { Char }`, and replacing one with a `MoveScalar` would orphan that
check — `verify::check_fault_observed` refuses the function. The same asymmetry
runs the other way at a use: re-boxing a promoted `Char` needs a faulting
`Materialize` and therefore a check this pass would have to synthesize.

Both doors are shut by one rule, and the rule is written as "cannot fault"
rather than "not `Char`" so that a future non-validating constructor opens both
without editing this. `Byte` is excluded one layer up: it has no descriptor at
all, so `DescriptorClass::of_scalar` answers `None` and there is nothing to
prove.

## Decision 7: the promoted slot's `Gc` local stays in the table

ADR-120 decision 7's shape, reused without change. The `Gc` local keeps its name,
its `symbol_id`, its static type and its position; it simply has no definitions.
`Function::debug_scalar_sources` points its debug slot at the `Scalar` local that
now holds its word, the backend's ADR-120-part-2 path stores that word at every
definition of the scalar, and `DebugSlotKind` decodes it on the way out.

**The debugger's half of this package is therefore almost entirely ADR-120 part
2's, already built.** That is the single largest reason the budget came in under
handover 28's estimate — and it is worth recording as a property of that design
rather than as luck: part 2 was built to give one elided temp its value back, and
it generalized to every promoted binding in the language without an edit.

## Decision 8: the debug slot is **biased**, so `var i = 0` is not `<uninit>`

A debug slot is one word and a claim zeroes its run, so the all-zero word means
"nothing written here yet". For a `Reference` slot that is exact — a `GcRef` is
`NonNull`. For a scalar slot it *cannot* be: 2^64 payloads do not fit in 2^64
words beside an "unwritten" state, so exactly one payload per kind must collide,
and no encoding avoids it. **The only question is which.**

Storing the payload raw makes the collision `0`, and therefore `false`, and
`0.0`. ADR-120 part 2 accepted that, correctly, because the only slots it reached
were temps whose box the block-local forwarding had elided — `<tmp#3: Int> @ "0"
= <uninit>` is a poor line in a rare place. **This package promotes bindings, so
the same collision reaches `var i = 0`**, which is close to the most common line
a Praxis program has. It arrived as a failing test
(`a_fault_between_a_definition_and_the_next_safepoint_shows_the_value`) rather
than as a discovery in the field.

`DebugSlotKind::store_bias` fixes it by choosing where the collision lands:

| kind | bias | the one payload that reads `<uninit>` |
|---|---:|---|
| `Reference` | 0 | none — `NonNull` has no zero |
| `Bool` | 1 | **none**: two payloads, biased to 1 and 2 |
| `Char` | 1 | **none**: `0..=0x10FFFF` biased clear of zero |
| `Byte` | 1 | **none**: `0..=255` biased clear of zero |
| `Float` | 1 | one quiet NaN (`0xFFFF_FFFF_FFFF_FFFF`) |
| `Int` | `i64::MIN` | `i64::MIN` |

Three kinds lose nothing at all. `Float` loses one NaN out of the 2^52 that are
NaN, all of which print `NaN`. `Int` loses a value a program could compute, and
`i64::MIN` against `0` is the whole trade — made toward the number nothing
reaches by accident. `small_int`'s range starts at `-256` and the sentinel idiom
that module names is `-1`; neither is near this.

**Why not a written-marker, which loses nothing.** A parallel byte per slot,
zeroed by the claim and set by each store, is exact — and is a *second store per
definition*, on the path ADR-120 part 2 already measures at 2.4% of the suite.
The bias is one `iadd_imm_s` on a register that is about to be stored anyway: an
ALU operation with no memory traffic. Exactness here is worth an instruction, not
a store.

**It also repairs a live ADR-120 defect.** `<tmp#3: Int> @ "0" = <uninit>` was
wrong on the tree before this package and is right after it.

## Decision 9: `Float` is promoted, and one program's output changes

`DynamicKey::eq` opens with a pointer-identity fast path, reflexive for every
type whose `equals` is — and `Float`'s is not, because NaN is not equal to
itself. Whether two NaNs deduplicate as `Set` members has therefore always
depended on whether they arrived as one object or two, and the language has
always answered the two spellings differently: two `0.0 / 0.0` expressions build
two objects and do not deduplicate; two reads of one binding hand over one object
and do.

Promotion materializes a promoted `Float` afresh at each use, moving the second
spelling onto the first's answer — which is what `float_equals` says and what
IEEE-754 says.

**It reaches far less than that makes it sound, and decision 5 is why.** A
`Float` bound and then used as a key is exactly the shape the profitability rule
declines: one box removed at the definition against one added at every `insert`.
It takes a `Float` that is *also* worth promoting — loop-carried arithmetic — for
any answer to move.
`run.rs`'s `a_nan_key_deduplicates_or_not_depending_on_whether_its_slot_was_promoted`
is both sides of that boundary in one program, so the reach is a test rather than
a paragraph.

**Two fixes would make the question moot, and neither is in this package.**
Gating the fast path on whether the descriptor's `equals` is reflexive costs one
load and one predictable branch per key comparison. Refusing `Float` as a
`CapKind::HashStable` type costs nothing at run time at all and is what Rust does
(`f64` implements neither `Eq` nor `Hash`, for this exact reason);
`praxis_hir::capability::supports_hash_stable` is already shaped for it and
already recurses structurally, so "or types that contain floats" would be free.
It would also contradict ADR-083 and ADR-138, both of which use `Set[Float]` as
their worked example, and a passage in the book's numbers chapter. **That is a
language decision with its own ADR, not a line in a performance one.**

## Decision 10: each package's gate reads the MIR its own package produced

Seventeen tests across four modules went red when this landed and **not one had
found a defect**. ADR-108's tests assert that a loop-invariant literal's `Alloc`
moved to the preheader; this package deletes that `Alloc`, so they read zero
allocations in the preheader with the hoist working perfectly. ADR-120's census
tests are documented as *post-W8-S0* measurements and were reading post-W8-S1
MIR. ADR-122's inner-loop census says "the post-W8-S0 inner-loop census, to the
site" and was measuring something else.

`build::lower_module` is therefore split into `lower_module_raw` and `optimize`,
and `test_support` grows two doors beside `lower_src_to_mir`:
`lower_src_to_mir_unoptimized` (the builder's own output — ADR-108's gate) and
`lower_src_to_mir_forwarded` (plus ADR-120's pass — ADR-120's and ADR-122's).
Both are behind the same `test-support` feature the module already uses, so
`cargo build` cannot reach either and no host can lower through one.

**This is not a pass manager and does not reopen ADR-108 §1.** There is no
registry, no ordering table, and no host that calls a pass: `lower_module` is
still the one door production code has and still runs both passes.

One test in `build::tests` reads the *middle* door and says so — 
`a_char_pattern_emits_a_comparison_and_not_a_jump` counts two `ExtractScalar
{ Char }` where the builder writes four, and its own comment explains that
`forward` folded the other two.

## Decision 11: the mark phase keeps its grey set

`Heap::mark` built a fresh `Vec<GcRef>` for its grey set on every collection,
grew it through the whole doubling ladder to the size of the transitive closure
— 8 MiB on a million-element working set — and dropped it. Sixty-odd times in
one run.

macOS's allocator does not return large freed regions promptly; it caches them.
`vmmap` on `pipeline` showed **64 cached `MALLOC_LARGE (empty)` regions holding
489 MiB** where the reverted arm held 4 regions and 16 MiB, and live malloc bytes
were ~84 MiB in both — so none of that half-gigabyte was in use. It was resident,
and peak RSS counts resident. Peak went **110 MiB → 592**.

The grey set now lives on the `Heap` and is `clear()`ed, so after the first
collection the mark phase allocates nothing. `pipeline` peaks at 106 MiB.

**ADR-121 did not create this churn; it made the same churn happen in less
wall-clock time**, which is what moved the cache from 4 regions to 64. The fix
belongs here anyway: a collector should not allocate a buffer proportional to the
live set on every collection, whatever the compiler above it is doing.

## Decision 12: a wrapper that can grow a buffer charges the pacer

This is the one to read.

`Heap::alloc_raw` charges `stride + owned_bytes_of(payload)` once, at
construction, and left later growth uncharged on a premise its own comment
states:

> Growth *after* this point — a `push` that reallocates — is still uncharged;
> **its elements are themselves paced allocations**, so the residual under-count
> is the spine, not the contents.

That was true when it was written. **This package falsified it.** When every
scalar the program computed was a heap object, an allocation-light program did
not exist: the arithmetic feeding a `push` paced the collector even when the
`push` did not. Promotion deletes exactly those allocations.

`bfs` fell from **41 collections to 6**, with an *identical* live set and a
**smaller** GC page heap (15.7 MiB against 37.7 — promotion working), and a peak
resident set of **224 MiB against 61**. Forcing the pacer to a 256 KiB ceiling
still bought only 10 collections, because nothing was advancing the counter at
all. The collector was not running because nothing told it anything had happened.

Every wrapper that can grow a buffer now charges the growth: `Vec::push`, **both
ends** of `Deque`, `Map::insert`, `Set::insert`, `Counter::set`, `BitSet::insert`
and both heaps. The delta is measured through the payload's own `owned_bytes()`,
which the descriptor callback also delegates to — so the size formula has one
statement rather than a fresh one at each growth site, which is what the first
draft of this got wrong (three sites, three copies of `capacity() *
size_of::<GcRef>()`).

`bfs` now collects 16 times and peaks at 55 MiB — below where it started, because
the reverted arm gains from this too (60 → 54).

**Nine tests pin it, one per collection, and their design is the point.** Each
pushes only values inside `small_int`'s interned range. An interned `Int` is an
immortal the allocator never charges for, so the spine is the only thing that can
move the counter; push un-interned values instead and all nine pass whether or
not the growth is charged, because the elements would be paying for it — which is
the falsified premise, restated as a way to write a worthless test. With
`charge_growth` stubbed out, all nine fail.

## Decision 13: what this says about the pacer, beyond the two bugs

**A pacer driven by allocation volume has a hidden dependency on the compiler not
being good at removing allocations.** That coupling was invisible for as long as
the compiler was not good at it, and every measurement in `benchmarks/` was taken
in that regime.

It is worth stating as a standing hazard rather than a fixed bug, because the
same shape will recur. Decision 12 closes the collection spines. It does **not**
close the general form: any future optimization that removes allocations removes
pacing signal with them, and the failure mode is not a wrong answer but a
collection that silently does not happen — which reads as a leak nobody connects
to the optimization that caused it. Handover 23's D-1 records the sibling
observation about `MAX_RECURSION_DEPTH` being a call count rather than a byte
budget; this is the same class.

A pacer whose input is *memory the process holds* rather than *bytes the
allocator was told about* would not have this coupling. That is a larger change
than this record should make, and it is the open question the next performance
round should open with.

## Measurement

**The instruction counts are the headline**, per handover 28 §2's rule: a
deterministic count is what a change of this shape earns, and this repo has
predicted "the clock cannot resolve this" twice and been wrong both times.

### Per iteration, hot path

`PRAXIS_DUMP_VCODE` + `benchmarks/periter.py`, arm A against arm B:

| loop | arm A | arm B | note |
|---|---|---|---|
| `dump.rs`'s canonical loop | **114–197**, 4 hot cycles | **39**, 1 cycle | |
| `collatz` inner loop | **121–165**, 4 cycles | **55–57**, 2 cycles | |
| `primes`' `is_prime` | **131–172**, 2 cycles | **82**, 1 cycle | |

**The collapse in the number of hot cycles is a result in its own right.** Each
inline box contributes a hot fast path *and* a hot slow path, so two boxes in a
loop mean four distinct per-iteration paths and no single number. Removing the
boxes removes the branch structure around them: the canonical loop has one
per-iteration count again for the first time since ADR-113.

The canonical loop's body is now `ConstInt`, `IntBinOp`, `IntCmp`, `CheckFault`
and `MoveScalar` — **zero** `ExtractScalar`, zero `Materialize`, zero `Alloc`.

### Whole entry point, machine instructions

| benchmark | arm A | arm B | ratio |
|---|---:|---:|---:|
| `mandelbrot` | 2412 in 301 blocks | **728 in 85** | **3.31×** |
| `hashwork` | 2927 in 415 | **1443 in 205** | **2.03×** |
| `collatz` | 803 in 110 | **500 in 59** | **1.61×** |
| `pipeline` | 2572 in 369 | **1722 in 241** | **1.49×** |
| `bfs` | 4789 in 700 | **3479 in 548** | **1.38×** |
| `tree` | 714 in 91 | **541 in 66** | **1.32×** |
| `primes` | 581 in 79 | **489 in 64** | 1.19× |
| `vm` | 2774 in 332 | **2612 in 326** | 1.06× |

`primes` and `vm` read low for opposite reasons and neither is disappointing.
`primes`' hot code is `is_prime`, a separate function, which goes 492 → 358
instructions and 131–172 → 82 per iteration. `vm`'s entry point is an interpreter
whose values are `Vec` elements and tagged-union payloads — `Bottom` under
decision 2, and correctly so.

### Correctness

* **All 46 corpus programs** under `tests/aoc-corpus` and `tests/input-parsers`
  print byte-identical output.
* `benchmarks/run.py --pilot` agrees across all three implementations of all
  eight benchmarks.
* The full workspace suite is green: 32 targets, 0 failures.

### The clock

`benchmarks/ab.py`, 5 reps of A,B,B,A with the leading arm alternating, paired
median of the per-pair ratios, both arms built from *this* tree with only the
`adr121-arm-a` toggle between them. `benchmarks/ab-ADR121.json` holds it.

| benchmark | paired | resolved |
|---|---:|---|
| `collatz` | **3.116×** (+211.6% ± 0.4%) | yes |
| `mandelbrot` | **2.396×** (+139.6% ± 0.7%) | yes |
| `primes` | **1.532×** (+53.2% ± 0.4%) | yes |
| `bfs` | **1.471×** (+47.1% ± 2.6%) | yes |
| `vm` | **1.407×** (+40.7% ± 0.6%) | yes |
| `hashwork` | **1.294×** (+29.4% ± 1.0%) | yes |
| `pipeline` | **1.180×** (+18.0% ± 0.9%) | yes |
| `tree` | 0.992× (−0.8% ± 0.3%) | **no** — under the 2% floor |
| **geometric mean** | **1.564×** | 7 of 8 |

`tree` is the row the clock could not read, and its shape is decision 2's: `walk`
is recursive, its parameter is `Bottom` under `ProvableDescriptors`, and its
boxes are call arguments and results rather than loop-carried slots. The
instruction count is the result there — 574 → 488 in `walk` — and the clock could
not tell.

**The load ceiling was waived** (`--max-load 4.0`; observed 2.91 at the start and
3.25 at the end, no competing build at either point). The palindrome charges
steady load to both arms and the MAD bar widens with what it cannot absorb, so a
resolved delta is real and an unresolved one may be the machine. **Do not compare
these against a figure taken at the 0.5 ceiling.**

**This absorbs credit from ADR-119, and it is the one measured second.** ADR-119
made each box cheaper and this removes the box, so the two interact
multiplicatively; ADR-119's 1.151× was measured against a tree where the boxes
still existed, and re-measuring it now would report less. Neither number is wrong
and they must not be added.

## The measurement arms

`praxis-mir/adr121-arm-a` makes `promote_scalars` return 0 without looking at the
function. `forward_boxes` still runs, so the two arms differ in the
whole-function transform alone. Everything else in the package —
`Inst::MoveScalar`, the verifier's rule, the backend's arm, decision 8's bias —
compiles unchanged in both.

```bash
cargo build --release -p praxis-cli                              # arm B
cargo build --release -p praxis-cli \
    --features praxis-mir/adr121-arm-a                           # arm A
```

Verified distinct before measuring: the canonical loop is 402 machine
instructions in arm A and 247 in arm B.
`promote::tests::the_measurement_toggle_decides_whether_the_pass_runs` asserts
both arms in one test, so a toggle that stops toggling is a test failure.

## Consequences

* **`praxis-mir` has two optimization passes and an `optimize` step.** The
  ordering is load-bearing: forwarding first, because it deletes the box/unbox
  pairs promotion would otherwise *price*, and a materialization already destined
  for deletion is a cost the profitability rule must not charge a candidate for.
* **`liveness::uses_mut` exists**, directly under `uses` and matching it arm for
  arm. It is this pass's correctness mechanism and has **no `_` arm**: a missed
  field is not a missed optimization the way ADR-120's is — it leaves a reader of
  a slot whose representation changed under it, which is a payload word read as a
  reference. A new `Inst` variant is a build error there.
* **ADR-116's headline has no denominator left in the sample loop.** W6 traded
  three ALU operations for one L1 load *per descriptor proof*; that loop now has
  zero proofs. W6 is not worth zero everywhere — `provable`'s suite census counts
  122 sites across the eight benchmarks — but any figure quoted as "W6 per
  iteration of the sample loop" is now a figure about an absence. The two tests
  carrying that count are renamed and carry the four-answer table.
* **ADR-108's hoist is a no-op for scalar literals.** The literal it moves to the
  preheader is now deleted outright. The hoist still runs and still matters for
  everything else; its gate reads the unoptimized door.
* **An adversarial-audit test needed its allocation pressure restored.**
  `a_dead_local_stops_being_reachable_from_its_frame` ran the collector as a side
  effect of `sum = sum + j` boxing an out-of-range `Int` per iteration. Promotion
  made that loop pure register arithmetic, no collection ran in *either* arm of
  the comparison, and the test read the result as a rooting failure. Its pressure
  loop now allocates a `Vec` per iteration, which no scalar optimization can
  remove. **A test whose mechanism is an incidental allocation is a test with a
  hidden dependency on the optimizer**, and this is the second one this round
  (ADR-120 found the first).

## Open questions

* **Parameters and returns, unboxed.** Decision 2 excludes parameters because
  `ProvableDescriptors` has no proof for them, which is the sound answer without
  an ABI change. `tree`'s recursive `walk` still re-extracts its parameter at
  every use — promotion collapses that to one extract at entry, but the caller's
  box remains. Passing scalars unboxed touches closures, the runtime and the
  debugger and deserves its own record.
* **Should the profitability rule know loop depth?** Decision 5's weight is
  binary. A nested loop whose inner body would pay and whose outer body would not
  is not a shape the suite contains, and inventing a loop-nesting analysis to
  handle one that does not exist is the trade ADR-108 declined.
* **W9 (tagged pointers) should be re-measured or closed.** Handover 26 declined
  it partly because it buys "the ~17 root-spill instructions per loop **that
  W8-S1 also removes**". W8-S1 has now removed them. The recommendation to
  decline should be re-stated against this tree or the item retired.
* **`Char` promotion needs a non-faulting box.** Decision 6's gate is exact, not
  conservative. A `praxis_alloc_char` that took a pre-validated payload — the
  validation is already done by every producer the language has — would open it.
