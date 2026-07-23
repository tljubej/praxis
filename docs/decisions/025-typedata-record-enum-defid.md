# ADR-025: TypeData record/enum via def-id indirection

**Date:** 2026-07-24 · **Status:** accepted

## Context

§19.7 (Milestone 7) adds nominal records, enums, anonymous structural records,
pattern matching, and closures. The static type system ([`TypeData`]) needs to
represent two new shapes: records (§4.5 nominal, §5.6 anonymous structural) and
enums (§4.6).

`Type` is a copyable `u32` handle into the `TypeDb` arena (ADR-007). A record or
enum definition carries a non-trivial payload — an ordered field list (`x: Int,
y: Int`) or an ordered variant list (`Empty | Number(Int)`). Inlining this
payload into `TypeData` would make the enum recursive (a record field's type can
itself be a record), inflating every arena slot and complicating the four
recursive type walks (unify, lower_levels, occurs, generalize, instantiate).

## Decision

Represent record and enum types with **def-id indirection**:

- Add two new `TypeData` variants: `Record { def: RecordDefId }` and
  `Enum { def: EnumDefId }`. `RecordDefId`/`EnumDefId` are `u32` indices into
  side-tables on `TypeDb`.
- `TypeDb` gains `record_defs: Vec<RecordDef>` and `enum_defs: Vec<EnumDef>`.
  `RecordDef { name: Option<String>, fields: Vec<RecordFieldDef> }` (anonymous
  records carry `name: None`); `EnumDef { name, variants: Vec<EnumVariantDef> }`.
- The four recursive walks (unify_concrete, lower_levels, occurs,
  generalize_walk, instantiate_walk) recurse into record fields and enum-variant
  payloads through the side-tables. `Collection` was also added to
  `instantiate_walk` (it was missing — a latent M5 bug).
- Constructors: `register_record(name, fields)`, `register_enum(name, variants)`,
  `anon_record(fields)`, `record_type(def)`, `enum_type(def)`.

### Anonymous record identity

Two anonymous records are the same type iff their field-name sets match and
their field types unify (§5.6). Each `anon_record` call mints a fresh def
(mirroring how `tuple`/`func` work); identity is established through
**unification**, which matches fields by name and links the two defs. This avoids
the unsoundness of construction-time dedup (which would share a def-id and skip
field-type unification).

### Nominal record identity

Two nominal records are the same type iff they share a def-id. Each
`register_record` call mints a fresh def, so two `struct Point { x: Int }`
declarations are distinct types (nominal, §4.5).

## Reason

- Def-id indirection keeps `Type` a cheap copyable `u32` and keeps arena slots
  uniform-size. The heavy data lives in the side-tables, accessed only when a
  walk needs field/variant detail.
- It avoids recursive `TypeData` size (a record field's type is a `Type` handle,
  not an inline `TypeData`).
- It mirrors how the runtime already separates type identity from shape: the
  runtime `RECORD` descriptor (ADR-024) serves every record shape via a payload
  schema pointer, just as `TypeData::Record` serves every record type via a
  def-id.

## Consequences

- M7-WS1 migrates the provisional anonymous-record representation (ADR-024's
  interleaved tuple) to the real `Record` variant. The input parser's
  `synthesize.rs` now produces proper `TypeData::Record` types.
- The resolver goes two-pass (pass 1 registers top-level names, pass 2 resolves
  bodies/annotations) so user `struct`/`enum` type names resolve before any
  annotation is checked. `KNOWN_TYPE_NAMES` is retired in favor of scope-based
  lookup; built-in scalars are seeded as `Builtin` symbols.
- ADR-024's provisional representation is **superseded** for the type system.
  The runtime `RecordPayload`/`RecordSchema` from ADR-024 remain — they are the
  value representation, orthogonal to the type representation.
