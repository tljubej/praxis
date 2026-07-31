# ADR-082: `find` answers the element, `position` answers the index, and a miss is `None`

**Date:** 2026-07-31
**Status:** Accepted — implemented
**Milestone:** Repair (REP-39)

## Context

§6.3 lists `find` and `position` as two of the sequence operations. They were
one operation with two names.

```praxis
let v = Vec()
v.push("alpha")
v.push("beta")
out(v.find(|s| s == "beta"))   // 1
```

Both catalog rows were `result: Int` with the doc "Index of the first matching
element, or -1 on miss", `position`'s adding "(alias of find)". In MIR they
shared a single `Sink::Find(_) | Sink::Position(_)` arm that seeded one
accumulator to `-1` and stored the counter into it on a hit. So a program could
not get an element out of `find` at all: the receiver above holds `Text` and the
method's result type is `Int`, whatever the element type is.

ADR-029 decision 5 decided the `-1` miss sentinel and decided nothing else. It
never decided that `find` answers an index — that is an implementation shortcut
that survived into the catalog and then into the documentation string.

Two things make this the moment to settle it. §4.7 already says what absence is,
and ADR-076 made the machinery real: `TypePattern::Option`, one canonical
`Option` def, and a runtime `option_schema` whose payload slot is unknown so the
value's own descriptor answers. And the sentinel is *in band*: `-1` is a legal
element of a `Vec[Int]` and a legal index of nothing, so `v.find(|x| x < 0)` on
`[10, -1, 30]` and `v.find(|x| x > 100)` on the same vector both answered `-1`.

## Decision 1: `find` answers `Option[T]` — the element, not its index

`Vec[T].find((T) -> Bool) -> Option[T]`, and the same on `Seq[T]`.

This is the reading §6.3 supports. `find` and `position` are listed separately;
if `find` answered an index the list would name one operation twice, and the
catalog's own doc string admitted as much by calling one an alias of the other.
It is also the only reading under which the operation is useful at a non-`Int`
element type, which is most of them.

The MIR sink carries a `Gc` accumulator and a seen-flag instead of a seeded
scalar. The seen-flag is the same one `min`/`max`/`reduce` already use for a
sink that has no answer until it has seen an element; the difference is that
`find` *does* have an answer for the empty sequence, and that answer is `None`
rather than the `EmptyCollection` fault D1 chose for `min`. Those are different
situations and ADR-076 already drew the line between them: a `find` that matches
nothing is ordinary domain-level absence, and an empty `min` is a caller
mistake.

## Decision 2: `position` keeps the index and answers `Option[Int]`

`Vec[T].position((T) -> Bool) -> Option[Int]`.

`position`'s question is "where", and the answer to "where" is an index. What
changes is only how a miss is spelled. Leaving `position` at `-1` while `find`
became an `Option` would have kept the in-band sentinel for exactly the type
where it is most confusable — `-1` is a perfectly ordinary `Int`, and a caller
who forgets the sentinel gets a plausible-looking index rather than a type
error.

So both search sinks answer `Option`, both stop at the first match, and the
whole difference between them is what they record when they stop: the element,
or the counter. That is one line of MIR apart, which is the right size for the
difference between two operations the design lists separately.

## Decision 3: the `-1` sentinel is retired, not kept as a second spelling

There is no `find_index`, and no overload that answers a bare `Int`. ADR-029
decision 5 is superseded rather than narrowed.

Keeping it would reintroduce the whole defect at one remove: a caller who wants
a number reaches for the sentinel form, and the sentinel form cannot distinguish
a hit from a miss any better than it could before. `match`ing an `Option[Int]`
is two more tokens and says which case is which.

## Consequences

Four `jit.rs` tests asserted the old contract and are **rewritten, not deleted**
(plan §8.2), each with the assertion it used to make and why that was wrong:
`pipeline_find_returns_index_on_hit`, `pipeline_find_returns_neg1_on_miss`,
`pipeline_position_is_alias_of_find` — whose *name* was the finding — and
`pipeline_empty_vec_find_is_neg1`.

Three further tests use `position` to measure per-stage index semantics
(ADR-071) and are untouched in substance: the index they measure now arrives
inside a `Some`, so each grew a `match`. That is the intended cost, and it is
the same cost every `Map.get` call site paid under ADR-076.

`find_reaches_a_non_int_element` is the assertion no arithmetic could make
before: a `Vec[Text]`'s `find` answers the `Text`.
