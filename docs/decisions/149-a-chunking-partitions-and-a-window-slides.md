# ADR-149: A chunking partitions and a window slides

**Date:** 2026-08-07
**Status:** accepted
**Milestone:** 12

## Context

`chunks` and `windows` have been named-and-absent since M8-WS8. §6.3 lists them
among the five barrier combinators; [ADR-029](./029-pipeline-fusion.md) decision
2 defers them with `sorted`/`unique`/`frequencies`; those three landed and these
two did not. The reason recorded in `builtins.rs` was a descriptor question:

> Both answer `Vec[Vec[T]]`, so their wrapper has to label the *outer* Vec with
> `collections::VEC` while the inner ones keep the element descriptor — a second
> descriptor decision that no program in the design document forces. […]
> Guessing the `Vec[Vec[T]]` labelling would.

**That question was already answered, and the note's own sentence is the
answer.** Writing the rows found no decision where one was expected, and saying
so plainly is worth more than dressing the gap up:

- A `Vec[Vec[T]]` is not a new shape. `outer.push(inner)` builds one today and
  prints `[[1]]`; `adopt_or_reject` labels the outer with `VEC` from the first
  element pushed. Whatever these wrappers produce has to match what `push`
  already produces, which leaves nothing to choose.
- A wrapper naming its result's label is not a new move either. Four already do
  it — `praxis_grid_positions`, `grid_neighbors4`, `grid_neighbors8` and
  `grid_find_all` pass `&tuples::TUPLE`, and `praxis_bitset_items` passes
  `&scalars::INT` — for exactly the reason these need to: the result's element
  kind is not the receiver's, so there is nothing to pass through and the
  wrapper says what it built. `Grid(0, 0, 1).positions()` is an empty `Vec`
  carrying an explicit `TUPLE` label, which is the empty case too.

So the deferral was a *refusal to commit without a forcing program*, which is a
reasonable thing to have done and a different thing from an open question. The
implementation choice that remains is a small one and decision 1 records it at
its real size.

What forces it now is the shape the language cannot express without these rows.
"Compare each element with its neighbour" — the most common question a puzzle
asks of a sequence after "add them up" — is an index and a bound:

```praxis
var increases = 0
var i = 1
while i < depths.len() {
    if depths[i] > depths[i - 1] { increases = increases + 1 }
    i = i + 1
}
```

That is the shape [ADR-145](./145-a-reversal-needs-the-whole-sequence-so-it-is-a-barrier.md)
removed for countdowns, still present one dimension over.

## Decision

### 1. The outer label is passed, not inferred

`&collections::VEC` for the outer `Vec`; the receiver's own element descriptor
for each inner one, passed through unchanged and null included, exactly as
`praxis_vec_reversed` passes its one through (REP-41).

*Which* label goes there is settled by `push`, per the context above. The only
thing left to choose is whether to write it or let [`vec_of`] infer it from the
first element, and that is decided by the lengths at which there is no first
element: `[].chunks(2)` and `[1, 2].windows(5)` are both `[]`. Inferring would
label `[1].chunks(2)` with `VEC` and `[].chunks(2)` with null — one type
carrying two labels, and the null is the one `vec_format` renders as `[]` and
`praxis_vec_push` treats as "adopt whatever arrives" (P0-11).

That is an argument about an empty collection, not about nesting. It applies to
`grid_positions` identically, and `grid_positions` already resolves it the same
way. The rule stated generally: **a wrapper whose result element kind is not its
receiver's names that kind, at every length.** These two rows are the fifth and
sixth to follow it, not the first.

### 2. `chunks` partitions and `windows` slides

`chunks(n)` is the consecutive non-overlapping runs of `n`, and **the last one
is short** when the length does not divide: `[1, 2, 3].chunks(2)` is
`[[1, 2], [3]]`. `windows(n)` is every consecutive run of *exactly* `n`, each
starting one element after the last: `[1, 2, 3].windows(2)` is
`[[1, 2], [2, 3]]`.

They differ in exactly one place, and it is the group that does not fill. A
chunking has to keep it, because a partition that dropped its tail would not be
a partition and every element-once guarantee would go with it. A window has to
drop it, because a run of `n` is a run of `n` — `[1, 2].windows(5)` is `[]`, and
that is an answer rather than an absence of one.

### 3. Zero and negative are the only refusal, and they fault

`n <= 0` raises `FaultKind::InvalidSize` before either wrapper walks anything.
A run of zero elements is not a short run: chunking a non-empty sequence into
them has no finite answer, and sliding one along a sequence has no useful one. A
negative run names nothing at all.

**It is a fault and not a clamp, and not an empty answer.** A clamp to 1 would
answer a plausible sequence for a size the program computed wrongly, which is
the class of defect [ADR-041](./041-bounded-extents-fault-instead-of-aborting.md) decision 1
built `VecExtent` to prevent one dimension down. An empty answer would be worse
still, because `[]` is a *correct* answer for `windows` at a large `n` — the two
would be indistinguishable at the call site.

`InvalidSize` is the kind rather than a new one because it is what it says: "a
size or extent the runtime cannot honour", which is `Vec(0 - 1, 0)`'s fault
already. What is new is the door — this is the first pipeline row to fault on an
*argument* rather than on an element or an empty receiver.

This is why the manifest rows are `AllocatesAndFaults` where `VecReversed` is
`Allocates`, and the distinction is load-bearing rather than descriptive: MIR
emits a `CheckFault` after a call only when the wrapper declares one, so an
`Allocates` row would set the fault into a context nothing reads and hand the
program a Unit sentinel typed as a `Vec[Vec[T]]` (ADR-088).

**A size larger than the receiver is not this fault**, and conflating the two is
the likeliest way to get this wrong later. `chunks(9)` on five elements is one
short chunk; `windows(9)` on five is no windows. Both questions have answers.

### 4. Both names are plural

`windows`, not `window`, and the argument is the catalog's own convention rather
than a reading of what each name is "about". Every row that answers many things
is plural: `frequencies`, `positions`, `cells`, `keys`, `values`, `items`,
`neighbors4`, `chunks`. A singular `window` would be the only exception, bought
with a distinction — the name is for the shape, not the pieces — that nothing
else in the language makes.

It is also what §6.3, ADR-028, ADR-029, ADR-127 and `pipelines.md` have all
called this row since before it existed, and what Rust's `slice::windows` calls
it. The catalog's duplicate-free `(receiver, name, arity)` key means only one
spelling can ever exist, so this could not have been softened later with an
alias — which is the reason to take the convention rather than depart from it.

### 5. Both are barriers, and no capability bound

`MethodLowering::RuntimeSymbol`, registered beside `sorted`, `unique`,
`reversed` and `frequencies`, and **not** classified by `classify_link`.

A grouping is a fact about positions in the whole sequence, so neither can
answer its first group from one element — ADR-029 decision 2's definition, the
same one `reversed` meets. And as with `reversed`, the receiver is `Iterable`
(ADR-127 decision 3), so `build::emit_iter_vec` materializes a `Set` or a `Range`
into a real `VecPayload` in front of the wrapper.

No `TypePattern::of_kind`, for `reversed`'s reason: a grouping reads **no
descriptor callback** — not `compare`, not `equals`, not `hash` — so there is no
element it can be handed that it cannot group. A `Vec` of closures groups where
`sorted()` on the same receiver is `Y006`.

### 6. Groups share their elements

`windows`' overlapping element is one object, not two. So is an element that
appears in a group and in the receiver.

This is the language's existing reference semantics stated at a new site rather
than a new rule — `var b = a` and a double `push` of one value already alias —
and it is the same decision [ADR-146](./146-a-collection-constructors-arity-is-its-shape.md)
decision 4 made for a collection fill. Copying would make `outer.windows(2)` on a
`Vec[Vec[T]]` quietly deep, which nothing else in the language is.

## Consequences

**What is bought.** `depths.windows(2).count(|p| p[1] > p[0])` replaces an index
and a bound. `depths.windows(3).map(|t| t.sum())` is a sliding total. A batch at a
time is `items.chunks(n)`. All of it over the ten iterables ADR-127 generalized,
from two rows. The `Vec[Vec[T]]` result is the first a *catalog row* declares,
though not the first the language builds — `outer.push(inner)` already did — so
the type-level half is where the new ground is, not the runtime's.

**What it costs.** Two rows on a table whose closedness is the point, and two
symbols. `RUNTIME_ABI_VERSION` does not move: appending a symbol has never been a
bump (ADR-059's consequences say so).

Neither wrapper is lazy, and neither is a fused stage. `v.windows(2).take(3)`
builds every window and then takes three. Making a grouping a fused stage would
need a stage with a buffer — the first in the fuser — and decision 5's reasoning
does not forbid the source-adjacent special case any more than ADR-145 decision
1 forbids its own peephole. It is the same deliberate non-goal: a row carries one
`MethodLowering`, and the general case has to keep working.

**The gates.**
`builtins::a_grouping_answers_a_nested_vec_with_no_bound_and_can_fault` is the
catalog half — it asserts the nesting, the *absence* of a bound and the presence
of a fault, which are the three a later edit would each undo for a plausible
reason.
`abi::a_grouping_declares_the_fault_a_reversal_has_not` states the effect
contrast directly, since `reversed` is the row these most resemble.
`infer_tests::a_grouping_answers_a_sequence_of_sequences` is decision 1's
type-level half: a row declaring `Vec[T]` would flatten the answer and nothing
in the row's own text would look wrong.
`runtime::abi::a_grouping_labels_the_outer_vec_even_when_it_is_empty` is
decision 1 at the wrapper, and it is written around the empty case because that
is the one the inferring version gets wrong.
`vec_chunks_partitions_and_keeps_a_short_tail` and
`vec_windows_slide_by_one_and_drop_a_run_that_does_not_fit` are decision 2, one
apiece, and each carries the boundary the other would fail —
`a_group_size_of_zero_or_less_is_an_invalid_size_fault` is decision 3 with the
empty *receiver* beside it, which is the case that looks like the fault and is
not.
`jit::a_group_size_of_zero_or_less_faults`,
`chunks_partitions_and_keeps_a_short_last_chunk`,
`windows_slide_by_one_and_drop_a_run_that_does_not_fit`,
`a_grouping_takes_every_iterable_and_a_chain_starts_again_from_it` and
`a_grouping_needs_nothing_of_its_elements_and_shares_them` are the same claims
end to end, the last covering decision 6.
`mir::a_barrier_materializes_a_non_vec_receiver_before_the_wrapper` gains both
rows, which is decision 5's materialization at a row that also takes an argument.
`tests/aoc-corpus/adr149_grouping_a_sequence.px` is the whole of it as a running
program, ending on the shape decision 3's context opens with.
