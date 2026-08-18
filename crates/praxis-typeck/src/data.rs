//! The shape of a static type, plus the lifecycle states of a type variable.
//!
//! [`TypeData`] is what sits in each arena slot. It reuses the built-in scalar
//! and collection *names* from [`praxis_stdlib`](::praxis_stdlib) (rule 20.3: one
//! vocabulary) and adds the inference-specific shapes on top: tuples, functions,
//! records, enums, and type variables.

use praxis_stdlib::type_pattern::{CollectionCtor, ScalarType};

use crate::type_id::{Type, VarId};

/// An opaque index into [`TypeDb::record_defs`](crate::db::TypeDb), identifying
/// one record definition (nominal or anonymous structural). Two `Record` types
/// with the same `RecordDefId` are the *same* type.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RecordDefId(pub u32);

impl RecordDefId {
    #[inline]
    #[must_use]
    pub const fn to_u32(self) -> u32 {
        self.0
    }
}

/// An opaque index into [`TypeDb::enum_defs`](crate::db::TypeDb), identifying one
/// enum definition. Two `Enum` types with the same `EnumDefId` are the same type.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct EnumDefId(pub u32);

impl EnumDefId {
    #[inline]
    #[must_use]
    pub const fn to_u32(self) -> u32 {
        self.0
    }
}

/// The concrete shape of an interned type.
///
/// The scalar variants come from [`praxis_stdlib::ScalarType`] — the type system
/// never re-declares "what the built-in scalars are" (rule 20.3). `Unit` is kept
/// as its own variant, mirroring how [`praxis_stdlib::TypePattern`] splits `Unit`
/// from `Scalar` (the stdlib models `Unit` as a sibling variant, not a scalar).
/// `UInt` and `Byte` are reserved but not constructible: an annotation naming
/// one surfaces as an "unknown type" name diagnostic (ADR-007), per §4.3.
///
/// [`Collection`](Self::Collection) lets the inference engine represent `Vec[T]`
/// and drive method dispatch (ADR-010) against the receiver type.
///
/// [`Record`](Self::Record) and [`Enum`](Self::Enum) go through def-id
/// indirection (ADR-025): the heavy field/variant data lives in side-tables on
/// [`TypeDb`](crate::db::TypeDb), keeping `Type` a cheap copyable `u32` handle
/// and avoiding recursive size/cycles. Both nominal (source-declared) and
/// anonymous structural records (from parser templates, ADR-024) use the
/// `Record` variant; anonymous records are keyed by a canonicalized field set.
#[derive(Clone, Debug)]
pub enum TypeData {
    /// A built-in scalar: `Int`, `Text`, `Bool`, `Char`, …
    Scalar(ScalarType),
    /// The unit type, with no payload (§4.3). Mirrors `TypePattern::Unit`.
    Unit,
    /// `Never` — the bottom type for diverging control flow (§4.3).
    ///
    /// Its own variant, not a [`Scalar`](Self::Scalar). A scalar is a type with
    /// a runtime representation — a descriptor, a payload width, a value you
    /// can hold — and `Never` has none of those *by definition*: no value ever
    /// has this type. Keeping it out of `ScalarType` is what lets every "is this
    /// a scalar?" question answer no for it, and lets
    /// [`join`](crate::db::TypeDb::join)'s absorbing case be an arm of its own.
    Never,
    /// A tuple `(T, U, …)`. A one-element "tuple" never exists as data — the
    /// parser keeps single parenthesized types as the inner type, so this variant
    /// always carries at least two elements.
    Tuple(Vec<Type>),
    /// A function `(P0, P1, …) -> R`.
    Func { params: Vec<Type>, result: Type },
    /// A collection `Ctor[T, …]`, e.g. `Vec[Int]` (§4.4, §11.2). The `ctor`
    /// names the collection kind (shared with `TypePattern::Collection`); `args`
    /// are the type arguments (one for `Vec[T]`, two for `Map[K, V]`, none for
    /// the nullary `BitSet` and `Range`).
    Collection {
        ctor: CollectionCtor,
        args: Vec<Type>,
    },
    /// A record type (§4.5 nominal, §5.6 anonymous structural). `def` is an
    /// index into [`TypeDb::record_defs`](crate::db::TypeDb) holding the
    /// [`RecordDef`] (name + params + ordered fields), and `args` is what this
    /// *instance* supplies for the def's [`params`](RecordDef::params). Two
    /// records are the same type iff their def-ids **and** their arguments
    /// agree; for anonymous records the def is keyed by a canonicalized
    /// (source-order-preserving for display, name-set-equal for identity per
    /// §5.6) field set.
    Record { def: RecordDefId, args: Vec<Type> },
    /// An enum type (§4.6). `def` indexes
    /// [`TypeDb::enum_defs`](crate::db::TypeDb); `args` instantiates the def's
    /// [`params`](EnumDef::params), so `Option[Int]` and `Option[Text]` are one
    /// def with two argument lists rather than two nominal definitions.
    Enum { def: EnumDefId, args: Vec<Type> },
    /// A type variable, in one of its lifecycle states (see [`VarState`]).
    Var(VarState),
}

/// A binding level (§5.3, ADR-008). Raised on entering a `var`/`fn` scope,
/// restored on leaving it; a variable records the level it was created at, and
/// generalization quantifies exactly the variables deeper than the binding site.
///
/// # Why a newtype
///
/// ADR-008's rule is `level(w) := min(level(w), level(v))` — a level only ever
/// *decreases* — so [`clamp_to`](Self::clamp_to) is the only mutator and it
/// cannot be written to raise one. A bare `u32` compared by hand at each use
/// puts the direction of that comparison back in the caller's hands.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Level(u32);

impl Level {
    /// The outermost (top-level) binding level.
    pub const OUTERMOST: Level = Level(0);

    /// One level deeper — what entering a `var`/`fn` scope produces.
    #[inline]
    #[must_use]
    pub const fn deeper(self) -> Level {
        Level(self.0 + 1)
    }

    /// Lower this level to `outer` if `outer` is shallower. The **only**
    /// mutator: Pottier's level-lowering rule is monotone-decreasing, and
    /// spelling it as `min` here is what makes the reversed comparison
    /// unwritable at the call site.
    #[inline]
    pub fn clamp_to(&mut self, outer: Level) {
        self.0 = self.0.min(outer.0);
    }

    /// Whether this level is strictly deeper (more nested) than `site` — the
    /// generalization test: a variable created inside the binding being
    /// generalized is quantifiable, one from an enclosing scope is not.
    #[inline]
    #[must_use]
    pub const fn is_deeper_than(self, site: Level) -> bool {
        self.0 > site.0
    }
}

/// The lifecycle of a type variable, as an explicit enum so each phase is
/// representable rather than signaled by convention.
///
/// - Fresh vars start [`Unbound`](VarState::Unbound) at a binding [`Level`].
/// - [`unify`](crate::unify)ing one [`Link`](VarState::Linked)s it to a concrete
///   type (or another var), resolving via [`TypeDb::prune`](crate::TypeDb::prune).
///
/// There is no third state. Quantification is not a property of the variable:
/// it is recorded by the [`Scheme`](crate::Scheme) that does the quantifying,
/// which is the only thing that knows it. A "generalized" flag on the arena
/// would be global state a later `generalize` could set under a scheme already
/// built as a monotype, leaving a `Scheme` whose body pointed at variables it
/// did not list.
#[derive(Clone, Debug)]
pub enum VarState {
    /// Not yet constrained. `level` is the binding level at which the var was
    /// created — used by generalization (only vars whose level is *deeper* than
    /// the generalization site are quantifiable) and by Pottier's level-lowering
    /// rule on unification.
    Unbound { level: Level },
    /// Unified to `target`. Follow the link via [`TypeDb::prune`](crate::TypeDb::prune);
    /// never inspect `target` without pruning first.
    Linked { target: Type },
}

/// One field of a record definition: its source name and its type (§4.5).
/// Field order in the definition is the canonical construction/display order;
/// §5.6 notes field order in *source* does not affect anonymous-record *identity*
/// after canonicalization, but display and construction preserve source order.
#[derive(Clone, Debug)]
pub struct RecordFieldDef {
    pub name: String,
    pub ty: Type,
}

/// The full definition of a record type (§4.5 nominal, §5.6 anonymous structural).
/// Lives in the [`TypeDb::record_defs`](crate::db::TypeDb) side-table, referenced
/// from [`TypeData::Record`] via a [`RecordDefId`].
///
/// `name` is `None` for anonymous structural records (parser-template-generated).
/// Two anonymous records with the same field names and types (in any order, per
/// §5.6) share one `RecordDef`; nominal records are distinct by name even with
/// identical fields.
#[derive(Clone, Debug)]
pub struct RecordDef {
    /// `None` for anonymous structural records; the declared name for nominal.
    pub name: Option<String>,
    /// The def's own type parameters. Empty for every record the language can
    /// declare today — there is no `struct P[T]` syntax — so a record's field
    /// types *are* its children. When it is non-empty, the field types are
    /// written in terms of these variables and an instance's
    /// [`args`](TypeData::Record::args) is what substitutes for them.
    pub params: Vec<VarId>,
    pub fields: Vec<RecordFieldDef>,
}

impl RecordDef {
    /// The number of fields.
    #[must_use]
    pub fn arity(&self) -> usize {
        self.fields.len()
    }

    /// Look up a field by name, returning its index and type.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<(usize, Type)> {
        self.fields
            .iter()
            .position(|f| f.name == name)
            .map(|i| (i, self.fields[i].ty))
    }
}

/// One variant of an enum definition (§4.6). A variant carries a payload — a
/// tuple of types for the variant's data. `Empty`/`Wall` are payload-less;
/// `Number(Int)` has a one-element payload; `Pair(Int, Text)` has two.
///
/// The payload is a plain `Vec`: an **empty** one *is* the payload-less case.
/// An `Option<Vec<Type>>` would give "no payload" two spellings, and `unify`
/// would have to treat `None` and `Some([])` as the same thing.
#[derive(Clone, Debug)]
pub struct EnumVariantDef {
    pub name: String,
    /// The variant's payload types, empty for a payload-less variant.
    pub payload: Vec<Type>,
}

impl EnumVariantDef {
    /// A variant with a payload.
    #[must_use]
    pub fn new(name: impl Into<String>, payload: Vec<Type>) -> EnumVariantDef {
        EnumVariantDef {
            name: name.into(),
            payload,
        }
    }

    /// A payload-less variant (`Empty`, `None`, `Wall`).
    #[must_use]
    pub fn bare(name: impl Into<String>) -> EnumVariantDef {
        EnumVariantDef::new(name, Vec::new())
    }

    /// Whether this variant carries a payload.
    #[must_use]
    pub fn has_payload(&self) -> bool {
        !self.payload.is_empty()
    }
}

/// The full definition of an enum type (§4.6). Lives in the
/// [`TypeDb::enum_defs`](crate::db::TypeDb) side-table, referenced from
/// [`TypeData::Enum`] via an [`EnumDefId`].
///
/// `name` is `None` for an anonymous enum (a `choice(...)` template, §7.5),
/// mirroring [`RecordDef::name`]. Two anonymous enums are the same type when
/// their variant signatures match; a nominal one is distinct by name.
#[derive(Clone, Debug)]
pub struct EnumDef {
    /// `None` for anonymous enums; the declared name for nominal.
    pub name: Option<String>,
    /// The def's own type parameters. The prelude `Option` is the one generic
    /// def today: `params = [T]`, `variants = [Some(T), None]`, and every
    /// `Option[X]` is that def with `args = [X]` — one def, many argument lists,
    /// so `unify` compares def-ids rather than names and signatures.
    pub params: Vec<VarId>,
    pub variants: Vec<EnumVariantDef>,
}

impl EnumDef {
    /// The number of variants.
    #[must_use]
    pub fn arity(&self) -> usize {
        self.variants.len()
    }

    /// Look up a variant by name, returning its index.
    #[must_use]
    pub fn variant(&self, name: &str) -> Option<usize> {
        self.variants.iter().position(|v| v.name == name)
    }
}
