//! The shape of a static type, plus the lifecycle states of a type variable.
//!
//! [`TypeData`] is what sits in each arena slot. It reuses the built-in scalar
//! and collection *names* from [`praxis_stdlib`](::praxis_stdlib) (rule 20.3: one
//! vocabulary) and adds the inference-specific shapes on top: tuples, functions,
//! and type variables.

use praxis_stdlib::type_pattern::{CollectionCtor, ScalarType};

use crate::type_id::Type;

/// The concrete shape of an interned type.
///
/// The scalar variants come from [`praxis_stdlib::ScalarType`] — the type system
/// never re-declares "what the built-in scalars are" (rule 20.3). `Unit` is kept
/// as its own variant, mirroring how [`praxis_stdlib::TypePattern`] splits `Unit`
/// from `Scalar` (the stdlib models `Unit` as a sibling variant, not a scalar).
/// M2 only *constructs* `Int`, `Text`, `Bool`, `Unit`, and `Never`; the reserved
/// scalars (`UInt`, `Float`, `Byte`, `Char`) surface as "unknown type" name
/// diagnostics if a user writes them (ADR-007), per §4.3.
///
/// M5 adds [`Collection`](Self::Collection) so the inference engine can represent
/// `Vec[T]` and drive method dispatch (ADR-010) against the receiver type. The
/// full collection set (Map/Set/Counter/Heap/Deque) lands in M8; M5 focuses on
/// `Vec`.
#[derive(Clone, Debug)]
pub enum TypeData {
    /// A built-in scalar: `Int`, `Text`, `Bool`, `Never`, …
    Scalar(ScalarType),
    /// The unit type, with no payload (§4.3). Mirrors `TypePattern::Unit`.
    Unit,
    /// A tuple `(T, U, …)`. A one-element "tuple" never exists as data — the
    /// parser keeps single parenthesized types as the inner type, so this variant
    /// always carries at least two elements.
    Tuple(Vec<Type>),
    /// A function `(P0, P1, …) -> R`.
    Func { params: Vec<Type>, result: Type },
    /// A collection `Ctor[T, …]`, e.g. `Vec[Int]` (§4.4, §11.2). The `ctor`
    /// names the collection kind (shared with `TypePattern::Collection`); `args`
    /// are the type arguments (one for `Vec[T]`, two for `Map[K, V]`, …). M5
    /// constructs `Vec` only; other ctors are reserved.
    Collection {
        ctor: CollectionCtor,
        args: Vec<Type>,
    },
    /// A type variable, in one of its lifecycle states (see [`VarState`]).
    Var(VarState),
}

/// The lifecycle of a type variable, as an explicit enum so each phase is
/// representable rather than signaled by convention.
///
/// - Fresh vars start [`Unbound`](VarState::Unbound) at a binding level.
/// - [`unify`](crate::unify)ing one [`Link`](VarState::Linked)s it to a concrete
///   type (or another var), resolving via [`TypeDb::prune`](crate::TypeDb::prune).
/// - [`generalize`](crate::generalize) promotes qualifying unbound vars to
///   [`Generalized`](VarState::Generalized) so a [`Scheme`](crate::Scheme) can
///   quantify over them and [`instantiate`](crate::generalize) can replace them
///   with fresh vars at each use site.
#[derive(Clone, Debug)]
pub enum VarState {
    /// Not yet constrained. `level` is the binding level at which the var was
    /// created — used by generalization (only vars whose level is *deeper* than
    /// the generalization site are quantifiable) and by Pottier's level-lowering
    /// rule on unification.
    Unbound { level: u32 },
    /// Unified to `target`. Follow the link via [`TypeDb::prune`](crate::TypeDb::prune);
    /// never inspect `target` without pruning first.
    Linked { target: Type },
    /// Promoted into a [`Scheme`](crate::Scheme)'s quantified set. Never unified
    /// directly — instantiate the scheme to get fresh vars in its place.
    Generalized,
}

impl TypeData {
    /// `true` iff this slot currently holds an unbound (as-yet-unconstrained)
    /// variable. Convenience used by diagnostics and occurs checks.
    #[must_use]
    pub fn is_unbound_var(&self) -> bool {
        matches!(self, Self::Var(VarState::Unbound { .. }))
    }
}
