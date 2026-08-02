# ADR-099: A `[a, b]` is a `Vec` literal, and a `Text` is the eleventh iterable

**Date:** 2026-08-02
**Status:** accepted — implemented
**Milestone:** post-M11 language surface
**Amends:** §4.11 (iterable shapes), §4.13 (`Text` behavior), §6.1 (collections).
Supersedes the standing gap `praxis-stdlib`'s own comment recorded for
`for c in text`.

## Context

Two programs that read like Praxis and were not:

```praxis
for c in [1, 2, 3] { out(c) }
```

```text
error[P001]: expected an expression
  2 |     for c in [1, 2, 3] {
    |              ^ expected an expression
```

```praxis
for c in "abc" { out(c) }
```

```text
error[Y005]: values of type `Text` cannot be iterated
```

They fail at opposite ends of the pipeline and are one gap: **the two things a
puzzle program most often wants to walk are a handful of literal values and the
characters of a line.** The `for` machinery was complete for both — ADR-066's
`IterPlan` and ADR-062's per-iterable clone had been in place for two
milestones — and neither had a way in.

The second was known and written down. `crates/praxis-stdlib/src/builtins.rs`
listed it under "deliberately absent":

> **`Text.chars()` and `for c in text`.** `for c in text` is `Y005` because
> `capability::iter_item` answers `None` for a scalar. A real gap — but it reads
> the same before and after ADR-086.

That is a true statement about ADR-086's scope and not a reason for the gap to
stay. ADR-086 is what makes closing it cheap: `t[i]` already answers a `Char`,
and `praxis_text_len`/`praxis_text_get` are already the pair that does it.

## Decision 1: a list literal is a `Vec`, spelled

`[a, b, c]` has type `Vec[T]`, and `[]` is `Vec[?T]` with the use deciding `?T` —
the same answer `Vec()` gives, because it *is* `Vec()`. The lowering is the
allocation a constructor call emits followed by one `praxis_vec_push` per
element, in source order.

So there is no new runtime wrapper, no ABI change, and no second kind of `Vec`.
A literal takes every `Vec` method, is subscriptable, is mutable afterwards, and
prints as a `Vec` — because there is nothing else it could be.

The alternative — an immutable "array" distinct from `Vec` — is a second
sequence type, and §2.3's priority order is why it is not taken: it would need
its own descriptor, its own catalog rows, its own `IterPlan` arm, and an answer
to what `[1] + [2]` means. `Vec` already exists and already answers all of them.

## Decision 2: the element type is a fresh variable, not the first element's

Inference mints one variable and unifies **each** element with it in turn,
rather than taking the first element's type as the expectation.

The two accept the same programs and report different ones. `[]` has no first
element, so the "first element" rule needs a special case for the empty literal
— and the empty literal is exactly the one whose type has to come from its use.
A fresh variable makes `[]` the ordinary case rather than the exception.

The **order** of each unification is `unify(element, el)`, so the type
established so far is `expected` and the offending element is `found`. `[1, "a"]`
reports "expected `Int`, found `Text`" *at the `"a"`* — REP-61's rule, which is
that the requirement goes first and what was written goes second.

## Decision 3: a `[` that begins an expression opens a list; one that continues an expression subscripts

This is REP-27's rule at the second bracket, and REP-27's own doc comment is what
says it was owed:

> Nothing in the grammar begins with `[` — there is no list literal — so a `[`
> after a line break can only continue the expression before it.

That premise is what this ADR removes. `m[k]` and `[k]` are the same two
characters, and their contents cannot break the tie any more than `Counter[Int]`
and `m[0]`'s could (ADR-065). Position is the whole rule:

```praxis
let n = total
[1, 2, 3]           // two statements, not `total[1, 2, 3]`
```

The cost is the mirror of REP-27's and is stated rather than hidden: a subscript
whose receiver ends a line and whose bracket begins the next is two expressions
now. No program in the corpus, the suite or the design doc is written that way,
and the fix is to move the `[` up. Every subscript any program actually writes —
`m[k]`, `grid[x, y]`, `m[k][j]`, `[1, 2][0]` — is on one line and unaffected.

Two parser tests asserted the *old* asymmetry, with the stale premise as their
comment. They were rewritten rather than deleted (§8.2): the subject is which
bracket continues an expression, and that is still the subject.

## Decision 4: `Text` yields `Char`, and it is the only iterable scalar

`capability::iter_item` answers `Char` for `Text`. Everything else follows from
the one door: `capability::check`'s `Iterable` arm, `resolve_deferred_iterable`'s
unification, the `for` binding's recorded type and `Y005`'s absence are all that
function's answer, and none of them needed a second arm.

**The item type is the decision, not merely that the loop is accepted.** A `Text`
that yielded `Text` would typecheck every body a `Char` does not, and the `for`
would then move `praxis_text_get`'s `Char` into a slot typed `Text` — the exact
shape REP-03's silent half was made of.

A `Char` is *not* iterable in turn. It is what iterating a `Text` produces, so
making it iterable would have no bottom.

## Decision 5: a `Text` iterates in place, and its accessors are the subscript's

`IterPlan` gains a `Text` arm naming `praxis_text_len` and `praxis_text_get` —
`InPlace`, not `Snapshot`, because a `Text` indexes itself in constant time. Four
of the eleven iterables now do.

They are the pair `t.len()` and `t[i]` already call, which is ADR-066 decision 4's
rule ("each iterable's order is its accessors'") reaching the one iterable that is
not a collection: `for c in t` and `t[i]` cannot disagree about what the *i*th
character is, because there is one function that answers. Indexing is by Unicode
scalar and not by byte for the same reason — the loop inherits it rather than
re-deciding it.

`iter_plan`'s dispatch is therefore on the static type's *shape* rather than on
its collection ctor alone. The exhaustive match over `CollectionCtor` is
unchanged and still has no default arm, which is ADR-066 decision 4's other half.

## Consequences

- **`for x in [1, 2, 3]` and `for c in "abc"` both run**, which is what the task
  asked for and what the two failing programs above now do.
- **No ABI version bump and no new runtime wrapper.** Both features are spellings
  for wrappers that already existed; `praxis_vec_new`, `praxis_vec_push`,
  `praxis_text_len` and `praxis_text_get` are unchanged.
- **No new diagnostic code.** A heterogeneous list is `Y001`, an unclosed one is
  `P001`, and a non-iterable is still `Y005`.
- **`Text` is the eleventh iterable**, and `iter_item`'s "scalars are not
  iterable" line is now "the *other* scalars are not iterable" — a distinction
  its test now makes, including for `Char`.
- **One `for` body still serves every iterable it is given** (ADR-062). A `Text`
  is one more clone with one more set of symbols, and `count("abc")` and
  `count(v)` in one program are two of them.
- **`Text.chars()` stays absent**, and for its original reason: `for c in t`
  expresses it, and two spellings for one question is what ADR-077 refused. What
  the `builtins.rs` note records now is that absence rather than the `for`'s.
- **A list literal is read-only for `p EXPR`** (ADR-034). Its allocation and its
  pushes are both its own — the only object either touches is the one the node
  just made — which is the read-only-ness `Range` already has. An element that
  mutates is still rejected, by the recursion.
- **The formatter needed no rule.** `[`, `]` and `,` were already tight in
  `needs_space`; a list literal inherits the comma spacing a call and a tuple get
  rather than getting a second answer.
