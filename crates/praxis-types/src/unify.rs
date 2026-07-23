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
            TypeData::Record { def } => {
                let def = self.record_defs[def.0 as usize].clone();
                for f in def.fields {
                    self.lower_levels(f.ty, min_level);
                }
            }
            TypeData::Enum { def } => {
                let def = self.enum_defs[def.0 as usize].clone();
                for v in def.variants {
                    if let Some(payload) = v.payload {
                        for p in payload {
                            self.lower_levels(p, min_level);
                        }
                    }
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
            TypeData::Record { def } => {
                let def = self.record_defs[def.0 as usize].clone();
                def.fields.into_iter().any(|f| self.occurs(var, f.ty))
            }
            TypeData::Enum { def } => {
                let def = self.enum_defs[def.0 as usize].clone();
                def.variants.into_iter().any(|v| {
                    v.payload
                        .map(|ps| ps.into_iter().any(|p| self.occurs(var, p)))
                        .unwrap_or(false)
                })
            }
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
            // Records unify iff same def-id, OR both anonymous with matching
            // field-name sets (in which case we unify field types pairwise by
            // name). Nominal records with different def-ids never unify even
            // with identical fields (§4.5: nominal).
            (TypeData::Record { def: d_a }, TypeData::Record { def: d_b }) => {
                if d_a == d_b {
                    return Ok(());
                }
                let ra = self.record_defs[d_a.0 as usize].clone();
                let rb = self.record_defs[d_b.0 as usize].clone();
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
            // Enums unify iff same def-id (nominal by name, §4.6).
            (TypeData::Enum { def: d_a }, TypeData::Enum { def: d_b }) if d_a == d_b => Ok(()),
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
