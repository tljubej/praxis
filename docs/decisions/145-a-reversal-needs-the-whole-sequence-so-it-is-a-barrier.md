# ADR-145: A reversal needs the whole sequence, so it is a barrier

**Date:** 2026-08-06
**Status:** accepted
**Milestone:** 12

## Context

Bottom-up dynamic programming is one of the most common shapes a puzzle program
takes, and it had no spelling but manual bookkeeping. From
[handover 31](../handovers/31-what-an-aoc-solve-found.md)'s day 7 solution:

```praxis
var y = height - 1
while y >= sy {
    …
    y = y - 1
}
```

The handover calls that "the only place in the program where the mechanism is
visible instead of the puzzle". `(0..5).rev()` was `Y110`, no sequence had a
`reversed`, and `5..0` is silently empty — the last of which is
[ADR-059](./059-a-range-is-a-value-and-a-descending-one-is-empty.md) decision 3
working exactly as designed, and is not a defect.

The handover frames the fix as a choice: a `Range` stage `rev()`, or a
`Vec.reversed()`. Since
[ADR-127](./127-a-pipelines-source-is-the-for-loops-and-a-collection-converts-by-naming-what-it-becomes.md)
decision 1 that is a false dichotomy — a `TypePattern::Iterable` receiver gives
all ten receivers one row, so a single entry subsumes both alternatives instead
of choosing between them.

## Decision

### 1. Reversal is a barrier, not a fused link

`MethodLowering::RuntimeSymbol(VecReversed)`, registered beside `sorted`,
`unique` and `frequencies`, and **not** classified by `classify_link`.

This is [ADR-029](./029-pipeline-fusion.md) decision 2's own definition doing
the work: a barrier cannot be fused into the loop feeding it because it needs
the whole sequence before it can answer anything. `sorted` has to see the
largest element before it knows the smallest is first; `reversed` has to see the
last element before it knows the first.

The reason it cannot be a link even *sometimes* is that a link is classified on
name and arity alone and is applied wherever the name appears in a receiver
chain. `v.filter(p).reversed()` does not know the filtered length up front, so a
fused reverse stage would be unsound anywhere but immediately adjacent to the
source.

**The source-adjacent peephole is a deliberate non-goal, not an oversight.**
Walking `emit_iter_item` at `len - 1 - idx` when `reversed` is the innermost
link would remove both `Vec`s that `for y in (0..n).reversed()` currently
allocates. That is a real optimization and it belongs to the fuser, not to this
row: a catalog row carries exactly one `MethodLowering`, and the general case
has to keep working.

### 2. One row, on the generic receiver, and no capability bound

`iterable_of_t()` with no `TypePattern::of_kind` — the visible difference from
`sorted`'s `Ord` and `unique`'s `HashStable`, and the row's own claim rather
than an omission.

`sorted` needs `Ord` because the wrapper orders through the element descriptor's
`compare` callback; `unique` needs `HashStable` because sameness is the
descriptor's `hash` and `equals`. Reversal reads **no descriptor callback at
all**, so there is no element it can be handed that it cannot reverse. A `Vec` of
closures reverses, where `sorted()` on the same receiver is `Y006`.

That is also why the manifest row is `Allocates` and not
`AllocatesAndFaults`: there is nothing for the wrapper to check, and a faulting
declaration would put a dead `CheckFault` after every call.

### 3. The name is `reversed`, not `rev`

Its two immediate neighbours in the same table, `sorted` and `sorted_by_key`,
are unabbreviated past participles answering the same question shape — "a new
`Vec` in some order" — as are `unique`, `frequencies`, `position` and
`enumerate`. And the book had already chosen the name for the thing that did not
exist: `appendix/programs.md` read "There is no `reversed` method; negating the
key is what the catalog leaves you."

One row and not two. ADR-127 decision 6 and the catalog's duplicate-free
`(receiver, name, arity)` key mean a second spelling could not be added later to
soften a wrong call here, so registering both would be the mistake, not the
hedge.

### 4. ADR-059 decision 3 is not reopened

`reversed()` answers `Vec[T]`, like every other stage. It does **not** answer a
descending `Range`.

Making it do so would need a direction bit on `RangeVal`, which reverses
ADR-059's load-bearing invariant: `RangeVal::new` normalizes an `end` below
`start` *to* `start`, so no range with a negative length exists, and that is
established by the only constructor. A pipeline's currency is `Vec` (ADR-127
decision 6), and this row spends it.

The cost is two n-element `Vec`s for `for y in (0..n).reversed()` — the
materializing walk over the `Range`, then the wrapper's answer — where the
`while` loop it replaces allocated none. That is the same order the language
already charges for `(0..n).map(|_| false)`. Decision 1's peephole is where that
goes if it ever matters.

### 5. `5..0` earns no diagnostic

The handover notes that a descending literal is silent and that this makes the
mistake easy. It stays silent, for three reasons in the order they bind.

**No analysis pass has ever emitted a `Severity::Warning`.** Nothing in the
compiler constructs one; the CLI tallies them and the LSP maps them, and that is
all. Emitting the first one is a policy decision with its own scope —
suppression, whether `check` still exits 0 by contract, whether it opens the
unused-binding family — and ADR-051's registry has no warning category. That is
its own ADR, not a rider on a catalog row.

**The book documents the rule with two examples whose purpose is to show `5..0`
empty.** `collections/range.px` and `control-flow/descending-range.px` both
exist to demonstrate it. A lint that fires on the language's own documentation
of its own rule is at the wrong altitude.

**What made the mistake dangerous was the absence of a right spelling**, and the
handover says so itself: "which raises the value of having the right spelling
available." Now that `(0..5).reversed()` exists, the fix is a sentence in the two
chapters that already teach the rule, which is what this change makes.

## Consequences

**What is bought.** A countdown is `for y in (0..n).reversed()`, on a `Range`, a
`Vec`, a `Set`, a `Text` or a `Map`, from one row. Day 7's manual decrement loop
has a spelling that is about the puzzle. A descending sort has a second, more
obvious spelling — `sorted().reversed()` beside `sorted_by_key(|t| 0 - t)` — and
the appendix sentence that said neither existed is now false and rewritten.

**What it costs.** The double materialization in decision 4, and one more row on
a table whose closedness is the point. `RUNTIME_ABI_VERSION` does not move:
appending a symbol has never been a bump (ADR-059's own consequences say so).

**The gates.**
`builtins::reversed_is_a_barrier_with_no_bound_on_its_element` is the catalog
half, and it asserts the *absence* of a bound, which is the part a later edit
would add for symmetry with its neighbours.
`abi::reversal_cannot_fault_where_ordering_can` states the effect contrast
directly.
`runtime::abi::vec_reversed_answers_a_new_vec_and_leaves_the_receiver_alone`
covers the new-`Vec` rule and the empty case;
`vec_reversed_needs_no_callback_where_sorted_needs_compare` is the two-sided
half at the wrapper.
`jit::a_countdown_is_a_reversed_range` is the handover's own gate — `43210` —
and it carries the **negative** gate beside it: `for y in 5..0` still runs zero
times and still says nothing, so a later lint cannot land without reopening
decision 5.
`jit::a_reversed_vec_is_new_and_needs_nothing_of_its_elements` covers a `Text`
and a `Deque` receiver reading through their own accessors, and a `Vec` of
closures reversing where `sorted` refuses.
