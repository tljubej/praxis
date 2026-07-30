//! Tests for the core inference engine (ADR-007/008).
//!
//! These exercise the type arena, unification, generalization, and rendering in
//! isolation — no syntax, no HIR. They are the foundation the rest of M2 builds
//! on, so a regression here is caught before anything downstream.

#![cfg(test)]

use praxis_stdlib::type_pattern::{CollectionCtor, ScalarType};

use crate::data::{RecordDefId, TypeData, VarState};
use crate::{CollectionArgs, FieldSet, Scheme, TupleElems, Type, TypeDb, VariantSet};

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
    assert_eq!(scheme.binders().len(), 1);
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
        scheme.binders().is_empty(),
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
    assert!(scheme.binders().is_empty());
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
    assert_eq!(scheme.binders().len(), 2);
    let rendered = db.render_scheme(&scheme);
    assert_eq!(rendered, "forall T U. (T, (T) -> U)");
}

/// **Rewritten** for TY-03/F10. It asserted that `generalize` sets a
/// `VarState::Generalized` flag on the arena slot — global state recording that
/// *some* scheme quantifies this variable, which is a fact no arena can hold:
/// a monotype built before the flag was set has a body containing a variable it
/// does not bind, and unification refuses to link one.
///
/// The scheme owns its binders now, so the property is stated where it lives:
/// generalization *collects* and mutates nothing, and the variable it collected
/// is still an ordinary unbound variable in the arena.
#[test]
fn a_scheme_owns_its_binders_and_generalization_mutates_nothing() {
    let mut db = TypeDb::new();
    let body = db.scoped_return(|db| {
        let v = db.fresh_var();
        db.func(vec![v], v)
    });
    let before = db.len();
    let scheme = db.generalize(body);

    assert_eq!(
        scheme.binders().len(),
        1,
        "the identity's var is quantified"
    );
    assert_eq!(scheme.body(), body, "the body is the type it was given");
    assert_eq!(db.len(), before, "generalization interns nothing");

    let var = scheme.binders()[0];
    assert!(
        matches!(
            db.data(var.as_type()),
            TypeData::Var(VarState::Unbound { .. })
        ),
        "a binder is an ordinary unbound variable; only the scheme knows it is bound"
    );

    // And it renders as a binder only because the scheme says so.
    assert_eq!(db.render_scheme(&scheme), "forall T. (T) -> T");
    assert_eq!(db.render(body), "(?T) -> ?T");
}

/// TY-03 directly: the state a `Scheme` could encode and cannot any more.
///
/// A monotype built from a type containing `v`, followed by generalization of a
/// *different* binding that also reaches `v`, used to leave the monotype's body
/// pointing at a `Generalized` slot — a variable it does not list, and one
/// `unify` has no arm for. The monotype silently stopped unifying with
/// anything.
#[test]
fn generalizing_one_scheme_does_not_change_another() {
    let mut db = TypeDb::new();
    let shared = db.scoped_return(|db| db.fresh_var());
    let mono = Scheme::monotype(shared);

    // A second binding reaches the same variable and generalizes it.
    let other = db.func(vec![shared], shared);
    let poly = db.generalize(other);
    assert_eq!(poly.binders(), &[db.var_id_of(shared).expect("a var")]);

    // The monotype is unaffected: it binds nothing, and its body still unifies.
    assert!(mono.binders().is_empty());
    let int = db.int();
    let instantiated = db.instantiate(&mono);
    db.unify(instantiated, int)
        .expect("a monotype's body is still an ordinary unbound variable");
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

/// REP-13: the brackets belong to the arguments, so a ctor with none has none.
///
/// The collection arm wrote `[` and `]` unconditionally, so every diagnostic
/// about a nullary collection named a type nobody can write: a `Y001` said
/// "found `Range[]`". `BitSet` has done it since it existed; `Range` (TY-34)
/// made it visible. Both nullary ctors are here because the rule is the arity,
/// not the name — and the unary and binary cases are re-asserted alongside so a
/// fix that dropped *every* bracket could not pass.
#[test]
fn a_collection_with_no_type_arguments_renders_without_brackets() {
    let mut db = TypeDb::new();
    for ctor in [CollectionCtor::Range, CollectionCtor::BitSet] {
        assert_eq!(ctor.arity(), 0, "{} is the nullary case", ctor.name());
        let t = coll(&mut db, ctor, vec![]);
        assert_eq!(db.render(t), ctor.name());
    }

    // …and an argument still prints, at every arity and nested inside a
    // nullary-carrying type.
    let i = db.int();
    let bits = coll(&mut db, CollectionCtor::BitSet, vec![]);
    let vec_of_bits = db.vec(bits);
    assert_eq!(db.render(vec_of_bits), "Vec[BitSet]");
    let map = coll(&mut db, CollectionCtor::Map, vec![bits, i]);
    assert_eq!(db.render(map), "Map[BitSet, Int]");
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
        TypeData::Record { def, .. } => *def,
        _ => unreachable!(),
    };
    let point2 = db.record_type(def, Vec::new()).expect("no params");
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
        TypeData::Enum { def, .. } => *def,
        _ => unreachable!(),
    };
    let tile2 = db.enum_type(def, Vec::new()).expect("no params");
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
    let _tile = enum_ty(
        &mut db,
        "Tile",
        vec![("Empty".into(), vec![]), ("Number".into(), vec![i])],
    );
    let TypeData::Enum { def, .. } = *db.data(_tile) else {
        panic!("expected an enum");
    };
    let def = db.enum_def(def);
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
    assert_eq!(scheme.binders().len(), 1);
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

/// TY-06. There is one `Option` def, so two uses of it are the *same nominal
/// type* applied to arguments — not two definitions a relaxed unification arm
/// has to put back together.
///
/// Rewritten from `same_named_enums_unify_structurally`, which stamped two
/// `Option` defs by hand and asserted they merged. That was the workaround, and
/// asserting it would be asserting the defect: two same-named nominal enums are
/// two types, exactly as two same-named records are.
#[test]
fn every_option_names_the_one_option_def() {
    let mut db = TypeDb::new();
    let int = db.int();
    let opt_a = db.option_of(int);
    let int2 = db.int();
    let opt_b = db.option_of(int2);
    db.unify(opt_a, opt_b).expect("Option[Int] ~ Option[Int]");
    for opt in [opt_a, opt_b] {
        let TypeData::Enum { def, args } = db.data(db.follow(opt)).clone() else {
            panic!("expected an enum");
        };
        assert_eq!(def, db.option_def(), "one def, not one per use site");
        assert_eq!(args.len(), 1, "the element type is an argument");
    }
    assert_eq!(db.render(opt_a), "Option[Int]");
}

/// Unifying two instances fixes the element type: `Option[?T]` unified with
/// `Option[Int]` pins `?T = Int`. The work happens in the *arguments* now; it
/// used to happen in a pairwise walk over two defs' variant payloads.
#[test]
fn option_instances_unify_through_their_arguments() {
    let mut db = TypeDb::new();
    let t = db.fresh_var();
    let opt_var = db.option_of(t);
    let int = db.int();
    let opt_int = db.option_of(int);
    db.unify(opt_var, opt_int)
        .expect("Option[?T] ~ Option[Int]");
    assert!(
        is_int(&db, t),
        "the element type ?T should be pinned to Int"
    );
}

/// …and two instances at *different* arguments do not. `Option` printed as a
/// bare name and carried its element type in a fresh def, so nothing about the
/// type told `Option[Int]` from `Option[Text]` (MONO-03's collision).
#[test]
fn option_at_two_element_types_is_two_types() {
    let mut db = TypeDb::new();
    let int = db.int();
    let opt_int = db.option_of(int);
    let text = db.text();
    let opt_text = db.option_of(text);
    assert!(
        db.unify(opt_int, opt_text).is_err(),
        "Option[Int] is not Option[Text]"
    );
    assert_ne!(db.canonical_key(opt_int), db.canonical_key(opt_text));
    assert_eq!(db.render(opt_text), "Option[Text]");
}

/// A wrong type-argument count is refused rather than interned (TY-07's rule,
/// now applying to nominal defs too).
#[test]
fn a_wrong_type_argument_count_is_unconstructible() {
    let mut db = TypeDb::new();
    let (int, text) = (db.int(), db.text());
    let def = db.option_def();
    assert!(matches!(
        db.enum_type(def, vec![int, text]),
        Err(crate::TypeCtorError::TypeArgCount {
            want: 1,
            got: 2,
            ..
        })
    ));
    assert!(matches!(
        db.enum_type(def, Vec::new()),
        Err(crate::TypeCtorError::TypeArgCount {
            want: 1,
            got: 0,
            ..
        })
    ));
}

/// **TY-06.** Instantiating a polymorphic scheme whose body names a nominal
/// type must not mint a *new definition* of that type.
///
/// This is the finding stated at the level it lived at. `instantiate` walked
/// the body and, reaching an enum, rebuilt it — which meant `register_enum`,
/// which meant a fresh `EnumDefId`. Every `Some(x)` in a program therefore
/// produced a nominally distinct `Option`, and the only thing holding the
/// program together was a unification arm that merged two enums when their
/// names and variant names matched.
#[test]
fn instantiating_a_scheme_does_not_mint_a_nominal_definition() {
    let mut db = TypeDb::new();
    // `forall T. (T) -> Option[T]` — the shape of `Some`'s prelude scheme.
    let some = db.scoped_return(|db| {
        let t = db.fresh_var();
        let opt = db.option_of(t);
        db.func(vec![t], opt)
    });
    let scheme = db.generalize(some);
    assert!(scheme.is_polymorphic(), "Some is polymorphic in T");

    let defs_before = db.enum_defs.len();
    for _ in 0..8 {
        let _ = db.instantiate(&scheme);
    }
    assert_eq!(
        db.enum_defs.len(),
        defs_before,
        "instantiation mints uses of a definition, never definitions"
    );
}

/// …and the instantiated body is a *usable* `Option`: the def is still the
/// canonical one, and the fresh element variable landed in the arguments where
/// unification can reach it.
#[test]
fn an_instantiated_option_is_the_canonical_def_at_a_fresh_argument() {
    let mut db = TypeDb::new();
    let none = db.scoped_return(|db| {
        let t = db.fresh_var();
        db.option_of(t)
    });
    let scheme = db.generalize(none);

    let first = db.instantiate(&scheme);
    let second = db.instantiate(&scheme);
    let arg_of = |db: &TypeDb, t: Type| match db.data(db.follow(t)) {
        TypeData::Enum { def, args } => {
            assert_eq!(*def, db.option_def());
            assert_eq!(args.len(), 1);
            args[0]
        }
        other => panic!("expected an Option, got {other:?}"),
    };
    let (a, b) = (arg_of(&db, first), arg_of(&db, second));
    assert_ne!(a, b, "each use gets its own element variable");

    let int = db.int();
    db.unify(a, int).expect("the first use may be Option[Int]");
    let text = db.text();
    db.unify(b, text).expect("the second may be Option[Text]");
    assert_eq!(db.render(first), "Option[Int]");
    assert_eq!(db.render(second), "Option[Text]");
}

/// The payload a *use* sees is the use's argument, not the definition's
/// parameter. Reading a variant payload straight off the def is what a caller
/// did when the def was per-site; now it has to go through the instance.
#[test]
fn a_variant_payload_is_read_through_the_instances_arguments() {
    let mut db = TypeDb::new();
    let text = db.text();
    let opt_text = db.option_of(text);
    let TypeData::Enum { def, args } = db.data(opt_text).clone() else {
        panic!("expected an enum");
    };
    let some = db.enum_def(def).variant("Some").expect("Some");
    let payload = db.variant_payload_of(def, &args, some);
    assert_eq!(payload.len(), 1);
    assert!(matches!(
        db.data(db.follow(payload[0])),
        TypeData::Scalar(ScalarType::Text)
    ));

    let none = db.enum_def(def).variant("None").expect("None");
    assert!(
        db.variant_payload_of(def, &args, none).is_empty(),
        "None carries nothing whatever the argument is"
    );
}

/// `canonical_key` is identity, where `render` was display (MONO-03). Two
/// separately-interned `Vec[Int]`s are one key; a nominal type keys by its def.
#[test]
fn a_canonical_key_groups_by_structure_and_by_definition() {
    let mut db = TypeDb::new();
    let (i1, i2) = (db.int(), db.int());
    let (v1, v2) = (db.vec(i1), db.vec(i2));
    assert_ne!(v1, v2, "two handles");
    assert_eq!(db.canonical_key(v1), db.canonical_key(v2), "one type");

    // Two same-named nominal records are two types, and their keys say so —
    // which a rendered string does not.
    let a = record(&mut db, "Point", vec![("x".into(), i1)]);
    let b = record(&mut db, "Point", vec![("x".into(), i1)]);
    assert_eq!(db.render(a), db.render(b));
    assert_ne!(db.canonical_key(a), db.canonical_key(b));

    // An unresolved variable is its own key, and following a link is what makes
    // a resolved one agree with what it resolved to.
    let var = db.fresh_var();
    assert_ne!(db.canonical_key(var), db.canonical_key(i1));
    db.unify(var, i1).expect("?a ~ Int");
    assert_eq!(db.canonical_key(var), db.canonical_key(i1));
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
        scheme.binders().is_empty(),
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
    assert_eq!(scheme.binders().len(), 1);

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
        TypeData::Record { def, .. } => *def,
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

    // Two independently-stamped anonymous defs, which is the arm that still
    // merges by signature — a *nominal* pair would be two types (F12).
    let no_payload = db.enum_(None, VariantSet::new(vec![bare]).expect("one variant"));
    let empty_payload = db.enum_(None, VariantSet::new(vec![empty]).expect("one variant"));
    db.unify(no_payload, empty_payload)
        .expect("a payload-less variant unifies with itself however it was built");
}

// --- TY-19: Never is the bottom type, and a branch point joins ---------------

/// `Never` is not a scalar. A scalar is a type with a runtime representation —
/// a descriptor, a payload width, a value you can hold — and no value ever has
/// type `Never`. Sitting in `ScalarType` made every "is this a scalar?"
/// question answer yes for it.
#[test]
fn never_is_its_own_type_and_not_a_scalar() {
    let mut db = TypeDb::new();
    let never = db.never();
    assert!(matches!(db.data(never), crate::TypeData::Never));
    assert!(!matches!(db.data(never), crate::TypeData::Scalar(_)));
    // Two `Never`s are two arena slots (the arena does not deduplicate) and
    // one type: identity is the canonical key, and it is distinct from `Unit`'s.
    let again = db.never();
    let unit = db.unit();
    assert_eq!(db.canonical_key(never), db.canonical_key(again));
    assert_ne!(db.canonical_key(never), db.canonical_key(unit));
    assert!(
        db.unify(never, again).is_ok(),
        "one Never unifies with another"
    );
    assert_eq!(db.render(never), "Never");
}

/// The absorbing law, in both directions and at every shape. This is the whole
/// of what a branch point needs: a path that produces no value contributes no
/// constraint.
#[test]
fn join_absorbs_never_from_either_side() {
    let mut db = TypeDb::new();
    let never = db.never();
    let int = db.int();
    let vec_text = {
        let text = db.text();
        db.vec(text)
    };
    for other in [int, vec_text, db.unit()] {
        let left = db.join(never, other).expect("Never joins with anything");
        assert_eq!(db.canonical_key(left), db.canonical_key(other));
        let right = db.join(other, never).expect("…from either side");
        assert_eq!(db.canonical_key(right), db.canonical_key(other));
    }
    // Never with Never is Never, not an error.
    let both = db.join(never, never).expect("Never joins with Never");
    assert!(matches!(db.data(both), crate::TypeData::Never));
}

/// A join is **not** general subtyping. Two ordinary types still have to unify,
/// so inference stays principal and a real mismatch is still a mismatch.
#[test]
fn join_of_two_ordinary_types_is_still_unification() {
    let mut db = TypeDb::new();
    let int = db.int();
    let text = db.text();
    assert!(db.join(int, text).is_err(), "Int and Text do not join");

    // A variable joined with a concrete type is *linked*, exactly as unify
    // would link it — the join is not a fresh third type.
    let var = db.fresh_var();
    let joined = db.join(var, int).expect("a variable joins");
    assert_eq!(db.canonical_key(joined), db.canonical_key(int));
    assert_eq!(db.canonical_key(var), db.canonical_key(int));
}

/// `join_all` is the arm/break-list form. Empty is `Never` — no path produces a
/// value — and a divergent element drops out rather than pinning the answer.
#[test]
fn join_all_seeds_with_never_and_reports_which_element_failed() {
    let mut db = TypeDb::new();
    let never = db.never();
    let int = db.int();
    let text = db.text();

    let empty = db.join_all([]).expect("an empty join is Never");
    assert!(matches!(db.data(empty), crate::TypeData::Never));

    let all_divergent = db.join_all([never, never]).expect("all divergent");
    assert!(matches!(db.data(all_divergent), crate::TypeData::Never));

    let mixed = db.join_all([never, int, never]).expect("one real branch");
    assert_eq!(db.canonical_key(mixed), db.canonical_key(int));

    let (index, _) = db
        .join_all([never, int, text])
        .expect_err("Int and Text disagree");
    assert_eq!(index, 2, "the failure names the element that disagreed");
}

// --- the constraint channel (F10, TY-29) -----------------------------------

/// A `FileSpan` for a constraint that has to have one. The channel does not
/// read it; the diagnostic that reports a failure does.
fn nowhere() -> praxis_source::FileSpan {
    praxis_source::FileSpan::new(
        praxis_source::SourceMap::new().intern("constraint_test.px", ""),
        praxis_source::Span::new(0, 0),
    )
}

/// The requirement a use discovers survives generalization by riding on the
/// scheme — and only the requirements about the scheme's *own* binders do. One
/// on a variable the enclosing scope still owns is not this scheme's to carry.
#[test]
fn a_scheme_claims_the_constraints_on_the_variables_it_quantifies() {
    let mut db = TypeDb::new();
    // An outer variable, created before the binding's scope: nothing here
    // quantifies it.
    let outer = db.fresh_var();
    let outer_id = db.var_id_of(outer).expect("a fresh var is a var");

    let body = db.scoped_return(|db| {
        let v = db.fresh_var();
        let inner_id = db.var_id_of(v).expect("a fresh var is a var");
        db.require(crate::Constraint::of_kind(
            inner_id,
            praxis_stdlib::CapKind::Eq,
            nowhere(),
        ));
        db.func(vec![v], v)
    });
    db.require(crate::Constraint::of_kind(
        outer_id,
        praxis_stdlib::CapKind::Ord,
        nowhere(),
    ));

    let scheme = db.generalize(body);
    assert_eq!(scheme.binders().len(), 1);
    assert_eq!(
        scheme.constraints().len(),
        1,
        "exactly the requirement on the binder"
    );
    assert_eq!(
        scheme.constraints()[0].cap.kind(),
        Some(praxis_stdlib::CapKind::Eq)
    );
    assert_eq!(
        db.pending_constraints().len(),
        1,
        "the outer variable's requirement is still the outer scope's"
    );
    assert_eq!(
        db.pending_constraints()[0].var,
        outer_id,
        "and it is the outer one that stayed"
    );
}

/// A monotype has no binders, so it has nothing a constraint could be about —
/// and `Scheme::monotype` says so by construction rather than by convention.
#[test]
fn a_monotype_carries_no_constraints_and_claims_none() {
    let mut db = TypeDb::new();
    let v = db.fresh_var();
    let id = db.var_id_of(v).expect("var");
    db.require(crate::Constraint::of_kind(
        id,
        praxis_stdlib::CapKind::Eq,
        nowhere(),
    ));

    let scheme = Scheme::monotype(v);
    assert!(scheme.constraints().is_empty());
    // Generalizing at a level that quantifies nothing claims nothing either.
    let generalized = db.generalize(v);
    assert!(generalized.binders().is_empty());
    assert!(generalized.constraints().is_empty());
    assert_eq!(
        db.pending_constraints().len(),
        1,
        "the requirement is still pending, not silently consumed"
    );
}

/// A pinned variable is one generalization will not quantify — that is the whole
/// of `pin_to_level`, and it is what TY-30 needs: a receiver a method was called
/// on has to stay one type, because there is one lowered body per function.
///
/// Both halves matter. The pin reaches *into* a composite (a receiver is usually
/// `Vec[?e]`, not a bare variable, and the element is what would otherwise be
/// quantified), and it is the only thing that changed — an unpinned variable at
/// the same depth still generalizes, so the rule is the pin and not the level.
#[test]
fn a_pinned_variable_is_not_quantified() {
    let mut db = TypeDb::new();
    let site = db.level();
    let (pinned, free) = {
        let prev = db.enter_level();
        let pinned = db.fresh_var();
        let free = db.fresh_var();
        db.pin_to_level(pinned, site);
        db.exit_level(prev);
        (pinned, free)
    };

    let body = db.func(vec![pinned], free);
    let scheme = db.generalize_at(body, site);
    let free_id = db.var_id_of(free).expect("a fresh var is a var");
    assert_eq!(
        scheme.binders(),
        &[free_id],
        "the pinned variable stays monomorphic; its neighbour at the same level does not"
    );

    // The pin follows structure: `Vec[?e]`'s element is what generalization
    // would otherwise reach, and pinning the collection pins it.
    let mut db = TypeDb::new();
    let site = db.level();
    let vec_of_var = {
        let prev = db.enter_level();
        let el = db.fresh_var();
        let v = db.vec(el);
        db.pin_to_level(v, site);
        db.exit_level(prev);
        v
    };
    let scheme = db.generalize_at(vec_of_var, site);
    assert!(
        scheme.binders().is_empty(),
        "a pinned Vec's element type is pinned with it"
    );
}

/// Instantiation re-emits: the requirement was written about the generic body's
/// variable, and what has to satisfy it is whatever *this* use puts in its
/// place. Two uses are two constraints, each about its own fresh variable.
#[test]
fn instantiating_re_emits_each_constraint_against_the_use_sites_variable() {
    let mut db = TypeDb::new();
    let body = db.scoped_return(|db| {
        let v = db.fresh_var();
        let id = db.var_id_of(v).expect("var");
        db.require(crate::Constraint::of_kind(
            id,
            praxis_stdlib::CapKind::Eq,
            nowhere(),
        ));
        db.func(vec![v, v], v)
    });
    let scheme = db.generalize(body);
    assert!(db.pending_constraints().is_empty(), "the scheme took it");

    let (_ty_a, map_a) = db.instantiate_with_mapping(&scheme);
    let (_ty_b, map_b) = db.instantiate_with_mapping(&scheme);
    let pending = db.pending_constraints();
    assert_eq!(pending.len(), 2, "one per use");
    let a = db.var_id_of(map_a[0]).expect("fresh var");
    let b = db.var_id_of(map_b[0]).expect("fresh var");
    assert_ne!(a, b, "two uses, two variables");
    assert!(pending.iter().any(|c| c.var == a));
    assert!(pending.iter().any(|c| c.var == b));
    assert!(
        pending.iter().all(|c| c.var != scheme.binders()[0]),
        "and neither is about the scheme's own binder"
    );
}

/// A capability that carries types is re-pointed too. `Iterable { item }`'s
/// item is written in the generic body's terms; left alone, every use would
/// constrain the scheme's own variable and the item type would be shared
/// between unrelated call sites.
#[test]
fn a_type_carrying_capability_is_substituted_at_the_use_site() {
    let mut db = TypeDb::new();
    let body = db.scoped_return(|db| {
        let v = db.fresh_var();
        let item = db.fresh_var();
        let id = db.var_id_of(v).expect("var");
        db.require(crate::Constraint::new(
            id,
            crate::Capability::Iterable { item },
            nowhere(),
        ));
        db.func(vec![v], item)
    });
    let scheme = db.generalize(body);
    assert_eq!(scheme.binders().len(), 2, "the receiver and the item");

    let (_ty, mapping) = db.instantiate_with_mapping(&scheme);
    let emitted = db.pending_constraints();
    assert_eq!(emitted.len(), 1);
    match &emitted[0].cap {
        crate::Capability::Iterable { item } => {
            assert!(
                mapping.contains(item),
                "the item names one of this use's fresh variables, not the scheme's"
            );
        }
        other => panic!("expected Iterable, got {other:?}"),
    }
}

/// Discharge is by *resolution*, not by age: a constraint whose variable has
/// been linked to a concrete type is ready to check; one that is still a
/// variable is not wrong yet, and answering it now is the optimism TY-29 is
/// about.
#[test]
fn only_a_resolved_constraint_is_dischargeable() {
    let mut db = TypeDb::new();
    let (resolved, unresolved) = (db.fresh_var(), db.fresh_var());
    let (r_id, u_id) = (
        db.var_id_of(resolved).expect("var"),
        db.var_id_of(unresolved).expect("var"),
    );
    db.require(crate::Constraint::of_kind(
        r_id,
        praxis_stdlib::CapKind::Eq,
        nowhere(),
    ));
    db.require(crate::Constraint::of_kind(
        u_id,
        praxis_stdlib::CapKind::Ord,
        nowhere(),
    ));

    assert!(
        db.take_dischargeable().is_empty(),
        "nothing is resolved yet"
    );

    let int = db.int();
    db.unify(resolved, int).expect("a fresh var unifies");
    let ready = db.take_dischargeable();
    assert_eq!(ready.len(), 1, "exactly the one that resolved");
    assert_eq!(ready[0].var, r_id);
    assert_eq!(
        db.pending_constraints().len(),
        1,
        "and the unresolved one is still waiting"
    );
    assert!(
        db.take_dischargeable().is_empty(),
        "draining takes; it does not copy"
    );
}

/// The same requirement discovered twice is one requirement. A `for` inside a
/// loop body would otherwise push one per pass over the tree.
#[test]
fn an_identical_requirement_is_recorded_once() {
    let mut db = TypeDb::new();
    let v = db.fresh_var();
    let id = db.var_id_of(v).expect("var");
    for _ in 0..3 {
        db.require(crate::Constraint::of_kind(
            id,
            praxis_stdlib::CapKind::Eq,
            nowhere(),
        ));
    }
    assert_eq!(db.pending_constraints().len(), 1);
    // …but a *different* capability on the same variable is a different
    // requirement, and both have to be checked.
    db.require(crate::Constraint::of_kind(
        id,
        praxis_stdlib::CapKind::Ord,
        nowhere(),
    ));
    assert_eq!(db.pending_constraints().len(), 2);
}
