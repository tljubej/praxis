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
//!
//! # One door, five questions (F10)
//!
//! [`check`] is the entry point, and every capability question routes through
//! it. The predicates below it are the rules; `check` is what makes them one
//! decision with one failure shape (`Err(offending inner type)`), so a caller
//! cannot ask half the question or answer it with the wrong predicate.
//!
//! Two of the five are new here and are the point of the stage:
//!
//! - [`supports_hash_stable`] is what a `Map` key must answer. [`supports_hash`]
//!   really is [`supports_eq`] — the descriptor's `hash` and `equals` callbacks
//!   are one fact — and that is exactly why it is the wrong question for a key
//!   (TY-32, D4).
//! - [`supports_numeric`] is what arithmetic and the numeric sinks require, and
//!   it is a *different* set from [`supports_ord`]: `Text` is orderable and is
//!   not a number (TY-31).
//!
//! [`supports_ord`] is also a different set from the runtime's
//! `TypeDescriptor::compare`, and the split is deliberate: this module answers
//! what the *source language* permits (`<`, `sorted()`, a heap element), while
//! the descriptor callback answers what order a *container* walks and prints its
//! keys in (ADR-138). Every type [`supports_hash_stable`] admits has the second
//! — it must, or `for k in m` would have no deterministic answer — and a tuple
//! is the case where the two come apart.
//!
//! A requirement about a type that is still a variable is not answered here. It
//! is deferred, on `praxis_types::constraint`'s channel, and discharged when the
//! variable resolves.

use praxis_stdlib::{CapKind, MethodCatalog};
use praxis_types::{data::TypeData, Capability, CollectionCtor, Type, TypeDb};

/// **The** decision function: does `t` have `cap`?
///
/// `Err(inner)` names the type that failed. For a scalar that is simply the
/// type itself; for a composite it is the *component* that failed, because
/// "`Vec[fn(Int) -> Int]` cannot be a key" is a worse message than "a function
/// value cannot be a key" — the program wrote the function, not the `Vec`.
///
/// Every other capability question in the compiler routes through here. Before
/// F10 there were four free predicates with no shared shape, two of which had
/// zero non-test callers and one of which (`supports_hash`) was *literally*
/// `supports_eq` — which is what admitted mutable collections as `Map` keys
/// (TY-32, RT-08).
///
/// An unresolved variable answers **yes** to everything. That is right here and
/// wrong nowhere: this function is asked about a specific type, and a variable
/// is not a type yet. Deferring the question is the constraint channel's job
/// (`praxis_types::constraint`), not this function's.
///
/// Takes `&mut TypeDb` because [`iter_item`] mints the `(K, V)` tuple a `Map`
/// yields, and `catalog` because [`Capability::HasMethod`] is a question only
/// the method catalog can answer.
pub fn check(
    db: &mut TypeDb,
    catalog: &MethodCatalog,
    t: Type,
    cap: &Capability,
) -> Result<(), Type> {
    match cap {
        Capability::Kind(CapKind::Eq) => fail_unless(supports_eq(db, t), db, t, CapKind::Eq),
        Capability::Kind(CapKind::Ord) => fail_unless(supports_ord(db, t), db, t, CapKind::Ord),
        Capability::Kind(CapKind::Hash) => fail_unless(supports_hash(db, t), db, t, CapKind::Hash),
        Capability::Kind(CapKind::HashStable) => {
            fail_unless(supports_hash_stable(db, t), db, t, CapKind::HashStable)
        }
        Capability::Kind(CapKind::Numeric) => {
            fail_unless(supports_numeric(db, t), db, t, CapKind::Numeric)
        }
        // Iterability is a yes/no about the receiver. *Which* item it yields is
        // unified by `Inferer::resolve_deferred_iterable`, which is the only
        // caller that has both an item to relate it to and somewhere to report a
        // disagreement (REP-04): the failure here is `Err(offending type)`, and
        // "iterates, but not at that element type" is a mismatch and not that.
        Capability::Iterable { .. } => {
            if db.var_id_of(db.follow(t)).is_some() {
                // Still a variable: optimistically yes, as everywhere else — and
                // without minting the fresh item `iter_item` would answer with.
                return Ok(());
            }
            match iter_item(db, t) {
                Some(_) => Ok(()),
                None => Err(db.follow(t)),
            }
        }
        Capability::HasMethod { name, params, .. } => {
            if db.var_id_of(db.follow(t)).is_some() {
                // Still a variable: optimistically yes, as everywhere else.
                return Ok(());
            }
            if crate::catalog::lookup(db, catalog, t, name, params.len()).is_empty() {
                Err(db.follow(t))
            } else {
                Ok(())
            }
        }
        // Having a field is a yes/no about the receiver; *which* type that field
        // holds is unified by `Inferer::resolve_deferred_field`, for the reason
        // `Iterable`'s item is (REP-04, REP-28).
        Capability::HasField { name, .. } => {
            let resolved = db.follow(t);
            if db.var_id_of(resolved).is_some() {
                // Still a variable: optimistically yes, as everywhere else.
                return Ok(());
            }
            match db.data(resolved) {
                TypeData::Record { def, args } => {
                    let (def, args) = (*def, args.to_vec());
                    match db.record_field_of(def, &args, name) {
                        Some(_) => Ok(()),
                        None => Err(resolved),
                    }
                }
                _ => Err(resolved),
            }
        }
    }
}

/// `Ok(())`, or the innermost type that failed `kind`.
fn fail_unless(held: bool, db: &TypeDb, t: Type, kind: CapKind) -> Result<(), Type> {
    if held {
        Ok(())
    } else {
        Err(offender(db, t, kind))
    }
}

/// The component of `t` that fails `kind` — the type a diagnostic should name.
///
/// Recurses into the same shapes the predicates do and stops at the first
/// component that fails, so a `Vec[Vec[fn]]` reports the function rather than
/// the outer `Vec`. Falls back to `t` itself when the failure *is* `t` (a
/// mutable collection under `HashStable`, a `Bool` under `Ord`).
fn offender(db: &TypeDb, t: Type, kind: CapKind) -> Type {
    let resolved = db.follow(t);
    let components: Vec<Type> = match db.data(resolved) {
        TypeData::Tuple(els) => els.to_vec(),
        TypeData::Collection { args, .. } => args.to_vec(),
        TypeData::Record { def, args } => {
            let mut v = args.to_vec();
            v.extend(db.record_def(*def).fields.iter().map(|f| f.ty));
            v
        }
        TypeData::Enum { def, args } => {
            let mut v = args.to_vec();
            v.extend(
                db.enum_def(*def)
                    .variants
                    .iter()
                    .flat_map(|va| va.payload.iter().copied()),
            );
            v
        }
        _ => Vec::new(),
    };
    for c in components {
        if !holds(db, c, kind) {
            return offender(db, c, kind);
        }
    }
    resolved
}

/// The predicate for `kind`, as one dispatch. Structural recursion lives in the
/// individual predicates; this is only the mapping from name to question.
fn holds(db: &TypeDb, t: Type, kind: CapKind) -> bool {
    match kind {
        CapKind::Eq => supports_eq(db, t),
        CapKind::Ord => supports_ord(db, t),
        CapKind::Hash => supports_hash(db, t),
        CapKind::HashStable => supports_hash_stable(db, t),
        CapKind::Numeric => supports_numeric(db, t),
    }
}

/// True iff values of type `t` may be compared with `==` / `!=` (§5.5).
///
/// Recursive over the type's structure. Functions are never equatable; a
/// composite is equatable iff all of its components are. An unresolved type
/// variable is optimistically equatable (see the module docs).
#[must_use]
pub fn supports_eq(db: &TypeDb, t: Type) -> bool {
    match db.data(db.follow(t)) {
        // Scalars: every implemented scalar is equatable.
        TypeData::Scalar(_) => true,
        TypeData::Unit => true,
        // `Never` has no values, so every capability holds vacuously.
        TypeData::Never => true,
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

/// True iff the runtime can compute a structural hash of a value of type `t`
/// (§5.5).
///
/// This really *is* [`supports_eq`]: the descriptor's `hash` and `equals`
/// callbacks are defined together and populated together, so "can be hashed"
/// and "can be compared" are one question about the representation.
///
/// It is **not** the question a `Map` key has to answer. That is
/// [`supports_hash_stable`], and conflating the two is TY-32: a `Vec` hashes
/// fine and stops being findable the moment it changes.
#[must_use]
pub fn supports_hash(db: &TypeDb, t: Type) -> bool {
    supports_eq(db, t)
}

/// True iff a value of type `t` may be a `Map` key or a `Set` element (D4,
/// TY-32/RT-08).
///
/// Hashable **and immutable**. The rule is mutability, not container-ness:
///
/// - The eight mutable collections — `Vec`, `Map`, `Set`, `Deque`, `Grid`,
///   `Counter`, `MinHeap`/`MaxHeap`, `BitSet` — are out. Hashing one by its
///   current contents and then mutating it moves the entry's bucket without
///   moving the entry, so the value can never be found again.
/// - Scalars, `Text`, tuples, records and enums are in **structurally**: a
///   tuple or record is a key iff every component is. That is Python's `tuple`
///   rule, and it is already how [`supports_eq`] recurses.
///
/// Python rejects `list`/`dict`/`set` outright for this reason. Rust permits
/// `HashMap<Vec<i32>, V>`, but only because the borrow checker makes mutating a
/// key the map still holds impossible; Praxis has `var` mutation and no borrow
/// checker, so it has Python's exposure without Rust's guardrail.
///
/// `Seq` is a compiler-internal pipeline source, never a value a program holds,
/// so it never reaches a key position; it is excluded with the rest.
#[must_use]
pub fn supports_hash_stable(db: &TypeDb, t: Type) -> bool {
    match db.data(db.follow(t)) {
        // Mutability, not container-ness (ADR-057 D4). Every collection whose
        // contents a program can change after it is stored is out — including
        // `BitSet`, whose members change, and the heaps, whose contents do.
        //
        // A `Range` is the exception, and it is one for the stated reason rather
        // than by accident: it has **no mutator at all** (ADR-059), so its two
        // bounds are as fixed as a tuple's elements, and a tuple of scalars has
        // always been a key. `Seq` is a lazy pipeline source whose elements are
        // produced on demand — it is compiler-internal and never a key.
        TypeData::Collection { ctor, .. } => match ctor {
            CollectionCtor::Range => true,
            CollectionCtor::Vec
            | CollectionCtor::Deque
            | CollectionCtor::Map
            | CollectionCtor::Set
            | CollectionCtor::Counter
            | CollectionCtor::MinHeap
            | CollectionCtor::MaxHeap
            | CollectionCtor::Grid
            | CollectionCtor::BitSet
            | CollectionCtor::Seq => false,
        },
        // A tuple is a key iff every element is.
        TypeData::Tuple(els) => els.iter().all(|e| supports_hash_stable(db, *e)),
        // A record/enum is one iff every argument and every contained type is.
        // Praxis records and enums are immutable in the sense that matters: a
        // field is set at construction and `praxis_record_set_field` exists for
        // `var` bindings of the *binding*, not of a shared object. If that ever
        // changes, this is the arm that changes with it.
        TypeData::Record { def, args } => {
            args.iter().all(|a| supports_hash_stable(db, *a))
                && db
                    .record_def(*def)
                    .fields
                    .iter()
                    .all(|f| supports_hash_stable(db, f.ty))
        }
        TypeData::Enum { def, args } => {
            args.iter().all(|a| supports_hash_stable(db, *a))
                && db
                    .enum_def(*def)
                    .variants
                    .iter()
                    .all(|v| v.payload.iter().all(|ty| supports_hash_stable(db, *ty)))
        }
        // Everything else answers as hashability does: scalars and `Unit` yes,
        // functions no, `Never` vacuously yes, an unresolved var optimistically.
        _ => supports_eq(db, t),
    }
}

/// True iff `t` admits arithmetic — `+`, `-`, `*`, `/`, unary minus, and the
/// numeric sinks (§4.12, TY-31).
///
/// `Int`, `UInt`, `Byte` and `Float`, and nothing else. `Bool` is not a number
/// however it is represented, `Char` is a scalar value and not an arithmetic
/// one, and `Text` has `+` only as concatenation, which is a different rule at
/// a different site.
///
/// `%` is narrower still — it is undefined for `Float` (TY-27) — so it is not
/// this capability.
#[must_use]
pub fn supports_numeric(db: &TypeDb, t: Type) -> bool {
    use praxis_stdlib::type_pattern::ScalarType;
    match db.data(db.follow(t)) {
        TypeData::Scalar(
            ScalarType::Int | ScalarType::UInt | ScalarType::Byte | ScalarType::Float,
        ) => true,
        // No values, so every capability holds vacuously — a divergent branch
        // must not be what makes an addition illegal.
        TypeData::Never => true,
        // An unresolved var is optimistically numeric; the channel defers it.
        TypeData::Var(_) => true,
        _ => false,
    }
}

/// True iff values of type `t` are orderable (§5.4 `SupportsOrd`, ADR-045):
/// usable in a heap, sortable, or comparable with `<`/`>`/`<=`/`>=`.
///
/// The orderable types are the scalars `Int`/`UInt`/`Byte`/`Float`/`Char`/`Text`.
/// **Nothing else is** — not tuples, not collections, not records or enums, not
/// functions.
///
/// This is the **source language's** order, and it is deliberately a different
/// and smaller set than the **container** order a runtime descriptor's `compare`
/// callback carries (ADR-138 decision 3). A container order exists for every type
/// a `Map` key or `Set` member can be — including `Bool`, `Unit`, `Range` and
/// every tuple, record and enum — because `out(m)` and `for k in m` have to name
/// a sequence and it has to be the same sequence on two runs. Having one does
/// not make a type comparable with `<`: a tuple has a container order and
/// `(1, 2) < (1, 3)` is still `Y006`, which is the line below, and widening this
/// function to "whatever has a `compare`" would quietly legalise it.
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
        TypeData::Scalar(ScalarType::Bool) | TypeData::Unit => false,
        // `Never` has no values to order, so ordering it is vacuously fine —
        // and a divergent branch must not be what makes a sort illegal.
        TypeData::Never => true,
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
/// `(K, V)` tuple (so a pipeline over a map threads key/value pairs). A `Text`
/// yields `Char` — the one scalar that is iterable, because it is the one with
/// members (§4.13). Functions, the other scalars, records, enums, and tuples are
/// not iterable.
///
/// # An unresolved receiver yields a *fresh* variable (REP-03)
///
/// It used to yield **itself**, and that is a legal program rejected:
///
/// ```praxis
/// fn total(r) { var t = 0
///   for i in r { t = t + i }
///   t }
/// ```
///
/// The loop variable and the iterator came back as one variable, so `t + i`
/// pinned the *iterator* to `Int` and the `for` reported `Y005` — "values of
/// type `Int` cannot be iterated" — about a parameter the program never typed.
/// Identically for `Vec`, `BitSet` and `Range`, which is why TY-34's gates all
/// annotate their iterated parameters.
///
/// A fresh variable is also what gives the deferred `Iterable { item }`
/// constraint two things to relate, which is what makes REP-04 checkable at all:
/// `Inferer::resolve_deferred_iterable` unifies the item this function answers
/// with the one the constraint carries once the receiver is known.
///
/// Takes `&mut TypeDb` because the `Map[K, V]` and `Counter[T]` cases mint fresh
/// tuple types to return as the `Item` — and because of the arm above.
#[must_use]
pub fn iter_item(db: &mut TypeDb, t: Type) -> Option<Type> {
    use praxis_stdlib::type_pattern::{CollectionCtor, ScalarType};
    // Inspect the type shape by reading it into owned data first, so the
    // immutable borrow of `db.data(...)` ends before the mutable `db.tuple`/
    // `db.scalar` calls below.
    let shape = match db.data(db.follow(t)) {
        TypeData::Collection { ctor, args } => Some((*ctor, args.clone())),
        // A `Text` yields its characters, and the `Char` it yields is the one
        // `t[i]` already answers (§4.13, ADR-086). One accessor answers both, so
        // `for c in t` and `t[i]` cannot disagree about what a character is —
        // which is the same "iteration order is the accessors' order" rule
        // ADR-066 applied to the ten collections.
        TypeData::Scalar(ScalarType::Text) => return Some(db.scalar(ScalarType::Char)),
        TypeData::Var(_) => return Some(db.fresh_var()),
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

    /// **`Text` is the one iterable scalar**, and the `Char` it yields is the
    /// one `t[i]` answers (§4.13, ADR-086).
    ///
    /// The item type is the whole assertion: a `Text` that yielded `Text` would
    /// typecheck every loop body a `Char` does not, and the `for` would then
    /// lower `praxis_text_get`'s `Char` into a slot typed `Text` — the shape
    /// REP-03's silent half was made of.
    #[test]
    fn text_yields_char() {
        let mut db = TypeDb::new();
        let text = db.text();
        let item = iter_item(&mut db, text).expect("Text is iterable");
        assert!(
            matches!(db.data(db.follow(item)), TypeData::Scalar(ScalarType::Char)),
            "a Text yields Char, got {}",
            db.render(item)
        );
        // …and it is not itself, which is what would make `for c in t` accept a
        // body that treats `c` as a `Text`.
        assert!(!is_text(&db, item));
    }

    #[test]
    fn the_other_scalars_and_functions_are_not_iterable() {
        let mut db = TypeDb::new();
        let (int, unit) = (db.int(), db.unit());
        assert!(iter_item(&mut db, int).is_none());
        assert!(iter_item(&mut db, unit).is_none());
        // A `Char` is not iterable: it is what iterating a `Text` *produces*, so
        // making it iterable in turn would have no bottom.
        let ch = db.scalar(ScalarType::Char);
        assert!(iter_item(&mut db, ch).is_none());
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

    /// **Rewritten** from `numeric_scalars_are_orderable` (plan §8.2, H18's last
    /// entry). The assertions it made are still true — `Int`, `Text` and `Char`
    /// are all orderable, and ADR-045 is what made that honest by giving the
    /// runtime a `compare` for each. What changed is that its *name* is now a
    /// claim about two different capabilities: this stage introduces
    /// `CapKind::Numeric`, and "numeric" and "orderable" are exactly the two
    /// things TY-31 and TY-32 separate. `Text` is orderable and is not a number.
    ///
    /// So it states the rule instead: the orderable set is the six scalars whose
    /// descriptors carry a `compare` callback, and it is neither a subset nor a
    /// superset of the numeric one.
    #[test]
    fn orderable_and_numeric_are_different_sets_of_scalars() {
        let mut db = TypeDb::new();
        let (i, t, c, f, b) = (db.int(), db.text(), db.char(), db.float(), db.bool());

        // Orderable: every scalar with a runtime `compare`.
        for ty in [i, t, c, f] {
            assert!(supports_ord(&db, ty), "{} is orderable", db.render(ty));
        }
        assert!(!supports_ord(&db, b), "Bool declares no order (ADR-045)");

        // Numeric: the arithmetic scalars, which is a *different* set.
        assert!(supports_numeric(&db, i));
        assert!(supports_numeric(&db, f));
        assert!(
            !supports_numeric(&db, t),
            "Text is orderable and is not a number — the distinction the old \
             name denied"
        );
        assert!(
            !supports_numeric(&db, c),
            "a Char is a scalar value, not an arithmetic one"
        );
        assert!(!supports_numeric(&db, b));
    }

    // --- D4: a key is hashable AND immutable --------------------------------

    /// TY-32 stated as the distinction it is. `supports_hash` was *literally*
    /// `supports_eq`, so every collection that could be hashed was admitted as a
    /// key — and hashing a `Vec` by its contents and then pushing to it moves
    /// the entry's bucket without moving the entry. D4 confirms the rejection.
    #[test]
    fn a_mutable_collection_is_hashable_but_is_not_a_key() {
        let mut db = TypeDb::new();
        let int = db.int();
        for ctor in [
            CollectionCtor::Vec,
            CollectionCtor::Set,
            CollectionCtor::Deque,
            CollectionCtor::Grid,
            CollectionCtor::Counter,
            CollectionCtor::MinHeap,
            CollectionCtor::MaxHeap,
        ] {
            let coll = db.unary_collection(ctor, int);
            assert!(
                supports_hash(&db, coll),
                "{ctor:?} of Int can be hashed — that was never the problem"
            );
            assert!(
                !supports_hash_stable(&db, coll),
                "{ctor:?} can change after it is stored, so it cannot be found again"
            );
        }
        // A Map and a BitSet are the same rule at their own arities.
        let map = db.map(int, int);
        assert!(!supports_hash_stable(&db, map));
        let bitset = db
            .collection(
                CollectionCtor::BitSet,
                praxis_types::CollectionArgs::Nullary,
            )
            .expect("BitSet is nullary");
        assert!(!supports_hash_stable(&db, bitset));
    }

    /// …and the rule is mutability, not container-ness. A tuple or record is a
    /// key iff every component is — Python's `tuple` rule, which is already how
    /// `supports_eq` recurses.
    #[test]
    fn a_tuple_or_record_is_a_key_exactly_when_its_components_are() {
        let mut db = TypeDb::new();
        let (a, b) = (db.int(), db.text());
        let plain = db.pair(a, b);
        assert!(supports_hash_stable(&db, plain), "(Int, Text) is a key");

        let (c, int_el) = (db.int(), db.int());
        let vec_of_int = db.vec(int_el);
        let with_a_vec = db.pair(c, vec_of_int);
        assert!(
            !supports_hash_stable(&db, with_a_vec),
            "one mutable component is enough to disqualify the whole tuple"
        );

        let (fx, fy) = (db.int(), db.int());
        let fields = praxis_types::FieldSet::from_pairs(vec![("x".into(), fx), ("y".into(), fy)])
            .expect("distinct field names");
        let point = db.record(Some("P".into()), fields);
        assert!(
            supports_hash_stable(&db, point),
            "a record of Ints is a key"
        );

        // A scalar, Text and Unit are keys; a function never is.
        let (i, t, u) = (db.int(), db.text(), db.unit());
        for ty in [i, t, u] {
            assert!(supports_hash_stable(&db, ty));
        }
        let (p, r) = (db.int(), db.int());
        let func = db.func(vec![p], r);
        assert!(!supports_hash_stable(&db, func));
    }

    // --- the one decision function (F10) ------------------------------------

    /// `check` is the only route to a capability answer, and it routes each of
    /// the five kinds to its own predicate. A kind wired to the wrong predicate
    /// is what `supports_hash = supports_eq` was.
    #[test]
    fn check_answers_each_capability_with_its_own_rule() {
        let mut db = TypeDb::new();
        let catalog = praxis_stdlib::builtin_catalog();
        let int = db.int();
        let vec_of_int = db.vec(int);
        let (p, r) = (db.int(), db.int());
        let func = db.func(vec![p], r);
        let text = db.text();

        // Int has all five.
        for kind in CapKind::ALL {
            assert!(
                check(&mut db, &catalog, int, &Capability::Kind(*kind)).is_ok(),
                "Int has {kind:?}"
            );
        }
        // A Vec is equatable and hashable, is not a key, is not ordered, is not
        // a number — five different answers about one type.
        assert!(check(
            &mut db,
            &catalog,
            vec_of_int,
            &Capability::Kind(CapKind::Eq)
        )
        .is_ok());
        assert!(check(
            &mut db,
            &catalog,
            vec_of_int,
            &Capability::Kind(CapKind::Hash)
        )
        .is_ok());
        assert!(check(
            &mut db,
            &catalog,
            vec_of_int,
            &Capability::Kind(CapKind::HashStable)
        )
        .is_err());
        assert!(check(
            &mut db,
            &catalog,
            vec_of_int,
            &Capability::Kind(CapKind::Ord)
        )
        .is_err());
        assert!(check(
            &mut db,
            &catalog,
            vec_of_int,
            &Capability::Kind(CapKind::Numeric)
        )
        .is_err());
        // A function has none of them.
        for kind in CapKind::ALL {
            assert!(
                check(&mut db, &catalog, func, &Capability::Kind(*kind)).is_err(),
                "a function value has no {kind:?}"
            );
        }
        // Text is ordered and is not a number.
        assert!(check(&mut db, &catalog, text, &Capability::Kind(CapKind::Ord)).is_ok());
        assert!(check(&mut db, &catalog, text, &Capability::Kind(CapKind::Numeric)).is_err());
    }

    /// The failure names the *component*, not the container. "a function value
    /// cannot be compared" is a message about what the program wrote; "`Vec[(Int,
    /// fn(Int) -> Int)]` cannot be compared" is a message about a type it never
    /// spelled.
    #[test]
    fn a_failure_names_the_component_that_failed() {
        let mut db = TypeDb::new();
        let catalog = praxis_stdlib::builtin_catalog();
        let (p, r) = (db.int(), db.int());
        let func = db.func(vec![p], r);
        let int = db.int();
        let inner = db.pair(int, func);
        let outer = db.vec(inner);

        let offender = check(&mut db, &catalog, outer, &Capability::Kind(CapKind::Eq))
            .expect_err("a function inside is not equatable");
        assert_eq!(
            db.follow(offender),
            db.follow(func),
            "the function, two levels down, not the Vec"
        );

        // …and when the container itself is the problem, it is the answer.
        let vec_of_int = db.vec(int);
        let offender = check(
            &mut db,
            &catalog,
            vec_of_int,
            &Capability::Kind(CapKind::HashStable),
        )
        .expect_err("a Vec is not a key");
        assert_eq!(db.follow(offender), db.follow(vec_of_int));
    }

    /// An unresolved variable answers yes to everything. That is right here —
    /// the question is about a specific type and a variable is not one yet — and
    /// it is why the constraint channel exists: deferring is its job, not this
    /// function's.
    #[test]
    fn an_unresolved_variable_satisfies_every_capability() {
        let mut db = TypeDb::new();
        let catalog = praxis_stdlib::builtin_catalog();
        let v = db.fresh_var();
        for kind in CapKind::ALL {
            assert!(check(&mut db, &catalog, v, &Capability::Kind(*kind)).is_ok());
        }
        assert!(check(&mut db, &catalog, v, &Capability::Iterable { item: v }).is_ok());
        assert!(check(
            &mut db,
            &catalog,
            v,
            &Capability::HasMethod {
                name: "len".into(),
                params: vec![],
                result: v,
            }
        )
        .is_ok());
    }

    /// `HasMethod` is the catalog's question, asked through the same door.
    /// `Vec[Int]` has `len`; an `Int` does not, and neither does a `Vec` asked
    /// for a name nothing declares.
    #[test]
    fn has_method_is_answered_by_the_catalog() {
        let mut db = TypeDb::new();
        let catalog = praxis_stdlib::builtin_catalog();
        let int = db.int();
        let vec_of_int = db.vec(int);
        let result = db.fresh_var();
        let len = Capability::HasMethod {
            name: "len".into(),
            params: vec![],
            result,
        };
        assert!(check(&mut db, &catalog, vec_of_int, &len).is_ok());
        assert!(check(&mut db, &catalog, int, &len).is_err());

        let nope = Capability::HasMethod {
            name: "no_such_method".into(),
            params: vec![],
            result,
        };
        assert!(check(&mut db, &catalog, vec_of_int, &nope).is_err());
    }

    /// `Iterable` routes to `iter_item`, which is the one answer about what a
    /// `for` may range over.
    #[test]
    fn iterable_is_answered_by_iter_item() {
        let mut db = TypeDb::new();
        let catalog = praxis_stdlib::builtin_catalog();
        let int = db.int();
        let vec_of_int = db.vec(int);
        let item = db.fresh_var();
        assert!(check(
            &mut db,
            &catalog,
            vec_of_int,
            &Capability::Iterable { item }
        )
        .is_ok());
        assert!(check(&mut db, &catalog, int, &Capability::Iterable { item }).is_err());
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

    /// The negative gate for ADR-138 decision 3. Every type a `Map` key can be
    /// now carries a runtime `compare` — a tuple, a `Bool`, a `Range` — because
    /// a container has to walk its keys in one deterministic sequence. That is
    /// **not** the same question as `<`, and the temptation to "fix the
    /// inconsistency" by widening this function is exactly what this test is
    /// here to refuse: doing so legalises `(1, 2) < (1, 3)`, which ADR-045
    /// decision 1 rejected on the grounds that nobody had picked a semantics
    /// for it. A key's container order is a rendering and iteration detail; `<`
    /// is a language feature.
    #[test]
    fn supports_ord_is_the_source_order_and_not_the_container_order() {
        let mut db = TypeDb::new();
        let (a, b) = (db.int(), db.int());
        let tup = db.pair(a, b);
        let bool_ty = db.bool();
        let unit = db.unit();

        // All four are legal `Map` keys, so all four have a container order.
        assert!(supports_hash_stable(&db, tup));
        assert!(supports_hash_stable(&db, bool_ty));
        assert!(supports_hash_stable(&db, unit));

        // None of them is `<`-comparable, and none of them may become so.
        assert!(!supports_ord(&db, tup), "(1, 2) < (1, 3) stays Y006");
        assert!(!supports_ord(&db, bool_ty));
        assert!(!supports_ord(&db, unit));
    }
}
