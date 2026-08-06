# ADR-144: A sequence of `Text` joins, and a sequence of `Char` becomes one

**Date:** 2026-08-06
**Status:** accepted
**Milestone:** 12

## Context

`out(grid.row(y))` prints `[., ., |]`. There is no way to get `..|`.

[Handover 31](../handovers/31-what-an-aoc-solve-found.md) records that from a
puzzle whose statement is nine consecutive pictures of a grid: watching the
beams fill in is the obvious way to check the simulation, and it is not
writable. `Vec[Char] → Text` had no route, with or without a separator, and
neither did `Vec[Text] → Text`:

```console
$ praxis check join.px
error[Y110]: no method `join` on type `Vec[Text]` taking 1 argument(s)
error[Y110]: no method `join` on type `Vec[Char]` taking 1 argument(s)
```

The handover proposes `join(Text)` on `Vec[Text]` **and** `Vec[Char]`, with
`Vec[Char].to_text()` as an alternative that "would cover the grid case alone".

That surface is not buildable, and finding out why is most of this decision.

## Decision

### 1. `join` is one row, on the generic receiver, with its item bounded to `Text`

`TypePattern::iterable(TypePattern::is_scalar("T", ScalarType::Text))`, at arity
one, lowering to `praxis_vec_join`. Every one of the ten pipeline receivers gets
it from that single row — a `Vec`, a `Set`, a `Map.keys()` — through
[ADR-127](./127-a-pipelines-source-is-the-for-loops-and-a-collection-converts-by-naming-what-it-becomes.md)
decision 3's materializing walk in front of the wrapper.

The bound rides on the *item* rather than being spelled as a second row, and the
two alternatives are both refused by machinery that already exists:

- **A generic `Iterable.join/1` beside a concrete `Vec[Char].join/1`** is a
  build-time panic. `MethodCatalogBuilder::finish` answers
  `AmbiguousWithIterable` for exactly this pair — both match `cs.join("")`, so
  which one resolves would be insertion order, and ADR-127 decision 6 refuses a
  precedence rule.
- **Two `Iterable` rows differing only in the item bound** compiled and passed
  every test in the workspace. `shadowed_by_iterable` returns `None` when both
  receivers are generic, the `Duplicate` check compares receivers with `==` and
  the two bounds are not equal, and `praxis_hir::catalog::lookup` matches an
  `Iterable` receiver on *shape* — so `hits.first()` would have picked the
  first-registered row and `cs.join("")` would have reported "expected `Text`,
  found `Char`". That is the same precedence rule, arriving silently instead of
  loudly.

The second is why this ADR also adds `MethodCatalogError::AmbiguousIterablePair`
and a check in `finish`. The state was representable and nothing refused it;
now it is a build failure, and no row in the table today is affected —
`count/0` and `count/1` differ by arity, which stays legal.

Spelling the bound on the item rather than writing `join` on a concrete
`Vec[Text]` is what makes the refusal readable. `["a"[0]].join("")` reports

```text
error[Y001]: expected Text, found Char
```

at the method name, rather than `Y110` on a receiver that plainly is a sequence.
And because `Bound::Is` *unifies* rather than merely permitting, `var v = [] ;
v.join(",")` pins `v` to `Vec[Text]` instead of reporting — the same property
`sum`'s `Int` bound has.

### 2. The separator is a required argument

The catalog has no optional arguments: a row's key is `(receiver, name, arity)`,
so `join()` beside `join(Text)` is two rows for one question. `join("")` is the
no-separator spelling, and it says at the call site which one the program meant.

### 3. A sequence of `Char` gets a differently-named row: `Vec[Char].to_text()`

Decision 1 leaves the `Char` case with nowhere to go under the name `join`, and
the answer is not to force it there. `chars.to_text()` is a better name for it
anyway: nothing goes *between* the characters, so there is no separator to pass
and `join("")` would have been ceremony.

The receiver is a concrete `Vec` and not `Iterable`, and that is deliberate
rather than incidental. `Text` is one of the ten pipeline receivers, so an
`Iterable.to_text/0` row would collide with the whole of
[ADR-143](./143-the-to-text-family-is-int-float-and-char.md)'s scalar family the
moment a `Text.to_text` was ever written, and would already shadow any concrete
sequence row. A `Vec` receiver is safe against all of it. It is also the honest
scope: a sequence of characters becomes a line because it has an *order*, which
a `Set[Char]` does not.

The element is a bounded variable for decision 1's reason, so `[1, 2].to_text()`
reports `expected Char, found Int` and `var v = Vec() ; v.to_text()` pins
`Vec[Char]`.

### 4. `join` renders nothing, and that is the boundary with ADR-143

A `Vec[Int]` is a type error at the item, not a sequence quietly stringified.
Had `join` accepted any element and rendered it, it would have been a universal
`to_text` under another name — the thing ADR-143 decision 4 declined to decide
as a rider.

The spelling is `ns.map(|n| n.to_text()).join(", ")`, and it says that it
renders. `Vec[Char]` separated by something is `cs.map(|c| c.to_text()).join("-")`
for the same reason; the un-separated grid line, which is the everyday one, is
one call.

## Consequences

**What is bought.** Drawing a grid is `for y in 0..g.height() { out(g.row(y).to_text()) }`,
which is how a grid puzzle is debugged. Building a line out of parts is
`parts.join(", ")` rather than a `+=` loop — and the book's own warning that
accumulating a long `Text` with `+` is quadratic now has a route that is not.

**What it costs.** Two rows, one new catalog error variant, and a name asymmetry
a reader has to absorb: `join` on ten receivers, `to_text` on one. The
asymmetry is the shape of the constraint rather than a preference, which is why
it is written down here and in the block comment above `vec_to_text`.

**Both wrappers are `AllocatesAndFaults`.** A well-typed program cannot hand
either a foreign element, so the fault path is a compiler bug reporting itself —
but declaring them `Allocates` would make MIR's `RedundantFaultCheck` forbid the
`CheckFault`, and the `abi_guard!` panic dummy would have nowhere to be
observed. `praxis_vec_sorted` is the precedent and it faults for the same
never-in-a-well-typed-program reason.

**No ABI version bump**, for ADR-143's reason: no `#[repr(C)]` type changed.

**`text.md` line 227 said `join` was deliberately absent** and is now false; it
is rewritten rather than deleted, since the sentence is one a reader quotes.

**The gates.**
`builtins::join_is_one_row_and_a_sequence_of_chars_has_its_own_name` pins the
shape, including the two negatives — one `join` row, and no `Iterable` row named
`to_text`.
`catalog::finish_rejects_two_generic_rows_at_one_arity` is the one that was red
for a state the table could previously hold; it is built from the exact `join`
pair this decision rejected.
`runtime::abi::vec_join_puts_the_separator_between_and_nowhere_else` and
`vec_to_text_renders_every_char` are the wrapper half, with the two
`refuses_a_non_*_element` tests for the fault path.
`jit::join_puts_the_separator_between_the_items` covers a `Vec`, a `Set`, the
empty sequence, and the `map(to_text).join` composition;
`jit::a_grid_row_renders_back_as_a_line` is the handover's own case, asserted as
a round trip through `read grid(char)`.
`stdlib/catalog-refusals` pins the two diagnostics in the book, so the wording a
reader is promised is executed.
