# ADR-024: Provisional structural records in M6

**Date:** 2026-07-23 · **Status:** accepted

## Context

§7.8 type derivation produces anonymous records from named-capture templates:
`lines(`{x:int},{y:int}`)` derives to `Vec[{x:Int,y:Int}]`. Nominal records and
enums formally land in M7 (§19.7), and §11.4's record descriptor dispatch is
part of that same milestone's scope.

But M6's input parser needs record-typed *results* to be useful for AoC puzzles
— a parser that can only capture into scalars or tuples cannot model the
named-field rows these puzzles are built from. The §19.6 deliverable (named
captures with derived record types) and its hover/acceptance criteria require
both a runtime value that formats as a record and a derived result type, even
though the formal record/enum chapter is one milestone away.

## Decision

Ship a **provisional structural record** in M6, split across runtime and type
system:

1. **Runtime** (`praxis-runtime/src/records.rs`):
   - `RecordField { name: &'static str, descriptor: *const TypeDescriptor }` —
     a field's source name plus the descriptor for the values stored there.
   - `RecordSchema { fields: &'static [RecordField] }` — the ordered shape of
     one record type, leaked to `&'static` per parser plan (mirroring how the
     JIT leaks function-name strings).
   - `RecordPayload { schema: *const RecordSchema, items: Vec<GcRef> }` — one
     field value per schema field, in schema order.
   - A single `RECORD` descriptor (`TypeId(8)`, name `"Record"`) serves *every*
     record because the per-shape knowledge lives in the schema referenced from
     the payload. It dispatches `trace`/`drop_value`/`format` element-wise
     through the schema (§11.4), so there are no scattered type switches.
   - `RECORD` is marked **non-equatable / non-hashable for M6**
     (`equals: None`, `hash: None`). Structural equality and hashing arrive with
     M7's full record story.

2. **Type system** (`praxis-input-parser/src/synthesize.rs`): for M6, named
   captures synthesize to a **provisional tuple-based representation**
   (`record_type` interleaves name-types and field-types into a single tuple).
   `TypeData` does not yet have a `Record` variant, so the record cannot be
   represented at its own nominal type. This is acceptable because the result
   type is only used for display (hover) and the runtime shape is correct.

This is **explicitly provisional**. M7 replaces the type-system representation
with a proper `Record` `TypeData` variant, adds nominal records/enums, pattern
matching, and structural equality/hashing so records can be map/set keys
(§19.7 acceptance criteria).

## Reason

- The parser's plans already build `RecordSchema`s at runtime from child plans'
  result types (field names are stored as `&'static str` in the plan), so a
  runtime record value is cheap to produce and its `format` is correct without
  any type-system support: it reads `{ x: 42, y: 99 }` straight off the schema.
- A single `RECORD` descriptor keyed off the payload's schema keeps the
  descriptor table flat — one entry per *type-family* (rule 20.3: never
  duplicate type knowledge), with shape living in the value, not the type id.
- Deferring equality/hashing avoids designing the structural-equality story
  (which interacts with M7's nominal/structural distinction and map-key
  semantics) before it is needed. M6 parsers store records in `Vec`s only, so
  nothing in scope requires record equality.
- The tuple-based type representation is a deliberate, visible stand-in: the
  result type drives hover only, and a wrong-looking display is preferable to a
  half-built `Record` variant that M7 would have to keep compatible.

## Consequences

- M6 programs can parse named-capture templates and store the results in
  vectors; the record values format correctly (`{ x: 42, y: 99 }`).
- The **type display is provisional** (tuple-based) until M7 — the hover for a
  named-capture parser shows the interleaved tuple, not a record type. This is
  recorded here so it is not mistaken for a bug.
- Records are **non-equatable/non-hashable** in M6: they cannot be compared with
  `==` or used as map/set keys. Code that needs this must wait for M7.
- There is **no pattern matching on records yet** — programs access fields
  through the parser's structure (the interpreter assembles fields by
  `field_index`), not through field-access syntax, which is an M7 deliverable.
- M7 must migrate both representations together: the `Record` `TypeData`
  variant and the `equals`/`hash` callbacks on the `RECORD` descriptor.
