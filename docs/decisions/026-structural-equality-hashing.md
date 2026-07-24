# ADR-026: Structural equality & hashing via descriptors + internal capability check

**Date:** 2026-07-24 · **Status:** accepted

## Context

§5.5 requires tuples, records, and enums to receive compiler-generated equality
and hash procedures, emitted/referenced through their type descriptors (§11.4).
A type is hashable iff all recursively contained fields are hashable; functions
are never hashable. §4.8 forbids user-visible traits — the compiler owns a
closed internal table deciding which shapes support `==`, and capability failures
must be translated into concrete language terms (§5.4: never mention "trait" or
"capability").

Before this ADR, the `RECORD` and `ENUM` descriptors carried `equals: None` /
`hash: None`, and tuples had no runtime representation at all (the `TypedExpr::Tuple`
MIR lowering returned a Unit stub). The `==` operator worked only on Int/Bool via
native `IntCmp`. This left the §19.7 acceptance criterion "use tuples and records
as set/map keys" open (the eq/hash machinery; end-to-end keying needs M8
containers).

The proven reference implementation was already in the tree: the `VEC` descriptor's
`vec_equals` / `vec_hash` (`collections.rs`) recurse element-wise through the
per-instance element descriptor, short-circuiting on the first non-equal element.

## Decision

Implement structural equality and hashing in three layers:

1. **Tuple runtime representation.** Tuples were type-system-only (no payload,
   no descriptor, MIR stub). Add `TuplePayload { schema, items }` + `TUPLE`
   descriptor (TypeId 10) + `AllocKind::Tuple` + `praxis_alloc_tuple` /
   `praxis_tuple_set` / `praxis_tuple_get` ABI wrappers. The codegen builds a
   `TupleSchema` (element-descriptor sequence) and leaks it to `&'static` like
   `RecordSchema`. **Tuple schema cache is keyed by the resolved element-descriptor
   sequence, not by the static `Type`** — the type arena does not structurally
   intern tuples (each `db.tuple(...)` mints a fresh slot), so keying on the `Type`
   id would build distinct schemas for two `(Int, Int)` literals and break
   structural equality. Keying on descriptor pointers gives true de-duplication.

2. **eq/hash callbacks on RECORD / ENUM / TUPLE.** Each walks element/field wise
   through the per-shape descriptors, mirroring `vec_equals` / `vec_hash`. Records
   compare schema-pointer identity first (schemas are interned by shape), then
   field values. Enums compare tag, then payload items via each `GcRef`'s own
   descriptor (the variant's payload types are fixed by the tag). Hashing mixes
   arity/tag first, then each element/field, to distinguish prefixes and variants.

3. **Internal capability check + `==` lowering.** A new `praxis-hir/src/capability.rs`
   answers `supports_eq(db, t)` / `supports_hash(db, t)` recursively: scalars/Unit
   yes; functions never; composites iff all components do. `infer_bin` calls
   `supports_eq` for `==` / `!=` and emits `Y004` ("values of type `T` cannot be
   compared with `==`") — wording that never mentions trait/capability. The MIR
   comparison lowering branches on the operand type: composite `==` / `!=` →
   `Inst::StructEq` → `praxis_struct_eq(ctx, a, b)`; scalars and ordering ops stay
   on the native `IntCmp` path. `praxis_struct_eq` reads `a.descriptor()` and
   dispatches to its `equals` callback. `!=` lowers as `!(==)`.

## Consequences

- Records, tuples, and enums are now equatable and hashable when their components
  are; functions (and composites containing them) are rejected at compile time
  with a concrete diagnostic. This closes the eq/hash half of the §19.7 "keys"
  criterion; end-to-end set/map keying closes in M8.
- The tuple runtime is now first-class (allocation, tracing, format, eq, hash).
  Tuple element *access* (`.0` / `.1`) remains a follow-up — not required for the
  keys criterion.
- The capability check is internal only (§5.4): no source syntax, no user-visible
  trait names.
- ABI bumped to 4 (new `praxis_alloc_tuple` / `praxis_tuple_set` /
  `praxis_tuple_get` / `praxis_struct_eq`).
