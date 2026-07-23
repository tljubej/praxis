//! Interned handles for types and type variables.
//!
//! Both [`Type`] and [`VarId`] are opaque `u32` indices into the [`TypeDb`](crate::TypeDb)
//! arena. They are `Copy` and cheap to pass around, which is the whole reason the
//! arena representation exists: every binding, expression, and constraint carries
//! types by value rather than through `Rc<RefCell<…>>` (ADR-007).
//!
//! The names are deliberately distinct from [`praxis_runtime::TypeId`](::praxis_runtime::descriptor::TypeId)
//! (the *runtime* descriptor id) — this is the *static* type system's id, and the
//! two must never be confused. The static `Type` ids are only meaningful inside the
//! [`TypeDb`](crate::TypeDb) that minted them.

/// An interned static type. Copyable; resolve it through [`TypeDb`](crate::TypeDb).
///
/// Equality on `Type` is *physical* identity (same arena slot), **not** structural
/// equality — two separately-interned `Int`s are equal here, but a `Type` that is a
/// type variable is equal only to itself until it is linked. Structural comparison
/// goes through unification, not `==`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Type(pub u32);

impl Type {
    /// Raw arena index. Public only so the `pretty`/`db` modules can index; not
    /// for external use.
    #[inline]
    #[must_use]
    pub const fn to_u32(self) -> u32 {
        self.0
    }
}

/// An interned type variable. A type variable is one of the things a [`Type`] can
/// *be* (see [`TypeData::Var`](crate::TypeData)); `VarId` is its identity.
///
/// Kept as a distinct newtype from [`Type`] so the inference engine never confuses
/// "a variable" with "an arbitrary type that might already be concrete".
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct VarId(pub u32);

impl VarId {
    #[inline]
    #[must_use]
    pub const fn to_u32(self) -> u32 {
        self.0
    }
}
