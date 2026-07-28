//! Bridge from the inference type system to the stdlib method catalog (§5.7,
//! §16.2, rule 20.3).
//!
//! Method *dispatch* (resolving `vec.push(x)` to a catalog entry at a call site)
//! needs collections, which land in M5 — so the full lookup is deferred. For M2
//! this module provides the [`Type → TypePattern`](type_to_pattern) bridge and a
//! [`lookup`] helper, exercised at the library level so the integration with the
//! existing `MethodCatalog` is proven and ready for M5 to drive from the
//! expression layer.

use praxis_stdlib::type_pattern::ScalarType as PatternScalar;
use praxis_stdlib::{MethodCatalog, MethodEntry, TypePattern};
use praxis_types::{data::TypeData, Type, TypeDb};

/// Convert an inferred [`Type`] into the catalog's [`TypePattern`] shape language.
/// Returns `None` for shapes the catalog does not model yet (type variables,
/// functions, unit, tuples). This is the single point where the two type
/// vocabularies meet (rule 20.3): everything else reads one or the other.
#[must_use]
pub fn type_to_pattern(db: &TypeDb, t: Type) -> Option<TypePattern> {
    match db.data(db.follow(t)) {
        TypeData::Scalar(s) => Some(TypePattern::Scalar(map_scalar(*s))),
        // Tuples bridge element-wise (so grid methods' `(Int, Int)` points match
        // the catalog's `Tuple[Int, Int]` param/result patterns).
        TypeData::Tuple(els) => Some(TypePattern::Tuple(
            els.iter()
                .map(|e| type_to_pattern(db, *e).unwrap_or(TypePattern::Var("T")))
                .collect(),
        )),
        TypeData::Func { .. } => None, // function-as-receiver not in catalog
        TypeData::Unit => Some(TypePattern::Unit),
        TypeData::Collection { ctor, args } => {
            // Bridge each type arg to a pattern (recursing); unresolved element
            // vars become `TypePattern::Var("T")` so the catalog's `Vec[T]`
            // entry matches. A concrete `Vec[Int]` maps element-wise.
            let pat_args: Vec<TypePattern> = args
                .iter()
                .map(|a| type_to_pattern(db, *a).unwrap_or(TypePattern::Var("T")))
                .collect();
            Some(TypePattern::Collection {
                ctor: *ctor,
                args: pat_args,
            })
        }
        TypeData::Var(_) => None, // an unresolved var cannot select a method
        // Records and enums carry no catalog method entries today (M7); they
        // cannot select methods. Their field access is lowered directly, not
        // through the catalog.
        TypeData::Record { .. } | TypeData::Enum { .. } => None,
    }
}

/// Look up catalog entries matching `receiver`/`name`/`arity`. Returns the
/// matching entries (usually zero or one, since the catalog rejects duplicate
/// `(receiver, name, arity)` triples). Returns empty if the receiver type is not
/// yet catalog-representable (e.g. a type variable).
///
/// We iterate the catalog's entries directly and compare each receiver against
/// the bridge pattern, rather than using `by_receiver_and_name`, because the
/// constructed `TypePattern` is a local whose lifetime cannot satisfy the
/// catalog iterator's borrow.
pub fn lookup<'a>(
    db: &TypeDb,
    catalog: &'a MethodCatalog,
    receiver: Type,
    name: &str,
    arity: usize,
) -> Vec<&'a MethodEntry> {
    let Some(pattern) = type_to_pattern(db, receiver) else {
        return Vec::new();
    };
    catalog
        .entries()
        .iter()
        .filter(|e| e.name == name && e.arity() == arity && pattern_matches(&e.receiver, &pattern))
        .collect()
}

/// Whether a catalog receiver pattern accepts a concrete runtime pattern.
///
/// `Var("T")` in the catalog entry is a type-variable wildcard: it matches any
/// concrete element (so `Vec[T].len()` matches `Vec[Int].len()`). All other
/// variants require exact equality.
fn pattern_matches(catalog_pat: &TypePattern, concrete_pat: &TypePattern) -> bool {
    match (catalog_pat, concrete_pat) {
        (TypePattern::Var(_), _) => true,
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
        _ => catalog_pat == concrete_pat,
    }
}

fn map_scalar(s: praxis_types::ScalarType) -> PatternScalar {
    match s {
        praxis_types::ScalarType::Bool => PatternScalar::Bool,
        praxis_types::ScalarType::Int => PatternScalar::Int,
        praxis_types::ScalarType::UInt => PatternScalar::UInt,
        praxis_types::ScalarType::Float => PatternScalar::Float,
        praxis_types::ScalarType::Byte => PatternScalar::Byte,
        praxis_types::ScalarType::Char => PatternScalar::Char,
        praxis_types::ScalarType::Text => PatternScalar::Text,
        praxis_types::ScalarType::Never => PatternScalar::Never,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_stdlib::type_pattern::{CollectionCtor, ScalarType};
    use praxis_stdlib::{MethodLowering, Purity, Stability};

    fn vec_push_catalog() -> MethodCatalog {
        MethodCatalog::build()
            .entry(MethodEntry {
                receiver: TypePattern::Collection {
                    ctor: CollectionCtor::Vec,
                    args: vec![TypePattern::Var("T")],
                },
                name: "push",
                params: vec![TypePattern::Var("T")],
                result: TypePattern::Unit,
                purity: Purity::Impure,
                lowering: MethodLowering::RuntimeSymbol(praxis_stdlib::abi::RuntimeSymbol::VecPush),
                doc: "Append a value.",
                stability: Stability::Stable,
            })
            .finish()
            .expect("distinct entries")
    }

    #[test]
    fn scalar_type_bridges_to_pattern() {
        let mut db = TypeDb::new();
        let int = db.int();
        assert_eq!(
            type_to_pattern(&db, int),
            Some(TypePattern::Scalar(ScalarType::Int))
        );
        let unit = db.unit();
        assert_eq!(type_to_pattern(&db, unit), Some(TypePattern::Unit));
    }

    #[test]
    fn type_variable_does_not_select_a_method() {
        let mut db = TypeDb::new();
        let v = db.fresh_var();
        assert!(type_to_pattern(&db, v).is_none());
    }

    #[test]
    fn lookup_returns_empty_for_scalar_receiver() {
        // The catalog's receiver pattern is Vec[T]; our bridge maps a concrete
        // Vec[Int] only once collections exist in TypeDb (M5). For now, verify
        // the lookup returns empty for a scalar receiver (no push on Int).
        let mut db = TypeDb::new();
        let int = db.int();
        let cat = vec_push_catalog();
        let hits = lookup(&db, &cat, int, "push", 1);
        assert!(hits.is_empty(), "Int has no .push");
    }
}
