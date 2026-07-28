# ADR-038: Built-in type identity is derived, and descriptors are `static`

**Date:** 2026-07-28
**Status:** Accepted
**Milestone:** Repair (foundation F1, stage S1)
**Supersedes:** ADR-028's "Identity by `TypeId`, not pointer" paragraph; the
"TypeIds 6–19" phrasing in ADR-028, ADR-024, ADR-026, ADR-027 and ADR-030 is now
descriptive of `BuiltinTypeId` discriminants rather than of hand-written literals.

## Context

Every runtime object reaches its payload-aware operations through a
`TypeDescriptor` (§11.4), and `TypeDescriptor.id: TypeId` was documented as
being type identity. Both halves of that claim were false in the tree:

1. **Ids were hand-written per descriptor.** Twenty-one descriptors each wrote
   `id: TypeId(N)` as a free integer literal. `FLOAT` (scalars.rs) and `TEXT`
   (text.rs) both wrote `TypeId(5)`, so the two types were indistinguishable by
   the field that was supposed to distinguish them — and every proposed
   `descriptor().id != X.id` guard was a verified no-op for exactly that pair.
   Nothing in the type system prevented `Float`'s descriptor from being labelled
   `Text`; the invariant was a convention maintained by hand across eleven files.

2. **Descriptors had no stable address.** Each was
   `pub const X: &TypeDescriptor = &TypeDescriptor { … }`, a const-promoted
   rvalue with no guaranteed unique address. ADR-028 drew the correct conclusion
   from that premise — compare by `TypeId`, never by pointer — but the premise
   is a choice, not a constraint, and it left the runtime with *no* cheap
   identity test, since the ids were not unique either.

The debugger compounded both: `descriptor_id_to_type` was a hand-written match
on integer literals that had drifted out of sync with the descriptors. It mapped
`TypeId(0)`/`(1)`/`(4)` — actually `Unit`, `Bool` and `Char` — to `Int`, `(2)`
(actually `Int`) to `Bool`, `(3)` (actually `Byte`) to `Char`, and swapped
`MinHeap` with `MaxHeap`.

## Decision 1: `TypeId` is derived from a closed `BuiltinTypeId` enum

`BuiltinTypeId` is the registry. `TypeDescriptor::builtin::<P>(b, …)` is the
only constructor for a built-in and derives `id` from `b`, so id uniqueness
reduces to enum-discriminant uniqueness, which rustc already enforces. The `id`
field is private; `TypeId`'s inner word is private; no integer literal can be
written anywhere in the workspace.

`BUILTINS: [&TypeDescriptor; BuiltinTypeId::COUNT]` is the lookup table, and
`BuiltinTypeId::descriptor()` / `TypeId::as_builtin()` are the two directions of
the correspondence. `builtins_are_indexed_by_their_id` asserts the array is
positionally correct; a new variant without an entry is a compile error on the
array length.

The alternative considered and rejected was the one the audit proposed: keep
`id: TypeId::builtin(BuiltinTypeId::Float)` written per descriptor plus a
`const _: () = assert!(unique)` over `BUILTINS`. That leaves the field writable
(so the `Float`-labelled-`Text` state remains representable) and the const
assert cannot compile anyway — descriptors must be `static` for decision 2, and
const-eval may not read a `static`'s value. The array check is therefore an
ordinary `#[test]`.

`size` and `align` are derived the same way, from the payload type parameter
`P`, so "a descriptor whose size disagrees with its payload" is likewise
unrepresentable.

## Decision 2: built-in descriptors are `static`, not `const`

Each descriptor becomes `pub static X: TypeDescriptor = …`. A `static` has one
address for the whole program, so `core::ptr::eq(a, b)` is now an authoritative
identity test and `TypeId` is for diagnostics, the debugger's exhaustive match,
and readability at comparison sites.

This **supersedes ADR-028's "Identity by `TypeId`, not pointer"**: its premise
(const descriptors may be duplicated across crate boundaries) was accurate for
`const`, and forcing single instantiation removes it rather than working around
it. Existing `TypeId`-equality comparisons in the ABI are left as-is — they are
now merely one of two correct spellings — but new identity checks may use
pointer equality, which is what the record/tuple/collection equality work
requires.

The cost is one `&` at each use site (`heap.alloc(&scalars::INT, …)`), which is
mechanical and compiler-checked.

## Decision 3: `compare` is declared now, populated later

`TypeDescriptor` gains `compare: Option<CompareFn>` alongside `equals` and
`hash`, so ordering becomes a descriptor operation like the other two. Every
descriptor declares `None`: **which types are orderable, and where NaN sorts,
are open language questions** (they amend ADR-026's ordering sentence) and are
not decided here. Declaring the field now means the answer, when it lands, does
not have to touch all twenty-one descriptors again.

## Consequences

- `Float` and `Text` are distinguishable at runtime for the first time; every
  downstream id-based guard can now mean something. This was a hard barrier: an
  id guard landed before this change would have shipped a green test that
  asserts nothing.
- `descriptor::tests::builtin_type_ids_are_globally_unique` is un-ignored and
  passing.
- The debugger's descriptor→type recovery is an exhaustive `BuiltinTypeId`
  match with no catch-all arm, which fixed the six mis-mappings listed above; a
  new built-in is now a compile error there. Recovering a collection's *real*
  element type (rather than defaulting to `Int`) still requires the
  bidirectional `Type ⇄ TypeDescriptor` bridge and is not addressed here.
- Test descriptors go through `TypeDescriptor::for_test`, whose ids are at the
  top of the `u32` range by construction, so a fixture can never collide with a
  real type.
