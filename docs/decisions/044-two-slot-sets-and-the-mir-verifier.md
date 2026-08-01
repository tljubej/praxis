# ADR-044: The collector's root set and the debugger's view are two sets, and a verifier keeps the first one honest

**Date:** 2026-07-29
**Status:** Accepted
**Milestone:** Repair (foundations F17, F18, stage S9 — MIR-16, MIR-02, MIR-01, MIR-09, MIR-10)
**Amends:** ADR-016 (liveness is one backward walk, and its output is a sealed
type); ADR-019 (a safepoint also *clears*); ADR-021 and ADR-035 (the debug
frame is driven by its own set); ADR-033 (`DebugLocal.value` is an
`Option<GcRef>`)

## Context

One `live_roots: Vec<LocalId>` per safepoint served two consumers, and one
`emit_spill` wrote it into both the shadow frame the collector walks and the
`DebugFrame` the crash REPL reads.

That coupling made a whole class of bugs invisible to each other.

**The root set was not exact, and could not be.** `liveness::annotate` ran a
correct backward fixpoint to get each block's live-in, then threw the result
away and walked the block *forward*, inserting definitions and never removing
anything (`apply_forward` had a comment saying so). Inside a block the root set
could only grow, so a value stayed rooted long past its last use (MIR-02).

**The frame was never cleared.** A slot written at one safepoint kept its value
until something overwrote it. The collector reads every slot below
`slot_count`, so a dead local's object stayed reachable for the rest of the
call (MIR-01). After RT-01 made swept storage reusable, that stopped being
merely a retention bug: a stale slot can name a *live object of a different
type*.

**And fixing either would have broken the debugger.** The reason `locals`
rendered `a` after its last use is precisely that the root set over-approximated.
Making it exact would have turned rendered values into `<uninit>`, and the two
tests that check this (`m11_locals_split_users_and_temps_with_types`,
`m11_temp_provenance_shows_materializing_expression`) would have gone red for a
reason that looks like a debugger regression and is actually a GC fix landing in
the wrong order. This is hazard H3, and it is why the split had to come first.

Two more things in the same shape. `DebugLocal.value` was a `GcRef` whose
"nothing here yet" value was `NonNull::dangling()` — an invalid `GcRef`
constructed in Rust, compared by pointer identity in three separate
`is_real_ref` copies. And `reduce`/`min_by`/`max_by` handed back an accumulator
that, on an empty sequence, no instruction had ever written (MIR-09).

## Decision 1: `RootSlots` and `DebugSlots` are different types with different contracts

`praxis_mir::annot` holds both.

- **`RootSlots`** is the GC root set: exact, sound, and what the shadow frame
  gets. It carries `live` (spill these) and `dead` (null these).
- **`DebugSlots`** is what the crash debugger must be able to render:
  deliberately over-approximate — `live_in(block) ∪ {defs seen so far}`, which
  is exactly the old forward walk, kept as-is and now named.

Neither has a public constructor that takes ids. A builder writes
`unannotated()`; `liveness::annotate` is the only thing that can fill one. That
seal removed 61 hand-written root lists from `build.rs` — every one overwritten
by the pass, several disagreeing with it, none able to be wrong in a way
anything noticed — and, with them, the `loop_roots` list the pipeline lowerings
threaded through fifteen helpers.

`Inst::CheckFault` carries **only** a `DebugSlots`. It allocates nothing, so it
roots nothing; it is where a fault diverts, so it is where a snapshot needs
current values. The old shape gave it a `live_roots` field and a comment
explaining that the field did not mean what its name said.

### Why the debug set stays over-approximate

It would be easy to read "exact" as better here too. It is not. `locals` after
`let a = 10` must show `a` whether or not anything reads it again — the user is
asking what the program's state *is*, not what the optimizer still needs. The
two sets answer different questions, and the only reason they were ever one set
is that nobody had asked the second question separately.

## Decision 2: a safepoint clears as well as spills

`RootSlots::dead` comes from a forward *may* dataflow over "which slots might
still hold a value": the frame starts all-null, only a safepoint writes it, and
after a safepoint the dirty set *is* the root set. So the clear list at each
safepoint is `dirty \ live`, which is minimal — the alternative, nulling every
non-root slot at every safepoint, costs `gc_count` stores per safepoint for no
extra safety.

The end-to-end gate is
`a_dead_local_stops_being_reachable_from_its_frame`: two programs that differ
only in whether a three-thousand-element `Vec` is read after the loop that
fills it. Without the clears both heaps come out the same size.

## Decision 3: `DebugLocal.value` is an `Option<GcRef>`

`GcRef` is `#[repr(transparent)]` over `NonNull`, so `None` is the all-zero
niche and the field is still one machine word at the same offset. Generated
code stores a raw pointer and gets `Some`; the zeroed slot a fresh frame starts
with *is* `None`. `is_real_ref` and `null_sentinel_ref` are deleted.

This costs nothing at runtime and makes the invalid state unrepresentable in
Rust — which matters, because constructing `GcRef(NonNull::dangling())` was UB
regardless of whether anything dereferenced it.

The ABI version goes 11 → 12 even though the layout is unchanged. The *meaning*
of the word changed: generated code now writes zero into a slot whose local has
died, and a runtime of the previous version reads that zero as a reference.

## Decision 4: an empty seeded sink faults

`reduce`, `min_by` and `max_by` seed from the first element. Given none, the
answer is `FaultKind::EmptyCollection` — the same kind `Deque.pop_front` and
heap `pop`/`peek` already raise — delivered through the ordinary fault check, so
control leaves by the function's fault epilogue and the caller gets the Unit
sentinel.

The accumulator is also initialized to Unit rather than left unwritten. That is
not belt-and-braces: the slot is a `Gc` local, so liveness roots it at the loop
header and the backend spilled an *undefined Cranelift value* into the shadow
frame for the collector to dereference, on every empty `reduce`, whether or not
anyone read the result.

`praxis_raise_empty_collection` returns the Unit sentinel rather than `Void` so
the MIR `Call` emitting it has an ordinary `Gc` destination. A `Void` row would
have put the context pointer into a rootable slot, which is the bug this stage
exists to remove.

`min`/`max` are untouched. They share the empty case but not the defect — their
accumulator starts at `0`, so empty yields a defined answer — and whether `0` is
the *right* answer is D1's question about absence in the source language.

## Decision 5: `verify` is a pass, and it runs everywhere

`praxis_mir::verify` runs after `annotate` at every host: the CLI, the
debugger's `reload` and `p EXPR`, and both codegen test harnesses (which are
the only host that runs the whole corpus). A failure is a compiler bug, so it
names function, block and instruction, and no code is generated.

Checked: slot-set members are `Gc` locals that exist; operands are in range;
`MoveGc` is `Gc → Gc`; a safepoint's root set was annotated; live and dead are
disjoint; branch targets exist; a `Return` yields a `Gc` local; a branch
condition is not one; an `Overflow::Bounded` site is not a division.

Two rules are deliberately absent, and saying which is part of the decision.

**`ScalarLiveAcrossSafepoint` is not implemented.** F17 predicted it would fire
on the eager `lower_seq_*` accumulators, and it does — a `sum`'s running `i64`
is live across every `praxis_vec_get` in the loop by construction. It is also
harmless: a scalar is a *copy* of a payload, so it cannot dangle when the object
it came from is collected. The invariant that matters is "no raw word in a slot
the collector reads", and `RootIsNotGc` plus `MoveGcFromScalar` state that
directly. Turning the weaker rule on would mean either an allocation per
iteration or weakening it until it says nothing.

**`OpaqueAtDescriptorSite` stays off until S15** (hazard H10). Pipeline
accumulators and fused-loop items have no correct type until HIR carries
inferred per-use types; `MirType::Opaque` is the honest answer today and
rejecting it would refuse to compile working programs.
`MirType::expect_known`/`MirTypeError` land with the rule that needs them.

**`MissingTerminator` is unrepresentable** — `Block.term` is not an `Option` —
so there is no rule, rather than a heuristic that guesses at placeholder
self-jumps.

## Decision 6: arithmetic that cannot overflow says so

`Inst::IntBinOp` carries `overflow: Overflow`. `Checked` is source-level
arithmetic. `Bounded` is a claim about the *site*: the `for`-loop index bump,
the pipeline index bump, the `count` accumulator and the `+ 0` scalar copy are
all bounded above by a collection's length, so `i64::MAX` is not a state the
program can reach. The backend emits bare arithmetic for those — dropping an
overflow predicate and a call per iteration.

This answers the question P0-08 left open. Those sites are not followed by a
`CheckFault`, so a rule "a faulting instruction is observed" would have flagged
them; the plan's own guess was "mark the increment non-faulting rather than add
a check", and that is what this is.

`Bounded` is illegal on `Div`/`Rem`: no bound on the operands rules out a zero
divisor, and `sdiv`/`srem` *trap* — a process abort, not a fault. The verifier
rejects it.

`MethodEntry.can_fault` was the other half. It was dead metadata (nothing read
it — `build.rs` emits an unconditional check after every method call) and it had
drifted: `bitset.insert` declared it could not fault while
`praxis_bitset_insert` raises `InvalidSize`. It is now derived from the ABI
manifest, like `MethodEntry::allocates()`.

## Consequences

- **Adding an instruction now touches five exhaustive matches**, not four:
  `ir.rs`, `liveness.rs` defs and uses, `verify.rs`'s `operands`, and
  `lower_inst`.
- **A safepoint emits stores for dead slots.** Code size grows slightly at
  safepoints that follow a value's death; nothing else changes.
- **A stale `GcRef` in a shadow slot is now a compiler bug rather than a
  latent leak**, because the slot is cleared and the verifier checks the
  set it was cleared from.
- **The debugger's fidelity is now a stated contract** rather than a side
  effect of imprecision. A future change that shrinks `DebugSlots` will fail
  `the_debug_set_still_shows_what_the_root_set_dropped`, not a CLI snapshot
  test three layers away.
- ~~**`v.sum()` overflow is still observed late.**~~ **Superseded by ADR-088**,
  which adds the rule this bullet was written to explain the absence of. The
  bullet's conclusion was right and **its mechanism was wrong**: it said the
  sticky fault is one "the host sees after `main` returns rather than unwinding
  at the sink". Measured at `1bd85d8` — `v.sum()` over `[i64::MAX, 1]` followed
  by `out(111)` faults *without printing `111`*, so it was observed inside the
  loop by the next iteration's header check, one element late and with a
  snapshot showing the following element's operands. Late, but not that late.
  The accumulator carries its own `CheckFault` now, so the overflow diverts at
  the addition.
