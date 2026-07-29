//! Tests for the core inference engine (ADR-007/008).
//!
//! These exercise the type arena, unification, generalization, and rendering in
//! isolation — no syntax, no HIR. They are the foundation the rest of M2 builds
//! on, so a regression here is caught before anything downstream.

#![cfg(test)]

use praxis_stdlib::type_pattern::{CollectionCtor, ScalarType};

use crate::data::{RecordDefId, TypeData, VarState};
use crate::{CollectionArgs, FieldSet, TupleElems, Type, TypeDb, VariantSet};

fn is_int(db: &TypeDb, t: crate::Type) -> bool {
    matches!(db.data(db.follow(t)), TypeData::Scalar(ScalarType::Int))
}

/// Build a 2-tuple without chaining borrows. The arena methods each take
/// `&mut self`, so `db.pair(db.int(), db.text())` would alias; this helper
/// is the ergonomic shape real callers (the inferer) use.
fn tup2(db: &mut TypeDb, a: crate::Type, b: crate::Type) -> crate::Type {
    db.pair(a, b)
}

/// Build `(a) -> r` without aliasing.
fn func1(db: &mut TypeDb, a: crate::Type, r: crate::Type) -> crate::Type {
    db.func(vec![a], r)
}

/// A tuple of arbitrary (valid) arity.
fn tup(db: &mut TypeDb, elements: Vec<Type>) -> Type {
    db.tuple(TupleElems::new(elements).expect("at least two elements"))
}

/// A collection whose arity matches its ctor.
fn coll(db: &mut TypeDb, ctor: CollectionCtor, args: Vec<Type>) -> Type {
    let args = CollectionArgs::new(ctor, args).expect("arity matches the ctor");
    db.collection(ctor, args).expect("arity matches the ctor")
}

/// A nominal record from `(name, type)` pairs.
fn record(db: &mut TypeDb, name: &str, fields: Vec<(String, Type)>) -> Type {
    let fields = FieldSet::from_pairs(fields).expect("distinct field names");
    db.record(Some(name.to_string()), fields)
}

/// An anonymous structural record (§5.6).
fn anon_record(db: &mut TypeDb, fields: Vec<(String, Type)>) -> Type {
    let fields = FieldSet::from_pairs(fields).expect("distinct field names");
    db.record(None, fields)
}

/// A nominal enum from `(name, payload)` pairs. An empty payload is a
/// payload-less variant (TY-05).
fn enum_ty(db: &mut TypeDb, name: &str, variants: Vec<(String, Vec<Type>)>) -> Type {
    let variants = VariantSet::from_pairs(variants).expect("distinct variant names");
    db.enum_(Some(name.to_string()), variants)
}

/// An anonymous enum (`choice(...)`, §7.5).
fn anon_enum(db: &mut TypeDb, variants: Vec<(String, Vec<Type>)>) -> Type {
    let variants = VariantSet::from_pairs(variants).expect("distinct variant names");
    db.enum_(None, variants)
}

#[test]
fn scalar_construction_preserves_structure_without_handle_deduplication() {
    let mut db = TypeDb::new();
    let a = db.int();
    let b = db.int();
    // `Type` is an arena handle, not a canonical structural id. Two calls mint
    // different handles, while structural unification still recognizes both as
    // Int. Checking only their payload shape would not test that distinction.
    assert_ne!(a, b);
    assert!(is_int(&db, a));
    assert!(is_int(&db, b));
    db.unify(a, b)
        .expect("separately allocated Int shapes unify");
    let unit = db.unit();
    assert!(matches!(db.data(db.follow(unit)), TypeData::Unit));
}

#[test]
fn unify_identical_scalars_succeeds() {
    let mut db = TypeDb::new();
    let a = db.int();
    let b = db.int();
    db.unify(a, b).expect("Int ~ Int");
    assert!(is_int(&db, a));
}

#[test]
fn unify_mismatched_scalars_fails() {
    let mut db = TypeDb::new();
    let a = db.int();
    let b = db.text();
    let err = db.unify(a, b).unwrap_err();
    assert!(matches!(err, crate::unify::UnifyError::Mismatch { .. }));
}

#[test]
fn unify_var_links_to_concrete() {
    let mut db = TypeDb::new();
    let v = db.fresh_var();
    let n = db.int();
    db.unify(v, n).expect("var ~ Int");
    // After unification, following the var reaches Int.
    assert!(is_int(&db, v));
    // The var is now Linked; its representative is concrete.
    assert!(matches!(db.data(db.follow(v)), TypeData::Scalar(_)));
}

#[test]
fn unify_var_to_var_links_them() {
    let mut db = TypeDb::new();
    let a = db.fresh_var();
    let b = db.fresh_var();
    db.unify(a, b).expect("var ~ var");
    // Constraining one then constrains the other.
    let n = db.int();
    db.unify(b, n).expect("b ~ Int");
    assert!(is_int(&db, a));
    assert!(is_int(&db, b));
}

#[test]
fn occurs_check_rejects_infinite_type() {
    let mut db = TypeDb::new();
    let v = db.fresh_var();
    // Build the type (v) -> v.
    let inner = v;
    let func = db.func(vec![v], v);
    let err = db.unify(v, func).unwrap_err();
    assert!(
        matches!(err, crate::unify::UnifyError::Occurs { .. }),
        "expected occurs failure, got {err:?}"
    );
    let _ = inner;
}

#[test]
fn unify_tuples_elementwise() {
    let mut db = TypeDb::new();
    let i = db.int();
    let t = db.text();
    let t1 = tup2(&mut db, i, t);
    let i2 = db.int();
    let t2_ = db.text();
    let t2 = tup2(&mut db, i2, t2_);
    db.unify(t1, t2).expect("(Int, Text) ~ (Int, Text)");

    // Different arity fails.
    let b3 = db.bool();
    let i3 = db.int();
    let t3 = db.text();
    let three = tup(&mut db, vec![i3, t3, b3]);
    let err = db.unify(t1, three).unwrap_err();
    assert!(matches!(err, crate::unify::UnifyError::Mismatch { .. }));
    let _ = b3;
}

#[test]
fn unify_functions_elementwise() {
    let mut db = TypeDb::new();
    let i = db.int();
    let t = db.text();
    let f1 = func1(&mut db, i, t);
    let i2 = db.int();
    let t2_ = db.text();
    let f2 = func1(&mut db, i2, t2_);
    db.unify(f1, f2).expect("(Int)->Text ~ (Int)->Text");

    // Mismatched return type fails.
    let i3 = db.int();
    let i4 = db.int();
    let g = func1(&mut db, i3, i4);
    assert!(db.unify(f1, g).is_err());
}

#[test]
fn prune_path_compresses_chains() {
    // v1 -> v2 -> Int: pruning v1 reaches Int and writes it back.
    let mut db = TypeDb::new();
    let v1 = db.fresh_var();
    let v2 = db.fresh_var();
    let n = db.int();
    db.unify(v2, n).expect("v2 ~ Int");
    db.unify(v1, v2).expect("v1 ~ v2");
    assert!(is_int(&db, v1));
}

// --- generalization & instantiation (ADR-008) -------------------------------

#[test]
fn generalize_quantifies_inner_level_vars() {
    // At the outer level, open an inner scope, create a var there, and build
    // `(v) -> v`. On exiting the inner scope and generalizing, `v` should be
    // quantified: the scheme is polymorphic.
    let mut db = TypeDb::new();
    let body = db.scoped_return(|db| {
        let v = db.fresh_var();
        db.func(vec![v], v)
    });
    let scheme = db.generalize(body);
    assert!(scheme.is_polymorphic(), "identity should generalize");
    assert_eq!(scheme.quantified.len(), 1);
}

#[test]
fn generalize_does_not_steal_outer_vars() {
    // A var created at the OUTER level must NOT be generalized by an inner scope.
    let mut db = TypeDb::new();
    let outer_v = db.fresh_var(); // at level 0
    let body = db.scoped_return(|db| {
        // Inner scope; reference the outer var.
        let unit = db.unit();
        db.func(vec![outer_v], unit)
    });
    let scheme = db.generalize(body);
    // The outer var is at level 0 == generalization level, so not quantified.
    assert!(
        scheme.quantified.is_empty(),
        "outer-level var must not be generalized"
    );
}

#[test]
fn instantiate_replaces_quantified_with_fresh_vars() {
    let mut db = TypeDb::new();
    let body = db.scoped_return(|db| {
        let v = db.fresh_var();
        db.func(vec![v], v)
    });
    let scheme = db.generalize(body);
    let inst1 = db.instantiate(&scheme);
    let inst2 = db.instantiate(&scheme);
    // The identity scheme instantiates to (V) -> V. Unifying inst1 with
    // (Int) -> Int fixes its V to Int...
    let i = db.int();
    let int_to_int = func1(&mut db, i, i);
    db.unify(inst1, int_to_int).expect("inst1 ~ (Int)->Int");
    // ...but inst2 is independent: it can still unify with (Text) -> Text,
    // which would be impossible if both shared the same fresh var.
    let t = db.text();
    let text_to_text = func1(&mut db, t, t);
    db.unify(inst2, text_to_text).expect("inst2 ~ (Text)->Text");
}

#[test]
fn monomorphic_scheme_instantiates_to_itself() {
    let mut db = TypeDb::new();
    let n = db.int();
    let scheme = db.generalize(n);
    assert!(!scheme.is_polymorphic());
    let inst = db.instantiate(&scheme);
    assert!(is_int(&db, inst));
}

#[test]
fn var_binding_is_not_generalized_in_place() {
    // A `var` binding keeps its type monomorphic even if a var sits inside it:
    // re-instantiating the (empty) scheme returns the original.
    let mut db = TypeDb::new();
    let v = db.fresh_var();
    let scheme = db.generalize(v);
    // `v` is at level 0; generalization at level 0 quantifies nothing.
    assert!(scheme.quantified.is_empty());
}

// --- pretty printing --------------------------------------------------------

#[test]
fn render_scalars_and_tuples_and_funcs() {
    let mut db = TypeDb::new();
    let i = db.int();
    assert_eq!(db.render(i), "Int");
    let u = db.unit();
    assert_eq!(db.render(u), "Unit");
    let t = db.text();
    let tup = tup2(&mut db, i, t);
    assert_eq!(db.render(tup), "(Int, Text)");
    let i2 = db.int();
    let t2_ = db.text();
    let f = func1(&mut db, i2, t2_);
    assert_eq!(db.render(f), "(Int) -> Text");
}

#[test]
fn render_polymorphic_scheme() {
    let mut db = TypeDb::new();
    let body = db.scoped_return(|db| {
        let v = db.fresh_var();
        db.func(vec![v], v)
    });
    let scheme = db.generalize(body);
    let rendered = db.render_scheme(&scheme);
    assert_eq!(rendered, "forall T. (T) -> T");
}

#[test]
fn render_quantified_two_var_scheme() {
    let mut db = TypeDb::new();
    let body = db.scoped_return(|db| {
        let a = db.fresh_var();
        let b = db.fresh_var();
        db.func(vec![a], b)
    });
    let scheme = db.generalize(body);
    let rendered = db.render_scheme(&scheme);
    assert_eq!(rendered, "forall T U. (T) -> U");
}

#[test]
fn unbound_var_renders_with_question_prefix() {
    let mut db = TypeDb::new();
    let v = db.fresh_var();
    assert_eq!(db.render(v), "?T");
}

#[test]
fn generalize_walks_into_tuples_and_functions() {
    let mut db = TypeDb::new();
    // Build (v, (v) -> w) entirely inside an inner scope.
    let body = db.scoped_return(|db| {
        let v = db.fresh_var();
        let w = db.fresh_var();
        let f = func1(db, v, w);
        tup2(db, v, f)
    });
    let scheme = db.generalize(body);
    assert_eq!(scheme.quantified.len(), 2);
    let rendered = db.render_scheme(&scheme);
    assert_eq!(rendered, "forall T U. (T, (T) -> U)");
}

#[test]
fn generalized_var_state_is_marked() {
    let mut db = TypeDb::new();
    let body = db.scoped_return(|db| {
        let v = db.fresh_var();
        db.func(vec![v], v)
    });
    let scheme = db.generalize(body);
    let var = scheme.quantified[0];
    assert!(matches!(
        db.data(var.as_type()),
        TypeData::Var(VarState::Generalized)
    ));
}

// --- collection types (M5 foundation: §4.4, §11.2) -------------------------
//
// Collection types drive the entire M5 method-dispatch surface, yet the
// unification, occurs check, and rendering paths for `TypeData::Collection`
// were entirely uncovered. These mirror the tuple/function coverage above.

#[test]
fn unify_vec_same_element_unifies() {
    let mut db = TypeDb::new();
    let i = db.int();
    let i2 = db.int();
    let a = db.vec(i);
    let b = db.vec(i2);
    db.unify(a, b).expect("Vec[Int] ~ Vec[Int]");
    assert_eq!(db.render(a), "Vec[Int]");
}

#[test]
fn unify_vec_mismatched_element_fails() {
    let mut db = TypeDb::new();
    let i = db.int();
    let a = db.vec(i);
    let t = db.text();
    let b = db.vec(t);
    let err = db.unify(a, b).unwrap_err();
    assert!(
        matches!(err, crate::unify::UnifyError::Mismatch { .. }),
        "Vec[Int] !~ Vec[Text], got {err:?}"
    );
}

/// TY-07. A wrong-arity collection used to be one `db.collection` call away,
/// and the only thing that noticed was a *unification* against a correctly
/// shaped one — which is the wrong place to notice, because a `Vec[Int, Text]`
/// that is never unified against anything simply flows on. This asserted that
/// mismatch; it now asserts that the type cannot be built at all.
#[test]
fn a_wrong_arity_collection_is_unconstructible() {
    let mut db = TypeDb::new();
    let i = db.int();
    let t = db.text();

    // Two arguments for a unary ctor: rejected while shaping the arguments…
    assert_eq!(
        CollectionArgs::new(CollectionCtor::Vec, vec![i, t]),
        Err(crate::TypeCtorError::CollectionArity {
            ctor: CollectionCtor::Vec,
            got: 2,
            want: 1,
        })
    );
    // …and again by the constructor, for a shape built without `new`.
    assert!(db
        .collection(CollectionCtor::Vec, CollectionArgs::Binary(i, t))
        .is_err());
    // Both ends of the range, on the ctors that make them tempting.
    assert!(CollectionArgs::new(CollectionCtor::Map, vec![i]).is_err());
    assert!(CollectionArgs::new(CollectionCtor::BitSet, vec![i]).is_err());
    // The right shape still builds.
    assert!(db
        .collection(CollectionCtor::Map, CollectionArgs::Binary(t, i))
        .is_ok());
}

/// TY-07's other two shapes: a one-element tuple, and a def with a repeated
/// name. All three were representable, and the duplicate cases were checked
/// only at whichever syntax caller remembered to.
#[test]
fn degenerate_tuples_and_duplicate_names_are_unconstructible() {
    let mut db = TypeDb::new();
    let i = db.int();
    let t = db.text();

    assert_eq!(
        TupleElems::new(vec![i]),
        Err(crate::TypeCtorError::TupleArity(1))
    );
    assert_eq!(
        TupleElems::new(Vec::new()),
        Err(crate::TypeCtorError::TupleArity(0))
    );
    assert!(TupleElems::new(vec![i, t]).is_ok());

    assert!(matches!(
        FieldSet::from_pairs(vec![("x".into(), i), ("x".into(), t)]),
        Err(crate::TypeCtorError::DuplicateField(name)) if name == "x"
    ));
    assert!(FieldSet::from_pairs(vec![("x".into(), i), ("y".into(), t)]).is_ok());

    assert!(matches!(
        VariantSet::from_pairs(vec![("A".into(), vec![i]), ("A".into(), vec![])]),
        Err(crate::TypeCtorError::DuplicateVariant(name)) if name == "A"
    ));
    assert!(VariantSet::from_pairs(vec![("A".into(), vec![i]), ("B".into(), vec![])]).is_ok());
}

#[test]
fn unify_vec_with_different_ctor_fails() {
    // Same args, different collection ctor: `Vec[Int]` vs `Deque[Int]`. These are
    // distinct nominal collection types (§4.4) and must not unify.
    let mut db = TypeDb::new();
    let i = db.int();
    let a = db.vec(i);
    let b = coll(&mut db, CollectionCtor::Deque, vec![i]);
    assert!(db.unify(a, b).is_err());
}

#[test]
fn unify_vec_element_var_links() {
    // `Vec[?T] ~ Vec[Int]` constrains ?T to Int (unification flows into the
    // element type).
    let mut db = TypeDb::new();
    let v = db.fresh_var();
    let vec_of_var = db.vec(v);
    let i = db.int();
    let vec_of_int = db.vec(i);
    db.unify(vec_of_var, vec_of_int)
        .expect("Vec[?T] ~ Vec[Int]");
    assert!(is_int(&db, v));
}

#[test]
fn render_collection_types() {
    let mut db = TypeDb::new();
    let i = db.int();
    let vec_int = db.vec(i);
    assert_eq!(db.render(vec_int), "Vec[Int]");
    // Map[K, V] exercises the multi-arg collection rendering.
    let t = db.text();
    let m = coll(&mut db, CollectionCtor::Map, vec![t, i]);
    assert_eq!(db.render(m), "Map[Text, Int]");
    // Nested: Vec[Map[Text, Int]].
    let nested = db.vec(m);
    assert_eq!(db.render(nested), "Vec[Map[Text, Int]]");
}

// --- record & enum types (M7, ADR-025) --------------------------------------
//
// Records and enums use def-id indirection: the heavy field/variant data lives
// in side-tables on TypeDb, referenced from TypeData::Record/Enum by a small
// index. These tests exercise construction, unification, generalization,
// instantiation, and rendering — the four recursions that had to be extended
// (unify_concrete, lower_levels, occurs, generalize_walk, instantiate_walk).

#[test]
fn nominal_record_renders_by_name() {
    let mut db = TypeDb::new();
    let i = db.int();
    let t = db.text();
    let point = record(&mut db, "Point", vec![("x".into(), i), ("y".into(), t)]);
    assert_eq!(db.render(point), "Point");
}

#[test]
fn anonymous_record_renders_structurally() {
    let mut db = TypeDb::new();
    let i = db.int();
    let t = db.text();
    let rec = anon_record(&mut db, vec![("x".into(), i), ("y".into(), t)]);
    assert_eq!(db.render(rec), "{ x: Int, y: Text }");
}

#[test]
fn nominal_records_with_same_name_but_distinct_def_ids_do_not_unify() {
    let mut db = TypeDb::new();
    let i = db.int();
    let a = record(&mut db, "Point", vec![("x".into(), i)]);
    // Same name, same fields → fresh def-id but... nominal records are distinct
    // by def-id (each register_record call mints a new one). So two separately
    // registered Points do NOT unify unless they share the def-id.
    let i2 = db.int();
    let b = record(&mut db, "Point", vec![("x".into(), i2)]);
    // Different def-ids: nominal records don't unify across registrations.
    assert!(
        db.unify(a, b).is_err(),
        "distinct nominal record registrations do not unify"
    );
    // But a record unifies with itself.
    db.unify(a, a).expect("record ~ itself");
}

#[test]
fn nominal_record_unifies_with_same_def_id() {
    let mut db = TypeDb::new();
    let i = db.int();
    let point = record(&mut db, "Point", vec![("x".into(), i), ("y".into(), i)]);
    // Re-intern the same def-id — these unify.
    let def = match db.data(db.follow(point)) {
        TypeData::Record { def } => *def,
        _ => unreachable!(),
    };
    let point2 = db.record_type(def);
    db.unify(point, point2).expect("same def-id unifies");
}

#[test]
fn anonymous_records_same_fields_unify() {
    let mut db = TypeDb::new();
    let i = db.int();
    let a = anon_record(&mut db, vec![("x".into(), i), ("y".into(), i)]);
    let i2 = db.int();
    let b = anon_record(&mut db, vec![("x".into(), i2), ("y".into(), i2)]);
    // Same field-name set → shared def-id → unify.
    db.unify(a, b).expect("anon records with same names unify");
}

#[test]
fn anonymous_records_order_independent_identity() {
    // §5.6: field order in source does not affect identity after
    // canonicalization. { x: Int, y: Int } and { y: Int, x: Int } unify
    // (identity is through unification, which matches fields by name).
    let mut db = TypeDb::new();
    let i = db.int();
    let j = db.int();
    let a = anon_record(&mut db, vec![("x".into(), i), ("y".into(), j)]);
    let i2 = db.int();
    let j2 = db.int();
    let b = anon_record(&mut db, vec![("y".into(), j2), ("x".into(), i2)]);
    db.unify(a, b).expect("order-independent identity unifies");
}

#[test]
fn anonymous_records_different_names_do_not_unify() {
    let mut db = TypeDb::new();
    let i = db.int();
    let a = anon_record(&mut db, vec![("x".into(), i), ("y".into(), i)]);
    let b = anon_record(&mut db, vec![("x".into(), i), ("z".into(), i)]);
    assert!(
        db.unify(a, b).is_err(),
        "{{x,y}} and {{x,z}} have different field sets"
    );
}

#[test]
fn record_field_var_unifies() {
    // A record field containing a type var: { x: ?T } ~ { x: Int } constrains ?T.
    let mut db = TypeDb::new();
    let v = db.fresh_var();
    let a = anon_record(&mut db, vec![("x".into(), v)]);
    let i = db.int();
    let b = anon_record(&mut db, vec![("x".into(), i)]);
    db.unify(a, b).expect("{x:?T} ~ {x:Int}");
    assert!(is_int(&db, v));
}

#[test]
fn enum_same_def_id_unifies() {
    let mut db = TypeDb::new();
    let i = db.int();
    let tile = enum_ty(
        &mut db,
        "Tile",
        vec![("Empty".into(), vec![]), ("Number".into(), vec![i])],
    );
    db.unify(tile, tile).expect("enum ~ itself");
    let def = match db.data(db.follow(tile)) {
        TypeData::Enum { def } => *def,
        _ => unreachable!(),
    };
    let tile2 = db.enum_type(def);
    db.unify(tile, tile2).expect("same def-id unifies");
}

#[test]
fn different_enums_do_not_unify() {
    let mut db = TypeDb::new();
    let i = db.int();
    let a = enum_ty(&mut db, "A", vec![("X".into(), vec![i])]);
    let b = enum_ty(&mut db, "B", vec![("X".into(), vec![i])]);
    assert!(db.unify(a, b).is_err(), "different enum names don't unify");
}

#[test]
fn record_def_lookups() {
    let mut db = TypeDb::new();
    let i = db.int();
    let t = db.text();
    let _ = record(&mut db, "Point", vec![("x".into(), i), ("y".into(), t)]);
    // Fetch the def via the last registered type.
    let defs = &db.record_defs;
    let def = &defs[0];
    assert_eq!(def.name.as_deref(), Some("Point"));
    assert_eq!(def.arity(), 2);
    let (idx, ty) = def.field("y").expect("field y exists");
    assert_eq!(idx, 1);
    assert!(is_text(&db, ty));
    assert!(def.field("z").is_none());
}

#[test]
fn enum_def_variant_lookup() {
    let mut db = TypeDb::new();
    let i = db.int();
    let _ = enum_ty(
        &mut db,
        "Tile",
        vec![("Empty".into(), vec![]), ("Number".into(), vec![i])],
    );
    let def = &db.enum_defs[0];
    assert_eq!(def.name.as_deref(), Some("Tile"));
    assert_eq!(def.arity(), 2);
    assert_eq!(def.variant("Number"), Some(1));
    assert_eq!(def.variant("Empty"), Some(0));
    assert!(def.variant("Missing").is_none());
    assert!(!def.variants[0].has_payload());
    assert!(def.variants[1].has_payload());
}

#[test]
fn record_generalizes_inner_vars() {
    // forall T. { x: T } — a polymorphic anonymous record.
    let mut db = TypeDb::new();
    let body = db.scoped_return(|db| {
        let v = db.fresh_var();
        anon_record(db, vec![("x".into(), v)])
    });
    let scheme = db.generalize(body);
    assert_eq!(scheme.quantified.len(), 1);
    // The generalized record still renders structurally.
    assert_eq!(db.render_scheme(&scheme), "forall T. { x: T }");
}

#[test]
fn record_instantiates_to_fresh_vars() {
    let mut db = TypeDb::new();
    let body = db.scoped_return(|db| {
        let v = db.fresh_var();
        anon_record(db, vec![("x".into(), v)])
    });
    let scheme = db.generalize(body);
    let inst1 = db.instantiate(&scheme);
    let inst2 = db.instantiate(&scheme);
    // Constrain inst1's x to Int, inst2's x stays free.
    let i = db.int();
    let int_rec = anon_record(&mut db, vec![("x".into(), i)]);
    db.unify(inst1, int_rec).expect("inst1 ~ {x:Int}");
    // inst2 should still be instantiable to Text.
    let t = db.text();
    let text_rec = anon_record(&mut db, vec![("x".into(), t)]);
    db.unify(inst2, text_rec).expect("inst2 ~ {x:Text}");
}

#[test]
fn enum_instantiates_payloads() {
    // forall T. enum E { Some(T) } — payload type generalizes.
    let mut db = TypeDb::new();
    let body = db.scoped_return(|db| {
        let v = db.fresh_var();
        enum_ty(db, "E", vec![("Some".into(), vec![v])])
    });
    let scheme = db.generalize(body);
    assert!(scheme.is_polymorphic());
}

#[test]
fn occurs_check_works_through_record_fields() {
    // Unifying ?T with { x: ?T } must fail the occurs check (infinite type).
    let mut db = TypeDb::new();
    let v = db.fresh_var();
    let rec = anon_record(&mut db, vec![("x".into(), v)]);
    let err = db.unify(v, rec).unwrap_err();
    assert!(
        matches!(err, crate::unify::UnifyError::Occurs { .. }),
        "expected occurs failure, got {err:?}"
    );
}

#[test]
fn vec_of_record_renders() {
    let mut db = TypeDb::new();
    let i = db.int();
    let rec = anon_record(&mut db, vec![("x".into(), i), ("y".into(), i)]);
    let vec_rec = db.vec(rec);
    assert_eq!(db.render(vec_rec), "Vec[{ x: Int, y: Int }]");
}

fn is_text(db: &TypeDb, t: crate::Type) -> bool {
    matches!(db.data(db.follow(t)), TypeData::Scalar(ScalarType::Text))
}

#[test]
fn anon_record_display_preserves_source_order() {
    // Each anon_record call preserves its own field order for display.
    // Identity through unification does not retroactively reorder fields.
    let mut db = TypeDb::new();
    let i = db.int();
    let j = db.int();
    let xy = anon_record(&mut db, vec![("x".into(), i), ("y".into(), j)]);
    assert_eq!(db.render(xy), "{ x: Int, y: Int }");
    let k = db.int();
    let l = db.int();
    let yx = anon_record(&mut db, vec![("y".into(), l), ("x".into(), k)]);
    assert_eq!(db.render(yx), "{ y: Int, x: Int }");
    let _ = RecordDefId(0); // exercise the debug impl / keep the import used
}

// ---- M9: enum unification for polymorphic / anonymous enums ----------------

/// Two nominal enums with the same name and variant signature unify and link to
/// one canonical def-id. This is the soundness fix that lets a polymorphic
/// `forall T. Option[T]` scheme work: each instantiation mints a fresh def, but
/// they unify by name+shape.
#[test]
fn same_named_enums_unify_structurally() {
    let mut db = TypeDb::new();
    let int = db.int();
    // Two independently-stamped `Option` defs (simulating two instantiation
    // sites of a polymorphic Option scheme).
    let opt_a = enum_ty(
        &mut db,
        "Option",
        vec![("Some".into(), vec![int]), ("None".into(), vec![])],
    );
    let opt_b = enum_ty(
        &mut db,
        "Option",
        vec![("Some".into(), vec![int]), ("None".into(), vec![])],
    );
    db.unify(opt_a, opt_b).expect("Option[Int] ~ Option[Int]");
}

/// Unifying fixes the element type across the two sites: `Option[?T]` unified
/// with `Option[Int]` pins `?T = Int` in both.
#[test]
fn same_named_enums_unify_payloads_pairwise() {
    let mut db = TypeDb::new();
    let t = db.fresh_var();
    let int = db.int();
    // Option[?T]
    let opt_var = enum_ty(
        &mut db,
        "Option",
        vec![("Some".into(), vec![t]), ("None".into(), vec![])],
    );
    // Option[Int]
    let opt_int = enum_ty(
        &mut db,
        "Option",
        vec![("Some".into(), vec![int]), ("None".into(), vec![])],
    );
    db.unify(opt_var, opt_int)
        .expect("Option[?T] ~ Option[Int]");
    assert!(is_int(&db, t), "Some's payload ?T should be pinned to Int");
}

/// Different names never unify, even with identical variant signatures —
/// `Color::Red` must never equal `Signal::Red`.
#[test]
fn differently_named_enums_do_not_unify() {
    let mut db = TypeDb::new();
    let color = enum_ty(
        &mut db,
        "Color",
        vec![("Red".into(), vec![]), ("Green".into(), vec![])],
    );
    let signal = enum_ty(
        &mut db,
        "Signal",
        vec![("Red".into(), vec![]), ("Green".into(), vec![])],
    );
    let err = db.unify(color, signal).unwrap_err();
    assert!(matches!(err, crate::unify::UnifyError::Mismatch { .. }));
}

/// Same name but different variant signatures do not unify.
#[test]
fn same_name_different_variants_do_not_unify() {
    let mut db = TypeDb::new();
    let a = enum_ty(
        &mut db,
        "E",
        vec![("A".into(), vec![]), ("B".into(), vec![])],
    );
    let b = enum_ty(
        &mut db,
        "E",
        vec![("A".into(), vec![]), ("C".into(), vec![])],
    );
    let err = db.unify(a, b).unwrap_err();
    assert!(matches!(err, crate::unify::UnifyError::Mismatch { .. }));
}

/// Anonymous enums (the synthetic name "") unify by variant-name signature, so
/// two independently-stamped `choice` results of the same shape unify.
#[test]
fn anonymous_enums_unify_by_variant_signature() {
    let mut db = TypeDb::new();
    let i0 = db.int();
    let i1 = db.int();
    let i2 = db.int();
    let i3 = db.int();
    let choice_a = anon_enum(
        &mut db,
        vec![("Multiply".into(), vec![i0, i1]), ("Enable".into(), vec![])],
    );
    let choice_b = anon_enum(
        &mut db,
        vec![("Multiply".into(), vec![i2, i3]), ("Enable".into(), vec![])],
    );
    db.unify(choice_a, choice_b)
        .expect("anon enums same shape unify");
}

/// `anon_enum` minted separately from a `choice` of different variant names must
/// NOT unify.
#[test]
fn anonymous_enums_different_shape_do_not_unify() {
    let mut db = TypeDb::new();
    let a = anon_enum(&mut db, vec![("Foo".into(), vec![])]);
    let b = anon_enum(&mut db, vec![("Bar".into(), vec![])]);
    assert!(db.unify(a, b).is_err());
}

// ---- adversarial level/generalization coverage -----------------------------

#[test]
#[ignore = "known bug: level lowering raises older variables instead of lowering inner ones"]
fn linking_an_outer_var_to_an_inner_type_prevents_inner_generalization() {
    // The outer variable is part of the environment. Once it is unified with a
    // type containing an inner variable, that inner variable must be lowered to
    // the outer level; otherwise generalizing the result would quantify a
    // variable that is still reachable from the outer environment.
    let mut db = TypeDb::new();
    let outer = db.fresh_var(); // level 0
    let body = db.scoped_return(|db| {
        let inner = db.fresh_var(); // level 1
        let pair = tup(db, vec![inner, inner]);
        db.unify(outer, pair).expect("outer var accepts inner pair");
        outer
    });

    let scheme = db.generalize(body);
    assert!(
        scheme.quantified.is_empty(),
        "a type reachable through an outer variable must stay monomorphic, got {}",
        db.render_scheme(&scheme)
    );
}

#[test]
fn instantiation_preserves_non_quantified_variable_identity() {
    // A scheme may contain both a quantified variable and a free (environment)
    // variable. Instantiation replaces only the quantified variable. Repeated
    // occurrences of the free variable must remain the same slot, otherwise
    // `(outer, T) -> outer` silently becomes `(A, T) -> B`.
    let mut db = TypeDb::new();
    let outer = db.fresh_var(); // free at the generalization site
    let body = db.scoped_return(|db| {
        let quantified = db.fresh_var();
        db.func(vec![outer, quantified], outer)
    });
    let scheme = db.generalize(body);
    assert_eq!(scheme.quantified.len(), 1);

    let instantiated = db.instantiate(&scheme);
    let (params, result) = match db.data(db.follow(instantiated)).clone() {
        TypeData::Func { params, result } => (params, result),
        other => panic!("expected function, got {other:?}"),
    };
    let int = db.int();
    db.unify(params[0], int)
        .expect("the free input variable accepts Int");
    assert!(
        is_int(&db, result),
        "the repeated free variable in the result must be the same variable"
    );
}

#[test]
fn deep_resolve_rewrites_record_field_links() {
    // `deep_resolve` promises a type whose composite leaves are concrete, not
    // merely linked. Record fields live in a side table and must participate in
    // that recursive resolution just like tuple/collection elements.
    let mut db = TypeDb::new();
    let field_var = db.fresh_var();
    let record = anon_record(&mut db, vec![("value".into(), field_var)]);
    let int = db.int();
    db.unify(field_var, int).expect("field var ~ Int");

    let resolved = db.deep_resolve(record);
    let def = match db.data(db.follow(resolved)) {
        TypeData::Record { def } => *def,
        other => panic!("expected record, got {other:?}"),
    };
    let field_ty = db.record_def(def).field("value").expect("value field").1;
    assert!(
        matches!(db.data(field_ty), TypeData::Scalar(ScalarType::Int)),
        "deep-resolved record field must not retain a Linked var: {:?}",
        db.data(field_ty)
    );
}

/// TY-05. `EnumVariantDef` documented `Some(vec![])` as equivalent to `None`
/// and `unify` then rejected the pair, because the two spellings fell through
/// its three-way payload match to a catch-all.
///
/// There is now one spelling, so the test says so at both levels the bug lived
/// at: the *representation* admits only the empty vector, and the two
/// constructors that used to disagree produce defs that unify.
#[test]
fn empty_enum_payload_and_no_payload_are_equivalent() {
    let mut db = TypeDb::new();
    let bare = crate::EnumVariantDef::bare("Only");
    let empty = crate::EnumVariantDef::new("Only", Vec::new());
    assert!(!bare.has_payload());
    assert!(!empty.has_payload());
    assert_eq!(bare.payload, empty.payload, "one payload-less spelling");

    let no_payload = db.enum_(
        Some("E".into()),
        VariantSet::new(vec![bare]).expect("one variant"),
    );
    let empty_payload = db.enum_(
        Some("E".into()),
        VariantSet::new(vec![empty]).expect("one variant"),
    );
    db.unify(no_payload, empty_payload)
        .expect("a payload-less variant unifies with itself however it was built");
}
