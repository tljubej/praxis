# ADR-069: A record and a tuple are patterns, and each has one constructor

**Date:** 2026-07-30
**Status:** Accepted — implemented
**Milestone:** Repair (stage S25 — REP-10)

## Context

```praxis
match p { P { x, y } => x }
```

`P001`, "expected `=>` in match arm". `parse_pattern` had four forms — `_`, a
literal, a name, and `Name(sub, …)` — and neither a record nor a tuple was one of
them. Two consequences followed, and only the first is obvious:

1. **There was no way to take a record or a tuple apart in a pattern.** A record's
   fields were reachable with `p.x` and a tuple's with `p.0` (REP-08), so the
   language was not blocked; but a `match` could only test the whole value.
2. **Their exhaustiveness signature was `Open`**, so `match p { P { x, y } => … }`
   was not merely unwritable — a `match` on a record or a tuple *needed a `_`*,
   whatever it wrote. `exhaustive.rs` lists them under "a type the checker cannot
   see into", beside an unresolved type variable.

The second is the one worth naming: the checker was already able to handle what
this ADR adds. Maranget's matrix handles a `Closed` signature with a single
constructor — that is what `Bool`'s two-literal signature already is, one
constructor smaller — so the fix is a parser and a lowering, and `exhaustive.rs`
gained two `Ctor` rows and no new case.

## Decision 1: the grammar is one production per composite, and the fields are a node

```text
pattern := "_" | literal | Ident
         | Ident "(" [pattern ("," pattern)*] ")"          // variant
         | Ident "{" [pattern_field ("," pattern_field)*] "}"  // record
         | "(" pattern ("," pattern)* ")"                  // tuple
pattern_field := Ident [":" pattern]
```

A record pattern's `{` is unambiguous where a record **literal**'s is not
(FE-06/ADR-050): a pattern is followed by `=>` or by `in`, never by a block, so
nothing else can be waiting for that brace. No suppression flag is involved and
none is needed.

A field is its own node kind, `PATTERN_FIELD`, rather than the `FIELD` a struct
declaration and a record literal share. Those hold a type and an expression; this
holds a *pattern*. The gain is that `P { x }` and `P { x: p }` are one node shape
with an optional child, so pairing a field with its sub-pattern never has to count
identifier tokens — and `P { x }` can never be read as `P(x)`, because a variant's
sub-patterns are bare `PATTERN`s and a record's are not.

**Parentheses in pattern position are always a tuple.** There is no grouping form,
because a pattern has no precedence to override, so `(p)` is a one-element tuple
pattern — and `TypeData::Tuple` carries two elements or more, so `Y123` reports it
against every type rather than silently accepting a paren the program did not need.
`()` is a parse error for the same reason: `Unit` is not a tuple.

## Decision 2: one constructor each, which is what makes a `_` unnecessary

`signature` answers `Closed(vec![Ctor::Tuple])` for a tuple and
`Closed(vec![Ctor::Record { def }])` for a record. So:

- `match p { P { x, y } => x + y }` is exhaustive with **no `_`**, and a `_` after
  it is `Y121` — the two halves of the same fact.
- `match p { P { x: 1, y } => y }` is `Y120`, and the witness names the shape:
  `` `P { x: _, y: _ }` ``. The recursion goes *through* the new constructors; it
  does not stop at them.
- An `Option[(Int, Int)]` is exhaustive at `Some((a, b))` and `None`, which is
  HIR-06's payload recursion meeting the new shapes.

The record's def travels in the `Ctor` for the reason a variant's does: a pattern
naming *another* record — which only an already-reported mismatch can produce —
must match nothing rather than collide with this column's constructor.

## Decision 3: a record pattern's sub-patterns are positional, and naming fewer is legal

`TypedPattern::Record` carries `subpatterns` indexed by the record's **declared
field order**, padded with `Wildcard`, exactly as `EnumVariant` carries a payload.
Two things fall out and both are deliberate:

- **MIR reads slot *i* for sub-pattern *i***, so the decision tree needs no field
  names and the three composite readers differ only in which instruction they
  emit: `EnumPayloadGet`, `LoadTupleElem`, `LoadField`. They are three because they
  are three *runtime symbols* — a record field and a tuple element already lower
  differently (REP-08) — and the walk that chains them is one, because the chaining
  is identical and three copies of it is three places for a fall-through to go
  missing.
- **A field the pattern does not name is a wildcard**, which is HIR-06's padding
  rule at a second kind of composite: `Some`, `Some(_)` and `Some(n)` are one test
  for the same reason `P { x }` and `P { x, y: _ }` are. The one-sidedness is the
  same too — a field the record *does not have* is reported, and it is the
  literal's own `Y114`, because it is the literal's own mistake read in the other
  direction. A field named twice is `Y115`: the second sub-pattern would silently
  replace the first, so one of the two bindings the program wrote would never
  happen.

A wildcard component is now **not read at all** — the load is skipped, not emitted
and ignored. That is what makes the padding free, and it applies to an enum
payload too: `Some` reads no payload where `Some(n)` reads one. Only a `Wildcard`
may be skipped; a `Bind` matches anything as well, but it needs the value.

## Decision 4: the head of a record pattern is a type name, checked like a literal's

`P { … }`'s `P` resolves through the ordinary name-reference path, so an undefined
one is `N001` — the same answer `let p = P { x: 1 }` gives. That is a departure
from the *variant* pattern's rule, which deliberately leaves an unresolved
constructor to the type level (`Y122`), and the difference is that a variant name
is ambiguous with a binding while `Name {` in pattern position is not: nothing else
it could be.

Inference then unifies the scrutinee with the head's type, so a pattern for another
record is the ordinary `Y001` reported where it is written, and a head naming
something that is not a record at all is `Y123` — with the reason spelled out,
because the shape is what is wrong and not the type it was matched against.

A tuple pattern has no head to name, so it **pins** instead: the elements are fresh
variables unified through the scrutinee, which is what makes
`fn first(t) { match t { (a, b) => a } }` infer `forall T U. ((T, U)) -> T` rather
than reporting. A wrong arity is then the same unification's mismatch.

## Consequences

- **`for (k, v) in m` is still not spelled**, and it is no longer a grammar
  question: the pattern grammar this ADR adds is the one a `for` binding would
  reuse. What is left is the *binding position* — `for` takes an `Ident` token, and
  giving it a pattern means an irrefutable destructuring in the loop header, a
  refutable one to report, and `TypedExpr::For`'s `binding: SymbolId` to reshape.
  ADR-066 decision 3 left the destructuring half here; this ADR leaves the header
  half to a row of its own, and the reason is scope rather than doubt.
- **`TypedPattern::sub_patterns` is written once.** Three walks wanted it — the
  usefulness matrix asks it twice and MIR's decision tree once — and each named
  `EnumVariant` by hand, which is how a new composite pattern silently becomes a
  catch-all in all three at once.
- **No new diagnostic code was spent.** `Y114`, `Y115` and `Y123` already name
  these mistakes; `Y022`, `Y116`, `Y125` and `N008` remain the next free codes in
  ADR-051's blocks.
