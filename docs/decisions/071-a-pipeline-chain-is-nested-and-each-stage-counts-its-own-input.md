# ADR-071: A pipeline chain is nested, and each stage counts its own input

**Date:** 2026-07-31
**Status:** Accepted — implemented
**Milestone:** Repair (stage S21 — MIR-03…MIR-08)

Amends **ADR-029**, whose decisions 2, 3 and 5 this supersedes in part.

## Context

ADR-029 delivered §6.3's "fuse common chains into loops" and it works: one loop,
no intermediate Vecs. Six defects sat under it, and five of the six are one
representational choice showing through in five places.

```praxis
v.filter(even).take(2).sum()          # took source positions 0 and 1
v.filter(even).enumerate()            # numbered 1, 3, 5
v.filter(even).zip(other)             # paired with other[1], other[3]
v.filter(even).position(p)            # answered a source index
v.flat_map(f).take(1)                 # kept the first of *each* inner Vec
v.flat_map(f).any(p)                  # went on evaluating p after answering
v.flat_map(f).flat_map(g)             # panicked the compiler
v.take(n)                             # answered Unit, then read it as a Vec
```

The plan is a flat `Vec<Stage>` and the emitter has a single index — the source
cursor. `flat_map`'s semantics are nested, so the emitter special-cased the
first one and re-entered the element-wise stage loop for `stages[i+1..]`; the
`Stage::FlatMap` arm of that loop was an `unreachable!` guarded by a comment
claiming the recognizer could not produce two. It could. And a chain of stages
reading one index is a chain of stages that all believe they are looking at the
source.

## Decision 1: the chain is recursive, and a splice holds what follows it

`Chain` is `Then(Stage, rest)` / `Splice { f, rest }` / `Sink(sink)`, and
`Stage` has **no** `flat_map` variant. That is the deletion that matters:
"a splice reached the element-wise emitter" is not guarded, it is
unrepresentable. `emit_plan` recurses; `emit_splice` emits an inner index loop
and calls `emit_plan` on `rest` inside its body, so a second splice arrives at
the same function with its own loop and nothing has to know it is the second.

The alternative — keep the flat list and make the `unreachable!` a real arm that
recurses — was rejected because it leaves the representation lying. A flat list
says the stages are a sequence; they are a tree the moment a `flat_map` is in
it, and every reader of that list would have had to remember.

A lowered mirror, `Plan`, carries each stage's *already-lowered* argument slot.
The emitter used to pull arguments from one `std::vec::IntoIter<LocalId>` shared
between the outer loop and the splice, with a written contract that each stage
advance it by exactly the right amount. An argument is a field now, and six
`.unwrap()`s and a class of silent closure mis-pairing went with the iterator.

## Decision 2: a stage's index is the sequence that reaches it

Every stage that asks "which element is this?" — `take`, `skip`, `zip`,
`enumerate` — and the two sinks that report one — `find`, `position` — owns a
**dense counter**: a `Gc` Int slot, read then bumped, so the value it sees is
the number of elements that arrived ahead of this one.

`v.filter(even).take(2)` means the first two *survivors*. This is not a
refinement of the old behaviour, it is the only reading of §6.3 that composes:
a stage cannot see the source, it can only see what the stage before it handed
it, and any other answer makes the meaning of a chain depend on how far from the
source it was written.

The rule is enforced structurally as well as stated. `emit_plan` and `emit_step`
do not take the loop index as a parameter at all. A source cursor belongs to the
loop that owns it — its bounds check and its `praxis_vec_get` — and there is now
no way for a stage to reach one.

The counters are allocated **before the outer loop scaffold**, and that
placement is the whole of the flat_map half. A counter zeroed inside the outer
body restarts for every inner Vec, which is exactly what the old inner index
did. Zeroed once, a counter counts the flattened stream by construction.

The bumps stay `Overflow::Bounded`. ADR-044 decision 6 makes that a claim about
the site: a counter is bounded by the number of elements the loop can deliver,
which after a `flat_map` is the flattened stream's length — still bounded by
what the process can allocate. `Checked` would buy nothing and cost a fault test
per element.

## Decision 3: a chain has one exit, and it is not the innermost loop

ADR-029 decision 3 had the fused loop push a `LoopCtx` and had short-circuiting
sinks jump to `b.loop_stack.last().break_target`. Inside a splice that is the
*inner* loop, so `any` stopped one inner Vec and the outer loop carried on:
the predicate ran on elements after the answer was decided (observable as a
`DivByZero` in a chain that had already answered `true`), and a later match
could overwrite `find`'s answer.

The emitter carries two block ids explicitly instead. **`pipeline_exit`** is
singular however deeply the chain nests — a splice adds an inner header, body
and increment, but never an inner place to stop the stream — and
**`continue_target`** is rebound by each splice to its own increment, because
while a splice is running "advance to the next element" does mean the next inner
element. `take`, `take_while`, `zip`'s stop and `any`/`all`/`find`/`position`
target the exit; `filter`'s drop, `skip`'s drop, `reduce`'s first-element seed
and the splice's tail target the continue, since each of those advances the
stream rather than ending it.

The per-combinator split — inner `take`, outer `any` — was considered and
rejected. §6.3 and ADR-029 decision 2 describe a chain as one sequence; a
`take(1)` that meant "one per inner Vec" would have no way to say the other
thing, and the exit-criterion test for `any` already encodes the answer.

`break_loop`/`continue_loop` and both of the fusion's `LoopCtx` pushes are
deleted. That is safe for a reason worth writing down rather than rediscovering:
every expression a fused chain contains — the source, each stage's closure, a
`zip`'s second source, the sink's closure and init — is lowered *above* the loop
scaffold, and a closure body is a separate MIR function, so no user
`break`/`continue` is ever lowered inside a fused loop body. `lower_break` and
`lower_continue` remain the loop stack's only readers.

Jumping from an inner block straight to the outer exit is fine for the backend:
MIR is slot-based rather than SSA and `seal_all_blocks` runs after the whole CFG
is built (ADR-015).

## Decision 4: a `take`/`skip` bound is an expression, evaluated once

The catalog types both parameters `Int` and says nothing about literals. The
recognizer matched `TypedExpr::Lit { value: Lit::Int(n) }`, and a non-literal
did not decline the *stage*, it declined the whole chain — which fell to the
eager combinator lowerer, which has no `take` arm, whose `_` arm returns the
Unit singleton for the enclosing `.sum()` to call `praxis_vec_len` on.

The bound is lowered once, before the loop, alongside every other pipeline
argument, and compared inside the loop by extracting both payloads. Once,
because that is what every other argument does and because a bound with a side
effect must not fire per element; the MIR gate asserts both the count and the
position relative to the loop header, which no behavioural test can see for a
pure bound.

Degenerate bounds keep the meaning the literal spelling had — `take(0)` and
`take(-1)` are empty, `skip(-1)` drops nothing — because it is the same
comparison, not a new case.

## Decision 5: the fused pair carries the type the catalog declares

ADR-029 decision 5 shipped `enumerate` and `zip` as "forward-compatible": they
built `(i, item)` and `(a, b)` tuples at a time when `.0`/`.1` did not exist. The
tuples carried `AllocKind::Tuple { ty: MirType::Opaque }` — the last two
unconditional `Opaque`s at a descriptor-producing site in the tree — so every
schema slot said "no static type" and formatting, hashing and equality
dispatched through each value's own header.

The type was already there: TY-31 corrected the rows to `Vec[(Int, T)]` and
`Vec[(T, U)]`, and the recognizer was discarding the node's type with a `..`. It
reads it now, one element-of from the call's own result type.

The `Opaque` fallback stays, and it is not decoration. A *half* of a known pair
may still be an inference variable — a `Vec` that was never pushed to — and
`tuple_schema_for` answers that with a **null slot** the runtime resolves off the
value's header (ADR-066 decision 5).

## Consequences

- **No new diagnostic code and no ABI bump.** Nothing `#[repr(C)]` changes, and
  every defect here was a wrong answer rather than a missing report. The next
  free codes are still `Y022`, `Y116`, `Y126`, `N008` (ADR-051), and
  `RUNTIME_ABI_VERSION` / `COMPILER_EXPECTED_ABI_VERSION` are still 13.
- **The MIR verifier's `OpaqueAtDescriptorSite` rule stays off**, and its note
  is rewritten to say why. It used to blame a catalog defect TY-31 had already
  fixed. What blocks it now is that `Opaque` is a *legal* answer for an
  unresolved inference variable; the rule that could be turned on is a narrower
  one — "no `Opaque` where the type could have been resolved" — and stating it
  needs a distinction `MirType` does not carry.
- **The eager fallback is now unreachable for a well-typed program.** With the
  literal-only restriction gone, all 23 registered `MethodLowering::Intrinsic`
  names are recognized, so `lower_pipeline_combinator`'s Unit-returning `_` arm
  has no way to be reached. It is deliberately kept — ADR-029 decision 1 names
  it the incremental-safety net, and deleting 350 lines of net in the same
  change that rewrites what it is a net for is the one edit nobody could bisect.
  Deleting it, with `emit_index_loop` and `alloc_empty_vec` following it out, is
  a reasonable later commit on its own.
- **`alloc_empty_vec` still writes `MirType::Opaque` and the reason has
  changed.** It is no longer that the catalog is wrong; it is that
  `praxis_vec_new` adopts the first pushed value's descriptor, so an empty
  collect-target has nothing to state. Deriving it here is a change to
  collection construction, and belongs with whatever makes the element
  descriptor authoritative rather than adopted.
- **Fourteen ignored regressions are un-ignored and green**, and seven new gates
  land with them. Two of the fourteen (`enumerate_materializes_…`,
  `zip_materializes_…`) already passed on REP-23's null-slot arity, which is why
  `a_fused_pairs_schema_names_its_element_types` exists: it is the only
  assertion that separates a real element type from the runtime falling back to
  a header.
