//! Schema-level type patterns used to describe receivers, parameters, and
//! results in the method catalog (§16.2).
//!
//! This is **not** the inference type system — that lives in `praxis-types`.
//! `TypePattern` is a small, self-describing shape language, enough to populate
//! the catalog and to be unified with the real type representation. Keeping it
//! separate is what keeps `praxis-stdlib` from depending on `praxis-types`.

use std::fmt;

/// What a catalog type variable is required to be.
///
/// Without a way to state this, `sum` would accept `Vec[Bool]` *and*
/// `Vec[Float]` — the first a nonsense addition of booleans, the second a
/// silent reinterpretation of float bits as an integer.
///
/// # Why the scalar shape is not a capability, and why the other one is
///
/// `sum`, `product`, `min` and `max` each lower to an `ExtractScalar` at
/// `ScalarKind::Int` followed by an `IntBinOp` or an `IntCmp`, so
/// [`CapKind`](crate::CapKind)`::Numeric` — which is `Int`, `UInt`, `Byte`
/// *and* `Float` — would bless `Vec[Float].sum()` and return the float's bits
/// added as an integer. A capability is the wrong *width* for an Int-only
/// lowering.
///
/// The capabilities the catalog would otherwise want are already enforced from
/// the receiver's **type** rather than per row, which is stronger: a `Map` key
/// must be hash-stable and a heap element orderable wherever that collection is
/// built, not only when a particular method is called
/// (`Inferer::require_collection_invariants`, ADR-057 Decision 3).
///
/// So the scalar arm is not a capability. The **second** arm is: `sorted`
/// orders its elements through the element descriptor's `compare` callback, and
/// a `Vec[T]` whose `T` is a function value has none. That is `CapKind::Ord`,
/// and it is a fact about the row rather than about the receiver's *type* — a
/// `Vec` is a perfectly good `Vec` of unorderable things right up until someone
/// sorts it — so `require_collection_invariants` is the wrong door for it and
/// the row has to say it itself.
///
/// The match on this enum in `praxis_hir`'s `apply_bounds` is exhaustive, so a
/// third arm is a compile error to add halfway rather than a silent omission.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bound {
    /// Exactly one scalar, and nothing else. Discharged by **unification**, so a
    /// failure is the ordinary `expected Int, found Bool` reported at the method
    /// name, and an element type nothing has named yet is *pinned* rather than
    /// merely permitted — which is what `v.map(f).sum()` needs.
    Is(ScalarType),
    /// One capability, and any type that has it. Discharged through the
    /// **constraint channel**, not by unification: a bound on a variable nothing
    /// has pinned yet cannot be answered, and `fn top(v) { v.sorted() }` is
    /// exactly that shape until a call site says what `v` holds. That is the
    /// whole reason this arm is not spelled as a set of scalars.
    Kind(crate::CapKind),
}

/// A pattern describing a type shape in a catalog entry.
///
/// # There is no placeholder arm
///
/// Every row writes a concrete pattern. A placeholder for rows whose shape is
/// not worked out yet would have to arrive *together with* the rejection that
/// makes it safe: the only thing `pattern_to_type` could instantiate one as is a
/// fresh inference variable, which unifies with anything, so "the type checker
/// rejects it if it is still present at use time" is a promise nothing keeps by
/// default.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TypePattern {
    /// A specific scalar type, e.g. `Int`.
    Scalar(ScalarType),
    /// A built-in collection type constructor applied to element type(s), e.g.
    /// `Vec[Int]` or `Map[Text, Int]`.
    Collection {
        ctor: CollectionCtor,
        /// Element type parameters. Length must match the constructor's arity.
        args: Vec<TypePattern>,
    },
    /// A type variable used inside a generic method's signature, e.g. `T` in
    /// `Vec[T].push(T)`. Two occurrences of the *same* variable name inside one
    /// entry refer to the same type; that equality is what the type checker
    /// enforces at a call site.
    ///
    /// `bound` is what the variable must satisfy. It is a fact about the
    /// *variable*, not about the position it is written in, so an entry
    /// declares it once — at whichever occurrence reads best — and
    /// [`MethodEntry::bounds`](crate::MethodEntry::bounds) finds it wherever it
    /// is. Declaring two different bounds for one name in one entry is a catalog
    /// authoring mistake and [`MethodCatalog::build`](crate::MethodCatalog::build)
    /// refuses it.
    Var {
        name: &'static str,
        bound: Option<Bound>,
    },
    /// The function type `(params) -> result`. Used for higher-order methods
    /// like `Vec[T].map`.
    Function {
        params: Vec<TypePattern>,
        result: Box<TypePattern>,
    },
    /// The unit type, used for methods like `Vec[T].push` that return nothing.
    Unit,
    /// A tuple `(T, U, ...)`. Used by grid methods that return/accept `(x, y)`
    /// points (§6.4). Structural identity is the element-type sequence.
    Tuple(Vec<TypePattern>),
    /// `Option[T]` — the prelude enum, applied to one argument (§4.7, F12).
    ///
    /// Its own arm rather than a `Collection` ctor because `Option` is not a
    /// collection: it is the one *generic enum def* the language has, and a
    /// catalog row spelling it has to lower to `TypeDb::option_of`, which names
    /// the single canonical def every `Option[T]` in a program shares.
    ///
    /// §4.7: "Option[T] represents normal domain-level absence. It is not an
    /// error channel." `Map.get` and `Grid.find` are the rows that need it: a
    /// miss is an absent value, not a `V` or an `(Int, Int)` standing in for
    /// one.
    Option(Box<TypePattern>),
    /// A receiver the pipeline walks: any of the ten iterables named by
    /// [`is_pipeline_receiver`], binding what it yields to `item` (ADR-127).
    ///
    /// # It is the one pattern that is not unified with the receiver
    ///
    /// Everywhere else a catalog receiver is instantiated and unified with the
    /// actual receiver — that is what pins `T` in `Vec[T].push(T)`. This one
    /// cannot be: it accepts ten different constructors, and unifying against
    /// any one of them pins the other nine out. What is unified is the **item**,
    /// against `capability::iter_item`'s answer for the receiver — the `for`
    /// loop's own answer to "what does this yield".
    ///
    /// One consequence is load-bearing and Decision 4 uses it: a row constrains
    /// *which* iterables it accepts by writing a shape into `item`.
    /// `Iterable { item: Tuple[K, V] }` is "a `Map` or a `Counter`", because
    /// those are the two whose item is a pair — and `[1, 2].to_map()` is an
    /// ordinary unification failure at the method name, not a row that resolves
    /// and then faults.
    Iterable { item: Box<TypePattern> },
}

/// The collection constructors a [`TypePattern::Iterable`] receiver accepts
/// (ADR-127 decision 1) — the `for` loop's list minus `Grid` and `Seq`.
///
/// **`Grid[T]` is excluded, and `grid.map` is why.** §6.4 requires `grid.map(fn)`
/// and it means the shape-preserving one, `Grid[T] -> Grid[U]`, cells in place. A
/// generic row would claim the name and answer `Vec[U]` instead. A grid enters a
/// pipeline through `grid.cells()` or `grid.positions()`, which already answer
/// `Vec`s. The exclusion is enforced rather than intended:
/// [`MethodCatalogBuilder::finish`](crate::catalog::MethodCatalogBuilder::finish)
/// refuses a concrete row that shares a `(name, arity)` with a generic one *on a
/// receiver in this list*, so a future `Grid[T].map/1` is allowed and a
/// `Set[T].map/1` is a build failure.
///
/// **`Seq[T]` is excluded because it has no values.** `praxis-repr` says a `Seq`
/// has no runtime representation, and nothing produces or consumes one
/// (ADR-127).
///
/// `Text` is the tenth receiver and is not here, because it is not a collection:
/// it is the one *scalar* with members (§4.13). [`is_pipeline_receiver`] is the
/// predicate that answers for all ten.
pub const PIPELINE_RECEIVERS: &[CollectionCtor] = &[
    CollectionCtor::Vec,
    CollectionCtor::Deque,
    CollectionCtor::Set,
    CollectionCtor::MinHeap,
    CollectionCtor::MaxHeap,
    CollectionCtor::Range,
    CollectionCtor::BitSet,
    CollectionCtor::Map,
    CollectionCtor::Counter,
];

/// Whether a *concrete* receiver pattern is one of the ten a
/// [`TypePattern::Iterable`] row accepts (ADR-127 decision 1).
///
/// A pure pattern-level test — ctor membership in [`PIPELINE_RECEIVERS`], or the
/// `Text` scalar — so it needs no `TypeDb` and both callers can ask it from
/// inside an immutable borrow. It deliberately says nothing about the row's
/// `item`: a row whose item shape excludes this receiver still *matches*, and
/// the item unification is what reports.
#[must_use]
pub fn is_pipeline_receiver(concrete: &TypePattern) -> bool {
    match concrete {
        TypePattern::Collection { ctor, .. } => PIPELINE_RECEIVERS.contains(ctor),
        TypePattern::Scalar(ScalarType::Text) => true,
        _ => false,
    }
}

/// Whether a catalog receiver pattern accepts a concrete runtime pattern.
///
/// `Var("T")` in the catalog entry is a type-variable wildcard: it matches any
/// concrete element (so `Vec[T].len()` matches `Vec[Int].len()`). A
/// [`TypePattern::Iterable`] receiver matches any of the ten
/// [`PIPELINE_RECEIVERS`]. All other variants require exact equality.
///
/// **This lives here because two callers ask the same question.**
/// `praxis_hir::catalog::lookup` decides dispatch and
/// `praxis_lsp::completion::dot_items` decides what `set.` offers. If the two
/// disagree the editor offers a method the compiler refuses; one function is
/// what makes that unrepresentable rather than merely unlikely.
#[must_use]
pub fn pattern_matches(catalog_pat: &TypePattern, concrete_pat: &TypePattern) -> bool {
    match (catalog_pat, concrete_pat) {
        (TypePattern::Var { .. }, _) => true,
        // The generic pipeline receiver (ADR-127). Note what is *not* consulted:
        // the row's `item`. `Iterable { item: (K, V) }` matches a `Set[Int]`
        // here, and the item unification `bind_receiver` performs is what
        // reports "expected `(K, V)`, found `Int`" at the method name.
        (TypePattern::Iterable { .. }, concrete) => is_pipeline_receiver(concrete),
        (
            TypePattern::Collection { ctor: c1, args: a1 },
            TypePattern::Collection { ctor: c2, args: a2 },
        ) => {
            c1 == c2
                && a1.len() == a2.len()
                && a1.iter().zip(a2).all(|(x, y)| pattern_matches(x, y))
        }
        // Tuples match element-wise (so a catalog `Tuple[Int, Int]` point
        // pattern matches a concrete `(Int, Int)`).
        (TypePattern::Tuple(a1), TypePattern::Tuple(a2)) => {
            a1.len() == a2.len() && a1.iter().zip(a2).all(|(x, y)| pattern_matches(x, y))
        }
        // `Option[T]` matches through its argument, for the same reason a
        // collection does.
        (TypePattern::Option(a), TypePattern::Option(b)) => pattern_matches(a, b),
        _ => catalog_pat == concrete_pat,
    }
}

impl TypePattern {
    /// An unconstrained type variable — `T` in `Vec[T].push(T)`.
    ///
    /// The overwhelmingly common case, and the reason [`TypePattern::Var`] is a
    /// struct variant rather than a second enum arm: a bound is an optional fact
    /// about a variable, so there is one kind of variable and not two.
    #[must_use]
    pub const fn var(name: &'static str) -> TypePattern {
        TypePattern::Var { name, bound: None }
    }

    /// A type variable that must satisfy `bound`.
    #[must_use]
    pub const fn bounded(name: &'static str, bound: Bound) -> TypePattern {
        TypePattern::Var {
            name,
            bound: Some(bound),
        }
    }

    /// A type variable required to be exactly `scalar` — the Int-only sinks.
    #[must_use]
    pub const fn is_scalar(name: &'static str, scalar: ScalarType) -> TypePattern {
        TypePattern::bounded(name, Bound::Is(scalar))
    }

    /// The pipeline receiver yielding `item` — `Iterable { item }`, spelled
    /// without the `Box` every row would otherwise write (ADR-127).
    #[must_use]
    pub fn iterable(item: TypePattern) -> TypePattern {
        TypePattern::Iterable {
            item: Box::new(item),
        }
    }

    /// A type variable required to have `kind` — the barrier combinators, whose
    /// runtime wrappers read a descriptor callback the element may not have
    /// (`sorted` needs `compare`, `frequencies` and `unique` need a key that
    /// stays findable after it is stored).
    #[must_use]
    pub const fn of_kind(name: &'static str, kind: crate::CapKind) -> TypePattern {
        TypePattern::bounded(name, Bound::Kind(kind))
    }

    /// Append every `(name, bound)` this pattern declares, recursing into
    /// composites. Order is source order, which is what makes a duplicate
    /// declaration reportable at the first occurrence.
    pub(crate) fn collect_bounds(&self, into: &mut Vec<(&'static str, Bound)>) {
        match self {
            TypePattern::Var { name, bound } => {
                if let Some(b) = bound {
                    into.push((name, *b));
                }
            }
            TypePattern::Collection { args, .. } | TypePattern::Tuple(args) => {
                for a in args {
                    a.collect_bounds(into);
                }
            }
            // A bound on the pipeline receiver's item is the row's own — `sum`'s
            // `Bound::Is(Int)` lives here — so the sweep has to reach it. Its
            // load-bearing half is that an item type nothing has pinned yet is
            // *pinned* to `Int` rather than merely permitted.
            TypePattern::Iterable { item } => item.collect_bounds(into),
            TypePattern::Option(inner) => inner.collect_bounds(into),
            TypePattern::Function { params, result } => {
                for p in params {
                    p.collect_bounds(into);
                }
                result.collect_bounds(into);
            }
            TypePattern::Scalar(_) | TypePattern::Unit => {}
        }
    }
}

/// Built-in scalar types (§4.3). The full set is named here even though `UInt`
/// has no runtime object of its own (§7.4: its type is `Int`) — these names
/// must not be reused for anything else.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ScalarType {
    Bool,
    Int,
    UInt,
    Float,
    Byte,
    Char,
    Text,
}

/// Built-in collection constructors (§6.1). `Range` and `BitSet` take no type
/// arguments; the others take one (`Vec`, `Set`, ...) or two (`Map`).
///
/// **`Seq` has no rows and no values.** It is the compiler-internal pipeline
/// source (§6.3), threading an element type through what a lazy chain would
/// need; the pipeline is eager (ADR-028 decision 2), so no row answers one.
/// Nothing produces a `Seq`, nothing consumes one, and retiring the constructor
/// itself is a mechanical follow-up rather than a decision.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CollectionCtor {
    Vec,
    Deque,
    Map,
    Set,
    Counter,
    MinHeap,
    MaxHeap,
    BitSet,
    Grid,
    Range,
    /// Compiler-internal lazy sequence (§6.3). Never appears in source.
    Seq,
}

impl CollectionCtor {
    /// The number of element type parameters this constructor takes.
    pub fn arity(self) -> usize {
        match self {
            CollectionCtor::Map => 2,
            // `BitSet` and `Range` are nullary in user syntax; the rest take one
            // element type.
            CollectionCtor::BitSet | CollectionCtor::Range => 0,
            _ => 1,
        }
    }

    /// The constructor a source name denotes, or `None` for any other name.
    ///
    /// The inverse of [`name`](Self::name), and the one authority for the
    /// mapping: HIR resolves a constructor call through it and MIR picks the
    /// allocation's ctor through it, so the two cannot come to disagree about
    /// which names construct a collection. `Seq` is deliberately absent — it is
    /// compiler-internal and no source name reaches it (§6.3).
    #[must_use]
    pub fn from_name(name: &str) -> Option<CollectionCtor> {
        Some(match name {
            "Vec" => CollectionCtor::Vec,
            "Deque" => CollectionCtor::Deque,
            "Map" => CollectionCtor::Map,
            "Set" => CollectionCtor::Set,
            "Counter" => CollectionCtor::Counter,
            "MinHeap" => CollectionCtor::MinHeap,
            "MaxHeap" => CollectionCtor::MaxHeap,
            "BitSet" => CollectionCtor::BitSet,
            "Grid" => CollectionCtor::Grid,
            "Range" => CollectionCtor::Range,
            _ => return None,
        })
    }

    /// The user-facing name of this collection constructor, e.g. `Vec`. `Seq`
    /// is internal and has no user-facing name; `name()` returns `"Seq"` only
    /// for diagnostics/debugging.
    pub fn name(self) -> &'static str {
        match self {
            CollectionCtor::Vec => "Vec",
            CollectionCtor::Deque => "Deque",
            CollectionCtor::Map => "Map",
            CollectionCtor::Set => "Set",
            CollectionCtor::Counter => "Counter",
            CollectionCtor::MinHeap => "MinHeap",
            CollectionCtor::MaxHeap => "MaxHeap",
            CollectionCtor::BitSet => "BitSet",
            CollectionCtor::Grid => "Grid",
            CollectionCtor::Range => "Range",
            CollectionCtor::Seq => "Seq",
        }
    }
}

impl ScalarType {
    pub fn name(self) -> &'static str {
        match self {
            ScalarType::Bool => "Bool",
            ScalarType::Int => "Int",
            ScalarType::UInt => "UInt",
            ScalarType::Float => "Float",
            ScalarType::Byte => "Byte",
            ScalarType::Char => "Char",
            ScalarType::Text => "Text",
        }
    }
}

impl fmt::Display for TypePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypePattern::Scalar(s) => f.write_str(s.name()),
            TypePattern::Unit => f.write_str("Unit"),
            TypePattern::Tuple(els) => {
                f.write_str("(")?;
                for (i, e) in els.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{e}")?;
                }
                f.write_str(")")
            }
            TypePattern::Option(inner) => write!(f, "Option[{inner}]"),
            // Not a type a user can write — no annotation names it — but the
            // completion table renders every receiver, and "the thing a `for`
            // walks" is what this says.
            TypePattern::Iterable { item } => write!(f, "Iterable[{item}]"),
            // The bound is not part of the type's spelling: it is a rule the
            // compiler enforces, and §5.4 forbids surfacing capability names to
            // the user. Completion and signature help show `T`.
            TypePattern::Var { name, .. } => write!(f, "{name}"),
            TypePattern::Collection { ctor, args } => {
                write!(f, "{ctor:?}")?;
                if !args.is_empty() {
                    f.write_str("[")?;
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{a}")?;
                    }
                    f.write_str("]")?;
                }
                Ok(())
            }
            TypePattern::Function { params, result } => {
                f.write_str("(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{p}")?;
                }
                write!(f, ") -> {result}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_arity_matches_design() {
        assert_eq!(CollectionCtor::Vec.arity(), 1);
        assert_eq!(CollectionCtor::Map.arity(), 2);
        assert_eq!(CollectionCtor::Set.arity(), 1);
        assert_eq!(CollectionCtor::BitSet.arity(), 0);
        assert_eq!(CollectionCtor::Range.arity(), 0);
        assert_eq!(CollectionCtor::Grid.arity(), 1);
    }

    #[test]
    fn scalar_names_match_user_syntax() {
        assert_eq!(ScalarType::Int.name(), "Int");
        assert_eq!(ScalarType::Text.name(), "Text");
    }

    #[test]
    fn pattern_display_matches_design_syntax() {
        assert_eq!(TypePattern::Scalar(ScalarType::Int).to_string(), "Int");
        assert_eq!(
            TypePattern::Collection {
                ctor: CollectionCtor::Vec,
                args: vec![TypePattern::var("T")],
            }
            .to_string(),
            "Vec[T]"
        );
        assert_eq!(
            TypePattern::Collection {
                ctor: CollectionCtor::Map,
                args: vec![
                    TypePattern::Scalar(ScalarType::Text),
                    TypePattern::Scalar(ScalarType::Int)
                ],
            }
            .to_string(),
            "Map[Text, Int]"
        );
        let func = TypePattern::Function {
            params: vec![TypePattern::var("T")],
            result: Box::new(TypePattern::var("U")),
        };
        assert_eq!(func.to_string(), "(T) -> U");
        assert_eq!(
            TypePattern::iterable(TypePattern::var("T")).to_string(),
            "Iterable[T]"
        );
    }

    fn collection(ctor: CollectionCtor, args: Vec<TypePattern>) -> TypePattern {
        TypePattern::Collection { ctor, args }
    }

    /// **ADR-127 decision 1.** The pipeline's receiver list is the `for` loop's
    /// minus two, and each exclusion is a decision rather than an oversight:
    /// `Grid` because §6.4 owes `grid.map` a shape-preserving row, `Seq` because
    /// it has no values.
    #[test]
    fn the_pipeline_walks_ten_receivers_and_not_a_grid() {
        let accepted = [
            collection(CollectionCtor::Vec, vec![TypePattern::var("T")]),
            collection(CollectionCtor::Deque, vec![TypePattern::var("T")]),
            collection(CollectionCtor::Set, vec![TypePattern::var("T")]),
            collection(CollectionCtor::MinHeap, vec![TypePattern::var("T")]),
            collection(CollectionCtor::MaxHeap, vec![TypePattern::var("T")]),
            collection(CollectionCtor::Range, vec![]),
            collection(CollectionCtor::BitSet, vec![]),
            collection(
                CollectionCtor::Map,
                vec![TypePattern::var("K"), TypePattern::var("V")],
            ),
            collection(CollectionCtor::Counter, vec![TypePattern::var("T")]),
            TypePattern::Scalar(ScalarType::Text),
        ];
        assert_eq!(
            accepted.len(),
            PIPELINE_RECEIVERS.len() + 1,
            "`Text` is the tenth receiver and the only one that is not a ctor"
        );
        for pat in &accepted {
            assert!(is_pipeline_receiver(pat), "{pat} is walked by a `for`");
        }

        for refused in [
            collection(CollectionCtor::Grid, vec![TypePattern::var("T")]),
            collection(CollectionCtor::Seq, vec![TypePattern::var("T")]),
            TypePattern::Scalar(ScalarType::Int),
            TypePattern::Tuple(vec![TypePattern::var("K"), TypePattern::var("V")]),
        ] {
            assert!(!is_pipeline_receiver(&refused), "{refused} is not walked");
        }
    }

    /// The `Iterable` arm matches on the *receiver's shape alone*. A row whose
    /// item is a pair still matches a `Set`, and the failure it earns is the
    /// item unification's — an ordinary "expected `(K, V)`, found `Int`" at the
    /// method name, rather than "no method `to_map`", which would be a worse
    /// message for the same mistake.
    #[test]
    fn an_iterable_row_matches_by_receiver_and_reports_by_item() {
        let to_map = TypePattern::iterable(TypePattern::Tuple(vec![
            TypePattern::var("K"),
            TypePattern::var("V"),
        ]));
        let set_of_int = collection(
            CollectionCtor::Set,
            vec![TypePattern::Scalar(ScalarType::Int)],
        );
        assert!(pattern_matches(&to_map, &set_of_int));
        // …and a `Grid` is refused at the door, which is what keeps `grid.map`
        // §6.4's row rather than this one's.
        let grid = collection(
            CollectionCtor::Grid,
            vec![TypePattern::Scalar(ScalarType::Int)],
        );
        assert!(!pattern_matches(&to_map, &grid));
    }

    /// `sum`'s `Int` bound lives on the pipeline receiver's *item*, and there is
    /// nowhere else in the row for it to live — so the sweep has to reach
    /// through the `Iterable` arm.
    #[test]
    fn a_bound_on_the_item_is_found() {
        let mut bounds = Vec::new();
        TypePattern::iterable(TypePattern::is_scalar("T", ScalarType::Int))
            .collect_bounds(&mut bounds);
        assert_eq!(bounds, vec![("T", Bound::Is(ScalarType::Int))]);
    }
}
