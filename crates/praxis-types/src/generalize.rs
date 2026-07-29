//! Type schemes, generalization, and instantiation (ADR-008).
//!
//! A **scheme** is "a type with some quantified variables", written `forall a b.
//! T`. Generalizing a `let`/`fn` binding turns its inferred monotype into a scheme
//! by quantifying over the type variables that are unbound *and* were created at a
//! level deeper than the binding site. Instantiating a scheme at a use site copies
//! the body with fresh variables in place of the quantified ones.
//!
//! The level discipline (§5.3, ADR-008) keeps this sound in the presence of `var`
//! and partial inference: a var that will be constrained by a *later* binding
//! never gets quantified, because it was created at the outer level.

use crate::data::VarState;
use crate::db::TypeDb;
use crate::fold::{fold, visit_only_composites, FoldMemo, TypeFolder};
use crate::type_id::{Type, VarId};

/// A type scheme: `forall <quantified>. body`. A bare monomorphic type is a scheme
/// with an empty `quantified` set.
#[derive(Clone, Debug)]
pub struct Scheme {
    /// The generalized variables, in the order they were discovered. Order is
    /// cosmetic (it only affects pretty-printed names) but kept stable for
    /// reproducible diagnostics and snapshots.
    pub quantified: Vec<VarId>,
    /// The scheme body. Quantified vars appear in it as [`VarState::Generalized`]
    /// slots; instantiating replaces them.
    pub body: Type,
}

impl Scheme {
    /// A monomorphic scheme (no quantified variables). Equivalent to just `body`.
    #[must_use]
    pub fn monotype(body: Type) -> Scheme {
        Scheme {
            quantified: Vec::new(),
            body,
        }
    }

    /// Whether this scheme is actually polymorphic.
    #[must_use]
    pub fn is_polymorphic(&self) -> bool {
        !self.quantified.is_empty()
    }
}

impl TypeDb {
    /// Generalize `body` at the *current* binding level: quantify over every
    /// unbound variable in `body` whose level is strictly greater than the current
    /// level, marking those slots [`Generalized`](VarState::Generalized).
    ///
    /// Must be called at the level of the *binding site* (i.e. after the inner
    /// scope has been exited), so vars introduced by later/outer bindings are not
    /// stolen.
    #[must_use]
    pub fn generalize(&mut self, body: Type) -> Scheme {
        let level = self.level();
        let mut quantified = Vec::new();
        self.generalize_walk(body, level, &mut quantified);
        Scheme { quantified, body }
    }

    fn generalize_walk(&mut self, t: Type, at_level: u32, out: &mut Vec<VarId>) {
        let mut folder = Generalizer {
            db: self,
            memo: FoldMemo::new(),
            at_level,
            quantified: out,
        };
        fold(&mut folder, t);
    }

    /// Instantiate `scheme` at the current level: copy its body, replacing each
    /// quantified variable with a fresh unbound var. Monomorphic schemes are
    /// returned with the body as-is (no allocation).
    #[must_use]
    pub fn instantiate(&mut self, scheme: &Scheme) -> Type {
        if scheme.quantified.is_empty() {
            return scheme.body;
        }
        // Map each quantified var id → a fresh var created at the current level.
        let mut mapping = vec![Type(0); scheme.quantified.len()];
        for slot in &mut mapping {
            *slot = self.fresh_var();
        }
        self.instantiate_walk(scheme.body, &scheme.quantified, &mapping)
    }

    fn instantiate_walk(&mut self, t: Type, quantified: &[VarId], mapping: &[Type]) -> Type {
        let mut folder = Instantiator {
            db: self,
            memo: FoldMemo::new(),
            quantified,
            mapping,
        };
        fold(&mut folder, t)
    }
}

/// Generalization as a folder (F9): every unbound variable deeper than the
/// current level becomes a binder of the scheme being built.
///
/// Inspection only — it rewrites variable *states* and collects binders, and
/// rebuilds no type, so the scheme body stays the very type that was
/// generalized.
struct Generalizer<'a, 'q> {
    db: &'a mut TypeDb,
    memo: FoldMemo,
    at_level: u32,
    quantified: &'q mut Vec<VarId>,
}

impl TypeFolder for Generalizer<'_, '_> {
    fn db(&mut self) -> &mut TypeDb {
        self.db
    }
    fn memo(&mut self) -> &mut FoldMemo {
        &mut self.memo
    }
    fn fold_var(&mut self, t: Type, var: VarId, state: &VarState) -> Type {
        if let VarState::Unbound { level } = *state {
            if level > self.at_level {
                self.db.generalize_var(var);
                self.quantified.push(var);
            }
        }
        t
    }
    visit_only_composites!();
}

/// Instantiation as a folder (F9): each quantified variable is replaced by its
/// fresh counterpart, and everything else is rebuilt only where that
/// substitution reached.
///
/// TY-02 is the identity preservation the fold gives for free: a body with no
/// applicable binder now comes back as the *same* handle instead of a fresh
/// copy of the whole tree, so instantiating a monomorphic-in-practice scheme
/// stops growing the arena — and stops minting a specialized record or enum def
/// per use site when no field type needed substituting.
struct Instantiator<'a, 'q> {
    db: &'a mut TypeDb,
    memo: FoldMemo,
    quantified: &'q [VarId],
    mapping: &'q [Type],
}

impl TypeFolder for Instantiator<'_, '_> {
    fn db(&mut self) -> &mut TypeDb {
        self.db
    }
    fn memo(&mut self) -> &mut FoldMemo {
        &mut self.memo
    }
    fn fold_var(&mut self, t: Type, var: VarId, state: &VarState) -> Type {
        // Only a *generalized* var is a binder. An unbound one belongs to the
        // enclosing scope and must survive instantiation unchanged (TY-02's
        // "instantiation preserves non-quantified variable identity").
        if !matches!(state, VarState::Generalized) {
            return t;
        }
        match self.quantified.iter().position(|q| *q == var) {
            Some(idx) => self.mapping[idx],
            // A generalized var the scheme does not bind. The hand-written walk
            // panicked here; leaving it alone is the conservative answer, and
            // TY-03's scheme-owned binders are what make the case
            // unrepresentable rather than merely survivable.
            None => t,
        }
    }
}
