//! The one walk over [`TypeData`] (F9).
//!
//! Five recursive walks over the type arena were written independently —
//! `lower_levels` and `occurs` (unify), `generalize_walk` and
//! `instantiate_walk` (generalize), and `deep_resolve` (db) — and every one of
//! them ended in a `_ => …` catch-all. A catch-all over `TypeData` is a silent
//! omission waiting to happen, and it has happened twice:
//!
//! * ADR-025 records `instantiate_walk` losing its `Collection` arm exactly
//!   this way.
//! * `deep_resolve`'s `_ => t` skips `Record` and `Enum` *today*, so the crash
//!   debugger's static-type capture (ADR-035) hands back a record whose fields
//!   are still unresolved variables.
//!
//! [`fold`] is the single traversal, its match is exhaustive with no catch-all,
//! and a folder overrides only the arms it cares about. Adding a `TypeData`
//! variant is now a compile error in one place instead of five silent skips.
//!
//! # Two things the fold does that no hand-written walk did
//!
//! **A cycle memo.** Record and enum children live in side tables, not in the
//! type slot, so a type can reach itself through a def — and none of the five
//! walks guarded against it. `deep_resolve` in particular would hang. Every
//! type is folded at most once per fold, and a type reached again while it is
//! still being folded maps to itself.
//!
//! **Identity preservation.** A composite is re-interned only if a child
//! actually changed. `instantiate` of a scheme with no applicable binder now
//! returns the *same* handle rather than a fresh copy of the whole tree, which
//! is what stopped the arena growing per instantiation (TY-02) — and, for a
//! record or enum, what stops a fresh nominal def being minted per use site
//! when nothing about the def needed specializing.
//!
//! # Writing a folder
//!
//! A folder is a struct holding `&mut TypeDb`, a [`FoldMemo`], and whatever
//! state the walk needs. Overriding no arm gives the identity fold. A folder
//! that only *inspects* — `occurs`, level clamping, generalization — overrides
//! [`TypeFolder::fold_var`], mutates or records what it must, and returns the
//! type it was given; nothing is then rebuilt anywhere.

use std::collections::HashMap;

use crate::ctor::{FieldSet, VariantSet};
use crate::data::{EnumVariantDef, RecordFieldDef, TypeData, VarState};
use crate::db::TypeDb;
use crate::type_id::{Type, VarId};
use crate::{CollectionCtor, EnumDefId, RecordDefId, ScalarType};

/// Which types a fold has already visited, and what they folded to.
///
/// Owned by the folder rather than by [`fold`] so that the default composite
/// arms — which call `fold` again for each child — share one memo for the whole
/// traversal. Without that, recursion through a record's side-table fields has
/// nothing to terminate it.
#[derive(Default, Debug)]
pub struct FoldMemo {
    seen: HashMap<Type, Type>,
}

impl FoldMemo {
    /// An empty memo.
    #[must_use]
    pub fn new() -> FoldMemo {
        FoldMemo::default()
    }
}

/// A traversal of the type arena. See the module docs.
///
/// Every method has a default, so a folder states only what it changes. The
/// composite defaults fold their children and rebuild only on change; the leaf
/// defaults are the identity.
pub trait TypeFolder {
    /// The arena being folded. Folders hold `&mut TypeDb` and hand it out here.
    fn db(&mut self) -> &mut TypeDb;

    /// This traversal's memo. One per fold, or recursion through a record def
    /// does not terminate.
    fn memo(&mut self) -> &mut FoldMemo;

    /// A scalar type (`Int`, `Text`, …). No children.
    fn fold_scalar(&mut self, t: Type, _scalar: ScalarType) -> Type {
        t
    }

    /// The `Unit` type. No children.
    fn fold_unit(&mut self, t: Type) -> Type {
        t
    }

    /// A type variable, in whatever state pruning left it: `Unbound` or
    /// `Generalized` (a `Linked` var is followed before the folder sees it).
    fn fold_var(&mut self, t: Type, _var: VarId, _state: &VarState) -> Type {
        t
    }

    /// A tuple. The default folds each element and re-interns only on change.
    fn fold_tuple(&mut self, t: Type, elements: &[Type]) -> Type {
        fold_tuple_default(self, t, elements)
    }

    /// A function type. The default folds parameters and result and re-interns
    /// only on change.
    fn fold_func(&mut self, t: Type, params: &[Type], result: Type) -> Type {
        fold_func_default(self, t, params, result)
    }

    /// A collection (`Vec[T]`, `Map[K, V]`, …). The default folds each argument
    /// and re-interns only on change.
    fn fold_collection(&mut self, t: Type, ctor: CollectionCtor, args: &[Type]) -> Type {
        fold_collection_default(self, t, ctor, args)
    }

    /// A record. Its field types live in the `TypeDb`'s def side table, so the
    /// default folds them there and registers a **specialized def** only if one
    /// changed — preserving the def id, and the nominal identity that hangs off
    /// it, whenever nothing did.
    fn fold_record(&mut self, t: Type, def: RecordDefId) -> Type {
        fold_record_default(self, t, def)
    }

    /// An enum. As [`fold_record`](Self::fold_record), over variant payloads.
    fn fold_enum(&mut self, t: Type, def: EnumDefId) -> Type {
        fold_enum_default(self, t, def)
    }
}

/// Fold `t` under `folder`.
///
/// The type is pruned first, so a folder never sees a `Linked` variable. The
/// match below is the crate's **only** exhaustive walk over [`TypeData`]; keep
/// it that way.
pub fn fold<F: TypeFolder + ?Sized>(folder: &mut F, t: Type) -> Type {
    let t = folder.db().prune(t);
    if let Some(&done) = folder.memo().seen.get(&t) {
        return done;
    }
    // Tie the knot before descending: a type reached again while it is still
    // being folded maps to itself, which is what makes a cyclic record def
    // terminate instead of recursing forever.
    folder.memo().seen.insert(t, t);

    let data = folder.db().data(t).clone();
    let folded = match data {
        TypeData::Scalar(scalar) => folder.fold_scalar(t, scalar),
        TypeData::Unit => folder.fold_unit(t),
        TypeData::Tuple(elements) => folder.fold_tuple(t, &elements),
        TypeData::Func { params, result } => folder.fold_func(t, &params, result),
        TypeData::Collection { ctor, args } => folder.fold_collection(t, ctor, &args),
        TypeData::Record { def } => folder.fold_record(t, def),
        TypeData::Enum { def } => folder.fold_enum(t, def),
        TypeData::Var(state) => folder.fold_var(t, VarId::from_raw(t.to_u32()), &state),
    };

    folder.memo().seen.insert(t, folded);
    folded
}

/// Fold every element of `types`, reporting whether any of them changed.
fn fold_all<F: TypeFolder + ?Sized>(folder: &mut F, types: &[Type]) -> (Vec<Type>, bool) {
    let mut changed = false;
    let folded = types
        .iter()
        .map(|&child| {
            let next = fold(folder, child);
            changed |= next != child;
            next
        })
        .collect();
    (folded, changed)
}

/// The default [`TypeFolder::fold_tuple`], callable from an override that wants
/// the ordinary behaviour for some tuples.
pub fn fold_tuple_default<F: TypeFolder + ?Sized>(
    folder: &mut F,
    t: Type,
    elements: &[Type],
) -> Type {
    let (folded, changed) = fold_all(folder, elements);
    if changed {
        // Arity is preserved by construction, so the original tuple's validity
        // carries over — no `TupleElems::new` round trip.
        folder.db().intern(TypeData::Tuple(folded))
    } else {
        t
    }
}

/// The default [`TypeFolder::fold_func`].
pub fn fold_func_default<F: TypeFolder + ?Sized>(
    folder: &mut F,
    t: Type,
    params: &[Type],
    result: Type,
) -> Type {
    let (folded_params, params_changed) = fold_all(folder, params);
    let folded_result = fold(folder, result);
    if params_changed || folded_result != result {
        folder.db().intern(TypeData::Func {
            params: folded_params,
            result: folded_result,
        })
    } else {
        t
    }
}

/// The default [`TypeFolder::fold_collection`].
pub fn fold_collection_default<F: TypeFolder + ?Sized>(
    folder: &mut F,
    t: Type,
    ctor: CollectionCtor,
    args: &[Type],
) -> Type {
    let (folded, changed) = fold_all(folder, args);
    if changed {
        folder
            .db()
            .intern(TypeData::Collection { ctor, args: folded })
    } else {
        t
    }
}

/// The default [`TypeFolder::fold_record`].
pub fn fold_record_default<F: TypeFolder + ?Sized>(
    folder: &mut F,
    t: Type,
    def: RecordDefId,
) -> Type {
    let rdef = folder.db().record_def(def).clone();
    let mut changed = false;
    let mut fields = Vec::with_capacity(rdef.fields.len());
    for field in &rdef.fields {
        let ty = fold(folder, field.ty);
        changed |= ty != field.ty;
        fields.push(RecordFieldDef {
            name: field.name.clone(),
            ty,
        });
    }
    if !changed {
        return t;
    }
    let name = rdef.name.clone();
    folder.db().record(name, FieldSet::preserving(fields))
}

/// Fold every type in `types` for effect, discarding the results.
///
/// The inspection-only half of the trait: a folder that records or mutates
/// something and rebuilds nothing uses this instead of the rebuilding defaults.
pub fn visit_all<F: TypeFolder + ?Sized>(folder: &mut F, types: &[Type]) {
    for &child in types {
        fold(folder, child);
    }
}

/// Fold a record def's field types for effect.
pub fn visit_record_fields<F: TypeFolder + ?Sized>(folder: &mut F, def: RecordDefId) {
    let field_types: Vec<Type> = folder
        .db()
        .record_def(def)
        .fields
        .iter()
        .map(|f| f.ty)
        .collect();
    visit_all(folder, &field_types);
}

/// Fold an enum def's variant payload types for effect.
pub fn visit_enum_payloads<F: TypeFolder + ?Sized>(folder: &mut F, def: EnumDefId) {
    let payloads: Vec<Type> = folder
        .db()
        .enum_def(def)
        .variants
        .iter()
        .flat_map(|v| v.payload.iter().copied())
        .collect();
    visit_all(folder, &payloads);
}

/// The five composite arms of an **inspection-only** folder: descend into every
/// child for effect, return the type unchanged.
///
/// A folder that only records or mutates — level clamping, the occurs check,
/// generalization — must not use the rebuilding defaults, because pruning a
/// linked child counts as a change and would intern a fresh composite in the
/// middle of unification. This states "I rebuild nothing" once instead of five
/// times per folder.
macro_rules! visit_only_composites {
    () => {
        fn fold_tuple(&mut self, t: $crate::Type, elements: &[$crate::Type]) -> $crate::Type {
            $crate::fold::visit_all(self, elements);
            t
        }
        fn fold_func(
            &mut self,
            t: $crate::Type,
            params: &[$crate::Type],
            result: $crate::Type,
        ) -> $crate::Type {
            $crate::fold::visit_all(self, params);
            $crate::fold::fold(self, result);
            t
        }
        fn fold_collection(
            &mut self,
            t: $crate::Type,
            _ctor: $crate::CollectionCtor,
            args: &[$crate::Type],
        ) -> $crate::Type {
            $crate::fold::visit_all(self, args);
            t
        }
        fn fold_record(&mut self, t: $crate::Type, def: $crate::RecordDefId) -> $crate::Type {
            $crate::fold::visit_record_fields(self, def);
            t
        }
        fn fold_enum(&mut self, t: $crate::Type, def: $crate::EnumDefId) -> $crate::Type {
            $crate::fold::visit_enum_payloads(self, def);
            t
        }
    };
}

pub(crate) use visit_only_composites;

/// The default [`TypeFolder::fold_enum`].
pub fn fold_enum_default<F: TypeFolder + ?Sized>(folder: &mut F, t: Type, def: EnumDefId) -> Type {
    let edef = folder.db().enum_def(def).clone();
    let mut changed = false;
    let mut variants = Vec::with_capacity(edef.variants.len());
    for variant in &edef.variants {
        // TY-05: one payload representation, so one arm. This was a two-arm
        // match over `Option<Vec<Type>>` whose `None` case existed only to say
        // "the empty payload, spelled the other way".
        let (payload, any) = fold_all(folder, &variant.payload);
        changed |= any;
        variants.push(EnumVariantDef {
            name: variant.name.clone(),
            payload,
        });
    }
    if !changed {
        return t;
    }
    let name = edef.name.clone();
    folder.db().enum_(name, VariantSet::preserving(variants))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A nominal record from `(name, type)` pairs.
    fn record(db: &mut TypeDb, name: &str, fields: Vec<(String, Type)>) -> Type {
        let fields = FieldSet::from_pairs(fields).expect("distinct field names");
        db.record(Some(name.to_string()), fields)
    }

    /// A nominal enum from `(name, payload)` pairs.
    fn enum_ty(db: &mut TypeDb, name: &str, variants: Vec<(String, Vec<Type>)>) -> Type {
        let variants = VariantSet::from_pairs(variants).expect("distinct variant names");
        db.enum_(Some(name.to_string()), variants)
    }

    /// The identity fold: overrides nothing.
    struct Identity<'a> {
        db: &'a mut TypeDb,
        memo: FoldMemo,
    }

    impl TypeFolder for Identity<'_> {
        fn db(&mut self) -> &mut TypeDb {
            self.db
        }
        fn memo(&mut self) -> &mut FoldMemo {
            &mut self.memo
        }
    }

    /// Rewrites every `Int` it meets to `Text`, and counts the variables it saw.
    struct IntToText<'a> {
        db: &'a mut TypeDb,
        memo: FoldMemo,
        vars_seen: usize,
    }

    impl TypeFolder for IntToText<'_> {
        fn db(&mut self) -> &mut TypeDb {
            self.db
        }
        fn memo(&mut self) -> &mut FoldMemo {
            &mut self.memo
        }
        fn fold_scalar(&mut self, t: Type, scalar: ScalarType) -> Type {
            if scalar == ScalarType::Int {
                self.db.text()
            } else {
                t
            }
        }
        fn fold_var(&mut self, t: Type, _var: VarId, _state: &VarState) -> Type {
            self.vars_seen += 1;
            t
        }
    }

    /// TY-02. A fold that changes nothing returns the handles it was given, all
    /// the way down — no re-interning, no arena growth. This is what makes
    /// `instantiate` of a monomorphic scheme free.
    #[test]
    fn an_identity_fold_returns_the_same_handles_and_interns_nothing() {
        let mut db = TypeDb::new();
        let (int, text) = (db.int(), db.text());
        let tuple = db.pair(int, text);
        let vec_of_tuple = db.vec(tuple);
        let func = db.func(vec![vec_of_tuple], int);
        let before = db.len();

        let mut folder = Identity {
            db: &mut db,
            memo: FoldMemo::new(),
        };
        assert_eq!(fold(&mut folder, func), func);
        assert_eq!(fold(&mut folder, vec_of_tuple), vec_of_tuple);
        assert_eq!(db.len(), before, "an identity fold interns nothing");
    }

    /// A change at a leaf rebuilds exactly the spine above it.
    #[test]
    fn a_changed_leaf_rebuilds_the_types_that_contain_it() {
        let mut db = TypeDb::new();
        let (int, bool_ty) = (db.int(), db.bool());
        let tuple = db.pair(int, bool_ty);
        let vec_of_tuple = db.vec(tuple);

        let mut folder = IntToText {
            db: &mut db,
            memo: FoldMemo::new(),
            vars_seen: 0,
        };
        let folded = fold(&mut folder, vec_of_tuple);
        assert_ne!(folded, vec_of_tuple);
        assert_eq!(db.render(folded), "Vec[(Text, Bool)]");
    }

    /// The fold walks into records and enums — the two `deep_resolve` skipped.
    #[test]
    fn records_and_enums_are_folded_through_their_defs() {
        let mut db = TypeDb::new();
        let int = db.int();
        let rec = record(&mut db, "P", vec![("x".into(), int)]);
        let int2 = db.int();
        let en = enum_ty(&mut db, "E", vec![("Some".into(), vec![int2])]);

        let mut folder = IntToText {
            db: &mut db,
            memo: FoldMemo::new(),
            vars_seen: 0,
        };
        let folded_record = fold(&mut folder, rec);
        let folded_enum = fold(&mut folder, en);
        assert_ne!(folded_record, rec, "a record's fields are folded");
        assert_ne!(folded_enum, en, "an enum's payloads are folded");
        assert_eq!(db.render(folded_record), "P");

        // The specialized def is a *new* def, and the original is untouched.
        let TypeData::Record { def } = *db.data(folded_record) else {
            panic!("still a record");
        };
        let field = db.record_def(def).fields[0].ty;
        assert_eq!(db.render(field), "Text");
    }

    /// A record whose fields need no change keeps its def id — so nominal
    /// identity survives a fold that had nothing to do.
    #[test]
    fn an_unchanged_record_keeps_its_def() {
        let mut db = TypeDb::new();
        let text = db.text();
        let rec = record(&mut db, "P", vec![("name".into(), text)]);
        let defs_before = db.record_defs.len();

        let mut folder = IntToText {
            db: &mut db,
            memo: FoldMemo::new(),
            vars_seen: 0,
        };
        assert_eq!(fold(&mut folder, rec), rec);
        assert_eq!(
            db.record_defs.len(),
            defs_before,
            "no field changed, so no specialized def was minted"
        );
    }

    /// Every type is visited once, however many times it appears. Without the
    /// memo a shared subterm is folded once per occurrence, which is how a
    /// deeply-shared type turns a linear walk exponential.
    #[test]
    fn a_shared_child_is_folded_once() {
        let mut db = TypeDb::new();
        let var = db.fresh_var();
        let pair = db.pair(var, var);
        let nested = db.pair(pair, pair);

        let mut folder = IntToText {
            db: &mut db,
            memo: FoldMemo::new(),
            vars_seen: 0,
        };
        fold(&mut folder, nested);
        assert_eq!(folder.vars_seen, 1, "the shared var is folded once");
    }

    /// A record that reaches itself through its own field terminates. Nothing
    /// builds one today — `occurs` rejects a cyclic *variable* link — but the
    /// side tables make it representable, and F12's `Record { def, args }` makes
    /// it reachable. The memo is what keeps this from hanging: without it this
    /// test does not fail, it runs until the stack ends.
    #[test]
    fn a_record_that_contains_itself_terminates() {
        let mut db = TypeDb::new();
        let placeholder = db.fresh_var();
        let node = record(&mut db, "Node", vec![("next".into(), placeholder)]);
        // Tie the knot: the field type *is* the record.
        db.link(VarId::from_raw(placeholder.to_u32()), node);

        let mut folder = Identity {
            db: &mut db,
            memo: FoldMemo::new(),
        };
        let folded = fold(&mut folder, node);
        // Resolving the field's link *is* a change, so the fold specializes the
        // def — and the specialized one is still a record that reaches itself.
        let TypeData::Record { def } = *db.data(folded) else {
            panic!("still a record");
        };
        let field = db.record_def(def).fields[0].ty;
        assert_eq!(db.follow(field), node);
    }
}
