# ADR-010: Method catalog bridge in M2; `.method()` dispatch deferred to M5

**Date:** 2026-07-23 · **Status:** accepted

## Context

M2's deliverables include "basic method catalog lookup" (§19). The full method
*dispatch* — resolving `vec.push(x)` at a call site to a `MethodCatalog` entry
and checking the argument types — requires collection types (`Vec`, `Map`, …),
which are an M5 deliverable. In M2 the `TypeDb` has no representation for
collections yet, so there is no receiver type to look up against.

## Decision

Ship the **bridge** (`praxis-hir::catalog`) in M2: a `Type → TypePattern`
conversion and a `lookup(db, catalog, receiver, name, arity)` helper that queries
the existing `praxis-stdlib` `MethodCatalog`. Exercise the integration at the
library level (scalar receivers return no methods; type variables select
nothing). Defer the *expression-level* dispatch (resolving `expr.method(args)`
during inference) to M5, where collections exist in `TypeDb` and the bridge can
map `Vec[Int]` → `Vec[T]`.

## Reason

- Rule 20.3 ("never duplicate type or method knowledge"): the bridge is the
  single point where the inference type system and the catalog's `TypePattern`
  shape language meet. Building it now proves the two vocabularies align and
  keeps M5 from reinventing the join.
- M2 has no collection types to dispatch on; pretending otherwise would mean
  stubbing `Type::Collection` before its semantics exist.
- The library-level tests pin the bridge contract so M5 can drive it from the
  expression layer without renegotiating the integration.

## Consequences

- `praxis-hir` depends on `praxis-stdlib` (it already did transitively; now it is
  direct, for the catalog types).
- `.method(...)` calls in M2 programs do not yet type-check the receiver (the
  callee resolves as a name if one is in scope, else `N001`). This is recorded
  as a known M2 limitation in the handover; the `.` token and field/method
  syntax parse but structured dispatch is M5+.
- The internal capability system (§5.4: `Numeric(T)`, `Iterable(T, Item)`, …) is
  likewise deferred to M7, where records/structural operations first need it.

## Superseded (2026-07-29, ADR-057)

The last consequence — "the internal capability system (§5.4) is likewise
deferred to M7" — has lapsed. M8 pipelines, M9 `Option` and ADR-037 floats all
landed against it, and ADR-026 assumes hashability is *enforced*. The deferral
is superseded by [ADR-057](./057-a-capability-requirement-rides-on-the-scheme-that-quantified-it.md),
which lands the constraint channel: capabilities are a `CapKind` vocabulary in
`praxis-stdlib`, a `Constraint` carried by the `Scheme` that quantified the
variable, and one exhaustive `praxis_hir::capability::check`.
