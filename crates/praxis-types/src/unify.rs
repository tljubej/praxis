//! Unification (ADR-007).
//!
//! [`unify`] makes two types equal by linking type variables to types. It enforces
//! the occurs check (reject infinite types) and applies Pottier's level-lowering
//! rule: when a variable at a deep level is linked to a type containing variables
//! at a shallower level, the shallower variables are *lowered* to the variable's
//! level — otherwise a later outer binding could constrain them after they have
//! been generalized (soundness bug).
//!
//! All failures are returned as [`UnifyError`]; unification never panics.

use crate::data::{TypeData, VarState};
use crate::db::TypeDb;
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
        // Occurs check: `var` must not appear inside `target`.
        if self.occurs(VarId(var.0), target) {
            return Err(UnifyError::Occurs {
                var: VarId(var.0),
                within: target,
            });
        }
        // Level lowering: any unbound var inside `target` at a shallower level
        // than `var` is pulled down to `var`'s level, so it cannot be generalized
        // out from under this binding.
        self.lower_levels(target, var_level);
        self.link(VarId(var.0), target);
        Ok(())
    }

    /// Recurse over `t`, lowering the level of any unbound var whose level is
    /// shallower than `min_level` down to `min_level`.
    fn lower_levels(&mut self, t: Type, min_level: u32) {
        let t = self.prune(t);
        let data = self.data(t).clone();
        match data {
            TypeData::Var(VarState::Unbound { level }) => {
                if level < min_level {
                    self.slots_set(t, TypeData::Var(VarState::Unbound { level: min_level }));
                }
            }
            TypeData::Tuple(els) => {
                for el in els {
                    self.lower_levels(el, min_level);
                }
            }
            TypeData::Func { params, result } => {
                for p in params {
                    self.lower_levels(p, min_level);
                }
                self.lower_levels(result, min_level);
            }
            TypeData::Collection { args, .. } => {
                for a in args {
                    self.lower_levels(a, min_level);
                }
            }
            _ => {}
        }
    }

    /// `true` if `var` occurs (transitively, after pruning) inside `t`.
    fn occurs(&mut self, var: VarId, t: Type) -> bool {
        let t = self.prune(t);
        if t.0 == var.0 {
            return true;
        }
        match self.data(t).clone() {
            TypeData::Tuple(els) => els.into_iter().any(|el| self.occurs(var, el)),
            TypeData::Func { params, result } => {
                params.into_iter().any(|p| self.occurs(var, p)) || self.occurs(var, result)
            }
            TypeData::Collection { args, .. } => args.into_iter().any(|a| self.occurs(var, a)),
            _ => false,
        }
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
        self.slots[t.0 as usize].data = data;
    }
}
