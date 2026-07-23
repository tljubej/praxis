//! Type interning, inference, and capability resolution (§5, §14.1).
//!
//! An HM-inspired inference engine with extensions for mutable variables, tuples,
//! function types, and let-generalization. Representation is an interned arena
//! (ADR-007): every [`Type`] is a copyable `u32` handle into a [`TypeDb`], and
//! type variables live in the arena so unification links them by mutation rather
//! than through `Rc<RefCell<…>>`. The scalar/collection vocabulary is reused from
//! [`praxis_stdlib`] (rule 20.3: one vocabulary); this crate adds the inference
//! layer on top.
//!
//! **M2 scope:** `Int`, `Text`, `Bool`, `Unit`, `Never`, tuples, functions, and
//! let-generalization. Collections, records, enums, closures, and the internal
//! capability system arrive with their own milestones (M5/M7).

pub mod data;
pub mod db;
pub mod generalize;
pub mod pretty;
pub mod type_id;
pub mod unify;

pub use data::{TypeData, VarState};
pub use db::{Slot, TypeDb};
pub use generalize::Scheme;
pub use type_id::{Type, VarId};
// Re-export the shared vocabulary so consumers reach it through `praxis_types`
// without depending on `praxis-stdlib` themselves. `ScalarType`/`CollectionCtor`
// live in `praxis-stdlib`'s `type_pattern` module; surface them from our root.
pub use praxis_stdlib::type_pattern::{CollectionCtor, ScalarType};
pub use praxis_stdlib::TypePattern;

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

    /// The `Never` type — the bottom type for diverging control flow (§4.3).
    #[must_use]
    pub fn never(&mut self) -> Type {
        self.scalar(Scalar::Never)
    }

    /// A function type `(params) -> result`.
    #[must_use]
    pub fn func(&mut self, params: Vec<Type>, result: Type) -> Type {
        self.intern(TypeData::Func { params, result })
    }

    /// A tuple type from the given elements (two or more).
    #[must_use]
    pub fn tuple(&mut self, elements: Vec<Type>) -> Type {
        self.intern(TypeData::Tuple(elements))
    }
}

#[cfg(test)]
#[path = "types_tests.rs"]
mod types_tests;
