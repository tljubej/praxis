# ADR-048: A nominal type is one definition applied to arguments, and `TypeKey` is its identity

**Date:** 2026-07-29
**Status:** Accepted
**Milestone:** Repair (stage S11 — F12's static half, TY-06; and MONO-03's cause)
**Amends:** ADR-025's `TypeData::Record { def }` / `Enum { def }`; the
same-named-enum unification arm added in M9

## Context

`TypeData::Enum { def }` carried *only* a def-id, so an enum's type arguments
had nowhere to live. The prelude `Option` needed them, and what it got instead
was a **fresh nominal definition per occurrence**: `option_type(db, elem)`
registered a new `EnumDef` whose `Some` payload was `elem`, and it was called
from three places — the `Some`/`None` prelude schemes, every `Option[T]`
annotation, and the input parser's `Optional` synthesis. Worse, `instantiate`
minted one more per *use site*, because folding an enum whose payload changed
had no way to say "same type, different argument" and so re-registered the def.

Four symptoms, one missing representation:

- **TY-06** — `id[Option[Int]]` and `id[Option[Text]]` were different *nominal
  types* rather than one type at two arguments, so nominal identity was
  meaningless for the only generic type the language has.
- The workaround was a relaxed `unify` arm: two enums with the same **name** and
  the same variant-name signature merged, unifying their payloads pairwise. That
  arm exists to put back together copies that should never have been made, and
  it makes two same-named nominal enums indistinguishable — the opposite of what
  §4.6 says nominal means.
- **MONO-03** — the monomorphizer keyed its cache on `db.render`, and `render`
  printed `Option` for every instance because the element type was not in the
  type. `id(Some(1))` and `id(Some("hi"))` hashed to one key, so the second call
  ran the first's `Int` clone over a `Text` payload.
- **RT-13/DBG-02** (S18/S15) — a runtime enum has no identity to record, and the
  debugger has no stable key to turn a value back into a type.

## Decision 1: `Record`/`Enum` carry arguments; a def carries parameters

```rust
pub enum TypeData { …,
    Record { def: RecordDefId, args: Vec<Type> },
    Enum   { def: EnumDefId,   args: Vec<Type> },
}
pub struct RecordDef { name: Option<String>, params: Vec<VarId>, fields: … }
pub struct EnumDef   { name: Option<String>, params: Vec<VarId>, variants: … }
```

A definition is registered **once per declaration** (or once per prelude
scheme), never per instantiation. `TypeDb::record_type(def, args)` and
`enum_type(def, args)` are the instance constructors and are **fallible**: a
count that disagrees with the def's parameters is `TypeCtorError::TypeArgCount`,
which is `Y007` when it comes from a user annotation. That is TY-07's rule
extended to nominal types — `Option[Int, Text]` used to intern quietly.

A def's field and payload types are written in terms of its own `params`, so a
*use* reads them through the instance:
`record_fields_of` / `record_field_of` / `variant_payload_of` substitute the
def's parameters by the instance's arguments. Substitution is identity — and
free — when `params` is empty, which is every record and every user-declared
enum: the language has no `struct P[T]` syntax, and `Option` is the one generic
definition.

## Decision 2: the fold has two shapes, chosen by whether the def is generic

`fold_record_default` / `fold_enum_default` (F9) branch on `params`:

- **Generic** — the instance's children *are* its arguments. Fold those, keep
  the def. This is the whole of TY-06: instantiation now produces a use of a
  definition, and can no longer produce a definition.
- **Non-generic** — the def's field types are the type's children, as before.
  Fold them and register a specialized def only on change. The anonymous
  structural records the input parser synthesizes are what needs this, and
  `deep_resolve` is what exercises it.

## Decision 3: one canonical `Option`, seeded by `TypeDb::new`

`TypeDb` holds `option_def: EnumDefId`, registered at construction with one
parameter `T` and variants `Some(T)` / `None`. `option_def()` names it and
`option_of(elem)` applies it. The three sites that spelled the variant list out
now call `option_of`; there is no other way to get an `Option`.

`T` is created at `Level::OUTERMOST` deliberately: only variables *deeper* than
a binding site are quantifiable, so nothing generalizes the definition's own
parameter, and `clamp_to` cannot lower it further. The parameter is inert until
an instance's argument substitutes for it.

## Decision 4: `unify`'s relaxed enum arm narrows to anonymous defs

Same def-id now unifies **pairwise on arguments** — that is where `Option[?T] ~
Option[Int]` pins `?T`. The name-and-signature arm is kept only for defs that
are *both anonymous and non-generic*: `choice(...)` templates (§7.5) still mint
one def per synthesis, and they merge structurally exactly as anonymous records
do. Two **named** enums with different def-ids no longer unify, which is what
§4.6 said all along and what the record arm has always done.

Unifying two *generic* defs structurally is explicitly refused: it would link one
definition's parameters to another's, which is a claim about the definitions
rather than about the two instances in hand.

## Decision 5: `TypeKey` is identity; `render` is display

```rust
pub enum TypeKey { Scalar(ScalarType), Unit, Tuple(Vec<TypeKey>),
    Func { … }, Collection { ctor, args },
    Record(RecordDefId, Vec<TypeKey>), Enum(EnumDefId, Vec<TypeKey>), Var(VarId) }
impl TypeDb { pub fn canonical_key(&self, t: Type) -> TypeKey; }
```

Nominal types key by **def id and arguments**, not by name: two same-named
declarations are two keys, which a rendered string cannot express. The
monomorphizer's cache key is now a `Vec<TypeKey>`; the mangled clone *name* is
still built from the rendered types, because a symbol has to be readable, and
`MonoPass` disambiguates with a counter if two distinct keys ever render alike.

`canonical_key` takes `&self` and interns nothing — a key describes a type
without creating one. Recursion terminates without a memo because a nominal
type's key holds its def id rather than its fields, so the side tables are never
walked.

## Consequences

- **`Option` renders as `Option[Int]`.** No snapshot moved (none contained a
  rendered `Option`), but any future one will show the argument.
- **A pattern's payload comes from the scrutinee.** `Some(n)` against an
  `Option[Int]` binds `n` at `Int` because inference *instantiates* the
  constructor's scheme and unifies it with the scrutinee. Reading the payload
  straight off the def now answers the definition's parameter `T`, which is why
  the lowering-side lookup no longer returns a payload at all.
- **The runtime half is not here.** `EnumSchema`, `EnumPayload`'s schema pointer
  and `praxis_alloc_enum`'s new parameter are RT-13, which the plan schedules in
  S18 and which needs a `RUNTIME_ABI_VERSION` bump of its own. Nothing in
  `praxis-types` is `#[repr(C)]`, so this stage spends none.
- **A generic record fails the compile.** `record_schema_for` builds a schema
  from the def's field types, which for a generic def would resolve descriptors
  for its parameters. The language cannot declare one and `TypedExpr::RecordLit`
  carries no arguments to substitute, so codegen refuses with a named
  diagnostic rather than emitting a wrong layout.
- **`ScalarType` and `CollectionCtor` derive `Hash`** so `TypeKey` can.

## Alternatives considered

**Keep a def per instantiation and canonicalize later.** This is what the M9
unification arm is, and its cost is that nominal identity means nothing: the
merge is by *name*, so two distinct declarations sharing a name would collapse,
and nothing at all distinguishes `Option[Int]` from `Option[Text]` until their
payloads are compared. It also grows the def table without bound — one entry per
`Some` in the program.

**Store the arguments in the def and intern defs structurally.** That makes a
def a type, which is the thing ADR-025 split apart to keep `Type` a cheap `u32`
handle; it also gives nominal and structural identity the same representation,
so `Point` and `Vector` with identical fields become one type.

**`SchemaIdentity::Nominal(u64)`, a generation-scoped def key** (F12's sketch).
The runtime already landed `Nominal(&'static str)` with RT-12 (ADR-045's
sibling), and comparing interned names by content cannot collide while needing
no key registry. When the runtime half lands in S18 it can carry arguments
alongside the name; nothing about `RecordSchema::same_type` changes here.
