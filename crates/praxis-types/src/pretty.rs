//! Pretty-printing for types and schemes.
//!
//! The rendered form is what diagnostics, hover, and snapshot tests show to a
//! human: `Int`, `(Int, Text)`, `(Int) -> Int`, `forall T. (T) -> T`. Unbound
//! variables (which only appear during a failed inference) render as `?T` so a
//! leak is obvious; quantified variables take stable names `T, U, V, …`.

use std::fmt::Write;

use crate::data::{TypeData, VarState};
use crate::db::TypeDb;
use crate::generalize::Scheme;
use crate::type_id::Type;

impl TypeDb {
    /// Render `t` to a string.
    #[must_use]
    pub fn render(&self, t: Type) -> String {
        let mut out = String::new();
        let mut names = NameAssigner::default();
        self.write_type(t, &mut out, &mut names);
        out
    }

    /// Render `scheme` as `forall A B. body` (or just `body` if monomorphic).
    #[must_use]
    pub fn render_scheme(&self, scheme: &Scheme) -> String {
        let mut names = NameAssigner::default();
        for q in &scheme.quantified {
            names.assign(q.0);
        }
        let mut body = String::new();
        self.write_type(scheme.body, &mut body, &mut names);
        if scheme.quantified.is_empty() {
            body
        } else {
            let qs: String = scheme
                .quantified
                .iter()
                .map(|q| names.name_for(q.0).to_string())
                .collect::<Vec<_>>()
                .join(" ");
            format!("forall {qs}. {body}")
        }
    }

    fn write_type(&self, t: Type, out: &mut String, names: &mut NameAssigner) {
        let t = self.follow(t);
        match &self.slots[t.0 as usize].data {
            TypeData::Scalar(s) => {
                let _ = out.write_str(s.name());
            }
            TypeData::Unit => {
                let _ = out.write_str("Unit");
            }
            TypeData::Tuple(els) => {
                out.push('(');
                for (i, el) in els.iter().enumerate() {
                    if i > 0 {
                        out.write_str(", ").ok();
                    }
                    self.write_type(*el, out, names);
                }
                out.push(')');
            }
            TypeData::Func { params, result } => {
                out.push('(');
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        out.write_str(", ").ok();
                    }
                    self.write_type(*p, out, names);
                }
                out.write_str(") -> ").ok();
                self.write_type(*result, out, names);
            }
            TypeData::Collection { ctor, args } => {
                let _ = out.write_str(ctor.name());
                out.push('[');
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        out.write_str(", ").ok();
                    }
                    self.write_type(*a, out, names);
                }
                out.push(']');
            }
            TypeData::Var(state) => match state {
                VarState::Generalized => {
                    let _ = out.write_str(names.name_for(t.0));
                }
                VarState::Unbound { .. } => {
                    // A leaking unbound var is a diagnostic smell; prefix `?`.
                    let _ = write!(out, "?{}", names.name_for(t.0));
                }
                VarState::Linked { .. } => unreachable!("follow resolves Linked"),
            },
        }
    }
}

/// Assigns stable, sequential names to variable ids encountered in print order.
/// Generalized vars (which get pre-seeded) take the canonical `T, U, V, …`
/// sequence; unbound leak vars fall through to the same sequence.
#[derive(Default)]
struct NameAssigner {
    /// Maps slot index → assigned name. Reused across one render.
    map: std::collections::HashMap<u32, String>,
    next: usize,
}

impl NameAssigner {
    fn assign(&mut self, slot: u32) {
        if self.map.contains_key(&slot) {
            return;
        }
        let name = greek_or_alpha(self.next);
        self.next += 1;
        self.map.insert(slot, name);
    }

    /// Name for a slot, assigning one on the fly if it has not been seen.
    fn name_for(&mut self, slot: u32) -> &str {
        self.assign(slot);
        self.map.get(&slot).expect("assigned").as_str()
    }
}

/// `T`, `U`, …, `Z`, then `A`, `B`, …, then a fallback `T0`, `T1`, …
fn greek_or_alpha(i: usize) -> String {
    const UPPER: &[char] = &[
        'T', 'U', 'V', 'W', 'X', 'Y', 'Z', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K',
        'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S',
    ];
    UPPER
        .get(i)
        .map(|c| c.to_string())
        .unwrap_or_else(|| format!("T{}", i - UPPER.len()))
}
