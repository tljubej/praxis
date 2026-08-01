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
use crate::type_id::{Type, VarId};

impl TypeDb {
    /// Render `t` to a string.
    ///
    /// Every unbound variable renders `?T`, because outside a scheme there is
    /// nothing that binds it — a leaking variable is a diagnostic smell and the
    /// `?` is how it shows. Inside a scheme, use [`render_scheme`](Self::render_scheme),
    /// which knows which variables the scheme quantifies.
    #[must_use]
    pub fn render(&self, t: Type) -> String {
        let mut out = String::new();
        let mut names = NameAssigner::default();
        self.write_type(t, &mut out, &mut names, &[]);
        out
    }

    /// Render `scheme` as `forall A B. body` (or just `body` if monomorphic).
    #[must_use]
    pub fn render_scheme(&self, scheme: &Scheme) -> String {
        let binders = scheme.binders();
        let mut names = NameAssigner::default();
        for q in binders {
            names.assign(q.to_u32());
        }
        let mut body = String::new();
        self.write_type(scheme.body(), &mut body, &mut names, binders);
        if binders.is_empty() {
            body
        } else {
            let qs: String = binders
                .iter()
                .map(|q| names.name_for(q.to_u32()).to_string())
                .collect::<Vec<_>>()
                .join(" ");
            format!("forall {qs}. {body}")
        }
    }

    /// Render `t` as it appears *inside* `scheme` — a variable the scheme binds
    /// prints as `T`, one it does not prints as `?T`.
    #[must_use]
    pub fn render_in_scheme(&self, t: Type, scheme: &Scheme) -> String {
        let binders = scheme.binders();
        let mut out = String::new();
        let mut names = NameAssigner::default();
        for q in binders {
            names.assign(q.to_u32());
        }
        self.write_type(t, &mut out, &mut names, binders);
        out
    }

    fn write_type(&self, t: Type, out: &mut String, names: &mut NameAssigner, binders: &[VarId]) {
        let t = self.follow(t);
        match self.data(t) {
            TypeData::Scalar(s) => {
                let _ = out.write_str(s.name());
            }
            TypeData::Unit => {
                let _ = out.write_str("Unit");
            }
            TypeData::Never => {
                let _ = out.write_str("Never");
            }
            TypeData::Tuple(els) => {
                out.push('(');
                for (i, el) in els.iter().enumerate() {
                    if i > 0 {
                        out.write_str(", ").ok();
                    }
                    self.write_type(*el, out, names, binders);
                }
                out.push(')');
            }
            TypeData::Func { params, result } => {
                out.push('(');
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        out.write_str(", ").ok();
                    }
                    self.write_type(*p, out, names, binders);
                }
                out.write_str(") -> ").ok();
                self.write_type(*result, out, names, binders);
            }
            TypeData::Collection { ctor, args } => {
                let _ = out.write_str(ctor.name());
                // Through the same writer the nominal types use, so a nullary
                // ctor prints bare: `Range`, not `Range[]` (REP-13).
                self.write_type_args(args, out, names, binders);
            }
            TypeData::Record { def, args } => {
                let rdef = self.record_def(*def);
                match &rdef.name {
                    Some(n) => {
                        let _ = out.write_str(n);
                        self.write_type_args(args, out, names, binders);
                    }
                    None => {
                        // Anonymous structural record: render as `{ x: T, y: U }`
                        // (§5.6; display preserves source order). The spaced
                        // form matches the runtime `record_format` output.
                        out.write_str("{ ").ok();
                        for (i, f) in rdef.fields.iter().enumerate() {
                            if i > 0 {
                                out.write_str(", ").ok();
                            }
                            let _ = out.write_str(&f.name);
                            out.write_str(": ").ok();
                            self.write_type(f.ty, out, names, binders);
                        }
                        out.write_str(" }").ok();
                    }
                }
            }
            TypeData::Enum { def, args } => {
                let edef = self.enum_def(*def);
                match &edef.name {
                    Some(name) => {
                        let _ = out.write_str(name);
                        self.write_type_args(args, out, names, binders);
                    }
                    // An anonymous enum (`choice(...)`) has no name to write —
                    // and used to write *nothing at all*, so every diagnostic
                    // about one read as though the type were missing: `Y001`
                    // said "expected `Int`, found " and `Y122` said "`` has no
                    // variant `Bogus`" (REP-56). It renders like the anonymous
                    // record it sits beside (§5.6): braces for "structural, no
                    // name", `|` between the variants, and each payload in
                    // parentheses so `{ A({ x: Int }) }` cannot be misread as a
                    // record. Every payload is written, so the rendering is
                    // total and no variant is silently invisible.
                    None => {
                        out.write_str("{ ").ok();
                        for (i, v) in edef.variants.iter().enumerate() {
                            if i > 0 {
                                out.write_str(" | ").ok();
                            }
                            let _ = out.write_str(&v.name);
                            if !v.payload.is_empty() {
                                out.push('(');
                                for (j, p) in v.payload.iter().enumerate() {
                                    if j > 0 {
                                        out.write_str(", ").ok();
                                    }
                                    self.write_type(*p, out, names, binders);
                                }
                                out.push(')');
                            }
                        }
                        out.write_str(" }").ok();
                    }
                }
            }
            TypeData::Var(state) => match state {
                // A variable the enclosing scheme quantifies prints bare; one it
                // does not is leaking, and the `?` is how that shows. The state
                // used to answer this (`Generalized` vs `Unbound`), which is
                // why the arena had to carry a flag about schemes at all (F10).
                VarState::Unbound { .. } => {
                    if binders.contains(&VarId::from_raw(t.to_u32())) {
                        let _ = out.write_str(names.name_for(t.to_u32()));
                    } else {
                        let _ = write!(out, "?{}", names.name_for(t.to_u32()));
                    }
                }
                VarState::Linked { .. } => unreachable!("follow resolves Linked"),
            },
        }
    }

    /// Write a type's arguments, `[A, B]`, or nothing when it has none.
    ///
    /// `Option` used to print as a bare name whatever it held, because the
    /// element type lived in a *fresh def per site* rather than in the type
    /// (TY-06) — which is also why the monomorphizer's `db.render` cache key
    /// could not tell `Option[Int]` from `Option[Text]` (MONO-03).
    ///
    /// Collections come through here too, which is REP-13: the collection arm
    /// wrote its own brackets unconditionally, so a `Y001` about a range said
    /// "found `Range[]`". One writer, one rule — the brackets belong to the
    /// arguments and a type with none has none.
    fn write_type_args(
        &self,
        args: &[Type],
        out: &mut String,
        names: &mut NameAssigner,
        binders: &[VarId],
    ) {
        if args.is_empty() {
            return;
        }
        out.push('[');
        for (i, a) in args.iter().enumerate() {
            if i > 0 {
                out.write_str(", ").ok();
            }
            self.write_type(*a, out, names, binders);
        }
        out.push(']');
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
