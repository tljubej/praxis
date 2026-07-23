//! Schema-level type patterns used to describe receivers, parameters, and
//! results in the method catalog (§16.2).
//!
//! This is **not** the inference type system — that lives in `praxis-types`
//! (Milestone 2). `TypePattern` is a small, self-describing shape language that
//! is enough to populate the catalog at Milestone 0 and to be unified with the
//! real type representation later. Keeping it separate avoids a dependency from
//! `praxis-stdlib` onto `praxis-types` before the type system exists.

use std::fmt;

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
    Var(&'static str),
    /// The function type `(params) -> result`. Used for higher-order methods
    /// like `Vec[T].map`.
    Function {
        params: Vec<TypePattern>,
        result: Box<TypePattern>,
    },
    /// The unit type, used for methods like `Vec[T].push` that return nothing.
    Unit,
    /// Opaque / unknown during early scaffolding. Catalog entries added before
    /// a full type pattern is worked out can use this placeholder; the type
    /// checker will reject it if it is still present at use time.
    Opaque,
}

/// Built-in scalar types (§4.3). The full set is reserved here even though the
/// first implementation may omit `UInt` and `Float` until the integer pipeline
/// is stable (§4.3) — their names must not be reused for anything else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScalarType {
    Bool,
    Int,
    UInt,
    Float,
    Byte,
    Char,
    Text,
    /// The bottom type for diverging control flow (§4.3).
    Never,
}

/// Built-in collection constructors (§6.1). `Range` and `BitSet` take no type
/// arguments; the others take one (`Vec`, `Set`, ...) or two (`Map`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
            ScalarType::Never => "Never",
        }
    }
}

impl fmt::Display for TypePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypePattern::Scalar(s) => f.write_str(s.name()),
            TypePattern::Unit => f.write_str("Unit"),
            TypePattern::Opaque => f.write_str("_"),
            TypePattern::Var(name) => write!(f, "{name}"),
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
        assert_eq!(ScalarType::Never.name(), "Never");
    }

    #[test]
    fn pattern_display_matches_design_syntax() {
        assert_eq!(TypePattern::Scalar(ScalarType::Int).to_string(), "Int");
        assert_eq!(
            TypePattern::Collection {
                ctor: CollectionCtor::Vec,
                args: vec![TypePattern::Var("T")],
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
            params: vec![TypePattern::Var("T")],
            result: Box::new(TypePattern::Var("U")),
        };
        assert_eq!(func.to_string(), "(T) -> U");
    }
}
