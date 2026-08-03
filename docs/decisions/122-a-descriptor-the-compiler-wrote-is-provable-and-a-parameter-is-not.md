# ADR-122: A descriptor the compiler wrote is provable, and a parameter is not

**Date:** 2026-08-03
**Status:** accepted — implemented
**Milestone:** post-M11 performance (handover 25 finding F-4.2, handover 26
package W11, split by handover 27 §5)
**Scope:** `crates/praxis-mir` only — a new `provable` module, a `defines()`
helper beside `verify::operands`, and one `VerifyError` variant. No `lower.rs`,
no `heap.rs`, no runtime, no ABI version, no new `unsafe`, and **no change to a
single emitted instruction** — which is measured below rather than asserted.

**This record does not claim ADR-102's proof elision**, and that is deliberate:
see *What was deliberately not done*.

## Context

ADR-102 made every scalar payload read a branch. `Inst::ExtractScalar` names a
width — `praxis_int_load` reads eight bytes — so generated code loads the
object's descriptor, compares it against the one that width belongs to, and
diverts to a refusal when they disagree. Handover 25 §3 priced that at 42 CLIF
instructions per iteration of its sample loop, 27% of the loop, and handover 25
§5's F-4.2 proposed removing it on the grounds that the front end has already
proved `i: Int`.

**Handover 26 §4 declined that, and it read the repair log to do so.** The proof
does not defend against a *program* error; it defends against a *compiler* one,
and the compiler has made that error four times. REP-56 is the sharpest: an
unresolvable field get lowers through `error_expr()` → `Lit::Unit` →
`Inst::ConstGc { Unit }`, and the arithmetic around it emits
`ExtractScalar { scalar: Int }` against that — an eight-byte read off a
descriptor **zero** bytes wide, from a program `praxis check` exits 0 on.
Measured in a release build, that was three different pointer-shaped numbers on
three consecutive runs, silently, with `rc=0`. REP-49 is the same shape at a
different site: `emit_pattern_test` put `Lit::Bool` in `Lit::Int`'s arm, so
`match b { true => …, false => … }` read eight bytes of a one-byte `BoolPayload`
and told the two immortals apart by their alignment padding.

So the interesting question is not "can the proof be deleted" but **"where does
the compiler already know the answer, and can it be made to say so"** — and the
counter-proposal handover 26 §4 offered is exactly that: trust the descriptor
only where *MIR itself emitted it*, never where the front end merely believes it.

Handover 27 §5 split the package in two. The backend elision is deferred (it is
a hard pair with both W6, already merged, and W8-S0b, in flight, so it has no
honest slot before the round's last gate). **This is the other half: the analysis
and the verifier rule it licenses.** It is worth having on its own, it costs the
compiled program nothing, and it turns REP-56 from a silent out-of-bounds read
into a refusal at the compiler.

## Decision 1: the proof is over MIR's own emissions, and `MirType` is not consulted

`praxis_mir::provable` answers, for each `LocalKind::Gc` local, *which
`DescriptorClass` does every definition of this slot produce*, over the lattice

```text
                       Top  — no definition seen
    Int Bool Char Float Unit Text Record Tuple Enum Closure Collection
                      Bottom — no proof
```

The producers are the three shapes where MIR wrote the descriptor down:

| instruction | class |
|---|---|
| `ConstGc { SmallInt \| Unit \| Bool }` | `Int` / `Unit` / `Bool` — `GcConst`'s only three variants |
| `Alloc { alloc: K }` | the `AllocKind`, which is total: eleven variants, eleven classes |
| `Materialize { scalar: K }` | the `ScalarKind` |
| `MoveGc { dst, src }` | whatever `src` resolves to, **by fixpoint** |
| everything else that writes a `Gc` local — `Call`, `CallIndirect`, `LoadField`, `LoadTupleElem`, `EnumPayloadGet`, `LoadCapture` | `Bottom` |

**`Local::ty` is deliberately not read.** It is `MirType::Known(Int)` on the
parameter in `is_prime(n)` and on the record field REP-56 could not resolve
alike, and believing it is the premise handover 26 refuted. The `Unit` sentinel
makes the front end's guarantee routinely false *by design*, at every fault path
of every `Faults` row — `AbiRet::Gc`'s own doc records it — held only by ADR-088's
positional `CheckFault` rule. So the analysis reads instructions, not types, and
the one thing it can be wrong about is being too pessimistic.

**The analysis is flow-insensitive, and that is what makes it sound over an IR
that is deliberately not SSA** (`ir.rs:3`, repeated at `ir.rs:249`). The answer
is a property of the *slot* — "every definition anywhere in the function produces
`K`" — so it holds at every point that reads the slot with no dominance and no
reaching-definitions question, which is precisely the four analyses ADR-108
declined to build. It is also what makes `MoveGc` chasing sound rather than
convenient: if every definition of `src` is a `K`, a copy of `src` is a `K`
wherever the copy happens, and the verifier's own `MoveGcFromScalar` rule
guarantees both ends are `Gc`.

### `defines()`, and why this is not a sixth exhaustive match

The analysis needs *the* local an instruction writes. `verify::operands` returns
the destination and the sources in one undifferentiated `Vec` — right for a range
check, useless here, since `Alloc { Record { fields } }` names six locals and
defines one. `liveness::defs` had the right answer in the wrong shape.

`verify::defines(&Inst) -> Option<LocalId>` is the new statement, and
`liveness::defs` is now `defines(inst).into_iter().collect()`. **ADR-044's
Consequences fix the count of exhaustive `Inst` matches at five and it is still
five**, because this replaced one rather than joining them. The `Option` is also
the sharper claim: *an instruction defines at most one local*, which a `Vec`
return leaves every caller to re-derive.

`def_of`, which classifies each definition, is the one match in this file that is
**not** exhaustive, and its `_` arm is deliberate. Every other exhaustive match
over `Inst` in this crate has a wrong answer available to it; this one does not.
A variant added later and not named contributes `Bottom`, which is an *absence of
proof*, so the cost of forgetting is a missed elision and never a verifier that
blesses a read it should have refused. That is the same trade handover 27 §3
makes for W8-S0's rollback guard, and it is worth more here than a sixth match
would have been.

## Decision 2: the two traps, and how each is made structural rather than checked

Handover 27 §5 names both. Both ship *silently unsound* if missed, which is the
worst available failure for a safety rule.

**Trap 1 — a local with no defining instruction is `Bottom`, permanently.**
Function parameters are `Gc` locals the builder creates with a real
`MirType::Known(p.ty)` and no defining instruction; so are closure-prologue
captures. "Every definition is a `K`-producer" is **vacuously true over an empty
set**, so a universal quantifier written the obvious way blesses every
parameter — at exactly the site where it is most tempting, `primes`' `is_prime(n)`,
whose two `ExtractScalar{Int}`s read a parameter.

The fix is not a check. `ProvableDescriptors::of` **seeds a definition-less local
`Bottom`**, and the fixpoint's worklist is the locals with a non-empty definition
list, so the meet is never taken over an empty set and there is no identity
element to get the wrong way round. The second half is the public surface:
`ProvableDescriptors::class` returns `Option<DescriptorClass>`, and the lattice
type — with its `Top` — is private. A caller cannot receive "no definition seen"
at all, let alone mistake it for a proof.
`a_parameter_has_no_definition_and_is_therefore_not_provable` is the witness,
and `extracting_a_payload_from_a_parameter_is_not_an_error_because_nothing_is_proved`
is the same fact at the rule.

**Trap 2 — the fixpoint must be a *greatest* fixpoint.** Optimistic start at
`Top`, iterate down to convergence. A pessimistic start is not merely imprecise,
it is wrong for the shape this exists for: `Bottom` is absorbing, so a loop
variable whose back edge assigns it from another loop variable resolves to
`Bottom` on the first pass and can never climb back out. The smallest program
that shows it is a swap —

```praxis
var a = 0
var b = 1
while … { let t = a; a = b; b = t }
```

— which lowers to three `MoveGc`s on the back edge, and under a forward pass
every one of `a`, `b`, `t` reports `Bottom`.
`a_pair_of_loop_variables_that_define_each_other_is_still_provable` is that
program in MIR.

The lattice has height three and values only descend, so it converges in at most
`2·locals + 1` rounds — which is the reason no worklist is needed, not a claim
that the bound is tight.

## Decision 3: the rule refuses a proved contradiction, never an absence of proof

For each `Inst::ExtractScalar { src, scalar: K }`: if `provable(src)` is a
**known** class and that class is not `K`, that is a
`VerifyError::ProvedDescriptorMismatch`. **`Bottom` is not an error**, and
neither is a `K` with no class of its own — `DescriptorClass::of_scalar` is
`None` only for `ScalarKind::Byte`, which has no `praxis_alloc_byte` row and
therefore no descriptor to contradict (`ScalarKind::BYTE_HAS_NO_WRAPPER`).

The asymmetry is the whole design. A "proof required" version would refuse
`primes` on the first function it compiled. This one has **zero false positives
by construction and zero measured**: 2024 tests across 31 suites, the eight
benchmark programs, and — separately — the whole corpus with W8-S0 merged in,
with not one `ProvedDescriptorMismatch` anywhere.

The cost is that it is silent where the front end is merely probably right, and
that is the honest position for a rule whose failure mode is refusing to compile
a correct program.

### Where the refusal lands, and two corrections to handover 26 §4

Handover 26 §4 says the MIR pass and verifier rule "turn REP-49 and REP-56 into
*build failures*". **Both halves need correcting.**

**It is not a `praxis check` failure, and must not be made one.**
`crates/praxis-cli/src/check.rs` never lowers to MIR — it names neither
`lower_module` nor `praxis_mir` at all — so nothing in this record moves what
`check` reports. The refusal is a `praxis run` refusal (`run.rs:120-129`) and a
`cargo test` panic in the two JIT harnesses. `run.rs` states the standing
position at the site, verbatim: *"A failure here is a compiler bug, never a
program error, so it is reported as one and no code is generated from it."* This
respects that. Making it a `check` failure would mean lowering to MIR inside
`check`, which is a different and much larger decision, and it would report a
compiler bug through a user-facing diagnostic channel.

**It catches REP-56 and REP-49. It does not catch REP-54 or TY-31's catalog
bound**, and calling it "the class" overclaims. The four were re-read here rather
than carried:

| defect | shape | caught? |
|---|---|---|
| **REP-56** | `error_expr()` → `Lit::Unit` → `ConstGc { Unit }`, then `ExtractScalar { Int }` | **yes** — `Unit ≠ Int`, proved |
| **REP-49** | `Lit::Bool` in `Lit::Int`'s arm: `ConstGc { Bool }`, then `ExtractScalar { Int }` on the literal operand | **yes** — `Bool ≠ Int`, proved |
| **REP-54** | a two-anon-capture template is tagged `&scalars::UNIT` while holding tuples | **no** — the descriptor is chosen inside `praxis_run_parser_plan`; MIR sees `Inst::Call`, which is `Bottom`, and there is no `ExtractScalar` in the shape at all |
| **TY-31's catalog bound** | `Vec[Bool].sum()` type-checks | **no** — `build.rs`'s `Sink::Sum` arm emits `ExtractScalar { Int }` on `item`, and `item` comes out of `praxis_vec_get`'s `Inst::Call`, which is `Bottom` |

It closes the half where MIR emitted the descriptor itself and is silent where
the descriptor comes out of the runtime. That is a real and stateable boundary,
and it is the same boundary Decision 1 draws for a different reason.

## The census: two columns, and one word of handover 27 to correct

Handover 27 §5's reason for insisting on two columns is that handover 26's
producer set omits `MoveGc`, and `TypedExpr::Path` hands back the *binding's
slot* — so every read of a user variable is an `ExtractScalar` whose `src` is
`MoveGc`-defined, and the literal set covers **none** of them. Run it one way and
W11's backend half is declined on an artifact of how one sentence was written.

Run mechanically over all eight `benchmarks/praxis/*.px`, every function the
module lowers, at **this tree** (W6 and W7 merged; **W8-S0 is not in it**):

| program | `ExtractScalar` sites | literal | chased |
|---|---:|---:|---:|
| `bfs` | 126 | 67 (53.2%) | 112 (88.9%) |
| `collatz` | 24 | 14 (58.3%) | 23 (95.8%) |
| `hashwork` | 39 | 22 (56.4%) | 33 (84.6%) |
| `mandelbrot` | 52 | 24 (46.2%) | 46 (88.5%) |
| `pipeline` | 60 | 37 (61.7%) | 46 (76.7%) |
| `primes` | 32 | 16 (50.0%) | 25 (78.1%) |
| `tree` | 51 | 33 (64.7%) | 36 (70.6%) |
| `vm` | 35 | 17 (48.6%) | 19 (54.3%) |
| **suite** | **419** | **230 (54.9%)** | **340 (81.1%)** |

`vm` is the floor at 54.3% and it is the interesting row: every `Int` box in its
interpreter loop is a `Deque` read or a call argument, neither of which MIR
writes a descriptor for. `collatz` is the ceiling at 95.8%.

**Handover 27 §5's hand count is exact.** It reports 29/56 literal and 54/56
chased over the `collatz`/`primes`/`mandelbrot` inner loops. Mechanically:

| inner loop | sites | literal | chased |
|---|---:|---:|---:|
| `collatz` (`while c != 1`) | 16 | 11 (68.8%) | 16 (100%) |
| `primes` (`is_prime`'s `while d * d <= n`) | 12 | 6 (50.0%) | 10 (83.3%) |
| `mandelbrot` (the escape loop) | 28 | 12 (42.9%) | 28 (100%) |
| **three inner loops** | **56** | **29 (51.8%)** | **54 (96.4%)** |

Four numbers, four agreements, hand count against mechanical census. That is
worth pinning rather than paraphrasing.

> **Amended when W8-S0 and W8-S0b merged.** Both tables above were measured on a
> tree without W8-S0, and the merge moved them — legitimately, and by more than
> anyone estimated. On the merged tree the suite is **219 sites, 30 literal
> (13.7%), 140 chased (63.9%)**, and the three inner loops are **29 sites, 2
> literal (6.9%), 27 chased (93.1%)**. The census tests carry the new digits and
> print the tables under `--nocapture`; the pre-merge numbers are kept here
> because the *comparison* is the finding.
>
> **The denominator nearly halved and the literal column collapsed by 41 points,
> and those are one fact rather than two.** W8-S0's producer set is
> `Materialize`/`Alloc`/`ConstGc` — which is *exactly* what the literal column
> counts. Every site the literal column could prove is a site W8-S0 would rather
> delete, so the two anti-correlate. The chased column, which resolves `MoveGc`
> transitively, barely moves in the inner loops (96.4% → 93.1%) because it was
> never counting the sites W8-S0 takes.
>
> This is the strongest argument the round produced against W11's backend half,
> and it arrived by measurement rather than by judgement: after W8-S0 there are
> less than half as many proofs left to elide, and the ones that remain are the
> ones this analysis is weakest at.

**The one word to correct is handover 27's own.** It glosses 29/56 = 52% as *"a
fail on the 'fewer than half' gate"*. 29 of 56 is not fewer than half — it is a
bare **pass**, by one site. What fails handover 26's gate is the post-W8-S0
figure in the same sentence, and that tree is not this one.

### The second column of the census, on the tree that is not this one

W8-S0 is on `w8-integration` and is held out of this branch deliberately: it
lands two debugger-fidelity tests red until W8-S0b repairs them. It was merged
onto a **scratch** branch (`w11-census-scratch`) to run the census a second time,
and **this branch's commit contains none of it** — `git log` and the diff say so.

| region | sites | literal | chased |
|---|---:|---:|---:|
| suite, whole module | 219 | 30 (13.7%) | 140 (63.9%) |
| three inner loops | 29 | 2 (6.9%) | 27 (93.1%) |

**Handover 27's post-W8-S0 estimate of 12/39 and 37/39 is wrong in both
directions, and both errors strengthen its conclusion.** The denominator is 29,
not 39 — W8-S0 deletes ten more inner-loop sites than the hand walk expected. And
the literal column collapses to 6.9% rather than 31%, for a reason that is
structural rather than accidental: **W8-S0 deletes exactly the sites the literal
column counts.** Its producer set is `Materialize`/`Alloc`/`ConstGc`, so every
`ExtractScalar` it forwards away is a literally-provable one, and what survives
is the `MoveGc`-fed remainder the literal column cannot see. The two columns do
not merely differ after W8-S0; they anti-correlate.

The chased column holds at 93.1% through the same transform. Whoever opens W11's
backend half at the wave-5 gate should re-measure rather than re-type these
digits — the tests print the table.

**Also observed on that scratch merge, and it is the thing that most needed
checking: W8-S0 and this rule are compatible.** The whole corpus under both,
`--no-fail-fast`, produces zero `ProvedDescriptorMismatch`. The six failures
there are W8-S0's own two debugger-fidelity tests, two descriptor-proof counts
that W8-S0 legitimately moves, and this record's two census digits.

## What it costs: nothing the program can see, and that is measured

**No wall-clock number is claimed.** Two other agents were compiling beside this
one throughout, so every build-phase timing is discarded by handover 26 §6's
protocol, and there is nothing here a stopwatch could resolve anyway.

The deterministic headline is a *null result*, and it is the right headline for a
package whose claim is "a pure win at no cost":

> The two arms emit **byte-identical code**.

Handover 25 §3's loop, `PRAXIS_DUMP_CLIF=all PRAXIS_DUMP_VCODE=all` through
`praxis run`, both arms:

| | arm A | arm B |
|---|---|---|
| CLIF, whole function | 302 in 52 blocks | 302 in 52 blocks |
| vcode, whole function | 411 in 58 blocks + prologue, 1796 bytes | 411 in 58 blocks + prologue, 1796 bytes |

The two dumps differ in exactly twelve lines — one `iconst` of a heap address and
the `movz`/`movk` pair that materializes it — and **two runs of the same arm
differ in the same twelve lines**, because the address is a fresh allocation.
With hex immediates normalized the two dumps have the same sha256. All eight
benchmark programs produce byte-identical stdout under both arms.

*This tree's baseline, stated because it has moved twice:* 302 CLIF / 411 vcode
whole-function. Handover 27 records 302/431 after W7 alone; W6 is merged here and
takes the machine-code figure to 411. W8-S0 is **not** in this tree, so the
240/344 figure quoted for it does not apply.

The analysis itself is one linear pass to collect definitions plus a fixpoint of
height three, per function, inside `verify` — which every host already runs after
`annotate`. It emits nothing.

### The arms

Arm A is this branch with this package's single toggle reverted — not `main`, not
the previous commit (ADR-113 records that mistake giving 14.4% where the truth
was 0.8%). The toggle is a cargo feature with exactly one reader,
`verify::check_proved_descriptor`, which compiles to an empty body under it:

```
cargo build --release -p praxis-cli                                # arm B
cargo build --release -p praxis-cli \
    --features praxis-mir/unproved-extract-scalar                  # arm A
```

**The analysis runs in both arms; only the rule that reads it is absent**, so the
arms differ in exactly the deliverable. It makes the compiler accept MIR it
should refuse — REP-56's shape compiles again — and the ADR-122 tests assert the
refusal, so `cargo test --features praxis-mir/unproved-extract-scalar` fails by
design, on exactly three tests and no others. The Cargo.toml comment says so.

| | sha256 | bytes |
|---|---|---:|
| `/tmp/praxis-arms/W11-safety-a` | `7ff9bd55ca9cb9b7169e0ea1353eea8941d9d49a8d1017a0562359e467b025e5` | 7 543 888 |
| `/tmp/praxis-arms/W11-safety-b` | `ddc77f0f01e784498257db1a4ee866fcbe5fa12c580274194691689801471457` | 7 544 112 |

224 bytes of compiler, and zero bytes of compiled program.

## What was deliberately *not* done

**ADR-102's runtime proof is not elided, and this record must not be read as
licensing it.** Handover 27 §5 defers the backend half to the round's last gate,
and the reason is scheduling before it is merit: §5's conflict matrix lists
W6/W11 and W11/W8-S0b as hard pairs that never run in parallel, W6 is already
merged and W8-S0b is being built beside this one. On the merits, after W6 the
residual per elided site is 4 machine instructions plus a deleted three-block
diamond rather than the 6 it was worth before, and the W6/W11 overlap **reads as
symmetric and is not**: the only descriptor-proof emitter is `emit_scalar_load`,
whose sole non-test caller is the `ExtractScalar` arm, so at any site W11 would
elide, W6 contributes exactly 0. The double count is 2 instructions per site.
Whoever opens it needs the post-W8-S0 census above and a fresh instruction count,
not this record's numbers.

**No lowering site is changed to make anything more provable.** The census is a
measurement of the builder as it stands. Rewriting a lowering to raise the chased
column would make the number an artifact of the measurement, which is the failure
this ADR's own census exists to avoid.

**`ScalarKind::Byte` is not given a class.** Its two `ir.rs` arms refuse rather
than answer `Int`'s, and `DescriptorClass::of_scalar` returns `None` for the same
reason: there is no `praxis_alloc_byte`/`praxis_byte_load` row, so there is no
descriptor. The consequence is that an `ExtractScalar { Byte }` is never refused
by this rule — an absence, not a contradiction. Nothing constructs a `Byte`
scalar today; whoever wires it adds the two manifest rows and the class together.

**The analysis is not exposed to the Cranelift backend.** It is `pub` on
`praxis_mir` because a MIR pass may want it, but nothing outside this crate reads
it, and the crate that would — the backend — is the one the deferral is about.

**`ProvedDescriptorMismatch` does not name a source span.** Every other
`VerifyError` names function, block and instruction, because a verifier failure
is a compiler bug and the locations that matter are MIR's. This follows the
file's rule rather than inventing an exception for the one error that happens to
correspond to something a user wrote.

## Consequences

- **`liveness::defs` is a delegation now.** It has the same signature, the same
  answer, and no match of its own. A reviewer should read it as a move; the
  behaviour claim is that a `Vec` of at most one element is what it always
  returned, which the type of `defines` now states.
- **`verify` is one pass longer per function.** It computes
  `ProvableDescriptors::of(f)` unconditionally, in both arms, before walking the
  blocks. This is not free at compile time and is free at run time; the trade is
  stated rather than hidden, and it is why the toggle sits at the *rule* and not
  at the analysis.
- **A latent `Materialize{Bool}`/`ExtractScalar{Int}` pun is now a build
  failure.** `verify::operands` accepted one (it checked range only), and
  handover 27 §3 names that as a hazard W8-S0's kind-equality gate had to work
  around. Where the source is provable it is now refused outright, which narrows
  what a future MIR pass has to defend against by hand.
- **The census is asserted, not printed.** Both census tests fail if a benchmark
  is edited or a lowering changes, and the fix is to re-run them and update the
  tables above. That is the intended cost: handover 25's 156/216 became
  unreproducible precisely because nothing checked it. The inner-loop test pins
  digits because they are handover 27's hand count; the suite test asserts the
  *gap between the columns*, because that is the finding and the digits are not.
- **The rule is silent on the majority of `vm`.** 54.3% chased is the worst row
  in the suite, and it is the row that says what this rule is: a defence against
  the compiler writing a descriptor down wrongly, not against the runtime
  handing one back wrongly. `vm` is mostly the second.
- **Handover 26 §4's "build failures" sentence is corrected in two places** — it
  is a `run` refusal and not a `check` failure, and it is two of the four
  defects and not the class. Both corrections are in Decision 3 rather than left
  to whoever next quotes the handover.

## Open questions

- **Should `Alloc { Collection { ctor } }` be one class or ten?** A `Vec` and a
  `Map` are both `DescriptorClass::Collection` here, which is enough for the rule
  (every collection is the wrong object for every scalar width) and not enough
  for anything finer. If a later rule wants "this `LoadField` is against a
  record with this schema", the class is the wrong granularity and the answer is
  a different analysis, not a wider enum.
- **Does the rule want the converse — an `ExtractScalar` whose source is
  provably *right* being allowed to skip its `CheckFault`?** `ScalarKind::load_symbol`
  is `Effect::Pure` for all four wired widths, so no check follows one today and
  the question is empty. It stops being empty the day a width validates, and the
  answer would then belong with ADR-088 rather than here.
- **Is the parameter case recoverable interprocedurally?** `is_prime(n)` is
  called from exactly one site in `primes`, with an `Int`. A summary-based
  version of this analysis would prove the parameter and unlock two sites per
  call — and it would also have to be re-run after monomorphization, handle
  recursion with the same greatest-fixpoint argument as Decision 2, and answer
  what a closure's parameter means. That is a package, not an extension, and it
  should be priced against the backend half rather than added to this one.
- **How many of the 340 provable sites execute?** The census is static and says
  so. It is the right denominator for "how much of the written corpus does the
  proof reach" and the wrong one for any claim about time, which is why nothing
  here converts it into a percentage of anything.
