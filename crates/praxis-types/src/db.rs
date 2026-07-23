//! The type arena ([`TypeDb`]): the single mutable store for all static types.
//!
//! Every [`Type`] and [`VarId`] is a `u32` index into one `TypeDb`. Type variables
//! live *in* the arena (as [`Slot`]s), so unification links them by mutating a
//! slot rather than threading `Rc<RefCell<…>>` through the program (ADR-007). This
//! keeps `Type` `Copy` and makes the variable lifecycle ([`VarState`]) explicit.
//!
//! The arena is **not** thread-safe; inference is single-threaded. Callers obtain
//! a `&mut TypeDb` for the duration of inference and then read it through shared
//! references for diagnostics/hover.

use praxis_stdlib::type_pattern::ScalarType;

use crate::data::{TypeData, VarState};
use crate::type_id::{Type, VarId};

/// One arena entry. A slot is either a concrete type shape ([`TypeData`]) or — for
/// the historical reason that `Type` and `VarId` share an index space — a variable
/// in one of its lifecycle states. Because [`TypeData::Var`] already carries the
/// [`VarState`], a `Slot` is just a `TypeData`; the newtype exists so the arena is
/// self-documenting and so we can later add provenance (e.g. a span) without a
/// second parallel array.
#[derive(Clone, Debug)]
pub struct Slot {
    pub data: TypeData,
}

/// The interned type store. Create one at the start of inference and mint/follow
/// types through it.
#[derive(Clone, Debug, Default)]
pub struct TypeDb {
    /// `pub(crate)` so the `unify`/`generalize` modules can mutate slots in place
    /// (link vars, lower levels). External callers go through the methods below.
    pub(crate) slots: Vec<Slot>,
    /// The current binding level, raised on each `let`/`fn` and lowered on exit.
    /// See ADR-008.
    level: u32,
}

impl TypeDb {
    /// A fresh, empty arena.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current binding level.
    #[inline]
    #[must_use]
    pub fn level(&self) -> u32 {
        self.level
    }

    /// Push the binding level one deeper. Returns the previous level so the caller
    /// can restore it (`db.set_level(prev)`) when the scope ends.
    #[must_use]
    pub fn enter_level(&mut self) -> u32 {
        let prev = self.level;
        self.level += 1;
        prev
    }

    /// Restore the binding level (after a scope ends).
    ///
    /// Per Pottier/Rémy, the level is just restored here; unbound vars retain the
    /// level they were created at. They are *lowered* only at the point they
    /// would otherwise be unsound — inside [`unify`](crate::TypeDb::unify), when a
    /// younger variable is linked to a type containing older variables. Lowering
    /// everything on scope exit would defeat generalization (it would pull inner
    /// vars down to the outer level, so `generalize` could never quantify them).
    pub fn exit_level(&mut self, prev: u32) {
        self.level = prev;
    }

    /// Convenience: run `f` with the level raised one step, then restore. Use this
    /// around every `let`/`fn` binding so their variables are created at the inner
    /// level (and so generalization quantifies exactly the right set).
    pub fn scoped(&mut self, f: impl FnOnce(&mut Self)) {
        let prev = self.enter_level();
        f(self);
        self.exit_level(prev);
    }

    /// Like [`scoped`](Self::scoped) but for a computation that produces a value
    /// (the common inference shape: open a scope, build a type, return it). The
    /// level is restored even though a value is returned.
    #[must_use]
    pub fn scoped_return<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let prev = self.enter_level();
        let out = f(self);
        self.exit_level(prev);
        out
    }

    // --- construction -------------------------------------------------------

    /// Intern an arbitrary type shape, returning its handle.
    #[must_use]
    pub fn intern(&mut self, data: TypeData) -> Type {
        let id = Type(self.slots.len() as u32);
        self.slots.push(Slot { data });
        id
    }

    /// Intern a scalar type — the overwhelmingly common case.
    #[must_use]
    pub fn scalar(&mut self, s: ScalarType) -> Type {
        self.intern(TypeData::Scalar(s))
    }

    /// The unit type (§4.3) — no payload.
    #[must_use]
    pub fn unit(&mut self) -> Type {
        self.intern(TypeData::Unit)
    }

    /// A fresh unbound variable at the *current* binding level.
    #[must_use]
    pub fn fresh_var(&mut self) -> Type {
        let var = VarId(self.slots.len() as u32);
        let id = Type(var.0);
        self.slots.push(Slot {
            data: TypeData::Var(VarState::Unbound { level: self.level }),
        });
        // `var` is recorded implicitly: the slot's own index *is* the var id.
        let _ = var;
        id
    }

    /// The variable identity of a type slot, if it is a variable.
    #[must_use]
    pub fn var_id_of(&self, t: Type) -> Option<VarId> {
        match &self.slots[t.0 as usize].data {
            TypeData::Var(_) => Some(VarId(t.0)),
            _ => None,
        }
    }

    // --- resolution ---------------------------------------------------------

    /// Follow links: return the representative `Type` for `t`. If `t` is a linked
    /// var, recurse until an unlinked slot is found (with path compression — write
    /// the final representative back so subsequent lookups are O(1)).
    ///
    /// The returned type is always either a concrete shape or an unbound/generalized
    /// variable; never a `Linked` var.
    pub fn prune(&mut self, t: Type) -> Type {
        match &self.slots[t.0 as usize].data {
            TypeData::Var(VarState::Linked { target }) => {
                let target = *target;
                let root = self.prune(target);
                // Path compression: point straight at the root.
                if root != target {
                    self.slots[t.0 as usize].data =
                        TypeData::Var(VarState::Linked { target: root });
                }
                root
            }
            _ => t,
        }
    }

    /// Shared-reference variant of [`prune`](Self::prune) that does not compress
    /// (read-only). Returns the representative of `t`.
    #[must_use]
    pub fn follow(&self, t: Type) -> Type {
        match &self.slots[t.0 as usize].data {
            TypeData::Var(VarState::Linked { target }) => self.follow(*target),
            _ => t,
        }
    }

    /// Borrow the data of `t`'s representative **without** pruning links. Callers
    /// that may hold the borrow across a mutation must use [`prune`](Self::prune)
    /// first and then index the returned handle.
    #[must_use]
    pub fn data(&self, t: Type) -> &TypeData {
        &self.slots[t.0 as usize].data
    }

    /// Link variable `v` to `target`, recording `target`'s representative. Used by
    /// [`unify`](crate::unify); not meant for ad-hoc callers.
    pub(crate) fn link(&mut self, v: VarId, target: Type) {
        self.slots[v.0 as usize].data = TypeData::Var(VarState::Linked { target });
    }

    /// Mark variable `v` as generalized (quantified by a [`Scheme`](crate::Scheme)).
    pub(crate) fn generalize_var(&mut self, v: VarId) {
        self.slots[v.0 as usize].data = TypeData::Var(VarState::Generalized);
    }
}
