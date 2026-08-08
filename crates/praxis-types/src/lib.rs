//! Type interning, inference, and capability resolution (§5, §14.1).
//!
//! An HM-inspired inference engine with extensions for mutable variables, tuples,
//! function types, and let-generalization. Representation is an interned arena
//! (ADR-007): every [`Type`] is a copyable `u32` handle into a [`TypeDb`], and
//! type variables live in the arena so unification links them by mutation rather
//! than through `Rc<RefCell<…>>`. The scalar/collection vocabulary is reused from
//! [`praxis_stdlib`] (rule 20.3: one vocabulary); this crate adds the inference
//! layer on top.

pub mod constraint;
pub mod ctor;
pub mod data;
pub mod db;
pub mod error;
pub mod fold;
pub mod generalize;
pub mod key;
pub mod pretty;
pub mod type_id;
pub mod unify;

pub use constraint::{Capability, Constraint};
pub use ctor::{CollectionArgs, FieldSet, TupleElems, VariantSet};
pub use data::{
    EnumDef, EnumDefId, EnumVariantDef, Level, RecordDef, RecordDefId, RecordFieldDef, TypeData,
    VarState,
};
pub use db::{Slot, TypeDb};
pub use error::TypeCtorError;
pub use fold::{fold, FoldMemo, TypeFolder};
pub use generalize::Scheme;
pub use key::TypeKey;
pub use type_id::{Type, VarId};
// Re-export the shared vocabulary so consumers reach it through `praxis_types`
// without depending on `praxis-stdlib` themselves. `ScalarType`/`CollectionCtor`
// live in `praxis-stdlib`'s `type_pattern` module; surface them from our root.
pub use praxis_stdlib::type_pattern::{CollectionCtor, ScalarType};
pub use praxis_stdlib::{CapKind, TypePattern};

use praxis_stdlib::type_pattern::ScalarType as Scalar;

impl TypeDb {
    /// The `Int` type (§4.3). The default integer type.
    #[must_use]
    pub fn int(&mut self) -> Type {
        self.scalar(Scalar::Int)
    }

    /// The `Text` type (§4.3). Immutable UTF-8.
    #[must_use]
    pub fn text(&mut self) -> Type {
        self.scalar(Scalar::Text)
    }

    /// The `Bool` type (§4.3).
    #[must_use]
    pub fn bool(&mut self) -> Type {
        self.scalar(Scalar::Bool)
    }

    /// The `Char` type (§4.3) — a single Unicode scalar value. The input parser
    /// produces `Char` values (`char` atom, `grid(char)`), and the runtime
    /// descriptor is `scalars::CHAR`.
    #[must_use]
    pub fn char(&mut self) -> Type {
        self.scalar(Scalar::Char)
    }

    /// The `Float` type (§4.3) — IEEE 754 binary64. Float literals (`3.14`)
    /// infer to this type, and `Int.to_float()` widens to it (§4.12).
    #[must_use]
    pub fn float(&mut self) -> Type {
        self.scalar(Scalar::Float)
    }

    /// The `Never` type — the bottom type for diverging control flow (§4.3).
    ///
    /// Not a scalar: no value has this type, so there is no representation to
    /// describe. See [`TypeData::Never`](crate::TypeData::Never) and
    /// [`TypeDb::join`](crate::TypeDb::join).
    #[must_use]
    pub fn never(&mut self) -> Type {
        self.intern(TypeData::Never)
    }

    /// A function type `(params) -> result`.
    #[must_use]
    pub fn func(&mut self, params: Vec<Type>, result: Type) -> Type {
        self.intern(TypeData::Func { params, result })
    }

    /// A pair type `(a, b)` — the tuple arity that is always legal, so no
    /// [`TupleElems`](crate::TupleElems) validation is needed at the call site.
    #[must_use]
    pub fn pair(&mut self, a: Type, b: Type) -> Type {
        self.tuple(crate::TupleElems::pair(a, b))
    }

    /// A unary collection type `Ctor[elem]` — `Vec`, `Set`, `Deque`, `Counter`,
    /// `MinHeap`, `MaxHeap`, `Grid`, `Seq`. Infallible because the shape is
    /// fixed at the call site; a nullary or binary ctor here is a caller bug and
    /// panics rather than returning a wrong-arity type.
    ///
    /// # Panics
    /// If `ctor` is not unary.
    #[must_use]
    pub fn unary_collection(&mut self, ctor: CollectionCtor, elem: Type) -> Type {
        self.collection(ctor, crate::CollectionArgs::Unary(elem))
            .expect("unary collection ctor")
    }

    /// The `Vec[T]` collection type (§4.4, §11.2). Convenience for
    /// [`collection`](Self::collection) with the `Vec` ctor.
    #[must_use]
    pub fn vec(&mut self, elem: Type) -> Type {
        self.unary_collection(CollectionCtor::Vec, elem)
    }

    /// The `Map[K, V]` collection type.
    #[must_use]
    pub fn map(&mut self, key: Type, value: Type) -> Type {
        self.collection(
            CollectionCtor::Map,
            crate::CollectionArgs::Binary(key, value),
        )
        .expect("Map is binary")
    }
}

#[cfg(test)]
#[path = "types_tests.rs"]
mod types_tests;
