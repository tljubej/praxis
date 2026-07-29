//! Internal capability resolution (§5.4, §5.5).
//!
//! The language has no user-visible traits (§4.8); instead the compiler owns a
//! closed table deciding which shapes support structural operations like equality
//! and hashing. This module answers those questions for the type system.
//!
//! The rules (§5.5) are:
//! - `Int`, `Bool`, `Char`, `Byte`, `Text`, `UInt`, `Float`, `Unit` are
//!   equatable and hashable.
//! - Tuples, records, and enums are equatable/hashable iff **every** contained
//!   type (tuple elements, record fields, every enum-variant payload) is.
//! - Collections (`Vec[T]`) are equatable/hashable iff their element type is.
//! - **Functions are never equatable or hashable** (function/closure values have
//!   no structural identity — §5.5).
//!
//! An unresolved type variable is optimistically treated as capable: inference
//! will constrain it, and the diagnostic for a concrete non-equatable type is
//! emitted from `infer_bin` against the resolved type. This keeps the capability
//! check out of the way of polymorphic inference.
//!
//! **Per §5.4, capability failures surfaced to the user must be translated into
//! concrete language terms and must never mention "trait" or "capability".** The
//! diagnostic wording lives in [`crate::diagnostics`].

use praxis_types::{data::TypeData, Type, TypeDb};

/// True iff values of type `t` may be compared with `==` / `!=` (§5.5).
///
/// Recursive over the type's structure. Functions are never equatable; a
/// composite is equatable iff all of its components are. An unresolved type
/// variable is optimistically equatable (see the module docs).
#[must_use]
pub fn supports_eq(db: &TypeDb, t: Type) -> bool {
    match db.data(db.follow(t)) {
        // Scalars: every implemented scalar is equatable. (`Never`, the bottom
        // type, has no values and so is vacuously equatable.)
        TypeData::Scalar(_) => true,
        TypeData::Unit => true,
        // A tuple is equatable iff every element is.
        TypeData::Tuple(els) => els.iter().all(|e| supports_eq(db, *e)),
        // Functions are never equatable (§5.5).
        TypeData::Func { .. } => false,
        // A collection is equatable iff its element type is.
        TypeData::Collection { args, .. } => args.iter().all(|a| supports_eq(db, *a)),
        // A record is equatable iff every field type is, and iff every type
        // argument is: a generic def's field types mention its parameters,
        // which are unresolved variables and answer optimistically, so the
        // arguments are where the real answer lives (F12).
        TypeData::Record { def, args } => {
            args.iter().all(|a| supports_eq(db, *a))
                && db
                    .record_def(*def)
                    .fields
                    .iter()
                    .all(|f| supports_eq(db, f.ty))
        }
        // An enum is equatable iff every variant's payload types are (a variant
        // with no payload is trivially equatable) and every type argument is.
        TypeData::Enum { def, args } => {
            args.iter().all(|a| supports_eq(db, *a))
                && db
                    .enum_def(*def)
                    .variants
                    .iter()
                    .all(|v| v.payload.iter().all(|t| supports_eq(db, *t)))
        }
        // An unresolved var is optimistically equatable (see module docs).
        TypeData::Var(_) => true,
    }
}

/// True iff values of type `t` may be used as a map/set key (§5.5). A type is
/// hashable under the same structural rules as equatability (the runtime's hash
/// and equals callbacks are defined together per descriptor).
#[must_use]
pub fn supports_hash(db: &TypeDb, t: Type) -> bool {
    supports_eq(db, t)
}

/// True iff values of type `t` are orderable (§5.4 `SupportsOrd`, ADR-045):
/// usable in a heap, sortable, or comparable with `<`/`>`/`<=`/`>=`.
///
/// The orderable types are the scalars whose descriptors declare a `compare`
/// callback: `Int`/`UInt`/`Byte`/`Float`/`Char`/`Text`. **Nothing else is** —
/// not tuples, not collections, not records or enums, not functions.
///
/// This used to answer *yes* for a tuple of orderable elements, and for a
/// collection or record of them, on the reasonable ground that a lexicographic
/// product is conventional. But no such ordering was ever lowered: MIR had one
/// integer compare, so `(1, 2) < (1, 3)` compiled into a comparison of two
/// payload words that happen to be schema pointers (P0-12). ADR-045 chose
/// rejection over a semantics nobody had picked; a composite ordering is a
/// language decision plus a recursive `praxis_value_cmp`, and both belong to
/// whichever milestone wants sorting by key.
///
/// `Bool` and `Unit` stay non-orderable for the older reason: the spec (§5.4)
/// leaves `SupportsOrd` compiler-defined, and ordering booleans is almost
/// always a mistyped `&&`.
#[must_use]
pub fn supports_ord(db: &TypeDb, t: Type) -> bool {
    use praxis_stdlib::type_pattern::ScalarType;
    match db.data(db.follow(t)) {
        TypeData::Scalar(
            ScalarType::Int
            | ScalarType::UInt
            | ScalarType::Byte
            | ScalarType::Float
            | ScalarType::Char
            | ScalarType::Text,
        ) => true,
        // Bool and Unit have no defined total order.
        TypeData::Scalar(ScalarType::Bool | ScalarType::Never) | TypeData::Unit => false,
        // Composites have no ordering lowering (ADR-045 decision 1).
        TypeData::Tuple(_)
        | TypeData::Func { .. }
        | TypeData::Collection { .. }
        | TypeData::Record { .. }
        | TypeData::Enum { .. } => false,
        // An unresolved var is optimistically orderable.
        TypeData::Var(_) => true,
    }
}

/// The `Item` type yielded when iterating a value of type `t`, or `None` if `t`
/// is not iterable (§5.4 `Iterable(T, Item)`, §4.11 "for loops over built-in
/// iterable shapes").
///
/// Recursive over the type's structure. A `Vec[T]`/`Deque[T]`/`Set[T]`/
/// `MinHeap[T]`/`MaxHeap[T]` yields `T`; a `BitSet` yields `Int`; a `Grid[T]`
/// yields its cell type `T`; a `Range` yields `Int`. A `Map[K, V]` yields the
/// `(K, V)` tuple (so a pipeline over a map threads key/value pairs). Functions,
/// scalars, records, enums, and tuples are not iterable. An unresolved type
/// variable is optimistically iterable, yielding itself (inference will pin it).
///
/// Takes `&mut TypeDb` because the `Map[K, V]` and `Counter[T]` cases mint fresh
/// tuple types to return as the `Item`.
#[must_use]
pub fn iter_item(db: &mut TypeDb, t: Type) -> Option<Type> {
    use praxis_stdlib::type_pattern::{CollectionCtor, ScalarType};
    // Inspect the type shape by reading it into owned data first, so the
    // immutable borrow of `db.data(...)` ends before the mutable `db.tuple`/
    // `db.scalar` calls below.
    let shape = match db.data(db.follow(t)) {
        TypeData::Collection { ctor, args } => Some((*ctor, args.clone())),
        TypeData::Var(_) => return Some(t),
        _ => None,
    }?;
    let (ctor, args) = shape;
    Some(match (ctor, &args[..]) {
        // Sequence-shaped collections yield their single element type.
        (CollectionCtor::Vec | CollectionCtor::Deque | CollectionCtor::Set, [el]) => *el,
        // Heaps yield their element type (iteration order is unspecified but
        // the element type is well-defined).
        (CollectionCtor::MinHeap | CollectionCtor::MaxHeap, [el]) => *el,
        // A grid yields its cell type.
        (CollectionCtor::Grid, [cell]) => *cell,
        // BitSet and Range are nullary collections of non-negative Ints.
        (CollectionCtor::BitSet, _) | (CollectionCtor::Range, _) => db.scalar(ScalarType::Int),
        // A map yields its (key, value) pairs as a tuple.
        (CollectionCtor::Map, [k, v]) => db.pair(*k, *v),
        // A counter yields its (key, value=Int) pairs.
        (CollectionCtor::Counter, [k]) => {
            let int_ty = db.scalar(ScalarType::Int);
            db.pair(*k, int_ty)
        }
        // `Seq[T]` is the compiler-internal pipeline source (M8 WS8); it
        // threads its single element type through the pipeline.
        (CollectionCtor::Seq, [el]) => *el,
        // An under/over-applied or malformed collection ctor is not iterable.
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_stdlib::type_pattern::ScalarType;
    use praxis_types::CollectionCtor;

    /// True iff `t` resolves to the scalar `Int` (test helper only).
    fn is_int(db: &TypeDb, t: Type) -> bool {
        matches!(db.data(db.follow(t)), TypeData::Scalar(ScalarType::Int))
    }

    /// True iff `t` resolves to the scalar `Text` (test helper only).
    fn is_text(db: &TypeDb, t: Type) -> bool {
        matches!(db.data(db.follow(t)), TypeData::Scalar(ScalarType::Text))
    }

    #[test]
    fn vec_yields_its_element_type() {
        let mut db = TypeDb::new();
        let int = db.int();
        let vec_int = db.vec(int);
        let item = iter_item(&mut db, vec_int).expect("Vec[Int] is iterable");
        assert!(is_int(&db, item));
    }

    #[test]
    fn map_yields_key_value_tuple() {
        let mut db = TypeDb::new();
        let (text, int) = (db.text(), db.int());
        let map = db.map(text, int);
        let item = iter_item(&mut db, map).expect("Map[Text,Int] is iterable");
        match db.data(db.follow(item)) {
            TypeData::Tuple(els) => {
                assert_eq!(els.len(), 2);
                assert!(is_text(&db, els[0]));
                assert!(is_int(&db, els[1]));
            }
            other => panic!("expected tuple item, got {other:?}"),
        }
    }

    #[test]
    fn counter_yields_key_and_int_value() {
        let mut db = TypeDb::new();
        let text = db.text();
        let counter = db.unary_collection(CollectionCtor::Counter, text);
        let item = iter_item(&mut db, counter).expect("Counter[Text] is iterable");
        match db.data(db.follow(item)) {
            TypeData::Tuple(els) => {
                assert_eq!(els.len(), 2);
                assert!(is_text(&db, els[0]));
                assert!(is_int(&db, els[1]));
            }
            other => panic!("expected tuple item, got {other:?}"),
        }
    }

    #[test]
    fn bitset_yields_int() {
        let mut db = TypeDb::new();
        let bitset = db
            .collection(
                CollectionCtor::BitSet,
                praxis_types::CollectionArgs::Nullary,
            )
            .expect("BitSet is nullary");
        let item = iter_item(&mut db, bitset).expect("BitSet is iterable");
        assert!(is_int(&db, item));
    }

    #[test]
    fn grid_yields_its_cell_type() {
        let mut db = TypeDb::new();
        let int = db.int();
        let grid = db.unary_collection(CollectionCtor::Grid, int);
        let item = iter_item(&mut db, grid).expect("Grid[Int] is iterable");
        assert!(is_int(&db, item));
    }

    #[test]
    fn scalars_and_functions_are_not_iterable() {
        let mut db = TypeDb::new();
        let (int, unit) = (db.int(), db.unit());
        assert!(iter_item(&mut db, int).is_none());
        assert!(iter_item(&mut db, unit).is_none());
        let (param, result) = (db.int(), db.int());
        let func = db.func(vec![param], result);
        assert!(iter_item(&mut db, func).is_none());
    }

    #[test]
    fn unresolved_var_is_optimistically_iterable() {
        // An unbound var is optimistically iterable, yielding itself: a `for`
        // over a not-yet-pinned source type-checks, and inference pins it later.
        let mut db = TypeDb::new();
        let v = db.fresh_var();
        assert!(iter_item(&mut db, v).is_some());
    }

    #[test]
    fn numeric_scalars_are_orderable() {
        let mut db = TypeDb::new();
        let (i, t, c) = (db.int(), db.text(), db.char());
        assert!(supports_ord(&db, i));
        assert!(supports_ord(&db, t));
        assert!(supports_ord(&db, c));
    }

    #[test]
    fn bool_and_unit_are_not_orderable() {
        // Bool/Unit have no defined total order; ordering them is a likely bug.
        let mut db = TypeDb::new();
        let (b, u) = (db.bool(), db.unit());
        assert!(!supports_ord(&db, b));
        assert!(!supports_ord(&db, u));
    }

    #[test]
    fn functions_are_not_orderable() {
        let mut db = TypeDb::new();
        let (p, r) = (db.int(), db.int());
        let func = db.func(vec![p], r);
        assert!(!supports_ord(&db, func));
    }

    /// **Inverted** by ADR-045, and this is the assertion that used to say the
    /// opposite: `tuple_is_orderable_iff_elements_are`, which was true of the
    /// capability check and of nothing else in the compiler. A tuple of
    /// orderable elements has no ordering *lowering*, so admitting it meant
    /// `(1, 2) < (1, 3)` compiled into a comparison of two schema pointers
    /// (P0-12). Composite ordering returns as a language decision plus a
    /// recursive runtime compare, not as an optimistic `all`.
    #[test]
    fn composites_are_not_orderable_even_when_their_elements_are() {
        let mut db = TypeDb::new();
        let (a, b) = (db.int(), db.int());
        let tup = db.pair(a, b);
        assert!(!supports_ord(&db, tup));

        let el = db.int();
        let vec_of_int = db.vec(el);
        assert!(!supports_ord(&db, vec_of_int));

        let (fx, fy) = (db.int(), db.int());
        let fields = praxis_types::FieldSet::from_pairs(vec![("x".into(), fx), ("y".into(), fy)])
            .expect("distinct field names");
        let rec = db.record(Some("P".into()), fields);
        assert!(!supports_ord(&db, rec));

        // Equality is unchanged: it *does* recurse, and it has a lowering.
        assert!(supports_eq(&db, tup));
        assert!(supports_eq(&db, rec));
    }
}
