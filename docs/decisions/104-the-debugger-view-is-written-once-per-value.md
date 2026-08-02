# ADR-104: The debugger's view is written once per value, and a frame is two slot-stack claims

**Date:** 2026-08-02
**Status:** accepted — implemented
**Milestone:** post-M11 performance (handover 21 finding §3.2)
**Amends:** ADR-021 decisions 2 and 3 and its Consequences bullet on the spill;
ADR-035 decision 3's last hop; ADR-044's emission story (its two-set *contract*
is untouched); ADR-033 decision 4. ADR-101's `SlotStack` is used as delivered.
MIR-16, MIR-01, MIR-02 and ADR-033 decisions 1–3 are preserved and the places
this change touches their edges are recorded below.

## Context

Every generated function pushed a **second** heap frame beside its shadow frame.
`praxis_push_debug_frame` collected a `Vec<DebugLocal>`, `into_boxed_slice`d it
with a `mem::forget`, and `Box::new`d a `DebugFrame` — two to three mallocs —
then the prologue made a third extern call, `praxis_set_frame_source_span`, to
record a number that is a compile-time constant. Each was wrapped in
`abi_guard!`'s `catch_unwind`. On top of that, `SpillCtx::spill_debug` emitted a
**second** store sequence at every GC safepoint *and* at every `Inst::CheckFault`,
over a deliberately over-approximate local set, into that frame.

None of it is read unless the program faults. Handover 21 §3.2 measured deleting
it outright at ~18–24%, and said plainly that the deletion was a measurement and
not a proposal: it removes a shipped feature, and §9.6 requires full top-frame
locals in *every* mode — `m10ws4_noninteractive_renders_backtrace_and_locals`
runs with `--debug never` and still asserts on rendered locals.

The handover proposed reconstructing the view from the shadow frame instead. The
first thing this ADR has to record is that that cannot work.

## Why reconstruction from the shadow stack is impossible

Two independent losses, not one.

**The nulls.** ADR-044 decision 2 nulls a shadow slot the moment its local dies
(`RootSlots::dead`), and `a_dead_local_stops_being_reachable_from_its_frame` is
the end-to-end gate that says it must. So the shadow stack has *already*
forgotten `xs` by the time a later statement faults. This is hazard H3 — the
hazard ADR-044 says the two-set split "had to come first" to prevent — arriving
by the other door, dressed as a performance win.

**The values that were never there at all.** `liveness::block_roots` computes
the root set as live-*before* an instruction (`transfer_inst` removes defs, then
adds uses), so an `Alloc`/`Materialize`/`Call` destination is by construction
excluded from its own safepoint's root set. The module doc says so: *"the
destination is written after the collection so it is not rooted."* A temp that
is materialized and consumed before the next safepoint is therefore in **no**
shadow slot at any point in the program's execution. It lives in a Cranelift
register and in the debug frame, and nothing else.

Traced by hand through `crates/praxis-cli/tests/fixtures/run/debug_temps.px`
(`let a = 10; let b = 20; let c = 30; a + b + c + 9223372036854775807`), the
shadow stack at the overflow holds **one** of about nine locals. `a`, `b` and
`c` were nulled; the `a + b` temp and the `MAX` literal's temp were never
written. `m11_locals_split_users_and_temps_with_types` asserts `a: Int = 10`,
`b: Int = 20`, `c: Int = 30` and `@ "a + b"`.

**Merging the two sets** — making `RootSlots := DebugSlots` so the shadow stack
holds everything — fails `a_dead_local_stops_being_reachable_from_its_frame` by
construction, and its retention is unbounded rather than a constant factor:
`DebugSlots::visible()` grows monotonically within a block, and a top-level
script is one huge block. ADR-044 §Context also names the sharper consequence:
*"After RT-01 made swept storage reusable, that stopped being merely a retention
bug: a stale slot can name a live object of a different type."*

Both forms are rejected. `a_local_the_root_set_dropped_is_still_renderable` and
`a_temp_that_never_reached_a_shadow_slot_is_still_renderable` are the two tests
that pin the losses at the `CrashSnapshot` level, one layer below the REPL
transcripts, so a future attempt fails on the property rather than on a rendered
string three layers away.

The cost was never the *fidelity*. It was the *rate*. So the change is to pay for
each value once and to stop allocating the frame at all.

## Decision 1: the debug slot is written at the definition, not at every safepoint

The backend emits **one store per `Gc` definition** —
`SpillCtx::store_debug_defs`, called after each `lower_inst` and once in the
prologue for each parameter — and emits nothing at safepoints or at
`Inst::CheckFault`. `SpillCtx::spill_debug` and `SpillCtx::spill_safepoint` are
deleted; the five safepoint arms call `spill_roots` directly.

**This produces the same slot contents at every point a snapshot can be taken.**
A debug slot is never cleared, so its content is *the value the most recently
executed store to it wrote*. The old spill wrote `builder.use_var(vars[L])`,
which is by definition the value of the most recently executed `def_var` of `L`
— or Cranelift's zero for a path where none executed, since
`cranelift-frontend`'s SSA builder zero-initializes a variable undefined along an
incoming edge. Writing at every `def_var` leaves exactly that same value in the
slot, and a frame's slots start zeroed, which is the same zero. Loop back-edges
and redefinitions need no separate argument: "most recently executed" is a
property of the run, not of the CFG.

It is worth saying what this argument replaces. The plan this ADR implements
expected a divergence — "a `Gc` local in `live_in(block)` with no definition on
the executed path: today `use_var` silently zeroes it, store-at-def would leave a
value from another path" — and proposed a new verifier rule,
`DebugLocalVisibleBeforeDefined`, to make that state unrepresentable. **The
divergence does not exist**, because stores are dynamic and reaching definitions
are static: if the other path did not execute, its store did not happen either.
The rule was not added. It would have rejected legal MIR (a local live-in on a
path that does not define it is exactly what Cranelift's zero-init exists for),
and adding a rule to forbid a state that cannot occur is worse than no rule —
it teaches the next reader that the state was possible.

The change does *gain* a few values. A local defined at the end of a block and
dead at the top of the next was in no debug point's `visible()` and so was never
written at all; it now shows what it was given. That is MIR-16's contract — *"a
value that has been produced stays renderable"* — being met more completely.

`DebugSlots` is unchanged and keeps its ADR-044 definition, its
`unannotated()`-only construction seal, its `liveness::annotate`-only filler and
its verifier check. What changes is that it is the **contract** rather than the
**emission plan**: whatever a point's `visible()` names, the def-stores have
already written. Narrowing it to a per-point delta would be the shrink
`the_debug_set_still_shows_what_the_root_set_dropped` exists to refuse, and this
does not do that.

`liveness::defs` is made `pub` and the backend drives its stores from it.
ADR-044's Consequences fix the number of exhaustive matches over `Inst` at five;
a private copy in the backend would have made it six, and the drift would present
as a local the debugger silently stops showing.

### What the `CheckFault` spill was for, and why it is not needed

Its comment named the case: *"a snapshot taken on the fault path sees
`<uninit>` for the `0` divisor in `x / 0`."* That divisor is a `Gc` local
produced by an `Alloc`, a `Materialize` or an `Inst::ConstGc` earlier in the
block, so store-at-def has already written it — earlier, in fact, than the
`CheckFault` would have.
`a_fault_between_a_definition_and_the_next_safepoint_shows_the_value` is that
program, asserted at the snapshot level.

## Decision 2: the static half of a frame is static

`FunctionDebugMeta` — the function's name, its source span, and its
`DebugLocalMeta` array — is one arena-interned record per lowered function,
shared by every call and every recursion level. The prologue names it with one
immediate.

That is six of the nine words ADR-021's `DebugFrame` carried. Two more,
`parser_path` and `parser_path_len`, have been null since M10a and no
`SnapshotFrame` field ever carried them; they are dropped rather than reserved.
The ninth, `parent`, is the entry below this one on a stack (decision 3).

`praxis_set_frame_source_span` is **deleted**. ADR-035 decision 3 threaded the
span AST → HIR `TypedFn` → MIR `Function.span` → backend; only the last hop
moves, from a runtime call in every prologue to a field the snapshot walker
reads. `m10b_ws1_snapshot_frame_carries_source_span` is the gate, and
`a_snapshot_orders_its_frames_innermost_first_with_each_functions_own_locals`
checks each frame gets its *own* function's span.

Interning is by content on top of the existing `DebugLocalMeta` cache, so
`repeated_identical_metadata_stops_growing_the_arena` still holds for a debugger
session that recompiles the same function on every `p EXPR` (DBG-05).

## Decision 3: a frame is two claims on two contiguous stacks

The `Runtime` owns two more `SlotStack`s, exactly as ADR-101 anticipated when it
made the type generic:

- `debug_values: SlotStack<Option<GcRef>>` — one word per `Gc` local per live
  call. Sized `SHADOW_STACK_SLOTS`, for the same two reasons: a call claims one
  slot per `Gc` local, a frame is at most `MAX_SHADOW_SLOTS` wide by
  `SlotCount`, and every prologue rejects `depth >= MAX_RECURSION_DEPTH` before
  it claims anything. Exhaustion is unrepresentable, so there is no bounds check.
- `debug_frames: SlotStack<DebugFrameEntry>` — one `{ meta, values }` pair per
  live call, `MAX_RECURSION_DEPTH + 1` of them (128 KiB).

`RuntimeContext.debug_top: *mut DebugFrame` becomes
`debug_frames: *mut DebugFrameStackHeader` — the same position and width, a
different thing pointed at, which is precisely what ADR-101 did to `roots`. That
keeps every later field's offset unchanged; §11.6's discipline in that struct,
restated three times in its own comments, is *append at the end, never reorder*.
`debug_values` is appended after `small_ints`. `RUNTIME_ABI_VERSION` goes
17 → 18.

The prologue claims both, writes the entry's two words, and keeps two bases in
Cranelift `Variable`s; the epilogue stores both back. **No malloc, no free, no
extern call, no `catch_unwind` landing pad.** The debug store becomes
`store [debug_base + slot*8]` — the same shape as the shadow spill, where it was
a load of `frame.locals`, an `iadd_imm_s` over a 48-byte `DebugLocal` stride, and
a store at displacement zero.

`praxis_push_debug_frame`, `praxis_pop_debug_frame` and
`praxis_set_frame_source_span` are deleted from the manifest, from
`praxis_runtime::abi::address` and from the runtime. `DebugFrame` goes with them.
All three matches are exhaustive, so the removal is compiler-checked.

### Two stacks, not one interleaved region

A single region with `{meta, values_base, v0, v1, …}` per frame would be one
claim and one pop instead of two, and a forward walk would recover the frames.
It is rejected: it puts a `*const FunctionDebugMeta` and a run of `Option<GcRef>`
in one array, so the slot type has to be a word that is sometimes one and
sometimes the other, and the snapshot walker's correctness rests on a count
rather than on a type. Two stacks cost one extra claim and one extra store per
call and make the confusion unspellable.

### The frame entry is claimed without zeroing, and the value slots are not

`emit_slot_stack_push` is split into `emit_slot_stack_claim` (move the cursor)
plus `emit_zero_slots`. Zeroing exists so a slot the function has *not* written
yet reads as "nothing here"; the frame entry has no such slot — both its words
are written in straight-line code immediately after the claim, with nothing
between, and its only reader is a fault epilogue far downstream. The value slots
do need it, and get it: a zeroed `Option<GcRef>` *is* `None` by the `NonNull`
niche (F18), which is what makes `<uninit>` an absence the type carries rather
than a sentinel to compare against.

### The value slots are typed so they cannot become a root set

`impl RootSet` lives on `SlotStackHeader<*mut GcHeader>`. `debug_values` is
`SlotStackHeader<Option<GcRef>>`, a different type, so it does not have one and
cannot be handed to the collector. That is ADR-044's split made structural: the
debug set is over-approximate and never cleared, and rooting it would be the
merge rejected above, arriving as a two-line change.

It also reads better. A shadow slot is a raw `*mut GcHeader` only because the
collector dereferences it and `GcRef` is `NonNull`; a debug slot holds a value or
nothing, which `Option<GcRef>` says exactly.

## Decision 4: the snapshot rejoins the two halves, so nothing downstream moves

`praxis_snapshot_debug_chain` walks `debug_frames`' claimed slots in reverse —
innermost first, which is the order the host renders and which ADR-021's `parent`
pointer used to buy — and materializes each `SnapshotFrame`'s `Vec<DebugLocal>`
by zipping `meta.locals[i]` with `entry.values[i]`.

**`DebugLocal`, `SnapshotFrame`, `CrashSnapshot` and `impl RootSet for
CrashSnapshot` are unchanged.** That is the load-bearing property of this design:
`crates/praxis-debugger` — `render.rs`, `evaluate.rs`, `repl.rs`, `session.rs` —
and all sixteen REPL transcript tests compile and pass with **zero edits**. If
they had needed edits, the fidelity would have changed and that would have been
the finding.

ADR-033 decision 1 is preserved verbatim: the innermost fault epilogue still
captures, still before any pop, still guarded by `SnapshotSlot::is_set()`. A
stack — unlike the `Box`ed chain — does not destroy the words a pop releases, so
capturing lazily at the host becomes *possible*. It is rejected: values above
`top` are in no arm of `RuntimeRoots`, so a collection between the unwind and the
read could free what they name, which is ADR-033 decision 2's rooting story.
ADR-033 decision 4's sentence, "the spill keeps snapshot values fresh", becomes
"the def-store keeps them fresh, at the same or earlier program points".

## Measurements

Apple M2 Pro, release build, `/usr/bin/time -p`, three runs per configuration,
minimum reported, the three binaries run **interleaved** (A,B,C,A,B,C,A,B,C) and
their stdout diffed every round. Baseline is the tree with handover 21 §3.1,
§3.5, §3.3, §3.4 and §3.6 already landed — so these are what §3.2 is worth
*after* the other five, not the handover's original numbers.

| | base | + decision 1 | + decisions 2–4 | total |
|---|---:|---:|---:|---:|
| 5M × no-op call | 0.32 s | 0.32 s | **0.08 s** | −75% |
| 20M × no-op call | 1.29 s | 1.29 s | **0.33 s** | −74% |
| `tree` @ 60 | 1.27 s | 1.20 s | **0.61 s** | −52% |
| `pipeline` @ 200,000 | 1.28 s | 1.23 s | **0.68 s** | −47% |
| `primes` @ 300,000 | 0.18 s | 0.17 s | **0.14 s** | −22% |
| `mandelbrot` @ 200 | 0.75 s | 0.62 s | **0.62 s** | −17% |
| `bfs` @ 80 | 4.09 s | 3.46 s | **3.46 s** | −15% |
| `hashwork` @ 800,000 | 0.50 s | 0.43 s | **0.43 s** | −14% |
| `vm` @ 400,000 | 1.15 s | 1.02 s | **1.00 s** | −13% |
| `collatz` @ 340,000 | 1.43 s | 1.30 s | **1.28 s** | −10% |
| `collatz` @ 60,000 | 0.21 s | 0.19 s | **0.19 s** | −10% |
| 10M × `i = i + 1` | 0.14 s | 0.13 s | **0.13 s** | — |

**The split is the point, and it lands where the analysis said it would.**

Store-at-def cannot move the no-op call and does not: 1.29 s before and after,
to the centisecond, over 20M calls. That benchmark has 5M–20M prologues and
essentially no safepoints, so it was paying for the mallocs and the three extern
calls and nothing else. Decisions 2–4 take **47 ns off every Praxis call**, and
the three other call-dominated benchmarks — `tree`, `pipeline`, `primes` —
follow it.

Conversely, `collatz` gets nothing from the frame becoming free, and that is not
noise either: `collatz.px` is a file of top-level statements, which
`praxis_hir::lower` folds into **one** synthetic entry function, so the whole
340,000-outer-iteration run performs exactly *one* prologue. Its 10% is entirely
store-at-def. `mandelbrot`, `bfs`, `hashwork` and `vm` are the same shape —
allocation-dense loops inside few calls — and take their whole win from decision
1 as well.

The loop with neither calls nor allocations is the control, and it does not move
under either stage.

Max RSS is unchanged — `tree` @ 60 peaks at 543.74 MiB before and 543.65 MiB
after — which is the second 12.29 MiB reservation being virtual, observed rather
than assumed. So is the startup floor (`collatz` at size 0: 7.13 MiB either way).
`benchmarks/run.py --pilot` — the release-build correctness gate `just ci`'s
debug build never exercises — passes with all three implementations agreeing.

## Consequences

- **The debugger shows strictly more than it did, and never less.** Every local
  that was renderable is renderable; a `Gc` local defined at the end of a block
  and dead at the top of the next is now renderable too. No `praxis-debugger`
  source file changed, and no assertion in the sixteen REPL transcript tests
  moved.
- **Three `#[no_mangle]` wrappers are gone**, and with them three `catch_unwind`
  landing pads per call. ADR-080's property is about the *set* of entry points
  and its source-scanning test still passes; inline Cranelift cannot panic, so
  the removed guards protected nothing that still exists.
- **`DebugLocal`'s 48-byte layout no longer constrains generated code.** ADR-035
  decision 1 appended `type_id` after `value` specifically so `DEBUG_VALUE_OFFSET`
  would stay stable; the debug values are now a separate 8-byte-stride array, so
  that constraint is simply gone. `DebugLocal` is still the snapshot's element
  type and still `#[repr(C)]`, but nothing emits an offset into it.
- **The `SlotStack` mechanism now has three instantiations and no third
  implementation.** ADR-101 wrote "a second stack is one more field and the same
  two calls"; it was, and the only addition either side needed was
  `emit_slot_stack_claim` and a crate-private `SlotStackHeader::claim`.
- **`RuntimeContext` is one word wider and `clear_for_rerun` asserts on two more
  stacks.** The balance property — every prologue matched by an epilogue — is now
  checked for all three, so an unbalanced debug epilogue is a `debug_assert`
  between runs rather than a snapshot that walks into a popped frame.
- **A latent soundness question is unchanged and is *not* this change's to fix.**
  `RuntimeRoots` has five arms and the debug values are not one of them, so a
  value that `RootSlots::dead` nulled but that a debug slot still names is
  unreachable for GC; a collection between the null and the fault frees it, and
  the snapshot then copies a dangling `GcRef` into a root set. This is true on
  `main` today, for exactly the same values, and this change alters neither the
  values nor their lifetimes. The contiguous stack does make the fix a two-line
  change — a sixth arm walking `[base, top)` — but that arm is the merge in
  disguise unless the values are rooted *weakly*, so it needs its own decision
  and its own measurement against
  `a_dead_local_stops_being_reachable_from_its_frame`. Recorded here so the next
  reader does not have to re-derive it.
- **`praxis_snapshot_debug_chain` is now the only `praxis_*` symbol the debug
  machinery has**, and it is called once per fault. The four the prologue and
  epilogue called are gone.
