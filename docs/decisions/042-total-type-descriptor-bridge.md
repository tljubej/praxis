# ADR-042: One total bridge between `Type` and `TypeDescriptor`; the JIT refuses rather than mislabels

**Date:** 2026-07-29
**Status:** Accepted
**Milestone:** Repair (foundation F11, stage S7 — P0-11, RT-10, RT-11; DBG-02 in part)
**Answers:** design decision D9
**Amends:** ADR-035 decision point 4, whose round-trip soundness claim was false;
ADR-013's "the element descriptor defaults to `INT`"; ADR-028's collection and
tuple equality

## Context

The static→runtime map and the runtime→static map were written independently,
in different crates, and were not inverses.

Forward (`praxis-codegen-cranelift/src/lower.rs`), three `_ => INT` arms:
`Float`, `Unit`, `Record`, `Enum`, a closure, `Range`, `Seq`, reserved `Byte`
and an unresolved type variable all became the `Int` descriptor. That descriptor
is not a label — it is the `equals`/`hash`/`format`/`trace` callback table a
record schema, a tuple schema and a collection payload dispatch through. A
`Float` field in a record schema meant `Int`'s equality callback reading an
`f64`; a `Unit` tuple element meant an `i64` load from a zero-sized payload.

Inverse (`praxis-debugger/src/evaluate.rs`): a hand-written integer match that
answered `Vec[Int]` for every vector and `Map[Int, Int]` for every map, because
it only ever looked at the top-level descriptor.

Each was locally defensible. Together they made ADR-035's claim — that the
static type id and the runtime descriptor agree — false in both directions.

## Decision 1: one crate owns both directions

`praxis-repr` holds `descriptor_for_type`, `type_for_value`,
`type_for_descriptor` and `element_descriptors_for`. It depends on
`praxis-types` and `praxis-runtime`; both codegen and the debugger already
depend on those, so it slots in with no cycle.

The forward map reduces to one exhaustive `Type → BuiltinTypeId` match plus the
runtime's own `BuiltinTypeId::descriptor()` table, so the two cannot drift. The
inverse dispatches on `BuiltinTypeId` too, through a new
`praxis_runtime::repr::instance_repr` that performs the payload reads once,
safely, and reports *what the value records about its own type*.

Colocation makes the round trip a test rather than a hope:
`every_builtin_value_round_trips` constructs a live sample of all twenty-one
built-ins and asserts `descriptor_for_type(type_for_value(v)) == v.descriptor()`
by pointer.

## Decision 2: the inverse walks live elements, not descriptors

`Vec[Text]` recovers as `Vec[Text]`, and a non-empty `Vec[Vec[Int]]` recovers
exactly — because the recursion follows a live element rather than the element
*descriptor*, which is `VEC` for every nested vector and therefore cannot
distinguish them.

Where a value genuinely does not record its type, the bridge says so instead of
guessing. A record or enum object carries a field schema, not which named type
it is (nominal identity is F12, S10); a closure records nothing about its
signature. The debugger falls back to the static `type_id` for those, exactly as
before.

## Decision 3 (D9): `Err` at a dispatch site fails the compile

The alternative — fall back to some descriptor — is the bug. A type with no
runtime object reaching a descriptor-producing site is an upstream compiler bug,
and refusing to emit is how it stays visible instead of becoming a wrong payload
read at runtime. The diagnostic names the type and the site (`record field
'x'`, `tuple element 2`, `collection type argument 0`).

Two exceptions, both because absence is *already* representable there:

- **Debug metadata** emits a null descriptor and `NO_STATIC_TYPE`, which is what
  `MirType::Opaque` locals already emit and what the debugger already renders as
  "no type column" (P0-02). A `Never`-typed local is the ordinary result of
  `return` or `panic()`, and refusing to compile a working program over
  incomplete debug info is not what D9 asked for.
- **A collection's element descriptor** may be null, which every `praxis_*_new`
  wrapper reads as "unknown element type".

## Decision 4: "cannot exist" and "not inferred yet" are different errors

`NoReprCause::NoSuchObject` vs `NoReprCause::Unresolved`. Only the second is
tolerated, and only where null is representable.

This is not softness — it is hazard H10. `let xs = Vec()` generalizes at the
`let`, so the construction site's own element type is never resolved; per-use
inferred types arrive with HIR-01/MONO-01 in S15. Failing the compile on an
unresolved variable today rejects `debug_backtrace.px`, which is to say it
rejects the fixture the whole crash debugger is tested against. `Vec[Range]` and
`Vec[Never]` remain hard errors.

## Decision 5: null is the encoding of "unknown", and `INT` is not

`VecPayload`/`DequePayload`/`GridPayload` keep a null element descriptor when
the constructor was not told one. Previously null was rewritten to `INT` at
construction, which is why an empty `Vec[Float]` reported that it held `Int`s,
and why `push` was given licence to *retag* a vector whose descriptor was `INT`
— including one that had been told `Int` deliberately.

`push` now **adopts** when the collection has no element type and **rejects**
(`FaultKind::TypeMismatch`) when it disagrees. Adoption is honest: the
`forall T. () -> Vec[T]` builtin genuinely has nothing to record at
construction. Retagging never was.

`Grid[T](w, h)` fills with the zero value of `T` rather than with `Unit` under a
`T` descriptor — the same lie one level down. Composite cell types have no zero
value the runtime can invent, so they raise `TypeMismatch` instead of being
filled with something of the wrong type.

## Decision 6: element type is part of collection identity; tuple shape is not schema identity

- **RT-10.** `Vec`/`Deque`/`Grid` equality compares element descriptors first.
  Two empty collections of different element types were previously equal, and a
  non-empty pair dispatched the *left* element descriptor's callback against the
  right's payloads.
- **RT-11.** Tuple equality compares shape slot by slot instead of comparing
  schema *addresses*. Three producers intern tuple schemas — codegen's cache,
  the runtime's `point_schema`, the input parser — so two `(Int, Int)` tuples
  could hold different pointers to the same shape and compare unequal. The slot
  descriptors now feed the hash as well, so `Eq` and `Hash` still agree.

## Consequences

- `descriptor_for_type` is fallible everywhere it is called. Four call sites
  propagate; one (debug metadata) degrades to null by design.
- `FaultKind` gains `TypeMismatch`. No ABI bump: generated code never switches
  on a fault kind and the enum's width is unchanged.
- Map/Set/Counter/heap payloads still hold a non-null `&'static` element
  descriptor and still default a null argument to `INT`. With the forward map
  fixed, that only bites when inference leaves the element type unresolved —
  the same S15 gap as everywhere else — but it is a remaining inconsistency.
- `empty_vec_float_has_the_float_element_descriptor_before_any_push` stays
  ignored: `let values: Vec[Float] = Vec()` never applies its annotation to the
  initializer, which is TY-08 (S13), not P0-11.
