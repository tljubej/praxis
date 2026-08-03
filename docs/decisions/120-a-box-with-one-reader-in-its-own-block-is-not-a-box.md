# ADR-120: A box with one reader in its own block is not a box, and deleting it costs the debugger a value

**Date:** 2026-08-03
**Status:** accepted — implemented (part 1 of 2)
**Milestone:** post-M11 performance
([handover 25](../handovers/25-two-mallocs-per-runtime-call.md) §5 F-3/F-5,
[handover 26](../handovers/26-ten-packages-six-waves-and-the-five-things-25-got-wrong.md)
§4 W8-S0,
[handover 27](../handovers/27-the-five-gates-and-what-26-got-wrong.md) §1 and §3,
wave 2)
**Amends:** nothing. ADR-015's non-SSA slot model, ADR-044's five exhaustive
`Inst` matches, ADR-088's positional fault rule and ADR-108's refusal to build a
standalone MIR pass framework are all *constraints this fits inside* rather than
things it changes, and each is where a decision below comes from.

**This record is part 1 of 2.** Part 1 is the transform. Part 2 (W8-S0b, wave 4)
is the scalar debug slot that repairs the debugger regression decision 6 records,
and it appends to this file rather than opening a new number: decisions 1–7 are
about the transform and are complete, and the slot decisions are 8 onward and are
not written yet. **Two tests are red on this branch and must stay red until part
2 lands** — see decision 6.

## Context

`crates/praxis-mir/src/build.rs` gives every expression one return convention: a
`Gc` local. It gives every arithmetic operator one operand convention: a
`Scalar`. Those two conventions meet at every interior node of every expression
tree, and what they emit there is a box immediately followed by an unbox:

```text
IntBinOp   dst=s1  lhs=… rhs=…      ; i * 3
CheckFault
Materialize dst=t1 src=s1 Int       ; praxis_alloc_int(s1)  — a GC safepoint
ExtractScalar dst=s2 src=t1 Int     ; praxis_int_load(t1)
IntBinOp   dst=s3  lhs=acc rhs=s2   ; acc + that
```

`s1` and `s2` hold the same word. The two instructions between them exist only
because the builder's two conventions disagree, and both are expensive: the
`Materialize` is a call, a shadow-frame spill of every live root, a pacing test
and a table read (ADR-113), and the `ExtractScalar` is a descriptor proof plus a
load (ADR-102).

Handover 25 found the same shape twice without seeing it was one shape. §5 F-3
described a comparison boxing a `Bool` and then unboxing it to branch — "15 CLIF
instructions to produce a condition that `icmp; brif` already had" — and
proposed a peephole in the *Cranelift lowering*. §5 F-5 described `mandelbrot`'s
ten float temporaries per iteration and priced escape analysis at "a week+, and
it needs the CFG work ADR-108 declined". Handover 26 §1 correction 2 noticed the
two were the same transform and deleted the first (W5) in favour of the general
one.

### Three things had to be settled before this could be built, and all three were wrong on paper

**1. "MIR has no scalar-to-scalar move."** Handover 26 §3 made wave 2's width
turn on this, on the grounds that adding a `MoveScalar` variant would edit
`lower_inst` in the file W6 and W7 were both rewriting. The premise is false —
`build.rs`'s `fn move_scalar` says in its own doc that "there is no scalar-move
`Inst`, so the idiom is `dst = src + 0`" — and handover 27 §3 pointed out that
neither horn of the dilemma is the answer anyway. Decision 1.

**2. "It lands `run.rs:752` red on purpose, and that is its measurement
signal."** Handover 26 says this four times, in §4, in §7 trap 4, in the wave-4
note and in §8's merge rule. It is false, and handover 27 §1 traced the five-link
chain that makes it false. Decision 6, which is the largest part of this record.

**3. "`mandelbrot`'s inner loop goes from 10 `Materialize{Float}` to 2."**
Handover 26 asserted it; handover 27 §9 listed it as hand-walked and unverified
and made measuring it a precondition of writing this Decision. It is measured
below and it is exactly right — which is worth saying plainly, because most of
what the two handovers disagreed about was not.

## Decision 1: the mechanism is block-local *use-rewriting*, and nothing is copied

`crates/praxis-mir/src/forward.rs` rewrites the **consuming instruction's operand
field** from the extracted local to the scalar the producer boxed, then deletes
the `ExtractScalar`, then deletes the producer if it became dead. In the sample
above, `IntBinOp`'s `rhs` becomes `s1`, and the two instructions in the middle
go.

Nothing is copied. No local leaves `Function::locals`. No `Inst` variant is
added. `crates/praxis-codegen-cranelift/` is not edited at all, which is what let
this package sit beside W6 and W7 in one wave over one file they both rewrite.

**Why not whole-function `LocalId` substitution.** MIR is deliberately not SSA
(`ir.rs`'s header says so, and repeats it at `Inst`), and this is load-bearing
rather than aspirational: `Assign` lowers to a `MoveGc` **into the binding's
existing slot**, both arms of a `lower_if` write one `dst`, and `emit_increment`
redefines the loop cursor in place. So a `LocalId` does not name one value, and
substitution is only valid under a dominance-and-reaching-definitions question —
which is exactly the four analyses ADR-108 declined to build. It happens to be
true at this tree that every `ExtractScalar` destination is freshly allocated on
the line above, across all 20 builder sites; that is a property of today's
builder, not an invariant, and both W11 and W8-S1 propose to change builder
shapes.

**Why not a `MoveScalar` variant.** Five arms, one of them `lower_inst`.

## Decision 2: it runs on the last line of `lower_module`, and that is the whole of the plumbing

It must run **before** `crate::annotate`, because it deletes GC safepoints and
`RootSlots`/`DebugSlots` are computed per safepoint. All five hosts —
`run::run`, the debugger's reload, `p EXPR`'s synthesizer, the backend's own
integration tests, `test_support` — do `lower_module → annotate → verify`
separately, so the last line of `lower_module` is the one place that ordering
holds with **no host edited**. That is ADR-108 §1's stated reason for refusing a
standalone pass, honoured rather than worked around.

It is safe by construction rather than by convention. Every builder site writes
`RootSlots::unannotated()`, and `RootSlots::set` is `pub(crate)` to
`liveness::annotate` alone. So this pass deletes safepoints whose slot sets hold
no answer yet. **It cannot invalidate an answer, because none exists.**

Placing it inside `lower_module` also means the closures and function-value
adapters `lower_module` appends are covered, which a host-side call before
`annotate` would also have got but a call inside `lower_fn` would not.

## Decision 3: five gates, and each one is answered by a program

`forward::consider` is the whole of the safety argument, and every `None` in it
is a gate. For an `ExtractScalar { dst: e, src: b, scalar: k }` at index `j` of
block `B`:

1. **`e` is defined exactly once, function-wide.** Without it, deleting this
   instruction leaves `e`'s *other* definitions reaching uses that were rewritten
   against this one.
2. **Every use of `e` is in `B`, and after `j`.** "In `B`" is what makes the pass
   block-local: MIR is not SSA, so a use in a successor may be reached by an edge
   on which `B` never ran. "After `j`" is *not* implied by "in `B`" — in a loop
   body, a use at a lower index reads the previous iteration's value.
   `a_box_read_from_two_blocks_keeps_both_reloads` is the first half;
   `a_hoisted_literal_is_not_forwarded_across_the_block_boundary` is the second.
3. **The nearest preceding definition of `b` is in `B`.** Nearest, so no
   redefinition of `b` can sit between it and the reload — which is what makes
   the payload being extracted *this* producer's.
4. **The producer is `Materialize`, `Alloc{Int|Bool|Float}` or `ConstGc`; its
   payload kind equals `k`; and it cannot fault.**
5. **The forwarded scalar is not redefined between the producer and the last
   rewritten use.**

Two of those five deserve their own sentences.

**Kind equality does work beyond the obvious.** `verify::operands` checks operand
*range* only, so a latent `Materialize{Bool}` / `ExtractScalar{Int}` pun verifies
today. Forwarding across one would turn a punned reload into a silent value
substitution — the pun would stop being observable at the same time it stopped
being harmless.

**`can_fault` is the exact boundary and not a conservatism.** It admits `Int`,
`Bool` and `Float` and excludes `Char`, because `praxis_alloc_char` validates its
Unicode scalar, its manifest row is `AllocatesAndFaults`, and ADR-088 puts a
`CheckFault` immediately after it. Deleting that producer would orphan the check
and `verify::check_fault_observed` would refuse the function — loudly, which is
the good outcome, but the gate means it never arises.
`a_char_box_is_not_forwarded_because_its_check_fault_would_be_orphaned` is
hand-built MIR rather than a source program, because ADR-107 gives the language
no char-literal syntax and `Lit::Char` is synthesized by the input parser alone.
A gate whose only witness is a program nobody can write is still a gate the pass
has to hold.

**Nothing here gates on safepoints between the producer and the consumer, and
that is deliberate.** Forwarding lengthens a `Scalar` local's live range, and
`ir.rs` says a scalar "must not survive a GC safepoint". The rule it is stating
is *"no raw word in a slot the collector reads"*, which `liveness::annotate`
enforces structurally — `roots.set` takes `live ∩ gc_locals`, so a scalar cannot
enter a root set — and which `verify.rs`'s header already records as the reason
`ScalarLiveAcrossSafepoint` is not implemented and will not be. A scalar is a
*copy* of a payload; it cannot dangle when the object it came from is collected.

## Decision 4: `ConstGc` is a third producer and a different transform

`ConstGc` boxes nothing: the value is an immediate in the instruction, and the
backend lowers it to two loads out of `RuntimeContext` (ADR-100's interned table,
or the `Bool`/`Unit` singletons). So there is no scalar to forward. The reload is
**replaced in place** by `Inst::ConstInt` carrying the same value, one for one,
and no operand moves at all; the `ConstGc` then goes if nothing else reads its
box.

Two loads and a descriptor proof become one `iconst`. `Bool(v)` becomes
`ConstInt { value: v as i64 }`, which is a shape the builder already emits —
`lower_logical_not` fills a `Scalar(Bool)` local with `ConstInt { value: 0 }` and
feeds it to an `IntCmp`.

This case fires less than it looks like it should, and the reason is ADR-108:
a loop-invariant literal's `ConstGc` is hoisted to the preheader, so the reload
in the body is a different block and gate 2 declines it. That is correct — the
hoist already removed the per-iteration cost the forward would have removed —
and `a_hoisted_literal_is_not_forwarded_across_the_block_boundary` pins it as
intended behaviour rather than a miss.

## Decision 5: rewriting is always safe; only *deleting* needs a licence

This is the design point, and it is what makes the two operand-rewriting helpers
safe to leave non-exhaustive.

Under the gates, the forwarded scalar holds — at every use of `e` — the exact
word `e` was loaded from. So rewriting a use of `e` to name the scalar cannot
change what the program computes. A rewrite helper that misses an operand field
has produced a *smaller* optimization, never a wrong one.

What needs a licence is deleting `e`'s definition, and the licence is a re-run of
the read-only `liveness::uses`/`term_uses` over the whole function: the
`ExtractScalar` is removed only when no use of `e` survives. **That converts a
non-exhaustive match from a correctness hazard into a missed-optimization
hazard**, and it is what leaves ADR-044's count of exhaustive `Inst` matches at
five rather than six. A site whose rewrite was not exhaustive is recorded in a
`wedged` set so planning does not find it forever; the pass then terminates
because every round either removes an instruction or wedges a local.

**This differs from handover 27 §3's phrasing, and the difference is a
simplification.** §3 said "delete the `ExtractScalar`, re-run `uses`, and if any
use of `e` survives, put it back". Asking *first* is the same guard with no undo
path — and an undo path is the thing that would have needed its own correctness
argument, because reversing an operand rewrite (`s → e`) would clobber a use of
`s` that was there all along.

`liveness::uses` and `liveness::term_uses` become `pub(crate)` for this. They
were bare `fn`s private to their module; handover 26 put `liveness.rs` in
W8-S0b's file list and handover 27 §3 corrected it into this package's.

**The terminator rewrite is mandatory, not an extra.** `lower_while` emits
`IntCmp` → `Materialize{Bool}` → `ExtractScalar{Bool}` → `Terminator::Branch` in
one block, and the boxed `Bool` is consumed *only* by the terminator. A pass that
walked `insts` alone would forward nothing in the single most common shape in the
language, pass every other test in `forward.rs`, and report a smaller win. It is
worth 8 of `vm`'s 8 `Materialize{Bool}` per iteration on its own.

## Decision 6: the debugger loses a value, two tests say so, and both are red on purpose

**Handover 26 said four times that this package lands
`crates/praxis-cli/tests/run.rs:752` red and called that its measurement signal.
It does not go red.** Handover 27 §1 read the chain; every link checks out at
this tree:

| step | what it does |
|---|---|
| the fixture is `a + b + c + 9223372036854775807` | so `a + b`'s box is an interior node this pass deletes |
| the debug store is driven by `praxis_mir::defs` (ADR-104) | a deleted producer defines nothing, so the slot is never written |
| `render.rs:178` keeps an uninit temp **that has a span** | rationale at `render.rs:164-172`: the faulting expression's own temp is what the user needs to see |
| `build_function_debug_meta` emits a `DebugLocalMeta` for every `Gc` local | so the span survives its definition's deletion |
| `run.rs`'s assertions are **provenance strings only** | `out.contains("@ \"a + b\"")` — never a value |

So the temp silently degrades and every test stays green. Measured, both arms,
the same fixture:

```text
arm A                                        arm B
<tmp#7:  Int> @ "a + b"              = 30    = <uninit>
<tmp#8:  Int> @ "a + b + c"          = 60    = <uninit>
<tmp#9:  Int> @ "922337203685477..." = 9223…  = <uninit>
<tmp#10: Int> @ "a + b + c + 922…"   = <uninit>  (unchanged: it faulted)
```

**The regression is wider than handover 27 predicted**, and this is the first
place that is recorded: three of the fixture's seven temps degrade, not one. The
third is the out-of-range `Int` literal, whose `Alloc{Int}` producer is
forwarded under decision 3's producer set. `@ "10"`, `@ "20"` and `@ "30"` still
render because a small-int literal's box is also `MoveGc`'d into the user binding
it initializes, so it has a second reader and gate 2 declines it.

Two things follow, and both are obligations rather than notes.

**A. This package added the assertion that goes red.**
`run.rs::a_forwarded_binop_temp_still_renders_the_value_it_materialized` asserts
`@ "a + b" = 30` — a value, not a provenance string — and it was added *before*
the pass landed. Without it, part 2 has nothing to turn green, and the fidelity
regression ships silently if wave 4 slips. It must not be relaxed, deleted, or
"fixed" by editing what it expects: the whole of its value is that a shipped §9
debugger guarantee cannot be narrowed without a test saying so.

**B. A second test was already there, and finding it is worth more than the
guess it replaces.** Handover 27 §9 asked "whether any test outside `run.rs`
asserts a debugger temp's value that becomes `<uninit>`" and called such a test a
*good* failure — the signal §1 says is missing. It exists:
`crates/praxis-codegen-cranelift/tests/jit.rs::a_temp_that_never_reached_a_shadow_slot_is_still_renderable`,
which runs the same fixture shape and asserts the crash snapshot carries a temp
whose value is `30`. It is the same regression one layer down, at the API rather
than the rendered text. Its own comment says it exists because it is "the loss
class a reconstruction-from-the-shadow-stack design cannot recover, and the
reason ADR-104 rejects one outright" — which is precisely the guarantee this
package narrows.

**The hand-off to part 2** is `forward::carry_debug_metadata`: when a producer is
deleted, its `debug_names`, `debug_kinds` and `debug_spans` entries are copied
onto the scalar that replaced it. Nothing renders them today, because a `Scalar`
local has no debug slot — that is what part 2 adds — but this pass is the last
point in the compiler that knows the two are one value, so the copy has to happen
here or not at all. Written once per scalar, first writer wins, so a chain of
forwards leaves the innermost expression's span rather than the outermost's.

**`just ci` is therefore not green on this branch**, by design, and the two
failures are exactly the two named above.

## Decision 7: the elided box's local stays in the table

`Function::locals` is indexed by `LocalId` with three parallel `Vec`s, and the
backend's `build_function_debug_meta` assigns each `Gc` local its `symbol_id` by
*position among `Gc` locals*. Removing an entry renumbers every `<tmp#N>` the
crash debugger prints, for every function, in exchange for nothing the runtime
needs. Nothing requires density.

The cost is one shadow slot and one debug slot per elided temp, zeroed by the
prologue's memset. That is a per-*call* cost against a per-*iteration* win, and
it is the right side of the trade for every program in the suite; a leaf function
called in a loop would be the shape that inverts it, and none exists here. If
part 2 or a later package wants the slots back, compaction is a renumbering pass
over `locals` plus `debug_*`, and it is a separate change with its own debugger
consequence.

The Cranelift side of this was handover 27 §9's first open question — *"does
cranelift-frontend tolerate a `declare_var`'d `Variable` that is never
`def_var`'d and never `use_var`'d?"* — and the answer is **yes**. `lower.rs`
declares one `Variable` per MIR local unconditionally; the three consumers that
could read one are `spill_roots` (driven by `RootSlots`, which never holds an
undefined local because liveness never reports it live), `store_debug_defs`
(driven by `praxis_mir::defs`, and a deleted producer defines nothing) and
`lower_inst`'s operand reads (there are none). 482 backend integration tests and
all eight benchmarks run correctly on arm B.

## Measurement

**Deterministic first, and it is the headline.** Handover 26 §6 says to report
the instruction count as the result and to say plainly when the clock cannot
resolve the difference. This is a build-phase package with two agents compiling
beside it, so **nothing here was timed** and no wall-clock number is claimed.

### The machine instructions, on handover 25 §3's loop

The program is the one every instruction count in the plan is quoted against:

```praxis
var i = 0
var acc = 0
let limit = 10
while i < limit {
    acc = acc + i * 3
    i = i + 1
}
out(acc)
```

`PRAXIS_DUMP_CLIF='<entry>'` and `PRAXIS_DUMP_VCODE='<entry>'` through
`praxis run`, per-iteration counts read by `dump.rs`'s own rule (the one
multi-block SCC; its first-emitted member is the header; at each branch take the
successor that is inside the component and is not cold):

| | arm A (whole fn) | arm B (whole fn) | arm A (per iteration) | arm B (per iteration) |
|---|---:|---:|---:|---:|
| CLIF | 311 in 55 blocks | **240 in 39** | 171 over 35 blocks | **111 over 24** |
| vcode | 458 in 67 blocks | **344 in 49** | 215 over 38 blocks | **133 over 27** |
| machine code | 1960 bytes | **1472 bytes** | — | — |

**Arm A reproduces `dump.rs`'s recorded baseline exactly** — 311/171/215 — which
is what makes arm B's numbers comparable to every other package's in this round.

**−82 aarch64 instructions per iteration, −38.1%.** Handover 25 §3 opened with
"four useful instructions in 216"; this loop is now four in 133. The whole
function is 114 machine instructions and 488 bytes smaller.

### The MIR census, over all eight benchmarks

Wave 0's `praxis_mir::test_support` census, per hot loop, arm A → arm B. The
loop is named by a snippet of its body (`Lowered::innermost_loop_over`), and
`tree`'s walk is a recursion rather than a loop so its whole function is counted.

| benchmark | region | `Materialize` A → B | `ExtractScalar` A → B | region total |
|---|---|---|---|---:|
| `collatz` | `3 * c` | Int 5→3, Bool 2→0 | Int 14→5, Bool 2→0 | 50 → **35** |
| `primes` | `d * d` (in `is_prime`) | Int 3→1, Bool 2→0 | Int 10→6, Bool 2→0 | 33 → **23** |
| `mandelbrot` | `x * x - y * y + x0` | **Float 10→2**, Int 1→1, Bool 2→1 | Float 22→14, Int 4→3, Bool 2→1 | 64 → **45** |
| `pipeline` | `i * 7919` | Int 4→2, Bool 1→0 | Int 10→6, Bool 1→0 | 31 → **23** |
| `hashwork` | `state * 1103515245` | Int 8→6, Bool 1→0 | Int 18→12, Bool 1→0 | 57 → **47** |
| `tree` | `fn walk` (whole) | Int 6→2 | Int 14→6 | 56 → **40** |
| `vm` | `while running` | Int 5→5, **Bool 8→0** | Int 26→17, Bool 9→1 | 211 → **186** |
| `bfs` | `dist_total + dist` | Int 5→4, Bool 2→0 | Int 11→8, Bool 3→1 | 57 → **49** |

Whole-module instruction totals, arm A → arm B: `collatz` 95→75, `primes`
124→100, `mandelbrot` 166→133, `pipeline` 246→200, `hashwork` 172→154, `tree`
210→162, `vm` 350→325, `bfs` 458→361.

Four things to read out of that.

**Handover 26's `mandelbrot` prediction was exactly right.** 10 float boxes to 2,
and the 22 reloads to 14. Handover 27 §9 was right to send it back for
measurement and wrong to doubt it. The two survivors are `x` and `y`, which are
loop-carried *assignments* — a `MoveGc` into a binding's existing slot — and are
therefore not this transform's shape at all. They are W8-S1's.

**Handover 26 was wrong to frame the package as a float transform, and handover
27 §3 was right to say so.** `collatz` allocates no float, and its inner loop
loses four boxes and eleven reloads. `tree`'s `walk` loses two thirds of its
boxes. The `Bool` half is the largest single line in the table: `vm` goes from 8
`Materialize{Bool}` per interpreter step to 0, which is handover 25 §5 F-3's
finding paid in full, generally, with no backend peephole.

**`vm`'s `Materialize{Int}` does not move**, and that is the shape of what this
pass cannot do: every `Int` box in that interpreter loop is a call argument
(`stack.push_back(k)`) or a loop-carried assignment. Handover 27 §4 said to
measure W10's residual on `vm` for exactly this reason, and this table is the
evidence that it was the right benchmark to name.

**The `ExtractScalar` reduction is larger than the `Materialize` reduction
everywhere.** That is the `ConstGc` case of decision 4: a reload of a small-int
literal becomes an immediate even where its box survives for another reader.

### Correctness

All eight benchmarks, at the frozen `sizes.json` sizes, one run per arm, stdout
compared byte for byte: **identical on all eight**, same exit code. That is the
"run a benchmark once, untimed, to check bytes" that handover 26 §6 allows, and
it is a correctness check rather than a measurement.

`cargo test --workspace`: **2005 pass, 2 fail**, and the two are decision 6's.
Every other test in the tree — including all of `build.rs`'s own MIR-shape
assertions — is green.

### Four tests changed, and each change is the pass working

Handover 27 §3 warned that `build.rs`'s ~23 `Materialize` assertions "will change
legitimately". **None of them did**, and the reason is worth recording so nobody
budgets for it again: they are `.any(…)` existence checks over a whole function,
and the *outermost* box of an expression always survives — it is what the
statement's result is bound to or returned as. What changed instead was four
counting assertions:

- `test_support::mandelbrots_inner_loop_materializes_ten_floats_per_iteration` →
  `…_materializes_two_floats_where_the_builder_wrote_ten`. The per-block split it
  also asserts moves from `(3, 7)` to `(0, 2)`: the escape test's three boxes
  were all interior nodes, and the body keeps only `x` and `y`.
- `test_support::…_extracts_twenty_two_float_payloads…` → `…_extracts_fourteen…`.
- `mir_shape.rs::a_float_temporary_reaches_the_backend_as_a_materialize_of_a_float`,
  2 → 1. Its subject is "what the backend is handed", so the post-pass count is
  the honest one for it to assert.
- `test_support::a_census_tells_a_float_materialize_from_a_bool_one` — the
  *program* moved, not the number. It was `a + b < b`, whose float box is an
  interior node and whose `Bool` box is a terminator operand, so after this pass
  there was nothing left for a census to tell apart. It is now a call argument
  and a `let`-bound comparison, which are the two shapes that really do keep
  their boxes.

## The measurement arms

`praxis-mir`'s `adr120-arm-a` feature makes `forward_boxes` return `0` without
looking at the function. Everything else — the tests, the debug hand-off, the
census helpers — compiles unchanged, so the two binaries differ in exactly this
transform. The baseline is *this branch with this package's one toggle reverted*,
not an earlier commit; ADR-113 records what the wrong baseline costs (14.4% where
the truth was 0.8%).

```bash
cargo build --release -p praxis-cli                                # arm B
cargo build --release -p praxis-cli --features praxis-mir/adr120-arm-a   # arm A
```

| arm | path | sha256 |
|---|---|---|
| A (toggle reverted) | `/tmp/praxis-arms/W8-S0-a` | `877281ea45a19d2a467bf7f5fcf8533d3d13bfbb21c2097d65ff8cb256f57e02` |
| B (this branch) | `/tmp/praxis-arms/W8-S0-b` | `a6bd3c8a17433855243467803fd799f92e2fe608b6793a8afa37f7f91b422e44` |

`forward::tests::the_measurement_toggle_decides_whether_the_pass_runs` asserts
both arms in one test — one box in arm B, two in arm A — so a toggle that stops
toggling is a test failure rather than a silently identical pair of binaries.

## Consequences

- **`praxis-mir` has an optimization pass, and `lower_module` is no longer a
  pure lowering.** That is a change to what the crate is, and the module doc in
  `lib.rs` says so. The seam it establishes for the next pass is
  `forward_boxes(&mut Function)` called from the end of `lower_module` — not a
  pass manager, and ADR-108's argument against building one is undisturbed.
- **The MIR the debugger's reload compiles is the optimized MIR**, because
  `lower_module` is what it calls. That is why decision 6's regression is
  visible in the crash REPL and not only in a crash snapshot.
- **`liveness::uses` and `term_uses` are `pub(crate)`.** They were private
  `fn`s. Both doc comments now say what reads them and why, because the second
  reader is what makes their exhaustiveness matter to something other than
  liveness.
- **A gate this pass declines is a measurable quantity, and three of them are
  named work.** Loop-carried assignments (`mandelbrot`'s `x`/`y`, `vm`'s
  registers) are W8-S1. Call results — `bs.contains(x)` returning a boxed `Bool`
  — are handover 27 §6's `Inst::BitsetContains` reshaping, which lands in W4b and
  is explicitly designed so that *this* pass removes the resulting pair. Boxes
  hoisted to a preheader are ADR-108 having already won.
- **W10's attributed share shrinks and its reach does not.** Handover 27 §4 fixed
  the schedule at W8-S0 first, for measurement honesty rather than engineering
  need: this pass reduces the *count* of allocations and W10 the *cost* of each,
  so whichever runs first absorbs the other's credit. The numbers above are what
  wave 3's gate should net against, and the `Materialize{Int}` column is the
  relevant one — it barely moves on `vm`, `hashwork` or `tree`.
- **Compile time is not measurably affected**, though the pass is quadratic by
  construction: it re-censuses the function after each rewrite, which is O(n) per
  rewrite over an n-instruction function. `bfs`'s entry is 423 instructions and
  takes ~60 rewrites. Written for a correctness argument a reader can check
  rather than for speed; if a program ever makes it matter, an incremental census
  is a local change with no consequence for any decision above.

## Open questions

- **Should gate 5's window be exact?** It refuses a redefinition of the forwarded
  scalar *at* the last rewritten use, where a defs-after-uses reading would allow
  one. The shape it costs (`s = f(s)` between a box and the last reload of that
  box) does not occur in anything `build.rs` emits, and "no definition in the
  window" is a sentence a reader can check. Re-open only with a program that
  loses a forward to it.
- **Should the pass forward *into* a successor block when the box's block
  dominates it and neither redefines anything?** That is the smallest useful
  extension and it is also the first one that needs a dominator tree, which is
  the line ADR-108 drew. `test_support` already builds dominator sets for its
  loop finder, so the analysis exists in the tree — on the test side of the
  feature wall, which is where it should stay until a package needs it in
  production.
- **Does the elided box's slot cost anything worth reclaiming?** Decision 7 keeps
  it for the debugger's numbering. `mandelbrot`'s entry now has **18 `Gc` locals
  of 69 that are never defined** (`collatz` 14 of 45, `vm` 17 of 185), each
  costing one zeroing store in a prologue that runs once. It becomes interesting
  only if a later package elides boxes in a small leaf function called in a loop
  — and `an_elided_box_keeps_its_slot_and_the_count_is_a_fifth_of_them` is where
  the number is, so a change in it is visible rather than inferred.
- **Is `wedged` ever non-empty in practice?** It cannot be today: decision 5's
  rewrite helpers cover every field of every `Inst` that can name a `Scalar`
  local, and the terminator. The set exists so that the *next* variant is a
  missed optimization rather than an infinite loop, and its emptiness is not
  asserted anywhere — asserting it would turn the safety net back into the
  exhaustive match it exists to avoid.
