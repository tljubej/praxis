//! Unification (ADR-007).
//!
//! [`unify`] makes two types equal by linking type variables to types. It enforces
//! the occurs check (reject infinite types) and applies Pottier's level-lowering
//! rule: when a variable is linked to a type containing variables at a *deeper*
//! level, those deeper variables are lowered to the linked variable's level —
//! otherwise generalizing at the inner level would quantify a variable the outer
//! environment can still reach (soundness bug; TY-01).
//!
//! All failures are returned as [`UnifyError`]; unification never panics.

use crate::data::{Level, TypeData, VarState};
use crate::db::TypeDb;
use crate::fold::{fold, visit_only_composites, FoldMemo, TypeFolder};
use crate::type_id::{Type, VarId};

/// Why two types could not be unified.
#[derive(Clone, Debug)]
pub enum UnifyError {
    /// Two structurally different concrete types (or scalar vs scalar mismatch).
    /// `expected`/`found` are the *representatives* at the failure point, included
    /// so diagnostics can pretty-print them without re-resolving.
    Mismatch { expected: Type, found: Type },
    /// The occurs check failed: a variable occurs inside the very type it would be
    /// linked to (e.g. unifying `a` with `(a) -> a`). This is an infinite type.
    Occurs { var: VarId, within: Type },
}

impl TypeDb {
    /// Make `a` and `b` equal, or return the reason they cannot be.
    pub fn unify(&mut self, a: Type, b: Type) -> Result<(), UnifyError> {
        let a = self.prune(a);
        let b = self.prune(b);
        if a == b {
            return Ok(());
        }
        // Order-insensitive handling of variables: handle the case where either
        // side is an unbound var before touching concrete types.
        let (da, db) = (self.data(a).clone(), self.data(b).clone());
        match (&da, &db) {
            // Var link to anything (with occurs check + level lowering).
            (TypeData::Var(VarState::Unbound { .. }), _) => self.link_var(a, b),
            (_, TypeData::Var(VarState::Unbound { .. })) => self.link_var(b, a),
            // Two concrete types: recurse structurally.
            _ => self.unify_concrete(a, b, da, db),
        }
    }

    /// Link unbound var `v` to `target`. Applies the occurs check and level lowering.
    fn link_var(&mut self, var: Type, target: Type) -> Result<(), UnifyError> {
        let var_level = match self.data(var) {
            TypeData::Var(VarState::Unbound { level }) => *level,
            _ => unreachable!("link_var called on non-unbound var"),
        };
        let var_id = VarId::from_raw(var.to_u32());
        // Occurs check: `var` must not appear inside `target`.
        if self.occurs(var_id, target) {
            return Err(UnifyError::Occurs {
                var: var_id,
                within: target,
            });
        }
        // Level lowering: any unbound var inside `target` *deeper* than `var` is
        // pulled out to `var`'s level, so an inner generalization cannot quantify
        // something this binding still reaches.
        self.lower_levels(target, var_level);
        self.link(var_id, target);
        Ok(())
    }

    /// Recurse over `t`, lowering every unbound var deeper than `outer` to
    /// `outer` (ADR-008, Pottier's rule `level(w) := min(level(w), level(v))`).
    fn lower_levels(&mut self, t: Type, outer: Level) {
        let mut folder = LevelLowerer {
            db: self,
            memo: FoldMemo::new(),
            outer,
        };
        fold(&mut folder, t);
    }

    /// `true` if `var` occurs (transitively, after pruning) inside `t`.
    fn occurs(&mut self, var: VarId, t: Type) -> bool {
        let mut folder = OccursCheck {
            db: self,
            memo: FoldMemo::new(),
            var,
            found: false,
        };
        fold(&mut folder, t);
        folder.found
    }

    fn unify_concrete(
        &mut self,
        a: Type,
        b: Type,
        da: TypeData,
        db: TypeData,
    ) -> Result<(), UnifyError> {
        match (da, db) {
            (TypeData::Scalar(x), TypeData::Scalar(y)) if x == y => Ok(()),
            (TypeData::Unit, TypeData::Unit) => Ok(()),
            (TypeData::Tuple(xs), TypeData::Tuple(ys)) => self.unify_seqs(a, b, xs, ys, "tuple"),
            (
                TypeData::Func {
                    params: ps_a,
                    result: r_a,
                },
                TypeData::Func {
                    params: ps_b,
                    result: r_b,
                },
            ) => {
                if ps_a.len() != ps_b.len() {
                    return Err(UnifyError::Mismatch {
                        expected: a,
                        found: b,
                    });
                }
                for (p, q) in ps_a.into_iter().zip(ps_b) {
                    self.unify(p, q).map_err(|_| UnifyError::Mismatch {
                        expected: a,
                        found: b,
                    })?;
                }
                self.unify(r_a, r_b).map_err(|_| UnifyError::Mismatch {
                    expected: a,
                    found: b,
                })
            }
            (
                TypeData::Collection {
                    ctor: c_a,
                    args: args_a,
                },
                TypeData::Collection {
                    ctor: c_b,
                    args: args_b,
                },
            ) if c_a == c_b => self.unify_seqs(a, b, args_a, args_b, "collection"),
            // Records unify iff same def-id, OR both anonymous with matching
            // field-name sets (in which case we unify field types pairwise by
            // name). Nominal records with different def-ids never unify even
            // with identical fields (§4.5: nominal).
            (TypeData::Record { def: d_a }, TypeData::Record { def: d_b }) => {
                if d_a == d_b {
                    return Ok(());
                }
                let ra = self.record_def(d_a).clone();
                let rb = self.record_def(d_b).clone();
                // Both must be anonymous to consider structural unification.
                if ra.name.is_some() || rb.name.is_some() {
                    return Err(UnifyError::Mismatch {
                        expected: a,
                        found: b,
                    });
                }
                if ra.arity() != rb.arity() {
                    return Err(UnifyError::Mismatch {
                        expected: a,
                        found: b,
                    });
                }
                // Match fields by name, unify pairwise. §5.6: order-independent
                // identity, so align by name rather than position.
                for fa in &ra.fields {
                    let Some((_, ty_b)) = rb.field(&fa.name) else {
                        return Err(UnifyError::Mismatch {
                            expected: a,
                            found: b,
                        });
                    };
                    self.unify(fa.ty, ty_b).map_err(|_| UnifyError::Mismatch {
                        expected: a,
                        found: b,
                    })?;
                }
                // Link the two defs so subsequent uses resolve to one. We adopt
                // the earlier def-id (d_a) as the canonical one by rewriting b's
                // slot to point at d_a.
                self.slots_set(b, TypeData::Record { def: d_a });
                Ok(())
            }
            // Enums unify iff same def-id, OR (M9) the two defs share the same
            // name and the same variant-name signature — in which case their
            // payloads unify pairwise by variant position and the later def-id
            // is rewritten to point at the canonical (earlier) one.
            //
            // Why the second clause: a polymorphic enum scheme such as the
            // prelude `forall T. Option[T]` is *instantiate*d into a fresh
            // `EnumDef` per use site (generalize.rs), so `Some(5)` and an
            // `Option[Int]` annotation carry different def-ids despite being
            // structurally the same named enum. Pure identity-only comparison
            // would reject them. Two user-declared enums can never share a name
            // in one scope (the resolver binds the name once), so this relaxed
            // arm can only fire for compiler-stamped copies of a polymorphic
            // enum — it never collapses two genuinely-distinct nominal types.
            // Anonymous enums from `choice(...)` share the synthetic name "" and
            // a variant-name signature, so two independently-stamped copies of
            // the same `choice` also unify here. This mirrors the anonymous
            // record arm above (structural identity by field-name set + link).
            (TypeData::Enum { def: d_a }, TypeData::Enum { def: d_b }) => {
                if d_a == d_b {
                    return Ok(());
                }
                let ea = self.enum_def(d_a).clone();
                let eb = self.enum_def(d_b).clone();
                // Same name + same variant-name signature is the precondition.
                let same_shape = ea.name == eb.name
                    && ea.variants.len() == eb.variants.len()
                    && ea
                        .variants
                        .iter()
                        .zip(&eb.variants)
                        .all(|(va, vb)| va.name == vb.name);
                if !same_shape {
                    return Err(UnifyError::Mismatch {
                        expected: a,
                        found: b,
                    });
                }
                // Unify each variant's payload pairwise (zip by declaration
                // order, which the name check above already aligned).
                //
                // TY-05: one representation, so one comparison. The three-way
                // match this replaces had a catch-all that rejected
                // `(None, Some([]))` — the very pair `EnumVariantDef`'s own doc
                // comment called equivalent.
                for (va, vb) in ea.variants.iter().zip(&eb.variants) {
                    if va.payload.len() != vb.payload.len() {
                        return Err(UnifyError::Mismatch {
                            expected: a,
                            found: b,
                        });
                    }
                    for (pa, pb) in va.payload.iter().zip(&vb.payload) {
                        self.unify(*pa, *pb).map_err(|_| UnifyError::Mismatch {
                            expected: a,
                            found: b,
                        })?;
                    }
                }
                // Adopt the earlier def-id (d_a) as canonical: rewrite b's slot
                // to point at d_a so subsequent uses resolve to one def.
                self.slots_set(b, TypeData::Enum { def: d_a });
                Ok(())
            }
            _ => Err(UnifyError::Mismatch {
                expected: a,
                found: b,
            }),
        }
    }

    fn unify_seqs(
        &mut self,
        a: Type,
        b: Type,
        xs: Vec<Type>,
        ys: Vec<Type>,
        _kind: &str,
    ) -> Result<(), UnifyError> {
        if xs.len() != ys.len() {
            return Err(UnifyError::Mismatch {
                expected: a,
                found: b,
            });
        }
        for (x, y) in xs.into_iter().zip(ys) {
            self.unify(x, y).map_err(|_| UnifyError::Mismatch {
                expected: a,
                found: b,
            })?;
        }
        Ok(())
    }

    // --- small internal helper kept private to this module ------------------

    fn slots_set(&mut self, t: Type, data: TypeData) {
        self.slots[t.to_u32() as usize].data = data;
    }
}

/// Pottier's level-lowering rule as a folder (F9): every unbound variable
/// inside the type being linked is pulled *out* to the level of the variable it
/// is linked to, so a later generalization at an inner level cannot quantify a
/// variable the outer environment can still reach.
///
/// # TY-01
///
/// The comparison used to run the other way — `if level < min_level`, writing
/// `min_level` back — which **raised** older variables into the inner scope
/// instead of lowering inner ones out of it. The effect is the soundness bug
/// the rule exists to prevent: linking an outer variable to a type containing
/// an inner one left the inner one deep, so generalizing the result quantified
/// a variable still reachable from the environment.
///
/// [`Level::clamp_to`] is the whole fix, and it is why `Level` is a newtype:
/// the rule is monotone-decreasing, so the reversed form is now unwritable.
///
/// Inspection only — it rewrites variable *states*, never the types that
/// contain them, so it uses [`visit_only_composites`] rather than the rebuilding
/// defaults.
struct LevelLowerer<'a> {
    db: &'a mut TypeDb,
    memo: FoldMemo,
    outer: Level,
}

impl TypeFolder for LevelLowerer<'_> {
    fn db(&mut self) -> &mut TypeDb {
        self.db
    }
    fn memo(&mut self) -> &mut FoldMemo {
        &mut self.memo
    }
    fn fold_var(&mut self, t: Type, _var: VarId, state: &VarState) -> Type {
        if let VarState::Unbound { level } = *state {
            let mut lowered = level;
            lowered.clamp_to(self.outer);
            if lowered != level {
                self.db
                    .slots_set(t, TypeData::Var(VarState::Unbound { level: lowered }));
            }
        }
        t
    }
    visit_only_composites!();
}

/// The occurs check as a folder (F9): does `var` appear anywhere inside the
/// type it is about to be linked to?
///
/// The fold visits every reachable type once rather than short-circuiting on
/// the first hit, which is what the memo buys — the hand-written version had no
/// memo and would recurse forever on a type that reaches itself through a
/// record def.
struct OccursCheck<'a> {
    db: &'a mut TypeDb,
    memo: FoldMemo,
    var: VarId,
    found: bool,
}

impl TypeFolder for OccursCheck<'_> {
    fn db(&mut self) -> &mut TypeDb {
        self.db
    }
    fn memo(&mut self) -> &mut FoldMemo {
        &mut self.memo
    }
    fn fold_var(&mut self, t: Type, var: VarId, _state: &VarState) -> Type {
        if var == self.var {
            self.found = true;
        }
        t
    }
    visit_only_composites!();
}
