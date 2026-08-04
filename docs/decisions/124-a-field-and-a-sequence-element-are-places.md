# ADR-124: A field and a sequence element are places, and a store replaces what is already there

**Date:** 2026-08-04
**Status:** Accepted — implemented

## Context

```praxis
struct Foo { x: Int, y: Int }
var foo = Foo { x: 1, y: 1 }
foo.x = 5     // Y021

var vec = [1, 2, 3, 4, 5]
vec[0] = 100  // Y020
```

Both were reported, and both reports were accurate about the implementation
rather than about the language.

`Y021` — "the left side of an assignment must be a name or an index" — was
ADR-064 Decision 4's answer to a target that names no storage, and a field was
inside it because no field store existed: `Point { x: 5, y: p.y }` was the only
way to change one field, in a language whose §4.2 already says a `let` binding
"may still point to a mutable object" and whose §4.5 records are exactly such
objects.

`Y020` on `vec[0] = 100` was ADR-064 Decision 2, which named the gap where it
made it: "giving `Vec` and `Deque` element stores is a *feature*, registered as
its own finding rather than smuggled in here". This is that feature.

## Decision 1: `Vec` and `Deque` get an `INDEX_STORE` row, and `Text` does not

The subscript surface is a catalog table (ADR-064 Decision 1), so adding a
receiver is adding a row — `praxis_vec_set` and `praxis_deque_set` beside the
`Map`/`Counter`/`Grid` stores that were already there. Six collections read;
**five** now store.

`Text` is the one reader left without a store, and that is §4.3's answer rather
than an omission: a `Text` is an immutable UTF-8 payload, so `t[0] = c` has
nothing to write through. `not_index_assignable`'s message — "values of type
`Text` cannot be assigned through 1 index(es)" — is now exactly about the type
it names, where before it had to describe `Vec` too.

**A sequence store replaces and never appends.** `v[v.len()] = x` is
`IndexOutOfBounds`, not a push. A store whose index decides between "replace"
and "grow" turns an off-by-one into a longer vector instead of a report, and
`push` already exists for the caller who meant to grow one. The gate is the
wrapper the row names: a catalog row pointing at `praxis_vec_push` would spell
`v[i] = x` and mean `v.push(x)`, so the test asserts the two lowerings differ
and that the store faults.

Both wrappers reconcile the element descriptor through the same
`adopt_or_reject` every push goes through, so a `Vec` that was never told its
element type adopts the first stored value's and an explicitly typed one raises
`TypeMismatch` rather than retagging itself (P0-11's rule, at a second door).

## Decision 2: a field store is its own statement, not a third catalog row

A subscript is dispatched on the receiver's *shape and arity*, which is what a
catalog table is keyed by. A field is selected by **name against one record
definition** and lowers to a slot index. That is the same distinction ADR-077
draws between `p.x` and `p.len()`, and it is why `TypedStmt::FieldAssign` sits
beside `TypedStmt::IndexAssign` rather than inside it.

Three things follow from carrying the receiver and the slot rather than a row:

- **The slot comes from `lower_field_place`, which the read uses too.** A store
  that derived its own index could disagree with the read beside it in
  `p.x += 1`, and both would still write *a* field. One function is what makes
  that unrepresentable.
- **Inference goes through `infer_field_get`.** So a store is exactly as generic
  as a read: `fn bump(q) { q.x += 1 }` defers on REP-28's `HasField` channel and
  is answered by whatever a call site puts in `q`'s place, and a receiver with no
  such field is `Y112` from the read's requirement rather than a second report in
  different words. A store that resolved the field itself would have been the
  TY-29 mistake — an answer thrown away at generalization and never re-asked.
- **`min=`/`max=` are refused, and not with `Y020`.** §6.2's updating stores mean
  "an absent entry accepts the first value"; a field is always present, so there
  is no operation to name. `Y016` ("`min=` is not defined for `Int`") is the
  shape of that — an operator a type does not have — where `Y020` would be about
  a subscript nobody wrote.

The numeric rule is `infer_assign`'s rather than `infer_place_assign`'s: the
requirement rides on the **target's** type, with the one exception ADR-085 gave a
binding — `+=` on a `Text` field is concatenation and needs no number.

## Decision 3: `StoreField` is an instruction, because the slot index is an immediate

`praxis_record_set_field` already existed — record construction fills its fields
through it — but MIR had no way to *call* it after construction: the index is a
`RawU32`, and MIR's `Call` passes `Gc` locals. Boxing the index to ride the
argument list is precisely what `LoadField` exists to avoid, because it puts an
integer in a slot the collector may dereference (P0-03).

So `Inst::StoreField { record, field_idx, value }` mirrors `Inst::LoadField`, and
takes `Inst::StoreScalar`'s standing in the passes that ask what an instruction
does: it writes *into* an existing object, so it **defines no local** and both
operands are uses. Neither half is a safepoint and neither is followed by a
`CheckFault` — both wrappers are `Effect::Pure`, and the verifier rejects a check
after an instruction that cannot fault as redundant (ADR-088).

## Decision 4: a compound operator evaluates its place once

`p.x += 1` is not desugared into `p.x = p.x + 1`, for ADR-064 Decision 3's
reason: MIR lowers each `TypedExpr` where it stands, so the desugared form names
the receiver twice and `pick(log).x += 1` would call `pick` twice. The receiver
is lowered once into the local both the `LoadField` and the `StoreField` name.

The arithmetic goes through `lower_compound_arith`, which is the same function
the binding and subscript paths use — so a `Float` field takes the float channel
and a `Text` field takes `praxis_text_concat`, without REP-64 having to be
rediscovered at a third site.

## Consequences

- **`Y021`'s message changes** to "the left side of an assignment must be a name,
  a field, or an index". `f() = 1` and `a + b[0] = 1` still report it; `p.x = 1`
  no longer does.
- **A tuple element is still not a place.** `t.0 = 1` is `Y021`. `praxis_tuple_set`
  exists, so the lowering would be a short step, but a tuple is the language's
  structural value — §5.5's map key, §6.4's coordinate — and giving it in-place
  mutation is a decision about identity that no finding asks for. A record is
  already mutable and hashable, so the hazard is not new; extending it is what
  this declines to do quietly.
- **`p` rejects a field assignment** (ADR-034), with the message the indexed
  store gets: the write outlives the expression, which is a stronger reason than
  a local assignment's.
- **The design document is amended** in §4.5 (a field is an assignable place) and
  §6.2 (which receivers a subscript reads and writes, and that a sequence store
  replaces). Both fences are executable and covered by the design-doc gate.
- **No diagnostic code is spent.** `Y022`, `Y116`, `Y126` and `N009` are still
  the next free ones (ADR-051), and that is itself the evidence that both
  features reuse reports which already existed rather than inventing a vocabulary
  for a surface the language already had words for.
