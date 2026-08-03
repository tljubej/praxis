# ADR-117: A raise that branches is its own observation, and only checked `Int` arithmetic can be one

**Date:** 2026-08-03
**Status:** accepted — implemented
**Milestone:** post-M11 performance (handover 25 finding F-6, handover 26 package W7)
**Amends:** [ADR-102](./102-a-check-is-a-branch-not-a-call.md)'s Consequences
bullet *"ADR-088 is untouched, and the reason is worth stating precisely"*, whose
closing sentence is falsified below and replaced with a weaker one that covers
both shapes. **[ADR-088](./088-a-faulting-instruction-is-observed-by-the-next-one.md)
itself is untouched**: its rule is about MIR, this record changes only what the
Cranelift lowering emits for a MIR pair it already required. No MIR instruction,
no verifier rule, no runtime behaviour and no ABI version moves.

## Context

Checked `Int` arithmetic emits this, once per operation, and has since ADR-102
made the raise a branch:

```
brif overflowed, block_raise, block_next
block_raise: call praxis_raise_int_overflow_if(ctx, 1); jump block_next
block_next:  load ctx.pending_fault; load fault.kind; brif kind, fault_epilogue, …
```

**The third line re-decides what the first line decided.** The only way
`pending_fault` is set at `block_next` is `block_raise`, which was entered on a
branch over the same predicate — so `load; load; brif` is three instructions
spent asking whether a branch two blocks up was taken.

Handover 25 §3 priced it at 9 CLIF instructions per iteration of its sample loop:
3 fallible operations × 3. That count is confirmed here by the MIR census wave 0
built (`praxis_mir::test_support`), which is the first time it has been counted
rather than read off a hand-annotated dump — see *The counts handover 25 and
handover 27 disagreed about*, below, where the census also settles a second
number the two documents contradict each other on.

**Checked `Int` arithmetic is the only instruction in the language whose fault
path the lowering emits itself**, and that is the whole reason this
specialization exists at all. Every other faulting instruction is a call into a
wrapper that sets `ctx.pending_fault` and *returns*; generated code cannot learn
that happened except by reading the slot, which is exactly what
`Inst::CheckFault` lowers to. The raise is different: it is a cold block this
lowering created, reached by a branch this lowering emitted, on a predicate this
lowering computed. There is nothing to read back.

## Decision 1: what replaces ADR-102's sentence

ADR-102's Consequences say, verbatim:

> **ADR-088 is untouched, and the reason is worth stating precisely.** Its rule
> is positional *within a MIR block* (`verify::check_fault_observed`), and this
> emits no MIR. **Every diamond converges before the `CheckFault` lowering, so
> the check runs on the raising path and the non-raising path alike.**

The second sentence is now false, and the same claim was written a second time as
a rustdoc section literally titled `# ADR-088 is untouched` on
`lower::raise_on_cold_path`, ending *"Both arms of the diamond converge at `cont`
before the `Inst::CheckFault` that MIR requires next lowers."* Both are rewritten
by this record rather than quietly edited, because the invariant they state is
the one the decision log exists to keep track of.

**The replacement, which covers both shapes and is what the lowering now
maintains:**

> On the raising path, control reaches the function's fault epilogue before any
> instruction after the raise executes.

At `RaiseExit::Observed` — an unfused site — that holds because the diamond
converges and the `CheckFault` below re-reads what the cold block wrote. At
`RaiseExit::Folded` it holds because the cold block *is* the branch to the
epilogue and there is nothing between them. The old sentence is the first case
mistaken for the rule.

**Why folding cannot lose a fault, stated as the argument and not as a test
result.** A folded `CheckFault` observes its predecessor and nothing else, and
that is ADR-088 decision 1's *converse* doing the work: a `CheckFault` must
immediately follow an instruction that can fault
(`VerifyError::RedundantFaultCheck`), so every faulting instruction earlier in
the block has its own check, and a fault raised by any of them has already
diverted to the epilogue. Control therefore reaches a checked `IntBinOp` only
with `pending_fault` clear, and the check after it can be answering exactly one
question. That is why ADR-088's converse — which handover 25 read as a tidiness
rule — is the precondition for this optimization existing.

**Both Div/Rem diamonds fold.** `lower_int_binop` emits two in sequence for a
division, and their conditions are mutually exclusive (`r == 0` versus
`r == -1`), which the arm already documented. At most one cold block therefore
runs, so the epilogue is entered with the kind that the raise which ran set —
identical to what the single downstream `CheckFault` would have observed.
`an_overflow_diverts_before_the_next_statement_runs` (ADR-102's behavioural
proof, in `tests/adversarial_audit.rs`) still passes unedited, and it is the
sharp version of this claim: `set_fault` overwrites unconditionally, so the
reported kind names the last operation that ran.

## Decision 2: the pair is a lowering *step*, not a look-ahead

The fold has two halves that must agree — the raise diverts, **and** the check is
silent. Written as two arms of `lower_inst` each peering at its neighbour, they
can disagree in two ways, and one of them is a program that keeps running past an
overflow. So the grouping is made once, before anything is emitted:

```rust
struct Step<'a> { insts: &'a [Inst], kind: StepKind }

enum StepKind {
    Lone,
    RaiseIntoFault { op: IntBinOp, dst: LocalId, lhs: LocalId, rhs: LocalId, on_fault: BlockId },
}

fn steps(insts: &[Inst]) -> Vec<Step<'_>>
```

`steps` is the only place the two adjacent instructions are examined. A fused
step covers both; the `CheckFault` is a member of that step and of no other, so
`lower_inst` never receives it and cannot emit it. And `RaiseIntoFault` carries
the **operation** rather than an `&Inst`, so the diverting form cannot be handed
an instruction that does not emit its own fault path — the illegal state is not
represented rather than checked for.

The same reasoning shapes the two enums the emitter takes:

```rust
enum OverflowReport { Bare, Checked(RaiseExit) }
enum RaiseExit { Observed, Folded(Block) }
```

A single `overflow: Overflow` plus a separate `exit` would admit *bounded
arithmetic with a fault target*, which emits no raise at all — so the target
would be dropped silently, taking the folded-away `CheckFault` with it. Nesting
the exit inside `Checked` is what makes that combination unspellable.

**The backend does not depend on the verifier for correctness.** `steps` fuses
only what it sees; a checked `IntBinOp` with no check after it lowers to
ADR-102's converging diamond exactly as before. A caller that skipped `verify`
gets slower code, not code that runs past a fault.
`a_checked_int_binop_with_no_check_after_it_is_its_own_step` pins that.

## Decision 3: the scope is narrow, and the ADR says so before the numbers do

Handover 26 §4 wrote W7's scope as *"on `bfs` and `vm` — the two benchmarks
dominated by runtime calls — this reaches almost nothing; it is a
`collatz`/`primes` change"*, and handover 26 §9 registered the number behind that
sentence as **unmeasured**. It is measured now
(`tests/fault_check_census.rs`), over the eight programs `benchmarks/run.py`
runs, counting `Inst::CheckFault`s whose immediate predecessor is a checked
`IntBinOp`:

| program | foldable / total |
|---|---:|
| `bfs` | 39 / 60 |
| `collatz` | 6 / 8 |
| `hashwork` | 15 / 18 |
| `mandelbrot` | 4 / 6 |
| `pipeline` | 21 / 30 |
| `primes` | 7 / 10 |
| `tree` | 21 / 32 |
| `vm` | **5 / 58** |
| **corpus** | **118 / 222 — 53%** |

**Handover 26 named the right benchmark and the wrong pair.** `vm` matches the
prose exactly — a `match` over ten opcodes with a `Deque` under it, where nearly
everything fallible is a wrapper call, and a wrapper call's check is precisely
the shape that cannot fold. `bfs` does not: it folds 65% of its checks, because
it is dominated by runtime calls at *run* time while being full of index and
counter arithmetic as *written*. Those two readings are not in contradiction —
one counts seconds and the other counts sites — but handover 26's sentence is a
claim about reach, and on the only axis this package can measure, `bfs` is not
where it said.

**This is a census of sites, not of executions**, and no honest reading turns it
into a runtime percentage: a program's hot loop is a handful of its blocks and
the rest is setup. It says how much of the *written* corpus the fold reaches.
What it is good for is the shape of the answer — 53% overall, 9% on `vm` — and
that shape is why this record reports instruction counts rather than a suite
geometric mean.

## What it is worth: the instruction counts

Handover 26 §6 asks the packages whose headline is an instruction count to report
that count and to say plainly when the clock cannot resolve the difference. This
one cannot: three instructions per fallible operation, on branches that are
perfectly predicted, is below what an unpinned M2 Pro laptop can measure.
**No wall-clock number is claimed here.**

Handover 25 §3's loop, through `PRAXIS_DUMP_CLIF` / `PRAXIS_DUMP_VCODE`, read
per-iteration by the rule in `dump.rs`'s module doc:

| | arm A | arm B | delta |
|---|---:|---:|---:|
| CLIF, whole function | 311 in 55 blocks | 302 in 52 blocks | **−9** |
| CLIF, per iteration | 171 over 35 blocks | 162 over 32 blocks | **−9 (−5.3%)** |
| vcode, whole function | 458 in 67 blocks, 1960 bytes | 431 in 58 blocks, 1876 bytes | −27, −84 bytes |
| vcode, per iteration | 215 over 38 blocks | 197 over 32 blocks | **−18 (−8.4%)** |

Arm A reproduces handover 25's baseline exactly — 311/458 whole-function,
171/215 per iteration — so the delta is against the number the whole plan is
quoted against, not against a re-derivation of it.

**The CLIF delta is 3 per folded check, and it is 3 everywhere.** Whole-program
static counts over the corpus, every compiled function, both arms:

| program | folded | CLIF | vcode |
|---|---:|---:|---:|
| `bfs` | 39 | 3933 → 3816 (**−117 = 39×3**) | 6093 → 5771 (−322) |
| `collatz` | 6 | 674 → 656 (−18) | 1037 → 986 (−51) |
| `hashwork` | 15 | 1653 → 1608 (−45) | 2422 → 2281 (−141) |
| `mandelbrot` | 4 | 1355 → 1343 (−12) | 2271 → 2225 (−46) |
| `pipeline` | 21 | 2396 → 2333 (−63) | 3538 → 3365 (−173) |
| `primes` | 7 | 914 → 893 (−21) | 1380 → 1314 (−66) |
| `tree` | 21 | 1946 → 1883 (−63) | 2843 → 2665 (−178) |
| `vm` | 5 | 2054 → 2039 (−15) | 3033 → 2992 (−41) |

Every CLIF delta is exactly three times the census's foldable count, on eight
programs, with no exception. That is the strongest available statement that the
change landed everywhere it should and nowhere it should not: the MIR census and
the emitted code are two independent counts of one thing and they agree to the
instruction.

**The machine-code delta is larger than the CLIF delta, and per folded check it
is 8–9 rather than 3.** Removing the check removes a *block boundary* as well as
two loads and a branch: at `opt_level = "none"` the pending-fault load is a real
load-use dependency, the `brif` becomes a conditional branch plus its
fall-through, and the fresh block the check used to open ends live ranges the
register allocator then has to rejoin. Per iteration of the sample loop the
figure is 6 rather than 9, because three of the nine sit in edge blocks the hot
path does not walk.

### The counts handover 25 and handover 27 disagreed about

Handover 27 §9 registered two numbers about the same loop that its predecessors
contradict each other on, and asked for the census to settle them. It does, and
they split:

- **`CheckFault`: three per iteration.** Handover 25 §3 is right. Pinned by
  `every_fault_check_in_the_sample_loop_is_folded_into_its_raise`, which also
  asserts the backend now emits zero of them.
- **Runtime type proofs: nine per iteration, not seven.** Handover 27 §9 is
  right, and its route to the number — a walk of `build.rs` — is confirmed by
  counting: eight `ExtractScalar{Int}` (two operands each for `i * 3`,
  `acc + …`, `i + 1` and `i < limit`) plus the condition's
  `ExtractScalar{Bool}`. Each is one `emit_scalar_load`, which is one descriptor
  proof (ADR-102). **This is W6's denominator**, and its stated acceptance
  criterion of "7 × 2 = 14 fewer per iteration" should be 18. Pinned by
  `the_sample_loop_proves_nine_descriptors_per_iteration_not_seven`, in this
  package because the census was already open here; it asserts nothing about
  W6's change.

### The arms

Arm A is this branch with this package's single toggle reverted — not `main`,
not the previous commit (ADR-113 records that mistake giving 14.4% where the
truth was 0.8%). The toggle is a cargo feature with exactly one reader, the
`cfg!` in `lower::steps`:

```
cargo build --release -p praxis-cli                                   # arm B
cargo build --release -p praxis-cli \
    --features praxis-codegen-cranelift/unfolded-check-fault          # arm A
```

A feature rather than W1's documented three-file revert because the mechanism
here is *one branch inside one function*, so the revert is one `cfg!` rather than
three files, and because a feature makes both arms buildable from one checkout
without touching the tree. It makes the compiler emit worse code on purpose and
the ADR-117 tests assert the folded shape, so `cargo test --features
unfolded-check-fault` fails by design; the Cargo.toml comment says so.

Correctness of the pair, checked before any count was believed: all eight
benchmarks at frozen `sizes.json` sizes produce **byte-identical stdout** under
both arms, and the three fault paths the fold actually changes — `IntOverflow`
from `+`, `DivByZero` from `/` and from `%`, and an overflow raised inside a
called function so the caller's *unfoldable* check still does the work — produce
byte-identical crash-debugger diagnostics, `<uninit>` renderings and backtraces
under both arms.

## What was deliberately *not* done

**No MIR pass, and no relaxation of ADR-088.** §10.4's "later optimization may
combine checks when safe" is explicitly *not* invoked: ADR-088 §1 notes that a
combined check relaxes both directions of its rule at once, and that is a much
larger change than this. Here the MIR is unchanged, both directions of the rule
still hold on it, and the specialization is entirely in what the backend emits
for a pair the verifier already guarantees. A `CheckFault` that MIR emits and the
backend does not is not a hole in the rule; it is the backend having a shorter
way to satisfy it.

**The fold is not extended to `ValueCmp`, `LoadField`, `Call` or any other
faulting instruction.** They are calls; the wrapper sets the slot and returns, so
the slot read is the mechanism rather than an inefficiency in it. Handover 27 §6
proposes giving `bs.contains(x)` the `Inst::ValueCmp` shape, and the same
question could be asked of an inlined `praxis_int_div` — but every version of
that is "give this operation an inline fault path", which is a different package
with a different risk.

**The `Inst::CheckFault` arm is not deleted, and neither is the `CheckFault`
manifest row.** 47% of the corpus's checks still go through it, and ADR-102's
Consequences already record why the row stays even where the call is not emitted:
`ScalarKind::load_symbol` is read by `Inst::fault_reason` to decide whether an
`ExtractScalar` needs a check, and deleting the row would move the verifier's
answer.

**The fault block is not marked cold.** With every check in a function folded,
the epilogue is entered only from cold blocks, so Cranelift could be told as
much — one line, and a plausible further win on layout. It is left out because it
is a claim about *every* function rather than about this pair, and because the
right form of it is probably `Terminator::Fault`'s own lowering asking whether
anything hot jumps to it. Registered below.

**The debugger's per-definition store is not special-cased.** The block loop now
walks a step's instructions rather than one instruction, which is a no-op today:
a `CheckFault` defines no local, and an `IntBinOp`'s `dst` is a `Scalar` local,
which has no debug slot. Were that ever to change, folding would move the store
off the raising path — the direction ADR-104 already argues for, since the
faulting operation's result was never produced and `<uninit>` is the honest
rendering where the converging shape stored the wrapped value.

## Consequences

- **`raise_on_cold_path`'s cold block no longer always rejoins.** Its rustdoc
  section `# ADR-088 is untouched` is rewritten to say which branch discharges
  the rule at which exit, and it names ADR-102's sentence as the special case it
  was. A reader who arrives at that function from ADR-102 now finds the
  correction rather than a contradiction.
- **`lower_inst`'s `Inst::IntBinOp` arm is now a delegation**, and the body it
  delegated to — `lower_int_binop` — lives beside the raise helpers. That is a
  ~150-line move with three call-site edits inside it and no behaviour change; a
  reviewer should read it as a move.
- **A folded raise leaves `dst` undefined on the fault path**, where the
  converging shape defined it with a wrapped value nothing was entitled to read.
  Both are invisible: the value has no debug slot, and the epilogue reads no MIR
  local. `a_folded_raise_jumps_to_the_fault_epilogue_with_no_arguments` pins the
  structural fact that makes this safe — `Terminator::Fault`'s epilogue takes no
  block parameters, because everything it reads (the two frame bases, `ctx`) is
  defined in the entry block, which dominates every raise.
- **The corpus census is asserted, not printed.** `tests/fault_check_census.rs`
  fails if any benchmark is edited, and the fix is to update it *and* the table
  above. That is the intended cost: a measurement quoted in an ADR and checked by
  nothing rots silently, which is how handover 25's 156/216 became
  unreproducible in the first place.
- **`emitted_fault_checks`, the test helper that counts remaining checks, matches
  the operand as a whole token.** `"v0+8".contains("v0+8")` is also true of
  `v0+80` — the shadow frame's fifth slot — and the substring version reported
  the prologue's stores as fault checks. This is handover 26 §7 trap 3 from the
  other direction, in the same file; the trap is the file's, not W6's.
- **`vm` gains 15 CLIF instructions' worth of nothing**, and that is the number
  to carry into the wave-5 re-ranking rather than a suite mean. If the next round
  wants `vm`, it wants the runtime-call path, which is W1 and W4b.

## Open questions

- **Should `Terminator::Fault`'s block be cold when nothing hot jumps to it?**
  After this change, a function whose every check is folded reaches its epilogue
  only from cold blocks, so the epilogue and its `praxis_snapshot_debug_chain`
  call sit in the middle of the hot layout for no reason. The question is a
  property of the whole emitted CFG rather than of one pair, which is why it is
  not in this record; the check is a predecessor walk in `lower_terminator`.
- **Can the raise pass the fault kind instead of a constant `1`?** The cold block
  currently calls `praxis_raise_int_overflow_if(ctx, 1)` — the condition argument
  ADR-102 kept so the wrapper's `if condition != 0` stays a true statement. Now
  that the block is *only* reached when the predicate held and *only* exits to
  the epilogue, an unconditional `praxis_raise_int_overflow` would save one
  `iconst` per raise and make the wrapper honest. It costs two manifest rows and
  two address-table arms, which ADR-102 already judged not worth it; the judgement
  should be re-taken by whoever next opens the manifest, not by this record.
- **How many of the 118 foldable sites execute?** The census is static and says so.
  A dynamic count needs instrumentation that does not exist and would answer a
  question this package does not turn on — the fold is free at every site,
  executed or not. It would, however, be the right denominator if anyone ever
  proposes the reverse trade.

---

## Amendment, 2026-08-03 — the fold is worth *more* after ADR-120, not less

**Amended by [ADR-120 part 2](./120-a-box-with-one-reader-in-its-own-block-is-not-a-box.md),
which carries the measurement.** ADR-120's block-local box/unbox forwarding
landed in the same wave as this package, and the natural expectation — another
package removing instructions from the same loop shrinks this one's share — is
wrong in both directions at once.

Re-measured on the merged tree with `unfolded-check-fault` as the only toggle
reverted:

| | this record | merged tree |
|---|---:|---:|
| CLIF, per iteration | −9 | **−9** |
| vcode, per iteration | −18 | **−28** |

**The CLIF delta does not move, and this record already says why it could not:**
three instructions per folded check, and the census still counts three foldable
checks per iteration. That is not luck — ADR-120 forwards no producer that
`can_fault`, so it cannot delete a `CheckFault` or the raise that precedes one.

**The machine delta grew**, and the explanation is also already here: "Removing
the check removes a *block boundary* as well as two loads and a branch… Per
iteration of the sample loop the figure is 6 rather than 9, because three of the
nine sit in edge blocks the hot path does not walk." The forwarding shortened
that hot walk from 32 blocks to 21, so the three folds are now all on it and the
per-fold figure on the walked path went from 6 to ~9.3 — which is the
whole-program 8–9 this record measured over the corpus.

The one thing this changes downstream is the sentence in "The counts handover 25
and handover 27 disagreed about": **runtime type proofs are five per iteration
on the merged tree, not nine**, and the test named there is renamed accordingly.
Nine was right about what `build.rs` emits; five is right about what reaches the
backend. ADR-116's amendment restates its own headline against the same
denominator.
