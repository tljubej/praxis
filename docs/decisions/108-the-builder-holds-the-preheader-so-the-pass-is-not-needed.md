# ADR-108: The builder already holds the preheader, so the pass is not needed

**Date:** 2026-08-03
**Status:** accepted — implemented
**Milestone:** performance repair (handover 23 §P-4, the LICM row)
**Amends:** ADR-100's Consequences bullet "LICM on MIR was considered and
deferred". The deferral was correct about the general pass and wrong about what
was left to win — it named out-of-range `Int` and `Text` and omitted `Float`.
This supersedes it: the general pass is not deferred, it is **declined**.

## Context

`lower_lit_gc` emits `ConstFloat` + `Alloc { AllocKind::Float }` for every
`Lit::Float`, so a float literal in a loop heap-allocates on **every
iteration** — the exact shape ADR-100 removed for in-range `Int` by interning,
and could not remove for `Float`. `benchmarks/praxis/mandelbrot.px`'s innermost
loop has two of them, the `4.0` in `while i < max_iter && x * x + y * y <= 4.0`
and the `2.0` in `y = 2.0 * x * y + y0`. An out-of-range `Int` literal is the
same shape and ADR-100 left it explicitly: `benchmarks/praxis/hashwork.px`'s
generator loop has five, `1103515245`, `12345`, `2147483648`, `65536` and
`98304`, allocated per step.

The obvious cure is loop-invariant code motion, and it was deferred twice — by
handover 21 §3.5 and again by ADR-100 — for a reason that is still true. MIR's
`Function` is a flat `Vec<Block>`. There is no predecessor map, no dominator
tree, no back-edge detection and no notion of a preheader; the only CFG helper
in the crate is `liveness::successors`, which is private and answers one
question. A correct general pass means building all four, and then it has to
handle the hard case anyway: a faulting `Alloc` and its `CheckFault` are paired
**positionally, within one block** by `verify::check_fault_observed`, so they
move together or not at all, and moving both out of a zero-trip loop raises a
fault the program does not have.

The observation that dissolves the problem is that **the builder creates the
loops**. `lower_while`, `lower_for` and `lower_loop` each write
`Terminator::Jump { target: header }` on the block they are standing in, and
that block is the preheader — the loop's only entry from outside, and therefore
a dominator of every block in it — by construction rather than by analysis. And
the values worth hoisting are *literals*, whose loop-invariance is not an
analysis result either; it is what the word literal means.

## Decision

### 1. The hoist happens in the builder, and there is no pass

`Builder` grows `loop_preheaders: Vec<BlockId>`, pushed where each of the three
loop lowerings writes the jump into its header and popped where it sets `b.cur`
to the exit. `box_invariant_literal` appends the literal's `Const*` + `Alloc`
pair into `loop_preheaders.last()` instead of `b.cur`. That is the whole
mechanism: no predecessor map, no dominator tree, no loop detection, no second
traversal of anything.

Appending to a block that is already closed is sound because a `Block` keeps
`insts` and `term` in **separate fields** — "closed" means its terminator is
written, not that its instruction list is sealed — so the appended instructions
land before the jump however long ago the jump was written.

A pass was the alternative and it is worse in a way beyond the four analyses.
`crates/praxis-cli/src/run.rs` runs `lower_module` → `annotate` → `verify` →
`Jit::compile`, and `praxis-debugger`'s session runs the same sequence
separately. A pass slots between two of those steps in *both* places, and a host
that forgot it would silently get unhoisted code. Doing it during lowering means
there is one pipeline and no opt-in.

### 2. The preheader stack is not a `LoopCtx` field, and the difference is `mandelbrot`

`LoopCtx` — the existing stack, which `break` and `continue` read — looks like
the right home and is not. It is pushed *after* a `while`'s condition is lowered,
deliberately: a `break` inside a condition belongs to the enclosing loop, and
pushing earlier would rebind it. But a condition's blocks are part of the loop
and re-execute per iteration, and `mandelbrot`'s `4.0` is in a condition and
nowhere else. Two stacks with two lifetimes is the honest shape — this one opens
at the preheader's jump and closes at the loop's exit, `LoopCtx`'s opens and
closes around the body.

### 3. Two preconditions, both checked rather than assumed

**Non-faulting.** `box_invariant_literal` hoists only when
`Inst::can_fault()` is false, which derives its answer from the ABI manifest
through the same instruction→symbol mapping the backend uses (MIR-10). It is not
a hand-written list of allocation kinds, because a list is a second statement of
the manifest and would drift from it.

The condition has to be exact in both directions. A faulting `Alloc` separated
from its `CheckFault` fails `verify::check_fault_observed` — which is the good
outcome — and a faulting `Alloc` moved *with* its check would raise, on entry to
a zero-trip loop, a fault the source never reaches. The same argument covers a
case that is not a loop at all: `while i < n && x <= 4.0` evaluates its right
operand in a block the header branches to, so hoisting out of that block runs
the allocation on iterations where the source short-circuits past it. For a
non-faulting allocation both are unobservable — the only thing it can do early
is trigger a collection, and *when* a collection happens is not language-visible.

This is why `AllocKind::Text` is not hoisted: `praxis_alloc_text` validates its
bytes, so its row is `AllocatesAndFaults`. It is also why P-4b needs no edit
here. Moving that validation out of the wrapper makes the row
`Effect::Allocates`, and the rule above then admits `Text` on its own — which is
the point of asking the manifest instead of carrying a list.

> **Amended by [ADR-111](./111-a-text-literals-bytes-are-the-compilers-promise.md)
> (same day).** That is what happened, and the claim held exactly: the row is now
> `Effect::Allocates` and **not one line of `box_invariant_literal` changed**.
> `AllocKind::Char` is what this paragraph now excludes — its wrapper validates a
> Unicode scalar whose untrusted source is `Int.to_char()` at run time, so the
> validation has no single caller to move to. See §5, which needed the one edit
> this did not.

**Shareable.** One allocation now stands for every evaluation, so the object is
shared across iterations. ADR-100 §Context is the argument that this is
unobservable for a scalar box, and it is a fact about the language rather than
an assumption: there is no identity operator, `==` on `Float` lowers to
`Inst::FloatCmp` over extracted payloads, a payload is never written after
allocation, and `DynamicKey`'s pointer comparison is a fast path *for* a
structural equality that is reflexive.

NaN is the exception, and it is why ADR-100 interned `Int` and declined `Float`.
`float_equals` is IEEE, so `NaN != NaN`, and one shared NaN would compare equal
to itself as a `Map`/`Set`/`Counter` key where two separate allocations do not.
`float_literal_may_be_shared` states that rule in one place and
`lower_lit_gc` applies it before calling the hoist. Nothing in the language
spells a NaN literal today — which is a rule in the lexer, a different file, and
this is the file whose correctness depends on it.

### 4. Innermost preheader, not outermost

Every enclosing loop's preheader dominates the site, so hoisting to any of them
is correct. The choice is a cost trade and it goes the conservative way:
`loop_preheaders.last()`.

Hoisting to the outermost preheader allocates once per *call* rather than once
per entry to the innermost loop, which sounds strictly better and is not. The
hoisted value is live from its new definition to its last use, so an outermost
hoist makes every literal in a nest a live root at every safepoint of every
enclosing loop — including the hot innermost one it is not used in. The backend's
`spill_roots` writes every `roots.live()` member at every safepoint with **no
delta tracking against what the frame already holds**, so that is one store per
safepoint per iteration bought with one allocation saved per outer step. In
`mandelbrot` that is eight uninvolved roots crossing a loop with about a dozen
safepoints per iteration. The innermost preheader takes the allocation off the
per-iteration path, which is the entire win, and lengthens the live range by
exactly the loop the value is used in.

### 5. The set of hoistable literals is exactly two, and it is stated at the call sites

`Lit::Float` (non-NaN) and out-of-range `Lit::Int`. Not `Lit::Text` or
`Lit::Char`, whose wrappers validate (§3); not `Lit::Bool`, `Lit::Unit` or an
in-range `Lit::Int`, which are `Inst::ConstGc` since ADR-100 and allocate
nothing — hoisting those would trade two loads for a live root, which is the
wrong direction. The two arms call `box_invariant_literal` and the rest do not,
so the set is visible in `lower_lit_gc` rather than encoded in a predicate
somewhere else.

> **Amended by [ADR-111](./111-a-text-literals-bytes-are-the-compilers-promise.md)
> (same day): the set is three.** `Lit::Text` joined it when
> `praxis_alloc_text`'s row became `Effect::Allocates`. Stating the set at the
> call sites is what made that an edit rather than an automatic consequence —
> §3's gate admitted `Text` on its own, but the arm still had to *call* the
> hoist. That is the cost of this design and it is the right one: the set stays
> readable in `lower_lit_gc`, and adding to it is a deliberate act with the
> shareability argument made at the site. `box_invariant_literal`'s `konst` is
> now `Option<Inst>`, because a `Text` literal's payload rides inside the
> `AllocKind` rather than through a scalar local — a wider signature, not a
> second decision.

## Consequences

- **Measured, best of N interleaved runs, same machine, the two binaries built
  from the same tree with and without this change:**

  | | before | after | |
  |---|---:|---:|---:|
  | 20M × `x = x + 2.0` (best of 3) | 0.726 s | **0.512 s** | 1.42× |
  | `hashwork` @ 800,000 (best of 4) | 0.458 s | **0.374 s** | 1.22× |
  | `mandelbrot` @ 200 (best of 5) | 0.645 s | **0.624 s** | 1.03× |
  | `mandelbrot` @ 400 (best of 3) | 2.475 s | **2.390 s** | 1.04× |
  | `collatz` @ 60,000 | 0.187 s | 0.184 s | — |
  | `primes` @ 300,000 | 0.143 s | 0.142 s | — |
  | `vm` @ 400,000 | 1.033 s | 1.037 s | — |
  | `tree` @ 60 | 0.632 s | 0.638 s | — |
  | `pipeline` @ 300,000 | 1.236 s | 1.151 s | noise |

  Every benchmark's output is byte-identical before and after.

- **`hashwork` is the headline and `mandelbrot` is not, which inverts the
  handover's expectation.** `hashwork`'s generator loop allocates five
  out-of-range `Int` literals per step against a body that does little else, so
  removing them removes most of its allocation. `mandelbrot`'s innermost loop
  allocates about ten `Float` boxes per iteration from the arithmetic itself —
  `x*x`, `y*y`, each sum — and only two from literals, so the ceiling on this
  change there is under 20% of its allocations and it lands at 3–4%. The
  arithmetic temporaries are escape analysis's problem, not LICM's; handover 23
  §2 already names that as the dominant remaining cost.

- **`tree` and `pipeline` are too noisy to attribute anything to.** Observed
  spread on a single unchanged binary was 0.63–1.67 s for `tree`. Both are
  reported as unchanged rather than as small wins or losses.

- **No new locals, so `MAX_SHADOW_SLOTS` pressure is exactly unchanged.**
  `lower_lit_gc` allocates the `Gc` destination and the `Scalar` source *before*
  choosing where to emit, so hoisting picks a block and nothing else — the same
  two locals exist either way. The backend sizes a frame with
  `SlotCount::new(gc_count)` over the function's `Gc` locals, not over any
  safepoint's live set, so a function that compiled before compiles after with
  a frame of the same width. What does change is *occupancy*: a hoisted value is
  live across its loop, so it joins the root set at every safepoint in that loop
  and costs one store each. That cost is real — it is why Decision 4 chose the
  innermost preheader — and it is what the `mandelbrot` number is net of.

- **The MIR verifier needed no new rule and got none.** A hoisted `Alloc` is a
  definition in a block other than the one that uses it, which
  `an_alloc_hoisted_out_of_its_using_block_still_verifies` has recorded as legal
  since ADR-100 — MIR is deliberately non-SSA and has no def-dominates-use rule.
  Every hoisting test runs `annotate` then `verify`.

- **The pacer sees fewer bumps in a loop whose only allocation was a literal.**
  ADR-100 Decision 3 worried about this from the other side and made `int_ref`
  pace even when it answers from the table, because the pacing counter is the
  collector's only trigger. A loop whose *only* allocation was a hoisted literal
  now offers the collector no turn — but such a loop also produces no garbage,
  so there is nothing for the turn to reclaim. Any loop that computes something
  still materializes its results, and every `Materialize` paces.

- **The general pass is declined, not deferred, and the four analyses should not
  be built for this reason.** They may be worth building for something else —
  escape analysis wants a real CFG — but the literal case is closed and it was
  the whole of LICM's remaining value after ADR-100. What this cannot reach is a
  loop-invariant *expression* over loop-invariant variables (`let k = a * b`
  inside a loop where neither moves). That needs the dominator tree and an
  invariance analysis, and it needs the fault-pairing question answered, because
  `a * b` on `Int` is checked arithmetic.

- **`lower_for`'s preheader was already doing this.** It emits the iterator
  snapshot and the index materialization before the header, with the comment
  "so it is one call per loop and not one per step". The hoist is that idea
  given a name and a second customer.
