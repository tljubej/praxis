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

use crate::data::{TypeData, VarState};
use crate::db::TypeDb;
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
        let t = self.prune(t);
        // Read the data into a local first so the immutable borrow ends before any
        // mutation (generalize_var) below.
        match self.data(t).clone() {
            TypeData::Var(VarState::Unbound { level }) => {
                if level > at_level {
                    let var = VarId(t.0);
                    self.generalize_var(var);
                    out.push(var);
                }
            }
            TypeData::Tuple(els) => {
                for el in els {
                    self.generalize_walk(el, at_level, out);
                }
            }
            TypeData::Func { params, result } => {
                for p in params {
                    self.generalize_walk(p, at_level, out);
                }
                self.generalize_walk(result, at_level, out);
            }
            TypeData::Collection { args, .. } => {
                for a in args {
                    self.generalize_walk(a, at_level, out);
                }
            }
            TypeData::Record { def } => {
                let def = self.record_defs[def.0 as usize].clone();
                for f in def.fields {
                    self.generalize_walk(f.ty, at_level, out);
                }
            }
            TypeData::Enum { def } => {
                let def = self.enum_defs[def.0 as usize].clone();
                for v in def.variants {
                    if let Some(payload) = v.payload {
                        for p in payload {
                            self.generalize_walk(p, at_level, out);
                        }
                    }
                }
            }
            // Scalars, Unit, Linked, and already-Generalized vars are left alone.
            _ => {}
        }
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
        let t = self.prune(t);
        match self.data(t).clone() {
            TypeData::Var(VarState::Generalized) => {
                let idx = quantified
                    .iter()
                    .position(|q| q.0 == t.0)
                    .expect("a Generalized var in a scheme body must be listed in `quantified`");
                mapping[idx]
            }
            TypeData::Tuple(els) => {
                let new: Vec<_> = els
                    .into_iter()
                    .map(|el| self.instantiate_walk(el, quantified, mapping))
                    .collect();
                self.intern(TypeData::Tuple(new))
            }
            TypeData::Func { params, result } => {
                let params: Vec<_> = params
                    .into_iter()
                    .map(|p| self.instantiate_walk(p, quantified, mapping))
                    .collect();
                let result = self.instantiate_walk(result, quantified, mapping);
                self.intern(TypeData::Func { params, result })
            }
            TypeData::Collection { ctor, args } => {
                let args: Vec<_> = args
                    .into_iter()
                    .map(|a| self.instantiate_walk(a, quantified, mapping))
                    .collect();
                self.intern(TypeData::Collection { ctor, args })
            }
            TypeData::Record { def } => {
                // A record def is shared; instantiation clones it with fresh
                // field types so the polymorphic record gets its own specialized
                // shape per use site.
                let rdef = self.record_defs[def.0 as usize].clone();
                let fields: Vec<_> = rdef
                    .fields
                    .into_iter()
                    .map(|f| (f.name, self.instantiate_walk(f.ty, quantified, mapping)))
                    .collect();
                match rdef.name {
                    Some(name) => self.register_record(name, fields),
                    None => self.anon_record(fields),
                }
            }
            TypeData::Enum { def } => {
                let edef = self.enum_defs[def.0 as usize].clone();
                let variants: Vec<_> = edef
                    .variants
                    .into_iter()
                    .map(|v| {
                        let payload = v.payload.map(|ps| {
                            ps.into_iter()
                                .map(|p| self.instantiate_walk(p, quantified, mapping))
                                .collect::<Vec<_>>()
                        });
                        (v.name, payload)
                    })
                    .collect();
                self.register_enum(edef.name, variants)
            }
            // Anything else (scalar, unit, unbound/linked var) is structural identity.
            other => self.intern(other),
        }
    }
}
