# ADR-046: A `Type` is minted by the arena, and a type constructor validates its own arguments

**Date:** 2026-07-29
**Status:** Accepted
**Milestone:** Repair (stage S11 — F5, TY-05, TY-07)
**Amends:** ADR-007's "`Type` is a copyable `u32` handle" (it still is — it is
no longer a *constructible* one); ADR-025's `EnumVariantDef` payload shape

## Context

`praxis_types::Type` was `pub struct Type(pub u32)`. The public field made a
forged arena handle a legal expression anywhere in the workspace, and that is
what P0-02 was: MIR's builder wrote `Type(0)` forty times to mean "I do not know
this local's type", and slot zero is a perfectly real type in whatever `TypeDb`
happened to be alive — usually `Int`. F16 converted those forty sites to
`MirType::Opaque`, which fixed the sites; nothing stopped the forty-first.

The constructors had the mirror problem. `TypeDb::intern` was public, so any
`TypeData` at all could be interned directly, and the shaped constructors did no
shaping:

- `db.tuple(vec![x])` interned a one-element `Tuple`. `TypeData::Tuple`'s own
  doc comment says a one-element tuple "never exists as data"; unification has
  no arm that can satisfy one, so the type flowed until something tried to
  compare it and reported a mismatch naming a type the user never wrote.
- `db.collection(ctor, args)` ignored `CollectionCtor::arity` entirely — the
  method existed and had **no caller**. `Map[Text]` and `Vec[Int, Text]` were one
  annotation away, and `praxis-repr`'s own test suite built a `Range[Int]`
  (`Range` is nullary) without anything noticing.
- `register_record` / `anon_record` / `register_enum` / `anon_enum` accepted
  duplicate names. `RecordDef::field` resolves by name and answers the first, so
  `struct P { x: Int, x: Text }` declared a record whose second `x` was
  unreachable rather than diagnosed.

And `EnumVariantDef.payload` was an `Option<Vec<Type>>` documenting `Some([])`
as equivalent to `None` — a documented equivalence that `unify` then rejected,
because the pair fell through its three-way match to a catch-all (TY-05).

The shared shape of all four: **the invariant was written down, and the type
could not hold it.**

## Decision 1: the arena is the only minter of a handle

`Type` and `VarId` have private fields and no public constructor.
`Type::from_raw` is `pub(crate)`, used by `TypeDb::intern` and `fresh_var`.

A caller that legitimately holds a raw `u32` — the debugger, which stores a
`Type::to_u32` in `DebugLocalMeta` and rehydrates it against the same `TypeDb`
later (ADR-035) — comes back through `TypeDb::type_from_raw(u32) -> Option<Type>`,
which checks the index against the arena it is asked of. Both debugger sites had
a hand-rolled bounds check *next to* the forged handle; one of them
(`evaluate.rs`) had none at all.

`VarId::as_type` is public and total: a `VarId` can only name a slot the arena
minted as a variable, so every variable *is* a type. It is a conversion, not a
forge.

## Decision 2: a constructor takes a shape that has already been checked

Four validated payloads, in `praxis_types::ctor`. Each is the *only* way to
reach its constructor:

| Payload | Refuses |
|---|---|
| `TupleElems::new(Vec<Type>)` | fewer than two elements |
| `CollectionArgs::{Nullary, Unary, Binary}` | a wrong-length argument list — the shape *is* the arity |
| `FieldSet::new` / `from_pairs` | a repeated field name |
| `VariantSet::new` / `from_pairs` | a repeated variant name |

`TypeDb::collection` re-checks the shape against `ctor.arity()`, because
`CollectionArgs::Unary` is legal to write and `Map` does not take it. Everything
else is infallible once the payload exists.

`TypeDb::intern` is `pub(crate)`. `register_record`/`register_enum` take
`Option<String>` for the name — `None` is the anonymous case — which is what
`anon_record`/`anon_enum` were, so the four constructors are two.
`EnumDef.name` became `Option<String>` to match `RecordDef.name`; the synthetic
`""` an anonymous enum carried was the same absence spelled as a value.

### Why validate here rather than at the syntax callers

The checks partly existed — in `praxis-input-parser`'s `validate`, and in the
resolver. But the constructors have callers that are not syntax callers: the
prelude seeding, the method catalog's `TypePattern` expansion, the parser
template synthesizer, and `praxis-repr` rebuilding a type from a runtime
payload. Each of those is a route that no syntax check covers, and two of them
were building illegal types today.

## Decision 3: an empty payload *is* the payload-less variant

`EnumVariantDef.payload` is a `Vec<Type>`. There is one spelling, so `unify`'s
three-way match is one length comparison, and the `.unwrap_or_default()` /
`map_or(Vec::new(), …)` mirrors at four call sites are gone with it. This is
TY-05, and it is a representation change rather than a fix: the bug was that two
representations existed for one thing.

## Consequences

**Two diagnostic codes are spent.** `Y007` names a wrong type-argument count at
the annotation that wrote it, and `Y008` names a duplicate field or variant at
the declaration. Both are cases the compiler previously either mis-reported
downstream or accepted silently. D13's block allocation for S13/S16 must treat
`Y007` and `Y008` as taken.

**The parser type synthesizer has an error channel.** `synthesize` returns
`Result<Type, TypeCtorError>`, threaded to a diagnostic in
`praxis-hir::parser_lower`. Its record and enum shapes take their *names from
user source* (`{x:int}` captures, `choice(...)` cases), so a duplicate is user
input, not an internal inconsistency — even though `validate` catches those
cases today. The two can now only disagree loudly.

**A malformed method-catalog row panics.** `pattern_to_type`'s collection arm
`expect`s: the catalog is compiler-authored data, and a wrong-arity row is a bug
in the compiler, not in the program being compiled. The standing sweep over the
catalog's invariants is S18's (RT-14/RT-15).

**A degenerate tuple resolves to what it actually is.** `tuple_or_degenerate` in
`praxis-hir` maps zero elements to `Unit` and one element to that element,
because neither is a tuple. The old code interned a one-element `Tuple` and let
it fail downstream.

**F5's sealing half was much smaller than the plan's L estimate**, and its
validated-constructor half was the work. F16 had already removed MIR's forty
`Type(0)` sites, leaving three forged handles workspace-wide — two debugger
rehydrations and one test helper in `exhaustive.rs`.

## Alternatives considered

**Keep `intern` public and validate only in the shaped constructors.** Rejected:
`intern` *is* the back door, and three crates were already using it to build
collections and tuples the shaped constructors would have refused.

**Return `Result` from every constructor.** Rejected: it puts a `?` at ~40 sites
whose arguments are statically the right shape (`db.vec(elem)`,
`db.pair(a, b)`), and an error nobody can act on is noise. The split is that a
*payload* is fallible to build and a *constructor* taking one is not, with
`collection` the single exception where the shape and the ctor can still
disagree.

**Make `Type` carry its `TypeDb`'s identity** so a handle from one arena cannot
be read by another. Rejected for now: it costs a word on a type whose whole
point is being a copyable `u32`, and the one cross-arena reader — the debugger —
is now explicit about which `TypeDb` it is asking. Worth revisiting if a second
arena ever outlives the first.
