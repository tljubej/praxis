# Ten packages, 0.3× CPython, and five published numbers that only a later package caught

**Date:** 2026-08-04
**Tree:** `976b44e`. Every number below was read out of this tree, out of
`benchmarks/ab-*.json`, or out of ADRs 114–122; nothing was re-run.
**Predecessor:** [`27-the-five-gates-and-what-26-got-wrong.md`](./27-the-five-gates-and-what-26-got-wrong.md).
This closes the round [26](./26-ten-packages-six-waves-and-the-five-things-25-got-wrong.md)
planned and 27 unblocked.

## The one-paragraph answer

**Ten packages landed and the suite went from 32× Rust / 0.8× CPython 3.14 to
12× / 0.3×** — 2.66× on the Praxis column across the eight, `vm` alone 4.50×,
faster than CPython on all eight where it was slower on two. **Three packages
are nearly all of it**: block-local box/unbox forwarding at **1.850×**, the
contiguous native root store at **1.386×**, and the inline bitmap claim at
**1.151×**, each measured by `ab.py` against this tree with its own toggle
reverted. That is the result. **The lesson is the other thing that happened: a
published number in this round was corrected five separate times, and every one
of the five was caught by a later package re-measuring, not by anybody reading.**
Two of the five were wrong when written — a hand-rolled per-iteration walker with
no set of cold blocks, and a plan whose stated measurement signal could not fire
— and three were correct when written and stale by the time the wave merged.
The counterweight is that the wave structure caught the worst of them *as a
failing test at a merge* rather than as a wrong number in a report, which is what
it was built for. `just ci` is green: 2073 tests, 31 suites, 0 failures.

**Do not read a geometric mean of 1.00 in this round as "no effect."** W6 and W7
each said in their own ADR that the clock could not resolve them, and the clock
resolved both — in opposite directions, reproduced across two passes. Their means
are cancellation, not neutrality. §2.

---

## 1. What landed, what it was worth, and what was not built

Ten packages across six waves, plus wave 0's shared tooling and handover 25's two
leftovers (ASan as a nightly job outside `ci`; `opt_level` closed explicitly at
`"none"`).

Every row is an `ab.py` paired A/B of **this tree against this tree with that
package's own toggle reverted** — never the previous commit, which ADR-113
records giving 14.4% where the truth was 0.8%. Palindromic A,B,B,A with the
leading arm alternating, the median of the per-pair ratios, and a resolution bar
of `max(2%, 1.4826 × MAD)` of those same ratios. `sizes.json` is frozen at one
sha256 across all sixteen sweeps in `benchmarks/`, checked.

| package | ADR | suite geomean | rows resolved | where it lands |
|---|---|---:|---:|---|
| **W8-S0** box/unbox forwarding | 120 pt 1 | **1.850×** | **8 of 8** | everywhere: `mandelbrot` 3.29×, `primes` 3.17×, `tree` 2.20× |
| **W1** native root store | 114 | **1.386×** | 3 of 6 | `vm` 3.43×, `bfs` 1.54×, `hashwork` 1.36× |
| **W10** inline bitmap claim | 119 | **1.151×** | **8 of 8** | `pipeline` 1.25×, `collatz` 1.22×, `mandelbrot` 1.21× |
| **W4b** + the W4 orphan | 118 pt 2 | 1.036× | 3 of 6 | `bfs` 1.125×, `tree` 1.071×, `vm` 1.029× |
| **W7** fold `CheckFault` | 117 | 1.005× | 2 of 7 | `primes` +3.1%, `collatz` **−2.9%** |
| **W4a** `ReprCVec` | 118 pt 1 | 1.000× | 0 of 6 | nowhere, by design |
| **W6** descriptor table | 116 | 0.999× | 2 of 6 | `primes` +4.8%, `collatz` **−6.7%** |
| **W11-safety** provable descriptors | 122 | 0.998× | 0 of 8 | nowhere: the two arms emit byte-identical code |
| **W2** `Text` in O(1) | 115 | 0.994× | 0 of 8 | nowhere the suite can see. §7 |
| **W8-S0b** scalar debug slot | 120 pt 2 | **0.976×** | 5 of 8 | it costs 2.4%. §6 |

W4b and W8-S0b were re-run: 1.072× and 0.968× on their second passes. **Neither
second pass is a suite figure** — W4b's ran five benchmarks (two of them its
controls) and W8-S0b's ran five, so their means are over subsets weighted toward
the rows that moved. The same is true of W6's 0.987× and W7's 1.001×, which
covered four and three benchmarks. Quote the eight-benchmark column.

**Five things were deliberately not built, each for a stated reason.** W5 was
deleted by handover 26 §1 — it and W8-S0's first stage are one transform, and
building both leaves two mechanisms for one shape. W9 (tagged pointers) is
declined; §9. W12 is deferred — it needs a fourth CLI mode nobody will type plus
an edit to normative §9.6 prose, to buy 3.4%. W8-S1 is behind this gate. And
W11's backend elision is deferred, on scheduling in handover 27 and now on the
census as well; §9.

---

## 2. A geometric mean of 1.00 is two resolved effects cancelling, not neutrality

**This is the single most misreadable pair of numbers in the round.** ADR-116
and ADR-117 each closed their Measurements section by saying the clock would not
be able to resolve them — eighteen instructions out of 215, and three
instructions per fallible operation on perfectly predicted branches. Both were
right about the magnitude and wrong about the outcome. Both resolved, **and in
both directions**:

| | `primes` | `collatz` | other non-control rows | suite geomean |
|---|---:|---:|---|---:|
| **W6** pass 1 | **+4.8%** | **−6.7%** | 4, none resolved | 0.999× |
| **W6** pass 2 | **+5.1%** | **−7.1%** | 1, unresolved | (4-benchmark subset) |
| **W7** pass 1 | **+3.1%** | **−2.9%** | 5, none resolved | 1.005× |
| **W7** pass 2 | **+3.3%** | **−3.0%** | — | (3-benchmark subset) |

Exactly two rows resolved in each package, they are the same two rows in both
packages, they have opposite signs, and both reproduce across independent passes
to within 0.4 points. **A reader who takes 0.999× as "W6 did nothing" has been
misled by an average.** W6 does something to `collatz` worth −6.7%, and something
to `primes` worth +4.8%, and the suite mean is those two cancelling against six
rows the clock could not read.

Neither ADR can explain the sign split, and neither should be quoted as if it
could. What ADR-116 predicted is exactly the shape of the risk: it trades three
independent ALU operations (`movz`/`movk`/`movk`) for one L1 load-use dependency,
and the M2 Pro's answer to that trade evidently depends on what else is in the
loop. `collatz`'s loop and `primes`' loop are the two the sample program is
supposed to be representative of, and they disagree.

**The consequence for how this repo measures.** "The instruction count is the
honest headline and the clock cannot resolve this" was the right *protocol* — it
kept two packages from claiming a percent-and-a-half they had not earned — and it
was wrong as a *prediction* twice out of two. The rule that survives is narrower:
report the count, run the sweep anyway, and report per-row resolution rather than
a mean. Both packages' ADRs need this sentence added and neither has it.

---

## 3. Five published numbers were corrected, and a later package caught every one

Nobody found any of these by reading. Each was found by the next package
measuring the same thing.

**1. ADR-102's stated invariant was false, and W7 is what falsified it.**
ADR-102 carried a Consequences bullet and a rustdoc section both titled
*"# ADR-088 is untouched"*, whose closing sentence was "both arms of the diamond
converge at `cont`". W7's whole mechanism is a raise block that does not
converge. Handover 25 §6 said W7 owed no ADR; its scoping agent disagreed and was
right. ADR-117 decision 1 replaces the sentence with one that covers both shapes
— *on the raising path, control reaches the function's fault epilogue before any
instruction after the raise executes* — and rewrites both copies rather than
editing the code comment quietly. Rewriting a previous ADR's invariant in a
comment with no record is the thing the decision log exists to prevent.

**2. ADR-116's and ADR-117's per-iteration figures were stale before the wave
merged.** Both counted the sample loop's descriptor proofs, both got **nine**
(correcting handover 25's stated seven), and both pinned nine as a test. W8-S0,
beside them in the same wave, deletes four of the nine. ADR-116's headline
arithmetic — "nine sites × two instructions = eighteen fewer per iteration,
exactly" — was exact against a tree that no longer exists. W8-S0b re-measured it
on the merged tree: **125 → 115, −10, five sites × two, exactly, again.** Both
records carry amendments and both tests now assert five while printing all three
answers and why they differ.

**3. ADR-122's census moved by 41 points, and the two columns anti-correlate.**
Measured before W8-S0: **419 `ExtractScalar` sites, 230 literal (54.9%), 340
`MoveGc`-chased (81.1%)**. After: **219 sites, 30 literal (13.7%), 140 chased
(63.9%)** — and after W4b, 218 / 30 / 140. The denominator nearly halving and
the literal column collapsing are *one* fact: W8-S0's producer set is
`Materialize`/`Alloc`/`ConstGc`, which is exactly what the literal column counts,
so every site W11 could prove is a site W8-S0 would rather delete. The chased
column barely moves in the inner loops (96.4% → 93.1%) because it was never
counting the sites W8-S0 takes. Handover 27's own post-W8-S0 estimate (12/39 and
37/39) was wrong in both numbers, and both errors understated the effect.

**4. Two hand-written walkers of one documented rule, wrong in two different
ways, and the second is the one that reached a record.** `dump.rs` has written
the per-iteration rule down since wave 0: the one multi-block strongly connected
component, and at each branch the successor that is inside it *and is not cold*.
The first walker read each IR's per-block counts correctly and then walked the
**CLIF** graph for both — worth 15 machine instructions per iteration (130
against the true 115) and in the same direction in both arms of an A/B, so it
survives subtraction and shows up only in a headline. ADR-118 part 2 caught it
and deliberately did not fix it. The second built each graph correctly and had
**no set of cold vcode blocks at all**: the vcode listing marks nothing `cold`,
coldness survives into it only as emission order, so wherever Cranelift emits the
out-of-line wrapper before the inline arm that walk stepped *into* the wrapper
and counted it. It is right on any path where no branch names a cold block first,
which is three of five rows of ADR-120 part 2's table and not the other two.
It produced ADR-117's amended −28. Both records are corrected in their own
voice; **the fold is −18 on W7's own tree and −17 on the merged one, unchanged
within one instruction**, and what actually grew is its *share*, 8.4% → 12.9%,
because W8-S0 shrank the loop around it. The walk is now `benchmarks/periter.py`,
in the tree, with a self-test, and it reproduces wave 0's recorded baseline
exactly — 311 CLIF in 55 blocks and 171 over 35, 458 vcode and 215 over 38 — a
pair that predates every walker in the round and whose two denominators differ.
**The lesson is not "register the defect", which is what ADR-118 part 2 did. It
is "put the walk in the tree", which is what wave 0 did for the dump hook and
what should have been done for the walk one level up.**

**5. "The clock cannot resolve this" was a wrong prediction twice.** §2.

---

## 4. Three guardrails rested on a signal that could not fire

Handover 26 names `crates/praxis-cli/tests/run.rs:752` four times and hangs three
rules on it: *W8-S0 lands it red on purpose, that is its measurement signal, do
not edit the test, do not merge without S0b, do not run alongside W12.*

**It does not go red.** Handover 27 §1 traced the five-link chain and ADR-120
decision 6 confirmed every link at the keyboard: the debug store rides
`praxis_mir::defs`, a deleted producer defines nothing, `render.rs:178` keeps an
uninit temp that has a span, `build_function_debug_meta` emits a
`DebugLocalMeta` for every `Gc` local defined or not, and every assertion in that
test checks a **provenance string** and never a value. The temp silently degrades
from `= 30` to `= <uninit>` and the suite stays green.

Two things came out of that which are worth more than the correction.

**W8-S0 had to *add* the assertion before its own pass could land.**
`a_forwarded_binop_temp_still_renders_the_value_it_materialized` asserts a value,
not a string, and it exists so that W8-S0b had something to turn green. Without
it the fidelity regression ships silently if wave 4 slips.

**The regression is three temps per fixture, not one.** Handover 27 predicted the
interior `a + b` node. Measured: `a + b`, `a + b + c`, and the out-of-range `Int`
literal — whose `Alloc{Int}` producer is in the forwarding set — all degrade.
The three that do not are small-int literals, whose box has a second reader
because it is also `MoveGc`'d into the binding it initializes. And handover 27 §9
asked whether a test outside `run.rs` asserts a temp's value; there is one, in
`crates/praxis-codegen-cranelift/tests/jit.rs`, and its own comment calls it "the
loss class a reconstruction-from-the-shadow-stack design cannot recover" — which
is precisely the guarantee W8-S0 narrowed.

**One small thing about handover 27's own account of this.** It says handover 26
rests on the signal in "§4, §7 item 4, §3's wave-4 note and §8's merge rule".
There is no merge rule in handover 26 §8; the "never merge it to main without
W8-S0b" sentence is in §4, beside the first one. The substance is unaffected.

---

## 5. The wave structure caught a cross-package interaction as a failing test

This is the argument for the whole way the round was run, and it is the one thing
here that cannot be inferred from the numbers.

W6 and W7 were built in the same wave, in separate worktrees, over one file. Each
independently counted **nine descriptor proofs per iteration** of handover 25
§3's loop, correcting handover 25's stated seven, and each pinned nine as a test
— `the_sample_loop_proves_nine_descriptors_per_iteration_not_seven` in `lower.rs`
and `…_nine_times_per_iteration` in `mir_shape.rs`. Both were right. Neither
could see that the third package in the same wave, W8-S0, deletes the
`ExtractScalar`s those nine are counted from.

**Merging the wave turned that into two failing tests rather than two wrong
numbers in a report.** That is handover 21 §3.6's recorded mistake — a percentage
from an earlier section acted on after it had expired — being caught by
construction. It is also, exactly, handover 26 §7 trap 7 arriving in the one form
that cannot be ignored.

The same thing happened a second time, in the other direction. W4b's orphan
commit (`Inst::BitsetContains`) measured itself **five CLIF instructions worse**
at the site — the box moved out of the wrapper and into `emit_inline_bool` at the
call site — and said in its own message that it pays when W8-S0 forwards the
resulting `Materialize`/`ExtractScalar` pair away. On the merged tree the block
holding `b.contains(k)` has a whole census of `{BitsetContains: 1}`. The
assertions flip to `== 0` rather than being deleted, because the zero is the
claim. **A package's cost was paid by a package it was measured against, twice in
one round, and both times the prediction was written down first.**

---

## 6. Git could not see two of the merge conflicts, and both will recur

Neither is a merge conflict in git's sense — no branch touched a line another
branch touched — and both are structural costs of building ten packages in
parallel worktrees.

**A struct field on one branch, that struct built by literal on another.**
W8-S0b added `debug_scalar_sources` to `ir::Function`; W11-safety builds a
`Function` by literal in its test module. Neither branch shared a file with the
other. The merge did not compile. The general shape: *any* package that adds a
field to a struct another package constructs positionally or exhaustively is a
silent conflict, and the population of such structs is not something a conflict
matrix built from file lists can see.

**Three `[features]` collisions, because every package added a measurement
toggle.** `praxis-codegen-cranelift`'s manifest got two `[features]` headings at
the W6/W7 merge, which cargo rejects as a duplicate key before a line of Rust is
compiled; `praxis-mir`'s got a third at the W11 merge; `praxis-runtime`'s
conflicted at W2. This is a direct consequence of the round's best measurement
rule — *the baseline is this tree with this package's own toggle reverted* — and
it will recur for exactly as long as that rule holds. The fix is the one wave 0
used for ADR numbers and ABI paragraphs: **pre-stub an owned line per package
under one `[features]` heading**, so writing a toggle is a one-line edit at a
line nobody else touches.

---

## 7. Four results that are worth more than their percentage

**W4b is the package the clock vindicated and the counts could not.** Every
static count for it went *up*: `bs.contains(k)` +16 vcode per iteration, `v[0]`
+8, `v.len()` +11, and whole functions 26 to 34 instructions bigger. Its ADR says
plainly why — an inline sequence is bigger than the `bl` it replaces because the
wrapper's body was never in the count, and *a count of the caller cannot price an
inlining*. The one row that is a result is the last column: one call per
iteration before, zero after. The clock then returned `bfs` **+12.5% / +11.6%**
across two passes and `tree` +7.1% / +7.3%.

It also produced the round's sharpest general statement, now an amendment to
ADR-122: **inlining a call in the backend buys the descriptor analysis nothing.**
W4b was filed as the counterweight to W8-S0 — a package that moves descriptors
out of `Inst::Call`, where the analysis is blind, into MIR's own emissions, where
it is not. The census moved by **one site**. Only `bs.contains(x)` became an
`Inst`; `praxis_vec_get` and `praxis_vec_len` inline in the *backend* and keep
their `Inst::Call`, so MIR cannot see them at all. What buys the analysis
something is giving the primitive its own `Inst`, which costs a variant, a
verifier arm, a liveness arm and a backend arm apiece.

**A control that is a target is not a control.** W10's first sweep named
`collatz` and `primes` as controls, on handover 26 §6's guidance that those are
the controls "for the allocator packages". W10 *is* the allocator package, and
its own entry-gate profile puts `collatz` at 24.5% and `primes` at 10.0% scalar
allocation. They moved, `ab.py` voided the sweep, and it was right to. Every
benchmark in the suite is at least 8.0% scalar allocation, so this package has no
control to declare, and its ADR says so rather than inventing one. The voided
run's numbers agree with the valid one to within 0.7 points on every row it
reached, which is two independent passes by another name.

**`run.py` never builds the binary it measures.** It builds the eight Rust
comparison binaries every sweep (`build_rust`), checks that
`target/release/praxis` exists, and dies if it does not — so the first
regeneration of `REPORT.md` measured a `praxis` from before W10 merged and
reported the nine-package numbers. It is caught only by noticing a row that
should have moved and did not. `run.py` already refuses to run with
`PRAXIS_GC_PACER` set, for the same class of reason ("a measurement that cannot
say which build it came from must not be recorded"); the missing guard is the
same guard.

**W2's win is invisible to the suite and always was.** No `.px` in `benchmarks/`
iterates a `Text`, seven of the eight contain no double quote at all, and all
eight read a single integer. The suite came back 0.994× with **8 of 8 rows
unresolved**, which is the prediction rather than a disappointment: a suite that
does not contain a shape cannot price a change to it. Measured on the shape the
language exists for, arm A's log-log exponent is **1.92–1.96** where arm B's is
**0.98**, on three separate text shapes, and the ratio grows with the input the
way a complexity fix does rather than the way a speedup does:

| | 16 kB | 32 kB | 64 kB | 128 kB | 256 kB |
|---|---:|---:|---:|---:|---:|
| `for c in t` over a slice | 5.5× | 15.0× | 44.5× | 125.4× | **309.1×** |
| `t[i]`, n subscripts | 3.5× | 8.9× | 22.5× | 55.2× | **126.3×** |

And the residual ADR-115 *declined* to fix is measured too, which is worth as
much as the wins: the licence lives on the owner, so **one two-byte scalar
anywhere in a 128 kB input costs 3.9 s where the same bytes without it cost
12 ms** — a factor of 330 for a single `é`, with arm A and arm B within 1% of
each other because neither can help.

---

## 8. The debugger costs 2.4% of the suite, resolved, and that is the price of §9

W8-S0b is the only package in the round that is a **cost**, and it was scheduled
expecting the clock would not see it. It saw it: **0.976× on the eight, 5 of 8
rows resolved** (`primes` −7.3% reproduced to a tenth of a point across two
passes; the three unresolved rows are the three smallest). It costs +4 CLIF and
+8 machine instructions per iteration of the sample loop and **nothing per call**
— the elided box's slot was already claimed and already zeroed.

Half of the eight is one `while` condition's `Bool` slot, because storing a
comparison forces the flag into a register and the branch then recomputes it.
That is the honest surprise: the largest single piece of the cost is the temp a
user is least likely to ask about, and it is not carved out, because a carve-out
would narrow a §9 guarantee *by kind* with no principle behind the choice.

**Record the cost rather than netting it away.** W8-S0 gave 1.850× and W8-S0b
took 2.4% of it back, and reporting the pair as "W8 was worth 1.81×" would hide
a decision inside an arithmetic. What was bought is that
`<tmp#7: Int> @ "a + b" = 30` still renders after the box that produced it is
gone, and that the collector provably cannot dereference an `f64` bit pattern as
a `GcHeader` — structurally, because `DebugValue::Scalar` holds no `GcRef` and
the post-sweep scan reaches a header only through `DebugValue::reference`.

This also sharpens W12 without settling it: it is the first item in the crash
debugger's ledger whose cost is on a *hot loop* rather than in a prologue.

---

## 9. The backlog, re-ranked against this tree

**Handover 26 §7 trap 7 applies to this section and to nothing else in the
document.** Every percentage in handover 25 was measured at `e4f42e6`, before ten
packages moved the denominator, and none of it is carried below.

**Before anything on this list: profile the tree.** `benchmarks/profile-wave5.json`
is the only profile the round produced, and it was taken at `8f368f1` — **before
W10 merged**. Its netted scalar-allocation shares (`mandelbrot` 32.5%, `vm`
28.8%, `pipeline` 27.2%, `collatz` 24.5%, `tree` 18.7%, `primes` 10.0%, `bfs`
8.0%) are the shares W10 was gated on, not the shares that remain. W10 removed
the *call*, not the allocation, and by 1.151× — so every row in that table is
now smaller by an unmeasured amount. Ranking anything on it is the mistake this
paragraph exists to prevent.

1. **W8-S1 — `Gc`→`Scalar` demotion for loop-carried locals.** Still first, and
   more expensive than it looks. Its target is now exactly quantified by
   ADR-120's census: `mandelbrot`'s two remaining inner-loop float boxes are `x`
   and `y`, and `vm`'s five remaining `Materialize{Int}` are call arguments and
   register assignments. Both are `MoveGc` into an existing slot, which W8-S0
   provably cannot touch — and that is the whole difficulty. W8-S0 was cheap
   *because* it was block-local; a loop-carried slot is precisely where ADR-120
   decision 1 says substitution needs dominance and reaching definitions, which
   are among the four analyses ADR-108 declined to build. Budget it as a new
   analysis, not as W8-S0 with a wider gate. **It interacts multiplicatively with
   W10 and will absorb credit from it** — W10 made each box cheaper, W8-S1
   removes the box — so whichever is measured second reports less, and the ADR
   must say which it is.
2. **`opt_level = "speed"` — one sweep, and re-opening it needs the reason
   stated, which is here.** Handover 25 §3 closed it and `CRANELIFT_FLAGS`'s doc
   says "this is the last measurement the flag gets", on the grounds that what
   the lowering emits is not redundant *to Cranelift*: the register allocator
   rematerializes descriptor addresses on purpose, the loads are through memory
   it cannot prove non-aliasing, and the proofs compare against addresses it
   cannot fold. **The first of those three no longer describes the loop.** W6
   deleted 27 `movz`/`movk` from the sample loop's 215 and there are no
   descriptor immediates left at a proof site; W8-S0 deleted the box/unbox
   pairs; W4b and W10 deleted the last opaque calls, and W10 replaced one with
   46 instructions of straight-line address arithmetic with folded immediates —
   which is exactly the material an egraph mid-end works on and which, at
   `"none"`, never runs. ADR-113 already recorded the first non-null result the
   flag ever produced (`collatz` −6.3%) immediately after removing *one* opaque
   call from that loop. This is one `ab.py` sweep and zero lines of code, and it
   is the cheapest thing on the list per unit of information.
3. **W12 / two code variants selected at `Jit::new`.** Moved up, because a piece
   of its ledger is now measured rather than estimated: W8-S0b alone is **2.4% of
   the suite, resolved on five of eight rows**, against handover 25's 3.4%
   estimate for the whole debug view at a denominator that has since moved. It is
   still the only item on this list that is a *decision* rather than an
   engineering task — a fourth CLI mode and an edit to normative §9.6 prose — and
   if it is revived, ADR-120's open question stands: measure `FramesOnly` before
   `None`, and try the cheaper `Bool` slot (one `cset` consumed by both the store
   and the branch) first, since that is half the cost on the sample loop.
4. **`praxis_vec_get`, `praxis_vec_len` and `praxis_text_len` as their own
   `Inst`s.** ADR-118 decision 10 says this is what would make their results
   provable *and* what would drop their ~17 root-spill instructions, since
   `liveness::is_gc_safepoint` is a shape match that excludes `ValueCmp`. The
   obstacle is an argument, not effort: `v[i]` answers a `GcRef`, and a `Gc` dst
   is a rooting question rather than a shape question, so `ValueCmp`'s reasoning
   does not transfer and the package has to make a new one. `praxis_text_len` is
   the same row for the same reason and ADR-115's open questions asked W4b to
   take it; it did not.
5. **The `lower_for` header hoist.** `lower_for` puts the plan's `len` call in
   the loop *header*, so `for c in t` re-evaluates it once per iteration — one
   runtime call and one `Int` box per step, on every in-place plan, in every
   program that iterates anything. `b.loop_preheaders` is on the stack at exactly
   that point, which is what ADR-108 built it for. For `Text` it is sound by
   immutability; for the other seven in-place plans it is not available without
   deciding what `for x in v { v.push(…) }` means, which ADR-066 left to the
   snapshot rule and did not answer.
6. **Should the claim scan more than one bitmap word?** ADR-119 bails when the
   cursor word is full, which on a page filling front-to-back is one wrapper call
   per 64 blocks; and it cedes the tail word, which is 60 blocks of 1340 — 4.5%
   of claims on a full page. Both are one load, an `orr` and a select, or four
   instructions and one more exported displacement. **Nobody has counted the
   frequency, and `periter.py` structurally cannot**: it is a run-time
   distribution, not a shape.
7. **`GridPayload`'s migration.** Mechanical — row-major, contiguous, exactly
   `vec_get`'s shape — but part 2 added the reason it is not free:
   `INLINE_VEC_SITE`'s `pub(crate)` constructor exists so that a `Grid` cannot be
   walked as a `Vec`, so a `Grid` arm is a third site and a third descriptor
   proof, not a reuse. It wants a reader first.
8. **W11's backend elision — declined again, and now on the merits.** Handover 27
   deferred it on scheduling. The census now argues against it independently:
   after W8-S0 there are **less than half as many proofs left to elide** (219 →
   218 sites against 419), the literally-provable column has collapsed to 13.8%,
   and what survives is the `MoveGc`-fed remainder this analysis is weakest at.
   Its residual per site fell from 6 machine instructions to 4 when W6 landed,
   and the W6/W11 overlap is not symmetric — at any site W11 elides, W6
   contributes exactly zero. The safety half already shipped and is the part
   worth having.
9. **W9 (tagged pointers) — declined, and the current profile strengthens the
   refusal rather than weakening it.** Handover 26 declined it pending a
   post-W8-S1 measurement. The intervening round has made the thing W9 removes
   *cheaper*: an in-range `Int` box was already a table read (ADR-113), an
   out-of-range one is now a 46-instruction inline claim with no call (ADR-119),
   and W8-S0 deleted a large fraction of the boxes outright. Low-bit tagging
   still narrows `Int` below §4.3's normative signed-64-bit payload; NaN-boxing
   still makes two NaNs one word and breaks `DynamicKey`'s pointer fast path.
   Either still rewrites 190 `payload::<T>()` sites, 133 `descriptor()` sites and
   both `heap_id()` sites, to buy the root spills **that W8-S1 also removes**.
   Re-measure after W8-S1, as handover 26 said, and expect the answer to be the
   same one.

**On budging for performance, one round later.** Handover 25 §6 said the top
eight items needed no language change and none of them took one — §4.3 and §18.2
are untouched, no design-document sentence moved, and the suite went 2.66×. The
two places where budging would still buy something are unchanged: W11's backend
half (a defence against compiler bugs, in release code) and W12 (§9's
unconditional debugger). Both are now priced against measurements rather than
estimates, and both come out smaller than they looked.

---

## 10. What is still unverified

This is the section a reader should trust the document for, and it is longer than
the last three because ten packages generated it.

**Every timing in this round is incomparable with handover 25's, and the reason
is mechanical.** All sixteen sweeps ran with `ab.py --max-load 6` and the load
gate explicitly waived; §6's 0.5 ceiling is unreachable on this machine, because
the editor's own UI holds it at 2–3 indefinitely with nothing building. Observed
1-minute load across the round ran **1.84 to 4.63** — not 2 to 4 — and the widest
was W6's second pass. `--max-load` refuses to waive the competing-build half and
that half was never waived; every result is stamped with both ends. A stationary
load is charged to both arms by the palindrome and widens the MAD bar with what
it cannot absorb, so a *resolved* delta is real and an *unresolved* one may be
only this machine. **Handover 25's numbers were taken at a different ceiling and
must not be subtracted from these.**

**There is no profile of this tree.** §9. The only one is pre-W10.

**The 2.66× is two `run.py` sweeps compared across a round, not a paired A/B.**
The Rust and Python columns of those same two sweeps drifted by up to ~6% and
~15% against each other on the median statistic, which is why per-package credit
is assigned by `ab.py` in the ADRs and never by subtracting one `REPORT.md` table
from the last.

**`periter.py` reports a cycle that never executes when the arm under test *is*
the cold path.** Its rule excludes cold blocks, correctly; ADR-119 hit this
measuring the out-of-range `Int` site, where arm A's out-of-range edge branches
to a cold block, so the reported 101 is the in-range cycle the program never
takes. That comparison had to be made block-for-block instead. A per-iteration
number for such an arm needs a walker that follows a cold edge, which is a
different rule.

**ASan does not instrument JIT-generated code**, and W4b, W10 and W8-S0b all put
new behaviour exactly there. The final run is 2071 passed / 0 failed / 0 reports
across 32 verified-instrumented executables, and it says nothing about the loads
W4b emits against a raw buffer pointer, the `GcHeader` stores W10 emits into a
page, or the `f64` bit pattern W8-S0b stores into a debug slot. Each of the three
substitutes a written structural argument, and each says so. **Nothing in the
round has an answer to "how much of the emitted half can be sanitized at all."**

**The counts nobody has converted into executions.** ADR-117's fold census is 118
of 222 sites (53%), `vm` 5 of 58 — *sites*, not executions, and a program's hot
loop is a handful of its blocks. ADR-122's 340 provable sites is the same kind of
number. Neither can be turned into a percentage of time without instrumentation
that does not exist.

**Per-package, the questions each record left open and nobody has answered:**

- **ADR-114** — `push_roots` now copies the whole store per collection, which for
  a 200,000-line parse is a 1.6 MB memcpy each time; whether `RootSet` should
  hand out a slice is an ADR-012 seam change. Whether the graph builtins should
  root per step rather than per search (the store's peak is the closure, not the
  frontier). Whether the `RefCell` still earns its keep — the argument that a
  collection cannot occur inside its `borrow_mut` is written, not enforced.
- **ADR-115** — whether a `Slice` of a multi-byte owner should be copied rather
  than tolerated, which needs a measurement of how much non-ASCII input this
  language actually sees. Whether `text_str`'s `from_utf8` is worth removing from
  its remaining callers.
- **ADR-116** — whether the context pointer stays in a register in a function
  with more live values than the sample loop; if it spills, each proof becomes
  two loads and the package's win goes to zero there. **The sample loop remains
  the only program in this repository whose emitted code anyone has counted.**
- **ADR-117** — whether `Terminator::Fault`'s block should be cold when nothing
  hot jumps to it, which is now the common case.
- **ADR-118** — whether `absent`-inline is right for a mostly-full `BitSet`; the
  arm was chosen for `bfs`, where the visited set is short early, and there is no
  measurement that distinguishes the two.
- **ADR-119** — whether `Heap::live_count` and `PageHeader::live_count` should be
  derived from the bitmaps at sweep time rather than maintained in two places
  (the runtime and generated code) and decremented in one.
- **ADR-120** — whether `wedged` is ever non-empty; it cannot be today, and its
  emptiness is deliberately *not* asserted, because asserting it would turn the
  safety net back into the exhaustive match it exists to avoid.
- **ADR-122** — whether the parameter case is recoverable interprocedurally.
  `is_prime(n)` is called from one site with an `Int`, and a summary-based
  version would unlock two sites per call — and would need re-running after
  monomorphization, a recursion story, and an answer for closure parameters.

**One claim in the round's own plumbing that is not what it sounds like.**
`Char`'s inline claim is described as needing only fault routing.
`InlineClaimSite::of(&CHAR)` would indeed answer `Some` — `Char` carries no
`owned_bytes` charge and its block is on the ladder, which
`only_a_descriptor_with_no_owned_bytes_charge_has_a_claim_site` asserts over
every builtin — but `scalars.rs` mints only `INT_CLAIM_SITE` and
`FLOAT_CLAIM_SITE`. **There is no `CHAR_CLAIM_SITE`**, so the work is a site
constant *and* an inline arm that reproduces `InvalidChar`'s raise with the same
message and the same `CheckFault` diversion, not fault routing alone. Its profile
share is 0.0% on every benchmark, and handover 23's P-4a may move the validation
into `small_char`'s bounds and change the arm's shape anyway.
