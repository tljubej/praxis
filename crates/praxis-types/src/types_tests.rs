//! Tests for the core inference engine (ADR-007/008).
//!
//! These exercise the type arena, unification, generalization, and rendering in
//! isolation — no syntax, no HIR. They are the foundation the rest of M2 builds
//! on, so a regression here is caught before anything downstream.

#![cfg(test)]

use praxis_stdlib::type_pattern::{CollectionCtor, ScalarType};

use crate::data::{TypeData, VarState};
use crate::TypeDb;

fn is_int(db: &TypeDb, t: crate::Type) -> bool {
    matches!(db.data(db.follow(t)), TypeData::Scalar(ScalarType::Int))
}

/// Build a 2-tuple without chaining borrows. The arena methods each take
/// `&mut self`, so `db.tuple(vec![db.int(), db.text()])` would alias; this helper
/// is the ergonomic shape real callers (the inferer) use.
fn tup2(db: &mut TypeDb, a: crate::Type, b: crate::Type) -> crate::Type {
    db.tuple(vec![a, b])
}

/// Build `(a) -> r` without aliasing.
fn func1(db: &mut TypeDb, a: crate::Type, r: crate::Type) -> crate::Type {
    db.func(vec![a], r)
}

#[test]
fn scalar_interning_is_stable() {
    let mut db = TypeDb::new();
    let a = db.int();
    let b = db.int();
    // Two separately-interned Ints have equal *structure*; follow resolves both.
    assert!(is_int(&db, a));
    assert!(is_int(&db, b));
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
    let three = db.tuple(vec![i3, t3, b3]);
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
        db.data(crate::Type(var.0)),
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

#[test]
fn unify_vec_mismatched_arity_fails() {
    // `Vec[Int]` (one arg) vs a collection with two args — different arg counts
    // must mismatch even when the ctor matches.
    let mut db = TypeDb::new();
    let i = db.int();
    let a = db.vec(i);
    let t = db.text();
    let b = db.collection(CollectionCtor::Vec, vec![i, t]);
    let err = db.unify(a, b).unwrap_err();
    assert!(matches!(err, crate::unify::UnifyError::Mismatch { .. }));
}

#[test]
fn unify_vec_with_different_ctor_fails() {
    // Same args, different collection ctor: `Vec[Int]` vs `Deque[Int]`. These are
    // distinct nominal collection types (§4.4) and must not unify.
    let mut db = TypeDb::new();
    let i = db.int();
    let a = db.vec(i);
    let b = db.collection(CollectionCtor::Deque, vec![i]);
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
    let m = db.collection(CollectionCtor::Map, vec![t, i]);
    assert_eq!(db.render(m), "Map[Text, Int]");
    // Nested: Vec[Map[Text, Int]].
    let nested = db.vec(m);
    assert_eq!(db.render(nested), "Vec[Map[Text, Int]]");
}
