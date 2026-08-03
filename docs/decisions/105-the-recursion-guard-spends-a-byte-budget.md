# ADR-105: The recursion guard spends a byte budget, and the budget is a value the host installs

**Date:** 2026-08-03
**Status:** Accepted
**Milestone:** post-M11 (handover 23, defect D-1)
**Amends:** ADR-101's sizing argument for `SHADOW_STACK_SLOTS` (the reservation
is now derived from the budget rather than from the product of two worst cases);
`RuntimeContext.recursion_depth` becomes `stack_left`, so `RUNTIME_ABI_VERSION`
goes 18 → 19. ADR-019's guard-before-push ordering is preserved verbatim.

## Context

`MAX_RECURSION_DEPTH = 8000` existed so that deep recursion faults cleanly
instead of overflowing the native stack and aborting the host. Its own doc said
it was "chosen with headroom under the native stack's abort threshold."

It counted **calls**. What runs out is **bytes**.

Measured on this backend (arm64, release) by bisecting the depth at which a
recursive Praxis program aborts, under `ulimit -s`:

| program | `Gc` locals | native frame |
|---|---:|---:|
| `fn narrow(n) { if n == 0 { 0 } else { narrow(n-1) + 1 } }` | ~4 | **86 B** |
| 14 live collections | 121 | **227 B** |
| 18 live collections | 153 | **262 B** |
| 22 live collections | 185 | **294 B** |

That is `99 + 1.06 × gc_locals`, and it means a frame's cost varies by a factor
of 3.4 across the range the language permits. A count calibrated so that the
*narrowest* frame is safe is therefore not safe for the widest one: at 8000
calls the narrow program uses 688 KiB and the wide one 2.35 MiB. Under
`ulimit -s 2048` the wide program aborts — `thread 'main' has overflowed its
stack`, SIGABRT, exit 134 — at a depth the guard was still letting through.
Reproduced on the pre-change binary at `54d2d9a`.

**The stack that matters is not the one this was calibrated for.** `praxis run`
calls the JIT entry on the process main thread: 8 MiB on macOS, where 2.35 MiB
fits. But `cargo test` runs every JIT test on a libtest thread, which passes no
`stack_size` and therefore gets std's `DEFAULT_MIN_STACK_SIZE` — **2 MiB**. The
smaller of the two stacks Praxis actually runs on is the one the whole test
suite lives on, and it is the one a wide frame overflows.

Handover 23 recorded the hard part as "deciding what the budget should be
derived from, since `getrlimit` on the main thread and a spawned thread's stack
size are different numbers." That is exactly right, and it is why the answer
below is not to ask.

## Decision 1: a frame spends bytes proportional to its width

The prologue charges

```
frame_cost(slots) = FRAME_BYTES_BASE + FRAME_BYTES_PER_SLOT × max(0, slots − REFERENCE_FRAME_SLOTS)
```

— 134 bytes, plus 2 for every `Gc` local past the eleventh. Both constants round
**up** from the fit above, so the model over-charges every real frame and the
budget is a ceiling rather than an estimate.

**The floor is not cosmetic**, and it was found by a failing test rather than by
design. A cost that fell to `FRAME_BYTES_BASE` at zero slots let the budget buy
9571 minimum-width frames — against a `DEBUG_FRAME_STACK_SLOTS` sized
`MAX_RECURSION_DEPTH + 1`, i.e. 8001. The crash debugger's frame stack overflowed
its reservation in `adr100_a_stack_overflow_restores_the_shadow_stack`, which
aborted the test binary. Making `FRAME_BYTES_BASE` the *minimum* any call can
spend is what re-establishes "at most `MAX_RECURSION_DEPTH` frames are live",
which is the premise that stack — and the debug value stack — are sized on.

`slot_count` is already computed twenty lines above the guard, so the cost folds
into a single immediate at compile time. The emitted sequence is what it was:

```
load    left, [ctx + STACK_LEFT_OFFSET]
icmp    left < COST            ; COST is this function's, folded
brif    -> stack_overflow
iadd    left, -COST            ; on the taken path only
store   [ctx + STACK_LEFT_OFFSET], left
```

Four instructions in the entry block and one in the body, exactly as before.
Charging by width costs nothing per call; it only changes which immediate is
used.

**What this changes for programs**, and the correction that matters most here.
The budget is `MAX_RECURSION_DEPTH × FRAME_BYTES_BASE`, and `FRAME_BYTES_BASE`
is the *reference* frame's cost — the eleven `Gc` locals of

```praxis
fn count(n: Int) -> Int { if n == 0 { 0 } else { 1 + count(n - 1) } }
```

the program `MAX_RECURSION_DEPTH` was chosen for.

Anchoring there rather than at a zero-slot frame was found by measurement, not by
reasoning. The first implementation charged proportionally from zero, and the
release binary then faulted at 6686 where it used to reach 7998. A zero-slot
function is a hypothetical — every real Praxis function boxes something, and the
simplest recursive one there is takes eleven `Gc` locals — so anchoring there
would have cut 16% of the depth off every ordinary program to fix a defect that
only ever affected wide frames.

With the reference anchor, an ordinary recursive program reaches exactly the
depth it did before, and a maximum-width frame faults at about 2160 calls
instead of 8000. That is not a regression; it is the guard finally doing its job
for the frame shape it was previously mis-sizing. The behaviour it replaces was
SIGABRT.

## Decision 2: the counter counts down, and the budget rides in the context

`recursion_depth: u32` becomes `stack_left: u32` — same offset, same width, the
*remaining* budget rather than a consumed count.

The direction is the design. Counting up needs the limit in generated code,
which fixes it at compile time for every host the JIT will ever run on.
Counting down puts the limit in the field, so:

- **`Runtime::context()` is the one door a stack size enters through.** Every
  caller of generated code goes through it — `praxis run`, the debugger's
  session, `p EXPR`, and the test helpers. A host that later learns its real
  stack size changes one line there; codegen never learns it at all.
- **Zero means exhausted**, which is the right thing for
  `RuntimeContext::placeholder` to say. Generated code must never run against a
  placeholder; if it did, it now faults `StackOverflow` on its first prologue
  instead of running with a full budget over a stack nobody sized.
- The epilogue is unchanged. It restores the absolute the prologue saved, so a
  variable-width charge costs it nothing: restoring an absolute does not need to
  know what was subtracted.

## Decision 3: the budget is one constant that fits under both stacks, and `StackBudget` seals it

`STACK_BUDGET_BYTES = MAX_RECURSION_DEPTH × FRAME_BYTES_BASE`
= 1,072,000 bytes (1.02 MiB charged; about 768 KiB actually consumed, since the
model over-charges).

**Why a constant rather than `getrlimit`.** The two stacks are 8 MiB and 2 MiB,
and `getrlimit` answers for the first and not the second — it gives a number
that is wrong exactly where the test suite lives. Querying would also make the
guard's behaviour depend on the host's `ulimit`, so the same program would fault
at different depths on different machines, and the depth at which a Praxis
program stops recursing would stop being a property of Praxis. One figure that
fits under both, with 2.3× headroom on the smaller, removes the question instead
of answering it badly.

Three alternatives were considered and rejected:

- **Spawn the execution thread ourselves with an explicit `stack_size`.** This
  would make the budget exactly knowable, and it is the right long-term answer.
  It is not this change: it moves where generated code runs, which touches the
  CLI, the LSP and the debugger's session model, and it is orthogonal to the
  defect. Decision 2 is what makes it a host-only change when someone wants it —
  the thread's owner installs its own `StackBudget` and nothing else moves.
- **Query `getrlimit` and derive.** Rejected above.
- **Keep the count and just lower it to 2700.** This is the one that looks
  cheapest and is worst. It makes every narrow recursion fault at a third of the
  depth it reaches today to protect a frame shape most programs do not have, and
  it still has no answer for a frame wider than the one the new number was
  picked for.

A host may **lower** the budget, through `Runtime::set_stack_budget`. It may not
raise it, and that is enforced rather than documented: `StackBudget::new` is the
only constructor and returns `None` above `STACK_BUDGET_BYTES`. The reason is
Decision 4 — the shadow-stack reservation is sized from that figure, and a
larger budget would make shadow-stack overflow reachable from generated code,
silently, because generated code does not check. Same shape as `SlotCount`, and
for the same reason: the bound is checked once, where the value is made, and
every consumer downstream may assume it.

## Decision 4: the reservation is now exact, and falls from 12.29 MiB to 4.77 MiB

`SHADOW_STACK_SLOTS` was `(MAX_RECURSION_DEPTH + 1) × MAX_SHADOW_SLOTS` —
"the deepest recursion" multiplied by "the widest frame", as if a program could
have both at once. It cannot, and until now nothing said so; the product was
just generous enough to cover the gap.

The byte budget says so directly. A frame spends `FRAME_BYTES_PER_SLOT` on every
slot it goes on to claim, out of a budget of at most `STACK_BUDGET_BYTES`, so
the slots of every live frame together number at most
`budget / FRAME_BYTES_PER_SLOT + MAX_RECURSION_DEPTH × REFERENCE_FRAME_SLOTS`
— 624,000, whatever mix of widths the program recurses through. The second term
is the slots the floor covers for free; the first is everything above it. The reservation is that plus one frame of headroom
for the Rust-side `push_frame` callers, which spend no budget and so are outside
the argument.

The `const` block beside it is kept and restated: a sizing error is a compile
error, not a test run. A second `const` block records the premise the first one
rests on — a slot must spend budget, or the budget bounds no number of them.

## Decision 5: the byte model is audited against Cranelift, not trusted

`frame_cost` is a measurement, and measurements go stale. A Cranelift upgrade, a
new target, or a lowering change that spills more could widen the real frame past
what the guard charges for it — and the symptom would be the SIGABRT this whole
change removes, reappearing years later in a build nobody connected to a codegen
bump.

So the model is checked. Cranelift knows the exact frame size once it has
compiled a function, and `audit_frame_cost` compares
`frame_layout().frame_to_fp_offset` (plus a fixed allowance for the return
address, saved frame pointer and clobber saves, which sit outside it) against
what the prologue spent. It is a `debug_assert`, so it runs on every function of
every program the **entire test suite** compiles, and costs release builds
nothing.

A `debug_assert` rather than a hard error for two reasons: the release compiler
should not pay for it, and the charge is deliberately generous, so being *over*
is the safe direction and the assert only fires when the model is under. If the
layout is unavailable on some target or Cranelift version, the audit is skipped
rather than failed — that leaves the model where it started, unaudited, which is
not worse than before.

## Consequences

- **The defect is closed.** A frame with twenty-two live collections recursing
  past its budget now faults `StackOverflow` instead of aborting the host. The
  reproduction that gave exit 134 on `54d2d9a` returns a clean fault.
- **`RUNTIME_ABI_VERSION` goes 18 → 19.** The field's layout did not move; its
  *quantity* did. A program compiled against v18 would subtract 1 per call from
  a field a v19 runtime hands it a byte budget in, so a wide frame would recurse
  a hundred times past the point the budget exists to stop — the abort restored
  by a mismatched build. Same class of bump as v12 and v17.
- **The shadow-stack reservation falls from 12.29 MiB to 4.77 MiB**, and the
  debug value stack with it. This is address space, not resident memory, so it changes no measurement;
  what it changes is that the number is now derived from something true.
- **`MAX_RECURSION_DEPTH` survives as the anchor**, not the mechanism. It is the
  depth a *reference-width* function reaches, and the figure the budget is
  defined from. Nothing compares against it any more.
- **`REFERENCE_FRAME_SLOTS` is a measurement and has a gate.**
  `adr105_a_reference_frame_still_recurses_as_deep_as_the_call_count_allowed`
  fails if a codegen change makes `count` wider than eleven `Gc` locals, and the
  fix then is to re-measure the constant rather than to lower the depth the test
  asserts.
- **A wide recursion faults earlier than it used to.** `adr100_a_wide_frame_recursing_deep_claims_and_gives_back_every_slot`
  recurses 600 frames of twenty collections and is unaffected. A program that recursed 3000 frames that wide used to survive on an
  8 MiB stack and abort on a 2 MiB one; it now faults on both. That is the
  intended trade and it is the only observable behaviour change.
- **The audit earned its keep on its first run.** It rejected the initial
  `FRAME_SETUP_BYTES` guess — an invented 64-byte allowance on top of
  `frame_to_fp_offset` — by reporting `count`'s frame as 144 bytes against a
  134-byte charge. Replacing the guess with Cranelift's own accounting (the
  setup area is the two words *above* FP, and `frame_to_fp_offset` covers
  everything below it) put the real figure at 96 and the model back in front of
  it. Decision 5 exists for exactly that class of error, and it caught one
  before the change left the branch.
- **The comment in that test which said "`MAX_RECURSION_DEPTH` is a call count,
  not a byte budget"** was an accurate statement of the defect, sitting in the
  test suite, describing the corner no test could reach. It is now a description
  of the fix.
