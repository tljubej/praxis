//! Type schemes, generalization, and instantiation (ADR-008).
//!
//! A **scheme** is "a type with some quantified variables", written `forall a b.
//! T`. Generalizing a `var`/`fn` binding turns its inferred monotype into a scheme
//! by quantifying over the type variables that are unbound *and* were created at a
//! level deeper than the binding site. Instantiating a scheme at a use site copies
//! the body with fresh variables in place of the quantified ones.
//!
//! The level discipline (§5.3, ADR-008) keeps this sound in the presence of `var`
//! and partial inference: a var that will be constrained by a *later* binding
//! never gets quantified, because it was created at the outer level.
//!
//! # A scheme owns its binders (F10)
//!
//! Quantification is recorded in exactly one place, the scheme's binder list.
//! `generalize` mutates nothing and `instantiate` substitutes by binder
//! membership, so generalizing one binding cannot change what another scheme's
//! body means.

use crate::constraint::Constraint;
use crate::data::VarState;
use crate::db::TypeDb;
use crate::fold::{FoldMemo, TypeFolder, fold, visit_only_composites};
use crate::type_id::{Type, VarId};

/// A type scheme: `forall <binders>. body`. A bare monomorphic type is a scheme
/// with no binders.
#[derive(Clone, Debug)]
pub struct Scheme {
    /// The quantified variables, in the order they were discovered. Order is
    /// cosmetic (it only affects pretty-printed names) but kept stable for
    /// reproducible diagnostics and snapshots.
    ///
    /// Private: a scheme's binders and its body are one fact, and no caller may
    /// set one without the other.
    binders: Vec<VarId>,
    /// The capability requirements on those binders (F10).
    ///
    /// A requirement inference discovers while checking the body — `a == b`
    /// needs `Eq(?a)` — rides here rather than being decided against the
    /// still-unresolved variable and discarded. Every instantiation re-emits it
    /// against the fresh variables the use site chose, so `equal(f, g)` at a
    /// function type is refused at the site that chose the function type.
    ///
    /// Only constraints on this scheme's own binders are kept. One on a
    /// variable the enclosing scope still owns is not this scheme's to carry —
    /// it stays pending, and the outer binding discharges or generalizes it.
    constraints: Vec<Constraint>,
    /// The scheme body. Binders appear in it as ordinary unbound variables;
    /// instantiating replaces them.
    body: Type,
}

impl Scheme {
    /// A monomorphic scheme (no binders). Equivalent to just `body`.
    ///
    /// Binders **and constraints** are empty by construction: a scheme with no
    /// quantified variables has nothing a constraint could be about.
    #[must_use]
    pub fn monotype(body: Type) -> Scheme {
        Scheme {
            binders: Vec::new(),
            constraints: Vec::new(),
            body,
        }
    }

    /// The quantified variables.
    #[inline]
    #[must_use]
    pub fn binders(&self) -> &[VarId] {
        &self.binders
    }

    /// The capability requirements this scheme carries on its binders.
    #[inline]
    #[must_use]
    pub fn constraints(&self) -> &[Constraint] {
        &self.constraints
    }

    /// The scheme body — the type, with its binders still in it.
    #[inline]
    #[must_use]
    pub fn body(&self) -> Type {
        self.body
    }

    /// Whether this scheme is actually polymorphic.
    #[must_use]
    pub fn is_polymorphic(&self) -> bool {
        !self.binders.is_empty()
    }
}

impl TypeDb {
    /// Generalize `body` at the *current* binding level: quantify over every
    /// unbound variable in `body` whose level is strictly deeper than the
    /// current level.
    ///
    /// Must be called at the level of the *binding site* (i.e. after the inner
    /// scope has been exited), so vars introduced by later/outer bindings are not
    /// stolen.
    ///
    /// Mutates nothing (F10): the binders are recorded on the scheme, and no
    /// quantified variable's arena slot is rewritten.
    #[must_use]
    pub fn generalize(&mut self, body: Type) -> Scheme {
        self.generalize_at(body, self.level())
    }

    /// [`generalize`](Self::generalize) at an explicit binding site, for a
    /// caller that holds a level open around several bindings.
    ///
    /// A declaration group does: one level for the group's mutually-visible
    /// signature placeholders, each declaration's own body a level deeper, and
    /// generalization back at the level the group was entered *from*. Reading
    /// `self.level()` there would answer the group's level and quantify
    /// nothing.
    #[must_use]
    pub fn generalize_at(&mut self, body: Type, site: crate::data::Level) -> Scheme {
        let mut binders = Vec::new();
        let mut folder = Generalizer {
            db: self,
            memo: FoldMemo::new(),
            site,
            binders: &mut binders,
        };
        fold(&mut folder, body);
        // The pending constraints this scheme *owns*: the ones whose variable
        // it just quantified. The rest stay pending for the enclosing binding,
        // which either discharges them or generalizes them itself. Taking all
        // of them would steal a requirement on an outer variable; taking none
        // would discard the requirement altogether.
        let constraints = self.claim_constraints(&binders);
        Scheme {
            binders,
            constraints,
            body,
        }
    }

    /// Every unbound variable in `t`, in first-encounter order.
    ///
    /// [`generalize_at`](Self::generalize_at) without the level test, and for
    /// the question generalization does not ask: *which* variables are in this
    /// type, so a caller holding someone else's binder list can tell whether
    /// they are the same ones. That is what lets a monotype recorded inside a
    /// generic body be rendered against the scheme that quantified its
    /// variables ([`render_with_binders`](Self::render_with_binders)).
    ///
    /// Inspection only, like generalization: no arena slot is written.
    #[must_use]
    pub fn unbound_vars(&mut self, t: Type) -> Vec<VarId> {
        let mut vars = Vec::new();
        let mut folder = FreeVars {
            db: self,
            memo: FoldMemo::new(),
            vars: &mut vars,
        };
        fold(&mut folder, t);
        vars
    }

    /// Instantiate `scheme` at the current level: copy its body, replacing each
    /// binder with a fresh unbound var. Monomorphic schemes are returned with
    /// the body as-is (no allocation).
    #[must_use]
    pub fn instantiate(&mut self, scheme: &Scheme) -> Type {
        self.instantiate_with_mapping(scheme).0
    }

    /// [`instantiate`](Self::instantiate), attributing any constraint the
    /// scheme re-emits to `site` — the expression that made this use.
    ///
    /// A constraint carried through generalization was written inside the
    /// generic body, and reporting a failure there points at code that is
    /// perfectly correct for every *other* instantiation. `site` is where the
    /// program chose the type that failed.
    #[must_use]
    pub fn instantiate_at(&mut self, scheme: &Scheme, site: praxis_source::FileSpan) -> Type {
        let (ty, mapping) = self.instantiate_with_mapping(scheme);
        self.attribute_reemitted(scheme, &mapping, site);
        ty
    }

    /// [`instantiate`](Self::instantiate), also returning the fresh variable
    /// each binder was replaced by, in binder order.
    ///
    /// The mapping is what a caller needs to say *which* type a use site chose
    /// for each quantified variable — monomorphization's key.
    #[must_use]
    pub fn instantiate_with_mapping(&mut self, scheme: &Scheme) -> (Type, Vec<Type>) {
        if scheme.binders.is_empty() {
            return (scheme.body, Vec::new());
        }
        // Map each binder → a fresh var created at the current level.
        let mapping: Vec<Type> = (0..scheme.binders.len())
            .map(|_| self.fresh_var())
            .collect();
        let mut folder = Instantiator {
            db: self,
            memo: FoldMemo::new(),
            binders: &scheme.binders,
            mapping: &mapping,
        };
        let body = fold(&mut folder, scheme.body);
        // Re-emit the scheme's constraints against *this* use's variables
        // (F10). This is the whole point of carrying them: the requirement was
        // written about `?a` inside a generic body, and what has to satisfy it
        // is whatever this call site puts in `?a`'s place.
        self.reemit_constraints(scheme, &mapping);
        (body, mapping)
    }
}

/// Replace each variable in `vars` by the type at the same index in `to`.
///
/// The same substitution [`instantiate`](TypeDb::instantiate) performs, exposed
/// for the other caller that has a binder list and a matching argument list: a
/// generic def's `params` against an instance's `args` (F12). Identity
/// preservation carries over — substituting into a type that mentions none of
/// `vars` returns the handle it was given.
pub(crate) fn substitute(db: &mut TypeDb, t: Type, vars: &[VarId], to: &[Type]) -> Type {
    debug_assert_eq!(vars.len(), to.len(), "one replacement per variable");
    if vars.is_empty() {
        return t;
    }
    let mut folder = Instantiator {
        db,
        memo: FoldMemo::new(),
        binders: vars,
        mapping: to,
    };
    fold(&mut folder, t)
}

/// Generalization as a folder (F9): every unbound variable deeper than the
/// binding site becomes a binder of the scheme being built.
///
/// Inspection only — it collects binders and rebuilds no type, so the scheme
/// body stays the very type that was generalized, and nothing is written to the
/// arena.
struct Generalizer<'a, 'q> {
    db: &'a mut TypeDb,
    memo: FoldMemo,
    site: crate::data::Level,
    binders: &'q mut Vec<VarId>,
}

impl TypeFolder for Generalizer<'_, '_> {
    fn db(&mut self) -> &mut TypeDb {
        self.db
    }
    fn memo(&mut self) -> &mut FoldMemo {
        &mut self.memo
    }
    fn fold_var(&mut self, t: Type, var: VarId, state: &VarState) -> Type {
        if let VarState::Unbound { level } = *state
            && level.is_deeper_than(self.site)
            && !self.binders.contains(&var)
        {
            self.binders.push(var);
        }
        t
    }
    visit_only_composites!();
}

/// The free-variable collection as a folder (F9): every unbound variable, with
/// no level test to exclude the ones an enclosing binding still owns.
///
/// Inspection only, like [`Generalizer`] — it rebuilds no type and writes
/// nothing to the arena.
struct FreeVars<'a, 'q> {
    db: &'a mut TypeDb,
    memo: FoldMemo,
    vars: &'q mut Vec<VarId>,
}

impl TypeFolder for FreeVars<'_, '_> {
    fn db(&mut self) -> &mut TypeDb {
        self.db
    }
    fn memo(&mut self) -> &mut FoldMemo {
        &mut self.memo
    }
    fn fold_var(&mut self, t: Type, var: VarId, state: &VarState) -> Type {
        if matches!(state, VarState::Unbound { .. }) && !self.vars.contains(&var) {
            self.vars.push(var);
        }
        t
    }
    visit_only_composites!();
}

/// Instantiation as a folder (F9): each binder is replaced by its fresh
/// counterpart, and everything else is rebuilt only where that substitution
/// reached.
///
/// The fold preserves identity for free: a body with no applicable binder comes
/// back as the *same* handle instead of a fresh copy of the whole tree, so
/// instantiating a monomorphic-in-practice scheme does not grow the arena — and
/// does not mint a specialized record or enum def per use site when no field
/// type needed substituting.
struct Instantiator<'a, 'q> {
    db: &'a mut TypeDb,
    memo: FoldMemo,
    binders: &'q [VarId],
    mapping: &'q [Type],
}

impl TypeFolder for Instantiator<'_, '_> {
    fn db(&mut self) -> &mut TypeDb {
        self.db
    }
    fn memo(&mut self) -> &mut FoldMemo {
        &mut self.memo
    }
    fn fold_var(&mut self, t: Type, var: VarId, _state: &VarState) -> Type {
        // Binder membership is the whole test (F10). Any arena-side "is
        // generalized" mark would be a fact about *some* scheme rather than
        // this one, and could both skip a variable this scheme binds and
        // substitute one it does not.
        match self.binders.iter().position(|q| *q == var) {
            Some(idx) => self.mapping[idx],
            None => t,
        }
    }
}
