# ADR-074: An enum value records which enum type it is

**Date:** 2026-07-31
**Status:** Accepted — implemented
**Milestone:** Repair (stage S18 — RT-13)

## Context

```rust
#[repr(C)]
pub struct EnumPayload {
    pub tag: u32,
    pub items: Vec<GcRef>,
}
```

That was the whole of a runtime enum value. A single `ENUM` descriptor serves
every enum in the language — the per-type knowledge is meant to live in the
payload, exactly as `RecordSchema` and `TupleSchema` do — and for enums it lived
nowhere at all. Three consequences followed directly:

- `enum_equals` compared the tag and then the payloads, so `Colour::Red` and
  `Light::Red` were the same value, and `enum_hash` put them in the same bucket.
- `enum_format` wrote `<variant 1: 3>`, its own comment saying "Without the
  compile-time variant names".
- `InstanceRepr` reported `Unrecorded("an enum value does not record its nominal
  identity")`, so the debugger could not name what it was looking at.

ADR-048 anticipated this and said so in its Consequences: "The runtime half is
not here. `EnumSchema`, `EnumPayload`'s schema pointer and `praxis_alloc_enum`'s
new parameter are RT-13, which the plan schedules in S18." This is that half.

The immediate practical pressure was D1 (ADR-076). `Map.get` answers `Option[V]`
now, and the `Option` it answers is built by the *runtime*, whose only knowledge
of `V` is the value it found — while the arms it must match against were compiled
from the *codegen's* `Option[Int]`. Two producers, one type. Without a schema
there was nothing for them to agree on.

## Decision 1: the schema is `SchemaIdentity` plus a variant list

```rust
#[repr(C)]
pub struct EnumVariantShape {
    pub name: &'static str,
    pub payload: &'static [*const TypeDescriptor],
}

#[repr(C)]
pub struct EnumSchema {
    pub identity: SchemaIdentity,
    pub variants: &'static [EnumVariantShape],
}
```

`SchemaIdentity` is **reused from `records.rs`**, not copied. It already draws
exactly the distinction enums need — a declared type is its name, a structural
one is its shape (§5.6) — and the input parser's `choice` (§7.5) synthesizes an
anonymous enum, so both arms have an enum client.

There is deliberately **no separate `name` field** beside `identity`, which the
recon sketch proposed. `SchemaIdentity::Nominal` already carries the declared
name and `Anonymous` says there is none; a second copy could only disagree with
the first.

`same_type` compares the identity, then every variant's name and payload arity,
then every payload slot. The shape check rides along with the name for the reason
`RecordSchema::same_type` gives: it is what keeps a debugger session that
reloaded a *changed* definition from comparing a stale value's payload through
the new definition's descriptors.

## Decision 2: a payload slot may be null, and null agrees with everything

A slot the producer had no static type for is a **null pointer**, and
`same_type` treats it as agreeing with whatever the other side says. This is
`TupleSchema`'s rule verbatim (ADR-066 decision 5), and it is load-bearing twice
over:

- The runtime's own `option_schema` has a null `Some` slot, because `Map.get`
  learns `V` from the value it found. The codegen's schema for the same
  `Option[Int]` names `INT`. They must be one type or nothing about D1 works.
- `let m = Map()` followed by a use that never inspects the value leaves `V` an
  inference variable (HIR-01/MONO-01). Refusing to compile that rejects a
  working program, which is REP-15's lesson.

Null is *unknown*, not a fourth type, and it is safe rather than merely
tolerated because the fallback reads the value's **own** descriptor off its
header. An object always knows what it is.

`enum_equals` therefore compares `descriptor_at(tag, i, x)` against
`descriptor_at(tag, i, y)` before it reads either payload. That is what keeps
`Some(1)` and `Some("1")` unequal under one `Option` schema, instead of reading
a `Text` header as an `i64` (P0-11).

### Alternative rejected: names and arities only

A schema of variant names and arities would have closed the nominal-identity half
and left the payload half open — under one `Option` schema, `Some(1)` and
`Some("a")` would still have reached `enum_equals`, which reads the first
operand's descriptor and applies it to the second. That is the type confusion
RT-12 closed for records; there is no reason to leave it open for enums, and the
tuple half already shows the shape of the answer.

## Decision 3: `praxis_alloc_enum` takes a schema and reads the arity from it

```rust
praxis_alloc_enum(ctx, schema_ptr: *const EnumSchema, tag: i64) -> GcRef
```

The `arity` parameter is **gone**, not merely joined by a schema. It was passed
independently of the shape it had to agree with, so "a two-slot allocation of a
one-slot variant" was a state a caller could reach. `praxis_alloc_tuple` already
reads its arity from `schema.descriptors.len()`; this does the same through
`schema.variants[tag].payload.len()`, and the disagreement stops being
representable.

A null schema, or a tag the schema has no variant for, allocates nothing and
answers the Unit sentinel — which is what `praxis_alloc_tuple` already answers a
null schema. That is a refusal on a path the *compiler* is responsible for having
prevented, not an absent value; `AbiRet::Gc`'s doc (ADR-076) draws that line.

## Decision 4: the tag's offset is derived, and the ABI bumps to 14

`schema` goes **first** in `EnumPayload`, mirroring `RecordPayload` and
`TuplePayload`. That moves `tag` from offset 0 to offset 8, and the `Inst::EnumTag`
lowering had the `0` written out as a literal with a comment beside it:

```rust
// The tag is a u32 at offset 0 of the payload.
let tag = builder.ins().uload32(MemFlags::trusted(), tag_ptr, 0);
```

It is `core::mem::offset_of!(EnumPayload, tag)` now, declared beside the four
offsets `lower.rs` already derives that way (`SLOTS_OFFSET`,
`RECURSION_DEPTH_OFFSET`, `UNIT_REF_OFFSET`, `DebugLocal::value`). The next
reorder is then a compile-time-derived constant rather than every `match` in the
language reading the wrong word.

`RUNTIME_ABI_VERSION` goes 13 → 14. This is S18's **one** bump (hazard H17), and
the two fault kinds of ADR-075 ride inside the same version window.

## Decision 5: `AllocKind::Enum` carries the value's static type

The MIR node had `enum_def_id` and `variant_idx` and dropped the `ty` that
`TypedExpr::EnumVariant` already carried. A def id alone cannot say what `Some`'s
payload descriptor is for a **generic** def, and `Option` is generic (F12).

`record_schema_for`'s refusal of a generic def is deliberately **not** copied to
`enum_schema_for`. That refusal is correct where it is — the language cannot
declare `struct P[T]` and a `RecordLit` carries no arguments to substitute — but
`Option` is the one generic def the language has, and it is the type of every
`Map.get`, `Grid.find` and graph-walk result. Refusing it would refuse the
feature. `TypeDb::variant_payload_of` substitutes the instance's arguments
instead.

The generation's enum-schema cache is keyed on `(generation, def_id, the resolved
payload descriptors)`. The last part is what a record key does not need: one
`EnumDefId` covers `Option[Int]` and `Option[Text]`, and a shared schema there
would dispatch `equals` through the wrong descriptor. The generation half carries
MIR-12/DBG-06's lesson unchanged.

## Decision 6: `option_schema` is a function, not a `static`

`*const TypeDescriptor` is neither `Send` nor `Sync`, so `pub static
OPTION_SCHEMA: EnumSchema` — which is how F21 sketched it — does not compile. The
runtime's own precedent is `tuples::point_schema`: an `OnceLock<SyncPtr>` with a
local `unsafe impl Send/Sync` around a `Box::leak`ed value. That idiom is copied
exactly rather than replaced by a blanket `unsafe impl Sync for EnumSchema`,
which would be a claim about every schema including the arena-allocated ones.

## Consequences

- **A runtime enum can be rendered.** `Some(3)` prints as `Some(3)`, `None` as
  `None`, `Number(7)` as `Number(7)`. A CLI fixture gates it.
- **Two enum types of one shape are two types**, and one type built through two
  separately allocated schemas is one type — the JIT-generation and
  parser-registry case that RT-11 and RT-12 already fixed for tuples and records.
- **Hazard H15 covers one more pointer.** A live `EnumPayload` holds a
  `*const EnumSchema` into a generation arena, so `Generation::retire`'s
  `HeapDrained` proof now discharges three kinds of schema pointer, and its
  safety note says so. The input parser's enum schemas live in `ParserSchemas`
  and are dropped by `retire_schemas` with the plans, which is IP-12's rule.
- **The parser's `choice` schemas have null payload slots.** The interpreter
  learns a case's value type from the value the child plan produced, never from a
  static type, and a null slot says exactly that. The arity is still exact, which
  is what sizes the payload.
- **DBG-02 is now doable and is not done here.** `InstanceRepr` can stop saying
  "an enum value does not record its nominal identity" whenever DBG-02's stage
  reaches it; nothing in S18 depends on it.
- **No new diagnostic code.** Nothing user-facing is reported that was not
  reported before; ADR-051 is unchanged.
