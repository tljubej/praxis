//! Interned handles for types and type variables.
//!
//! Both [`Type`] and [`VarId`] are opaque `u32` indices into the [`TypeDb`](crate::TypeDb)
//! arena. They are `Copy` and cheap to pass around, which is the whole reason the
//! arena representation exists: every binding, expression, and constraint carries
//! types by value rather than through `Rc<RefCell<…>>` (ADR-007).
//!
//! The names are deliberately distinct from
//! [`praxis_runtime::TypeId`](::praxis_runtime::descriptor::TypeId) (the
//! *runtime* descriptor id) — this is the *static* type system's id, and the two
//! must never be confused. The static `Type` ids are only meaningful inside the
//! [`TypeDb`](crate::TypeDb) that minted them.

/// An interned static type. Copyable; resolve it through [`TypeDb`](crate::TypeDb).
///
/// Equality on `Type` is *physical* identity (same arena slot), **not** structural
/// equality — two separately-interned `Int`s are equal here, but a `Type` that is a
/// type variable is equal only to itself until it is linked. Structural comparison
/// goes through unification, not `==`.
///
/// # Sealed
///
/// The field is private and there is **no public constructor**. A `Type` is an
/// index into the arena that minted it, so a hand-written `Type(0)` is a forged
/// handle that names whatever happens to sit in slot zero. The only producers
/// are [`TypeDb`](crate::TypeDb)'s own constructors; a caller holding a raw
/// `u32` from outside (the debugger rehydrating a stored `type_id`) must come
/// back in through the checked
/// [`TypeDb::type_from_raw`](crate::TypeDb::type_from_raw).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Type(u32);

impl Type {
    /// Mint a handle from a raw arena index. `pub(crate)` on purpose: the
    /// checked public route is [`TypeDb::type_from_raw`](crate::TypeDb::type_from_raw).
    #[inline]
    #[must_use]
    pub(crate) const fn from_raw(raw: u32) -> Type {
        Type(raw)
    }

    /// Raw arena index. Stable for as long as the minting `TypeDb` lives, which
    /// is what lets the debugger store one alongside a value and rehydrate it.
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
///
/// Sealed for the same reason as [`Type`], and obtained the same way:
/// [`TypeDb::fresh_var`](crate::TypeDb::fresh_var) mints one,
/// [`TypeDb::var_id_of`](crate::TypeDb::var_id_of) recovers one from a type.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct VarId(u32);

impl VarId {
    #[inline]
    #[must_use]
    pub(crate) const fn from_raw(raw: u32) -> VarId {
        VarId(raw)
    }

    #[inline]
    #[must_use]
    pub const fn to_u32(self) -> u32 {
        self.0
    }

    /// The variable's slot, as a type. A `VarId` can only name a slot that a
    /// `TypeDb` minted as a variable, so this is a total conversion rather than
    /// a forged handle: every variable *is* a type.
    #[inline]
    #[must_use]
    pub const fn as_type(self) -> Type {
        Type(self.0)
    }
}
