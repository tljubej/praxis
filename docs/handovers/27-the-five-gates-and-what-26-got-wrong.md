# The five gates handover 26 left open, and the signal that does not fire

**Date:** 2026-08-03
**Tree:** `1535eb6` (docs-only above `e4f42e6`; no source file differs).
**Predecessor:** [`26-ten-packages-six-waves-and-the-five-things-25-got-wrong.md`](./26-ten-packages-six-waves-and-the-five-things-25-got-wrong.md)
is the execution plan. This resolves the questions it registered as blocking —
W8-S0's mechanism, W10's price, W11's census — and corrects it where reading the
code contradicted it.

**How this was produced.** Three read-only investigations against this tree, one
per gate, and a fourth agent told to settle their disagreements by reading rather
than averaging. Nothing here was built, run or timed; §6 says what that costs.

## The one-paragraph answer

**W8-S0 stays inside `praxis-mir`, so wave 2 is three agents wide** — the
mechanism is neither of the two handover 26 posed, and the premise behind the
dilemma is false: `build.rs:3732` is a `fn move_scalar` whose own doc says "there
is no scalar-move `Inst`, so the idiom is `dst = src + 0`". **The plan's most
load-bearing claim is wrong in the dangerous direction**: handover 26 says four
times that W8-S0 lands `crates/praxis-cli/tests/run.rs:752` red on purpose and
calls that its measurement signal. It does not go red. The debugger silently
degrades a temp from `= 30` to `= <uninit>` and every test stays green, so all
three of the plan's guardrails around that package were built on a signal that
will not fire. **Every `lower.rs` line number in handover 26 is ~35 lines low**;
locate by symbol, never by line. W10 is re-scoped *up* in reach and made
conditional on a measured gate; W11 splits into a safety half worth building now
and a backend half that has no honest slot before wave 5; and the orphaned W4
follow-up closes inside W4b on a precedent already in the tree.

---

## 1. The signal that does not fire

Handover 26 §4, §7 item 4, §3's wave-4 note and §8's merge rule all rest on this:

> **Stage 0 lands `crates/praxis-cli/tests/run.rs:752` RED on purpose.** That is
> its measurement signal, not a failure.

It stays green. The chain, each link read:

| step | file | what it does |
|---|---|---|
| the fixture is `a + b + c + 9223372036854775807` | `crates/praxis-cli/tests/fixtures/run/debug_temps.px` | so `a + b`'s `Materialize` is an **interior** node whose only consumer is the next node's `ExtractScalar` in the same block — exactly the shape W8-S0 deletes |
| the debug store is driven by `praxis_mir::defs` | `lower.rs` `store_debug_defs` | a deleted producer defines nothing, so the slot is never written |
| an uninit temp **with a span** is kept, not skipped | `render.rs:178` — `} else if local.value.is_some() \|\| local.span().is_some() {` | rationale at `render.rs:164-172` |
| the span survives the deletion | `lower.rs` `build_function_debug_meta` | it walks `mir.locals` and emits a `DebugLocalMeta` for every `Gc` local, defined or not |
| the assertions are provenance strings only | `run.rs:772-783`, `794`, `798` | `out.contains("@ \"a + b\"")` — never a value |

Actual outcome: `<tmp#N: Int> @ "a + b" = <uninit>` where `= 30` used to render,
and nothing goes red.

**Consequence, and it is a wave-2 obligation.** W8-S0 must *add* the assertion
before the pass lands — extend `run.rs` around 777 with
`assert!(out.contains("@ \"a + b\" = 30"))`, or add a sibling test. Otherwise
W8-S0b has nothing to turn green and the fidelity regression ships silently if
wave 4 slips.

---

## 2. Line-number drift, and the rule that follows

`lower.rs` is unchanged since `e4f42e6`, but handover 26's citations are
uniformly low. Verified against `1535eb6`:

| symbol | handover 26 says | actually |
|---|---:|---:|
| `emit_inline_intern` | 2114 | **2149** |
| `inline_scalar_load_of` | 2274 | **2309** |
| `emit_scalar_load` | 2355 | **2390** |
| the descriptor `iconst` | 2378 | **2413** |
| the sole `CallTarget::Runtime` arm | 1542 | **1577** |
| `lower_inst` | 891 | **912** |
| `payload_load` (test helper) | 3316 | **3360** |
| `a_bool_extract_reads_one_byte_and_a_char_four` | 3391 | **3436** |
| `spill_roots` | ~754 | **789** |
| `store_debug_defs` | ~816 | **851** |
| `is_gc_safepoint` | "L377" | `liveness.rs:382` |

`praxis-mir` citations are accurate to within ~6 lines.

**The §5 conflict-matrix disjointness verdicts still hold** — the shift is
uniform, so no pair of regions that was disjoint has become adjacent. But an
agent handed a region by line number lands in the wrong place. **Locate by symbol
name.**

---

## 3. Gate 1 — W8-S0's mechanism. Wave 2 stays three wide.

Handover 26 §3 made this the question that decides wave 2's width, on the premise
that MIR has no scalar-to-scalar move. **The premise is false**
(`build.rs:3732`), and neither horn of the dilemma is the right answer anyway.

**The mechanism is block-local *use-rewriting*, gated by a whole-function
census.** Rewrite the consuming instruction's operand field from `e` to `s`,
delete the `ExtractScalar`, then delete the producer if it became dead. Nothing
is copied, no local disappears from the table, no new `Inst` variant exists, and
`crates/praxis-codegen-cranelift/` is not edited at all — which is what keeps
W8-S0 beside W6 and W7.

- **Lives in** `crates/praxis-mir/src/forward.rs` (new), `pub mod forward;` in
  `lib.rs`, one call line at the end of `lower_module` (`build.rs`, fn at 37).
  `lower_module` does not call `annotate` — all five hosts do
  `lower_module → annotate → verify` separately — so hooking inside it gives
  "before `annotate`" with zero host edits, which is ADR-108 §1's stated reason
  for refusing a standalone pass. Safe by construction: every builder site writes
  `RootSlots::unannotated()`, and `RootSlots::set` is `pub(crate)` to
  `liveness::annotate` alone, so the pass deletes safepoints whose slot sets hold
  no answer yet. It cannot invalidate an answer, because none exists.
- **Census first**, one linear pass: `def_count`, `use_count`, and each use's
  block, from `liveness::defs` (already `pub`) plus `uses` and `term_uses`, which
  must be promoted to `pub(crate)`. No new exhaustive match over `Inst`.
- **Producers:** `Materialize { scalar: K }`; `Alloc { AllocKind::Int|Bool|Float }`;
  and `ConstGc`, which is a separate *free* case — replace the `ExtractScalar`
  in place with `Inst::ConstInt`, one-for-one, no operand rewrite.
- **Gates, all checked and none assumed.** `!producer.can_fault()` is the right
  gate and does real work: it admits `Int`/`Bool`/`Float` and excludes `Char`,
  whose `AllocChar` row is `AllocatesAndFaults` and whose deletion would orphan
  the `CheckFault` that `verify::check_fault_observed` requires. Same block,
  consumer after producer. Kind equality on both sides — which is also what stops
  the pass turning a latent `Materialize{Bool}`/`ExtractScalar{Int}` pun (which
  `verify::operands` accepts today; it checks range only) into a silent value
  substitution. No redefinition of `b` between, none of `s` after. `e` defined
  once and used only in this block.
- **The terminator rewrite is mandatory, not optional.** `lower_while` emits
  `Materialize{Bool}` → `ExtractScalar{Bool}` → `Terminator::Branch` in one
  block, and the extracted `Bool` is consumed **only** by the terminator. A pass
  that walks `insts` alone forwards nothing in the single most common shape in
  the language, passes every test, and reports a smaller win.
- **The rollback guard is the design point, not a `debug_assert`.** The two
  operand-rewriting helpers are deliberately non-exhaustive (`_ => vec![]`); after
  rewriting, re-run the read-only `uses`/`term_uses` over the whole function and,
  if any use of `e` survives, put the `ExtractScalar` back. That converts a
  non-exhaustive match from a correctness hazard into a missed-optimization
  hazard, and it is what leaves ADR-044's count of exhaustive `Inst` matches at
  five rather than six.
- **Leave the local in the table.** `Function.locals` is indexed by `LocalId` with
  three parallel `Vec`s; removing an entry renumbers every `<tmp#N>` the debugger
  prints, because `build_function_debug_meta` assigns `symbol_id` by position
  among `Gc` locals. Nothing requires density.
- **Hand-off to S0b:** when the producer is deleted, copy `debug_names[b]`,
  `debug_kinds[b]` and `debug_spans[b]` onto `[s]`.

**Why not whole-function `LocalId` substitution.** MIR is deliberately not SSA
(`ir.rs:3`, repeated at `ir.rs:249`), and this is not aspirational: `Assign`
lowers to `MoveGc` **into the binding's existing slot**, both arms of a
`lower_if` write one `dst`, and `emit_increment` redefines the loop cursor. So
substitution is only valid under a dominance-and-reaching-definitions question —
exactly the four analyses ADR-108 declined to build. It is *accidentally* true at
this tree that every `ExtractScalar` dst is freshly allocated on the line above,
across all 20 builder sites; that is a property of today's builder, not an
invariant, and both W11 and W8-S1 propose to change builder shapes.

**Why not a `MoveScalar` variant.** Five arms, one of them `lower_inst` in the
wave-2 bottleneck file — which is what would have moved W8-S0 out of the wave.

### W8-S0 is bigger than handover 26 says, and not where it says

`TypedExpr::Bin` materializes **every** intermediate node, `Int` as well as
`Float`. Handover 26 noticed this only for floats and frames the package as a
`mandelbrot` change throughout. It also forwards away `collatz`'s `3 * c`,
`primes`' `d * d` and `n % d`, `pipeline`'s `x * 3`, `hashwork`'s `state * M`,
and `tree`'s three inner adds. Budget its ADR and its measurement for the suite.

Two costs handover 26 did not price. `liveness.rs` joins W8-S0's file list (wave
2), not just W8-S0b's (wave 4). And `build.rs`'s own MIR-shape tests run through
`lower_module`, so they now observe post-pass MIR — `Materialize` appears 23
times in that file, several inside assertions, and any test asserting one inside
a `while`/`if` condition or a float expression tree will change *legitimately*.
So will `jit.rs` assertions that infer "a collection ran" from a live-object
count.

---

## 4. Gate 2 — W10 is re-scoped up, and made conditional

Handover 26 re-priced W10 downward on the grounds that its whole justification is
`praxis_alloc_float` and that W8-S0 removes 8 of `mandelbrot`'s 10 float boxes.
**The premise is right and the conclusion is wrong, because W10 inlines the
claim, not the float path.**

After ADR-113, generated code inlines **zero actual allocations**: ADR-113
inlined a table *read*. Out-of-range `Int` still takes `emit_inline_intern`'s
cold call; `Float` and `Char` take an unconditional `call_symbol` with no pacing
test and no inline arm at all. The eligible set is provably exactly two arms —
out-of-range `Int` and `Float` — because `occupy` charges
`stride + descriptor.owned_bytes_of(payload)` and no scalar descriptor carries an
`owned_bytes` callback. **Every one of the eight benchmarks allocates
out-of-range `Int` in its hot loop**, so W10's reach is suite-wide.
(`mandelbrot` is confirmed the only float-allocating benchmark; `pipeline`'s only
decimal is in a comment.)

**Schedule: stays in wave 3, after W8-S0.** The two are multiplicative — W8-S0
reduces the *count* of allocations, W10 the *cost* of each — so the end state is
order-independent but the attributed percentage is not, and whichever runs first
absorbs the other's credit. The engineering schedule is free here (W8-S0 is
`praxis-mir`, W10 is `lower.rs` + `heap.rs`/`page.rs`, no shared file), so the
order is chosen purely for measurement honesty and the ADR should say so.

**Wave-3 entry is a gate, not a slot.** Wave 2's mandatory re-baseline decides
it: extend §6's `sample` set from {`mandelbrot`, `bfs`, `vm`} to include
`collatz`, `primes`, `tree` and `pipeline`, read `claim_block` + `alloc_raw` +
`praxis_alloc_int` + `praxis_alloc_float` off it, and **net out composite
allocations** — `pipeline`'s tuples, `tree`'s records, `bfs`'s inner `Vec`s —
which W10 structurally cannot inline. Under 10% on every benchmark and W10 is not
built this round. It is the plan's highest memory-safety risk for 2–3 days and a
350–450 line ADR; it must clear a bar.

**W10 is not blocked on W6.** §3's "both consume W6's descriptor table" is wrong
for W10 — `lower.rs` already bakes descriptor addresses as `iconst`s at the proof
site. W6 is a two-instruction discount per header store, not a prerequisite.

**The `Safepoint` obligation, which is the ADR's first Decision.** The clause W10
falsifies is `heap.rs:56-58`, verbatim: *"the token is permission to collect, not
permission to allocate. The inline path allocates nothing — it hands back an
immortal the runtime minted before main ran."* The replacement is three parts:

1. **Entry** — every store in the claim sequence is dominated, in the emitted
   Cranelift CFG, by the branch on `collection_is_due`. Wave 0's
   `assert_dominates` checks it; this is strictly stronger than ADR-113's "is it
   the entry block's terminator", which cannot survive a claim site that is not
   first in its function — and no real one is.
2. **Duration**, which is the clause that actually replaces "it allocates
   nothing" — between the pacing branch and the last store there is no call and
   no point at which a collection can begin, so *not due on entry* implies *not
   due throughout*. That is what makes the block unsweepable before the reference
   reaches a root slot, and what stops a re-entrant claim being handed the same
   free bit.
3. **State** — the sequence leaves the heap field-for-field as
   `alloc_raw → claim_block → occupy` would, **including both live counters**,
   because both are decremented and never recomputed, so a skipped increment
   underflows them and `relink_pages` puts a page with live blocks on the empty
   pool.

Store order is header → payload → `allocated` bit → counters, **and the ADR must
say plainly that the order is not what makes the sequence safe — part 2 is.** The
order is a severity ranking against a collection part 2 says cannot occur: it
removes the one unrecoverable failure (the sweep dereferencing an unwritten
header as a `*const TypeDescriptor` and making an indirect call through it) and
leaves only bookkeeping ones. Claiming more is what ADR-113 forbids. Pin all
three with IR tests, not comments.

W10 also owes an explicit amendment to ADR-113 decision 2, whose own text says of
its two exported offsets that "this pair is the whole export surface, and its
narrowness is deliberate". W10 adds eight `PageHeader` offsets and three
`GcHeader` ones. That is a permanent cost against a win just re-priced downward
and must be argued, not filed as an ABI paragraph.

### What W10's ADR is allowed to promise

Three numbers, none of them a suite percentage.

1. **Deterministic headline:** the emitted machine-instruction count of the
   inline claim versus the call it replaces, at a named site, via wave 0's dump
   hooks. Produce it *before* writing the Decision.
2. **Wall-clock that does not expire:** handover 25 §3's two 20M-iteration `Int`
   microbenchmarks — 0.32 s interned versus 0.49 s outside the intern range, i.e.
   8.5 ns per real allocation. That loop's box is a loop-carried assignment,
   which W8-S0 provably cannot forward. State what fraction of the 8.5 ns W10
   removes and what fraction is the amortized sweep it does not.
3. **The real acceptance test, already on record:** ADR-113 left a reproducible
   regression owing — `tree` +2.0% and `pipeline` +1.4%, confirmed across two
   independent five-rep passes. Those two pay ADR-113's pacing test in front of a
   call they were making anyway. **W10 is that repair. If it does not retire
   them, it did not land.** This is a claim about a shape, so W8-S0 shrinks its
   magnitude but cannot invalidate it.

**Forbidden:** a suite geometric mean; any percentage carried from handover 25
(measured at `e4f42e6`, before W1's 1.22× moves the denominator); the "63%
allocator" share, which overclaims by the collection share; and any number
carried on `mandelbrot`, the worst benchmark in the suite to price W10 on — W8-S0
takes it from 10 float boxes to 2 and W8-S1 to 0, so a `mandelbrot` number
expires twice. Measure the residual on **`vm`**: every `Int` box in its
interpreter loop is a call argument or a loop-carried assignment, neither of
which is W8-S0's shape.

---

## 5. Gate 3 — W11 splits, and the census as written asks the wrong question

**Build the MIR-pass-and-verifier-rule half in wave 3. Defer the backend half to
the wave-5 gate.**

The deferral is a scheduling fact before it is a judgement: §5 lists W6/W11 and
W11/W8-S0b as hard pairs that never run in parallel, W6 is wave 2 and W8-S0b is
wave 4, so the backend half's only honest slot is wave 5 regardless of the
census. On the merits, after W6 lands W11's residual per elided site is 4 machine
instructions plus a deleted three-block diamond, not the 6 it is worth today.
Handover 26's overlap claim is verified and in the direction it states — the only
descriptor-proof emitter is `emit_scalar_load`, whose sole non-test caller is the
`ExtractScalar` arm, so at any site W11 elides, W6 contributes exactly 0 — but it
reads as symmetric and is not. The double count is exactly 2 instructions
per site.

**The safety half, which is worth having regardless.** A `praxis_mir::provable`
*analysis* (not a transform): for each `Gc` local, a descriptor class over
`Top > {Int, Bool, Char, Float, Unit, Text, …} > Bottom`, with `ConstGc`,
`Alloc`, `Materialize` as producers, `MoveGc` resolving to its source **by
fixpoint**, and every other `Gc`-defining instruction — `Call`, `CallIndirect`,
`LoadField`, `LoadTupleElem`, `EnumPayloadGet`, `LoadCapture` — mapping to
`Bottom`. The rule: for each `ExtractScalar { src, scalar: K }`, if `provable(src)`
is a known class that is not `K`, that is a `VerifyError`. **`Bottom` is not an
error.** The rule refuses a proved contradiction, never an absence of proof —
which is what makes it deployable today with zero false positives and no cost.

**Two traps that must be in the ADR.** A local with *no* defining instruction is
`Bottom` permanently: function parameters and closure-prologue captures have
none, and "every definition is a K-producer" is vacuously true over an empty set,
so a naive universal quantifier blesses every parameter — the one way this ships
silently unsound, at exactly the site (`primes`' `n`) where it is most tempting.
And the fixpoint must be a *greatest* fixpoint, because a loop variable has a
definition on the back edge and a single forward pass reports `Bottom` for every
loop counter.

**What it catches, stated honestly:** REP-56 (an unresolvable field get returns
`error_expr()` → `Lit::Unit` → `ConstGc{Unit}`, then `ExtractScalar{Int}` against
it) and REP-49. **Not** REP-54 or TY-31's catalog bound — both produce their bad
descriptor inside a runtime call, which is `Bottom`. It closes the half where MIR
emitted the descriptor itself and is silent where it comes out of the runtime.
It is also not a `praxis check` failure (`check.rs` never lowers to MIR) and must
not be made one: `run.rs` states the standing position that a verifier failure is
a compiler bug, never a program error.

**The census must chase `MoveGc` or it answers the wrong question.**
`TypedExpr::Path` returns the variable's slot local directly, and every
`let`/`var`/`Assign` writes that slot with `Inst::MoveGc` — so every read of a
user variable is an `ExtractScalar` whose `src` is `MoveGc`-defined, and handover
26's literal three-producer set covers **none** of them. Hand census over
`collatz`/`primes`/`mandelbrot` inner loops: **29/56 = 52% literal today and
12/39 = 31% after W8-S0** (a fail on the "fewer than half" gate) versus **54/56 =
96% and 37/39 = 95% with transitive `MoveGc` chasing** (a clear pass). `MoveGc`
chasing is sound by the verifier's own invariant — both operands must be `Gc` —
and the backend arm is a register copy. **Report both columns.** Run it without
chasing and W11 is declined on an artifact of how one sentence was written.

One point in W11's favour nobody has made: unlike W8-S0, the backend half has no
crash-debugger interaction at all. `ExtractScalar` writes a `Scalar` local, and
scalar locals have no debug slot today.

---

## 6. Gate 4 — the W4 orphan closes inside W4b

Handover 26 §8 registered it as unowned and guessed at closing it by adding a
fourth producer to W8-S0. **Mechanically impossible:** W8-S0 forwards the *scalar
operand* of a `Materialize`, and `Inst::Call` has none — the box comes out of the
callee. Adding `Call` is a different transform, and the only thing it could
license is W11-style proof elision, which leaves the box/unbox round trip the
bullet is about entirely intact. The return-descriptor information it names is
not in `praxis-stdlib/src/abi.rs` either: `AbiRet` is
`Gc | GcUnit | RawI64 | Ptr | Void` and carries no type — and a return-type column
would be *false* on the fault path of any `Faults` row, because `AbiRet::Gc`'s
own doc records the `Unit` sentinel coming back there.

**The shape that closes it, on a precedent already in the tree:** give
`bs.contains(x)` its own MIR instruction with a `Scalar(Bool)` dst and an ABI row
of `-> RawI64`, exactly as `==` on composites got `Inst::StructEq` and ordering
got `Inst::ValueCmp`. `BitsetContains` is `(Ctx, Gc, Gc) -> Gc, Pure` today,
against `StructEq … -> RawI64, Pure` and `ValueCmp … -> RawI64, Faults`. The
builder then materializes the result, so `lower_expr_gc`'s "returns a `Gc` local"
contract is unchanged — and W8-S0, already landing in wave 2, removes the
resulting pair block-locally wherever the use is a branch. In `bfs`,
`!visited.contains(nb)` goes through `lower_logical_not`, which emits its
`ExtractScalar{Bool}` on the call's dst in the same block. Exactly that shape.

**It also buys what handover 26 says W4b cannot.** §4 says the inline arm keeps
the root spills because `is_gc_safepoint` treats every `Inst::Call` as a
safepoint. True of `Call`, but that is not the whole rule: `liveness.rs:382-391`
is a shape match over `Alloc | Materialize | Call | CallIndirect | StructEq`, and
`ValueCmp` is **deliberately absent**. So a `Pure` primitive given the `ValueCmp`
shape is not a safepoint, and the ~17 root-spill instructions go too. This
*satisfies* ADR-113's rule rather than violating it — the narrowing is made in
MIR, where the safepoint property is stated, not by a backend arm reading what it
happens to emit.

**Price: ~0.5–1 day on top of W4b, six files, two commits.** The largest piece is
not the backend: `praxis-stdlib/src/catalog.rs` needs a third `MethodLowering`
arm, which ripples to `MethodEntry::allocates`/`can_fault` and **four exhaustive
matches in `praxis-hir/src/lower.rs`**. That is the reason this is not a two-hour
change and the thing easiest to under-price from a summary. Land it as a
separately revertable commit inside W4b so a regression bisects to the ABI/MIR
shape change or to the backend inlining, not to both.

---

## 7. What this does to the waves

| wave | handover 26 | now |
|---|---|---|
| 0 | INFRA | + the census must report **two columns** (literal, and `MoveGc`-chased) |
| 1 | W1, W2, W4a | unchanged |
| 2 | W6, W7, W8-S0 | unchanged — W8-S0 stays in `praxis-mir` |
| 3 | W4b, W10 | **W4b (+ the orphan), W10 (gated), W11-safety** — 3 wide, at the build cap |
| 4 | W8-S0b | unchanged |
| 5 | decision gate | + W11's backend half, W8-S1, W9 |

W11-safety touches neither `lower.rs` nor `heap.rs`/`page.rs`, so wave 3's
`W4b → W10` merge order is unaffected and it merges independently.

---

## 8. Handover 26's errors, collected

1. **`run.rs:752` does not go red.** §1 above. The plan's three guardrails around
   W8-S0 all rest on it.
2. **"There is no scalar-to-scalar move in MIR"** — `build.rs:3732` is
   `fn move_scalar`. The premise that put wave 2's width in doubt is false.
3. **Every `lower.rs` line number is ~35 low.** §2.
4. **"W10's entire justification is `praxis_alloc_float`"** — W10 inlines the
   claim, and every benchmark allocates out-of-range `Int`.
5. **W8-S0 framed as a float transform** — `TypedExpr::Bin` materializes `Int`
   intermediates too. This understates W8-S0 and further overstates what is left
   for W10; the two errors point in opposite directions and do not cancel.
6. **"Both consume W6's descriptor table"** — false for W10.
7. **W11's producer set omits `MoveGc`**, so it covers no user variable at all.
   52%/31% versus 96%/95%.
8. **"Turn REP-49 and REP-56 into build failures"** — it is a `praxis run`
   refusal, not a `check` failure, and it catches two of the four, not the class.
9. **The W6/W11 overlap reads as symmetric.** It is not; the double count is 2
   instructions per site.
10. **The W4 orphan cannot be closed by a fourth W8-S0 producer.**
11. **W4b "keeps the root spills" is not forced** — `is_gc_safepoint` excludes
    `ValueCmp`, and that is the door.

## 9. What is still not verified

Nothing below was built, run or timed. Each has the one measurement that settles
it.

- **How many times W8-S0 fires.** The shape is verified; the count (8 in
  `mandelbrot`'s inner loop? 2? 12? the suite-wide `Int` figure?) is not. → wave
  0's census over all eight benchmarks, **before** ADR-120's Decision is written.
- **Handover 25 §3 counts 7 type-proof sites** in its sample loop; a walk of
  `build.rs` gives 9, and the `CheckFault` count corroborates 9. W6's acceptance
  criterion is stated as "7 × 2 = 14 fewer per iteration" and would be 18. → the
  census on that exact source, before wave 2 is cut.
- **Whether cranelift-frontend tolerates a `declare_var`'d `Variable` never
  `def_var`'d and never `use_var`'d** — what W8-S0 leaves behind per elided `Gc`
  temp. Every consumer was traced and none should touch it. → build once, run one
  program. It is the pass's first smoke test.
- **Whether any test outside `run.rs` asserts a debugger temp's value** that
  becomes `<uninit>`. → `cargo test --workspace` on the W8-S0 branch. If one
  exists it is a *good* failure — it is the signal §1 says is missing.
- **What fraction of each benchmark's `claim_block`/`alloc_raw` time is scalar
  boxes versus composites.** Handover 25 §2's rows are aggregates. → the wave-2
  re-baseline read per-callsite. This is W10's go/no-go and cannot be derived by
  reading.
- **How much of the 8.5 ns per out-of-range `Int` allocation is the claim path
  versus the amortized sweep.** → an allocation-only variant of the 20M-iteration
  microbenchmark with the collection threshold raised past the run.
- **W10's actual instruction count.** Estimate 20–25 with 3–4 conditional bails,
  against a call whose real cost is `abi_guard!`'s `catch_unwind` region plus
  `RuntimeRoots::from_context`. → `PRAXIS_DUMP_VCODE` at one claim site.
- **Whether W10's tail-word case should bail to the cold arm or reproduce
  `tail_mask`.** Bailing is simpler and provably correct, costs one more branch,
  and cedes the last ≤63 blocks of every page to the wrapper. → build both, count
  instructions, time `collatz`.
- **Whether `stride` folds to a compile-time immediate at every eligible W10
  site.** → dump the CLIF at one `Alloc{Float}` site; `iconst` or load.
- **Whether W4b and W10 interact through `emit_inline_intern`.** If both diffs
  touch its body, §3's `W4b → W10` merge order is the wrong direction. → have both
  agents state it at the wave-3 cut.
- **The real W11 census, mechanically**, in both columns, over the post-W8-S0
  tree. Nobody should commit a day to the backend half before this.
- **Whether every `Effect::Pure` wrapper is genuinely total**, which is what makes
  "a `Pure` call never returns the `Unit` sentinel" true. → audit the `Pure` rows
  against their wrapper bodies for any early return of a sentinel.
