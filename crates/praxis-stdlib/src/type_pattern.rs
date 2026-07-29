//! Schema-level type patterns used to describe receivers, parameters, and
//! results in the method catalog (§16.2).
//!
//! This is **not** the inference type system — that lives in `praxis-types`
//! (Milestone 2). `TypePattern` is a small, self-describing shape language that
//! is enough to populate the catalog at Milestone 0 and to be unified with the
//! real type representation later. Keeping it separate avoids a dependency from
//! `praxis-stdlib` onto `praxis-types` before the type system exists.

use std::fmt;

/// What a catalog type variable is required to be (TY-31).
///
/// Having no way to state this at all is how `sum` came to accept `Vec[Bool]`
/// *and* `Vec[Float]` — the first a nonsense addition of booleans, the second a
/// silent reinterpretation of float bits as an integer.
///
/// # Why the one shape is not a capability
///
/// The plan expected these bounds to be [`CapKind`](crate::CapKind)s, and the
/// finding is worded that way ("numeric/orderable element types"). Writing them
/// found the opposite: `sum`, `product`, `min` and `max` each lower to an
/// `ExtractScalar` at `ScalarKind::Int` followed by an `IntBinOp` or an `IntCmp`,
/// so `Numeric` — which is `Int`, `UInt`, `Byte` *and* `Float` — would bless
/// `Vec[Float].sum()` and return the float's bits added as an integer. A
/// capability is the wrong *width* for an Int-only lowering.
///
/// The capabilities the catalog would otherwise want are already enforced from
/// the receiver's **type** rather than per row, which is stronger: a `Map` key
/// must be hash-stable and a heap element orderable wherever that collection is
/// built, not only when a particular method is called
/// (`Inferer::require_collection_invariants`, ADR-057 Decision 3).
///
/// So there is one arm. The match on it in `praxis_hir`'s `apply_bounds` is
/// exhaustive, so a capability arm — which would route through the constraint
/// channel rather than through unification, because a capability about an
/// unresolved variable has to be *deferred* — is a compile error to add
/// halfway rather than a silent omission.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bound {
    /// Exactly one scalar, and nothing else. Discharged by **unification**, so a
    /// failure is the ordinary `expected Int, found Bool` reported at the method
    /// name, and an element type nothing has named yet is *pinned* rather than
    /// merely permitted — which is what `v.map(f).sum()` needs.
    Is(ScalarType),
}

/// A pattern describing a type shape in a catalog entry.
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
    /// `bound` is what the variable must satisfy (TY-31). It is a fact about the
    /// *variable*, not about the position it is written in, so an entry declares
    /// it once — at whichever occurrence reads best — and
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
    /// Opaque / unknown during early scaffolding. Catalog entries added before
    /// a full type pattern is worked out can use this placeholder; the type
    /// checker will reject it if it is still present at use time.
    Opaque,
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

    /// A type variable that must satisfy `bound` (TY-31).
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
            TypePattern::Function { params, result } => {
                for p in params {
                    p.collect_bounds(into);
                }
                result.collect_bounds(into);
            }
            TypePattern::Scalar(_) | TypePattern::Unit | TypePattern::Opaque => {}
        }
    }
}

/// Built-in scalar types (§4.3). The full set is reserved here even though the
/// first implementation may omit `UInt` and `Float` until the integer pipeline
/// is stable (§4.3) — their names must not be reused for anything else.
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
/// arguments; the others take one (`Vec`, `Set`, ...) or two (`Map`). `Seq` is
/// the compiler-internal pipeline source (M8 WS8, §6.3): it is never user-named
/// and has no runtime representation, but it threads the element type through
/// lazy pipelines.
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
    /// Compiler-internal lazy sequence (M8 WS8, §6.3). Never appears in source.
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
            TypePattern::Opaque => f.write_str("_"),
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
    }
}
