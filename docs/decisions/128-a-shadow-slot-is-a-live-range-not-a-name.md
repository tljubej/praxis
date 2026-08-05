# ADR-128: A shadow slot is a live range, not a name, and the debugger's slot is the one that keeps a name

**Date:** 2026-08-05
**Status:** proposed — not implemented
**Milestone:** post-M11 performance

**The decisions are in the order they should be built.** Decision 1 is a
one-line constant with its own A/B and no dependency on the rest; decisions 2–4
are one change; decision 5 is separable and last. That ordering is deliberate —
decisions 1 and 2 both remove the same two `memset` calls from the same
prologue, so measuring them together would leave neither attributable.

**Amends ADR-019**, whose spill assigns "one slot per `Gc` local, the local's
position among `Gc` locals". That mapping is what decision 2 replaces; the spill
it feeds is otherwise untouched.

**Amends ADR-101's account of `MAX_SHADOW_SLOTS`**, which says the constant
"caps one frame's width, which is what makes the reservation arithmetic in
`SHADOW_STACK_SLOTS` close". True, but it reads as though the cap is what bounds
the stack. It is not: the budget guard bounds every claimed slot on its own (see
Context), and the cap contributes only the headroom term for Rust-side pushes.
The practical effect of that misreading is that the cap has been treated as a
performance dial — it was one under ADR-019, when it *was* the width of every
frame, and has not been one since ADR-101.

**Amends ADR-104's index-parallelism.** The debug value stack claims the same
`SlotCount` as the shadow stack, "so a local's shadow slot index doubles as its
debug-local index and the two stacks are index-parallel for free". Decision 3
ends that: the two index spaces now answer different questions and are sized
differently. What ADR-104 built is otherwise kept whole, including
`FunctionDebugMeta`'s layout, which does not change.

**Does not touch ADR-105.** The budget charge stays on the count of `Gc` locals.
Decision 4 says why, and it is the decision most likely to be got wrong.

## Context

### The failure

A 40-line function does not compile:

```
error: JIT compilation failed: Cranelift error: function `main` has 201 Gc
locals, exceeding MAX_SHADOW_SLOTS (192)
```

The source is twenty repetitions of `var v = [1, 2, 3]` and `out(v.len())`. The
largest set of `Gc` locals that is simultaneously live at any safepoint in it is
**2**. The corpus already works around this:
[`adr127_pipeline_over_every_iterable.px`](../../tests/aoc-corpus/adr127_pipeline_over_every_iterable.px)
is split into six functions and says so in its header — "It is split into
sections because one function's `Gc` locals are bounded by `MAX_SHADOW_SLOTS`,
not because the sections mean anything on their own."

### What the width actually is, measured

Per function: `gcloc` is the frame width today; `live` is the largest
`RootSlots::live()` at any safepoint; `rootc` is what degree-ordered greedy
coloring of the co-live relation achieves; `dbgvis` is the largest
`DebugSlots::visible()`; `nameless` is the `Gc` locals carrying neither a source
name nor a span.

| program | function | gcloc | live | rootc | dbgvis | nameless |
|---|---|---:|---:|---:|---:|---:|
| `primes` | `is_prime` | 33 | **1** | 1 | 2 | 7 |
| `tree` | `build` | 45 | 5 | 5 | 14 | 5 |
| `tree` | `walk` | 25 | 5 | 5 | 8 | 7 |
| `bfs` | `open_cell` | 14 | **0** | 0 | 2 | 1 |
| `pipeline` | `__closure_0..3` | 7–12 | **0** | 0 | 1–3 | 1–5 |
| `bfs` | `<entry>` | 178 | 17 | 17 | 19 | 36 |
| `vm` | `<entry>` | 185 | 10 | 10 | 83 | 51 |
| `mandelbrot` | `<entry>` | 69 | 18 | 18 | 21 | 6 |

Over all 71 functions of `tests/aoc-corpus`: the widest frame is 110 and the
largest co-live root set is **11**, which is `REFERENCE_FRAME_SLOTS` — after
decision 2 essentially every function in the corpus is at or under the width the
budget model is calibrated against, and pays `FRAME_BYTES_BASE` and nothing
more.

**Greedy coloring reached `live` exactly in every function measured**, so nothing
here needs an optimal colorer.

### The cost, in the emitted code

`primes` exists to isolate "scalar arithmetic and call overhead"; `is_prime` is
its hot function, called once per candidate. `PRAXIS_DUMP_CLIF='is_prime'` on
this tree, `block17` — the prologue:

```
v11 = iconst.i64 264
v16 = call fn2(v10, v418, v11)   ; fn2 = %Memset — the shadow claim
v24 = call fn3(v18, v418, v11)   ; fn3 = %Memset — the debug value claim
```

Two libc calls per invocation, 264 bytes each — 33 slots of 8 — to root **one**
pointer and to keep **two** locals renderable. 33 clears `SLOT_ZERO_UNROLL_MAX`
(32) by one, which is what turns the run of stores into a call, in the prologue
[`lower.rs`](../../crates/praxis-codegen-cranelift/src/lower.rs) says exists so
that "the common prologue makes no calls at all". The same block carries
`iconst.i32 178`, which is `frame_cost(33)`.

Everything a call pays scales with the width it *declares*:

- the prologue zeroes it twice, once per stack;
- `frame_cost` charges for it, so the depth it may recurse to falls;
- `push_roots` walks it at every collection, since the scan is one linear pass
  over `[base, top)` and a null slot is skipped but still read.

### Why the cap is not the problem

`MAX_SHADOW_SLOTS` appears in exactly two places that survive to run time: the
size of the two reservations, and `SlotCount::new`, which is a compile-time
check. **It appears in no generated code.** Raising it changes the cost of no
program that compiles today; it changes only which programs compile. The
reservation is bounded without it — every frame pays `FRAME_BYTES_PER_SLOT` per
slot past `REFERENCE_FRAME_SLOTS` out of `STACK_BUDGET_BYTES`, so the claimed
slots of all live frames sum to at most `budget / FRAME_BYTES_PER_SLOT +
MAX_RECURSION_DEPTH × REFERENCE_FRAME_SLOTS` whatever the per-frame widths are.

So raising the cap is cheap and correct, and it is *not* this record's answer,
because a raised cap leaves `is_prime` doing two memsets per call. The number
that has to come down is the width.

### The premise decisions 2 and 3 rest on

**A shadow slot is write-only from generated code.** The collector is
non-moving, so nothing is ever loaded back out of a slot: the backend spills
before a safepoint and re-reads its Cranelift `Variable` afterwards. Verify this
still holds before implementing — it is the whole reason two locals may share a
slot with no reload hazard.

A second observation makes the shape of this obvious: at `opt_level = "speed"`
(the tree's setting since
[`module.rs`](../../crates/praxis-codegen-cranelift/src/module.rs)) Cranelift's
register allocator already assigns *native* stack slots by live range. The
compiler is doing this optimization one layer down, on the same locals. The
shadow stack is the last array in the pipeline handed out by name.

## Decision 1: the unroll threshold is a code-size budget, and no prologue makes a call

`SLOT_ZERO_UNROLL_MAX` is 32: at or below it `emit_zero_slots` writes a run of
stores, above it it calls `%Memset`. **That number was never measured.** It
appears in one commit (`488330f`, ADR-101's handover) and in no ADR and no
handover. Its doc comment asserts two things:

> Below it the call and its argument setup cost more than the stores; above it —
> a function with dozens of live `Gc` locals — the stores are both slower and a
> kilobyte of prologue.

The second half is arithmetic and true. The first half — that a run of stores
becomes *slower* than a call somewhere around 32 slots — has nothing behind it.
A 264-byte `memset` is a call: argument setup, a branch to a stub, libc's own
size dispatch, against 33 straight stores with no branch at all. ADR-101's own
figure is the nearest datum, and it points the other way: it priced the old
1536-byte prologue memset at **~32 ns per call**.

So the threshold is not a speed crossover. It is a **code-size budget**, and it
should be set as one. On aarch64 a store is 4 bytes of code:

| slots | inline stores | code, both stacks |
|---:|---:|---:|
| 33 (`is_prime`) | 66 | 264 B |
| 185 (`vm` `<entry>`) | 370 | 1.5 KB |
| 4096 (decision 3's cap) | 8192 | **32 KB** |

The first two are nothing; the third is why "always unroll, no threshold" is not
the answer. Praxis JITs on every run, so emitted instructions are also emission,
regalloc and encoding time on the critical path.

Three parts, then:

- **raise the threshold well above 32**, since the argument for 32 does not
  exist;
- **zero in word-sized chunks, and never call `memset`.** This run is never a
  general `memset` and cannot be one: it is always a whole number of slots,
  contiguous, starting word-aligned. A slot is one machine word on both stacks —
  a shadow slot is a `*mut GcHeader`, a debug value slot is an `Option<GcRef>` —
  which is not an assumption to add but one `emit_zero_slots` already asserts
  (`slot_bytes == GC.bytes()`, panicking rather than leaving a wider slot's tail
  untouched). Write the arithmetic in words rather than the literal 8; the
  figures here are the 64-bit targets in play, and nothing in this record should
  be the reason a 32-bit port is a rewrite.

  That is the whole argument against the call. `memset`'s cost is mostly
  *generality* — dispatch on a byte count that may be anything, fix-ups for a
  ragged head and tail, size-class branches — and every one of those cases is
  unreachable here. A run of word stores at word-aligned addresses needs none of
  it, and needs nothing arranged to be correct: the element type's alignment
  already covers the access, so `MemFlags::trusted()`'s `aligned` stays honest
  with no work.
- **above the ceiling, an inline loop rather than a call.** A loop of word
  stores: bounded code, no libc, at any width — which is what makes ADR-101's
  "the common prologue makes no calls at all" true rather than aspirational.

**What must not change is the small case.** For a four-slot frame the four
stores are already optimal, and replacing them with a loop — counter setup, a
branch per iteration — would regress nearly every function in the language.
`lower.rs` already refuses `FunctionBuilder::emit_small_memset` for exactly this
reason, its threshold being 4. Keep the unrolled run; change only what happens
above it.

**Deferred, and only if the code-size ceiling turns out to bind: wider stores.**
Two slots per instruction, or four, with a tail of at most one word store —
which moves the ceiling out by the same factor. It is listed here so it is not
re-derived, not because it is wanted now, and it is strictly more expensive to
get right than the paragraph above. A store wider than the element type is an
access the element type's alignment does not justify, and a two-word-aligned
frame base needs two independent things: an over-aligned reservation
(`SlotStack::new` is `vec![zero; n].into_boxed_slice()`, so `Layout::array::<T>`
promises one word and no more, whatever `mmap` happens to return today), and
every claim rounded to an even slot count. Neither is a hardware requirement —
aarch64 permits the unaligned access — but `MemFlags::trusted()` sets `aligned`
as an assertion, an odd-word two-word store can split a cache line, and x86's
aligned-move forms fault. Tearing is not among the risks *today*: collection is
synchronous on the mutator thread at a safepoint, and the prologue finishes
before the function reaches its first one — a premise a concurrent or
background-marking collector would silently invalidate.

This decision is first because it is one constant plus a lowering arm, it stands
alone, and it is the cheapest way to find out what the two memsets in `is_prime`
are actually worth. Rough sizing, to be replaced by a measurement: ~1.6M calls
at the benchmark size, two libc calls each; at 10 ns apiece that is ~32 ms
against a 343 ms median.

## Decision 2: a root slot index is a color, not a position

The backend assigns shadow-slot indices by coloring the interference relation
"these two `Gc` locals appear in the same safepoint's `RootSlots::live()`". A
local that appears in no live set gets **no slot at all**. The frame's width is
the number of colors used, and that is the count that becomes the `SlotCount`,
the shadow claim and the zeroed run.

Degree-ordered greedy is sufficient and is what the measurements above used. The
assignment must be **deterministic** — same MIR in, same slots out — because
`PRAXIS_DUMP_CLIF` output and the snapshot suites depend on it.

`liveness.rs` does not change. It already computes the input.

**The one subtlety.** `RootSlots::dead()` is a set of *locals* whose slots may be
stale (MIR-01). Translated naively to slots it is wrong: a dead local sharing a
slot with a live one would null the live one's value. The translation is a set
difference at the slot level, done in the backend where the map lives:

```
dead_slots = { slot(l) : l ∈ roots.dead() } \ { slot(l) : l ∈ roots.live() }
```

which is correct by construction — a slot occupied by a live root is written
with that root's value, and writing it *is* the erasure of whatever dead local
shared it.

**Make the coloring's invariant unconstructible rather than tested.** The map
should be built by a constructor that takes the live sets and refuses — or
`debug_assert`s, at minimum — a coloring in which two locals of one live set
share an index. Every consumer downstream then assumes it, in the same way
`SlotCount` is assumed. This is the property whose violation is a silent wrong
answer from the collector, so it should not be a comment.

## Decision 3: the debug value stack keeps the per-name index space, and gets its own cap

The crash debugger must render a local the program has finished with — that is
the whole content of `DebugSlots` being deliberately over-approximate — so its
slots cannot be colored by the relation in decision 2, and `FunctionDebugMeta`
resolves a slot to a name and a type *statically*, once per function.

So the two index spaces separate:

- **root slots** — colored, per decision 2, capped by `MAX_SHADOW_SLOTS`, which
  at a corpus maximum of 11 becomes a bound nothing approaches;
- **debug value slots** — dense, one per `Gc` local, in MIR local order, exactly
  as today. `build_function_debug_meta` is unchanged, and so is the ABI it
  writes.

The dense space needs its own bound, because `MAX_SHADOW_SLOTS` was it and no
longer is: introduce `MAX_DEBUG_VALUE_SLOTS`, and size it for the thing it now
limits — how many `Gc` locals a function may have, which is a property of the
source text a programmer can see. **4096** is the suggested value; `bfs` and
`vm` are already at 178 and 185, so 192 was closer to biting than anyone
noticed. The cost is address space only: one slot on each of the two
reservations, 16 bytes per unit, ~64 KB at 4096.

`DEBUG_VALUE_STACK_SLOTS` is then sized by its own headroom term rather than
inheriting `SHADOW_STACK_SLOTS`, with the same budget-derived first two terms
and the same `const` assert. The assert is not optional: it is what turns a
later "this reservation looks large, let me shrink it" into a build failure
instead of a silent overflow of a stack generated code does not bounds-check.

In `lower.rs` this is one `HashMap` becoming two: `root_slot_of` (colored, feeds
`spill_roots` and the shadow claim) and `debug_slot_of` (dense, feeds
`store_debug_local`, `store_debug_defs`, `elided_box_slots` and the debug value
claim). Nothing else in the backend needs to know there were ever one.

## Decision 4: the budget charge stays on the count of `Gc` locals

`frame_cost` keeps taking the **dense** count, not the colored one.

The temptation is obvious and wrong. `FRAME_BYTES_PER_SLOT` is not rent on a
shadow slot; it is a calibrated proxy for the *native* frame, and the guard it
feeds exists so that deep recursion faults cleanly instead of the host aborting.
Charging the colored count would under-report a function whose native frame is
large, and the failure mode of under-reporting is SIGABRT with no diagnostic —
precisely what ADR-105 was written to remove.

Two further reasons it must be the dense count:

- it is also the debug value stack's width, and that stack's reservation is
  bounded by exactly this charge (decision 3);
- at `opt_level = "speed"` the native frame tracks live ranges, so charging the
  dense count is now *conservative*. Erring high here is free; erring low is a
  crash.

Revisiting the calibration is a separate package with its own measurement. Do
not fold it into this one.

## Decision 5: only the locals the debugger cannot render are candidates for removal

The `Gc`-local count is high partly because expression temps each get one, and
those are wanted: they are what the crash debugger renders as `<tmp#N: Type> @
"expr"`. This record does not remove them.

What it does put in scope is the set carrying **neither a name nor a span** —
`alloc_gc` sites passing `debug_kind: Temp` and `debug_span: None`. The debugger
has nothing to say about these: `build_function_debug_meta` writes an empty name
and a `(0, 0)` span, so they render as `<tmp#N>` with no type column and no
provenance. There are 7 of them in `is_prime` (one per literal, by inspection),
51 in `vm`'s `<entry>`, 36 in `bfs`'s.

The gate is the debugger, not the count: a local may be dropped only if no
snapshot loses a line a user could have used.

Last, and separately, so that its effect is separable from decision 2's.

## Verification

**Correctness.**

- The existing shadow-stack suite is parameterised by the constants, not by
  literals, so it follows them. `rejects_an_oversized_frame`,
  `the_reservation_covers_every_slot_the_budget_can_buy` and
  `a_wide_frame_spends_more_budget_than_a_narrow_one` all need a debug-stack
  sibling after decision 3.
- A new test that two locals live at one safepoint never share a slot, driven
  off the constructor in decision 2.
- Decision 1 needs a test that no emitted prologue contains a call at any width
  the caps allow — the statement is only worth making if it cannot rot.
- The crash-debugger snapshot suites are the gate on decisions 3 and 5 and must
  not move at all. If any `locals` output changes, decision 3 has leaked into
  the debug index space.
- `tests/aoc-corpus` runs unchanged, and
  `adr127_pipeline_over_every_iterable.px` becomes re-mergeable into one
  function — worth doing as the record's own proof, and worth *not* doing in
  the same commit as the measurement.

**Performance.** `benchmarks/ab.py`, per handover 26 §6, and **twice, not once**:
decisions 1 and 2 remove the same two calls from `is_prime`'s prologue, so a
single sweep over both would price them jointly and attribute them to whichever
was written first.

- *Decision 1's sweep.* Arm A is this tree with `SLOT_ZERO_UNROLL_MAX` at 32 —
  one constant. `primes` is where the prediction lives; `collatz` and
  `mandelbrot` are controls, their hot code being inside a single `<entry>`
  claimed once.
- *Decision 2's sweep.* Arm A is this tree with the coloring reverted to
  positional assignment — one toggle, not the previous commit. `primes` and
  `tree` carry the prediction (`tree`'s recursion shrinks both the prologue and
  the root scan); `bfs` and `pipeline` carry the zero-slot closures; same two
  controls.

The harness's quiescence gate refuses to run while an editor's `praxis lsp` is
alive, load average included. That is not optional, and neither is stating the
prediction before measuring: this is the second time in this repo that a change
was reasoned about at the instruction level and the effect turned out to be
somewhere else (handover 25 §3).

**Reproducing the tables above.** They came from a throwaway `praxis-cli`
example that runs the front end through `annotate` and reports the columns per
function; it is not in the tree. Landing it — as an example, or as a
`PRAXIS_DUMP_SLOTS` beside `PRAXIS_DUMP_CLIF` — is recommended for the same
reason `dump.rs` gives for existing at all: every number in this record is
otherwise a measurement someone has to re-derive by hand-editing the compiler.

## What this does not change

The spill itself, the safepoint set, the liveness pass, `FunctionDebugMeta`, the
GC's scan, the collector's semantics, and the bump-allocated frame of ADR-101.
`MAX_SHADOW_SLOTS` keeps its value and its meaning; it simply stops being
reachable.

## Aside for the implementer

`lower.rs` carries about a dozen comments justifying decisions "at `opt_level =
"none"`" — that a value live across a call becomes a native stack slot, that
Cranelift will not fold a displacement, that the mid-end never runs. The tree
has been at `"speed"` since the fifth measurement recorded in `module.rs`. The
comments are historical rather than wrong-in-effect, but at least one of them
(re-reading the slot-stack header rather than carrying it across the body) is a
live design choice whose stated premise no longer holds, and it sits in the same
prologue this record edits. Re-measure it or re-word it; do not leave it as an
argument that reads as current.
