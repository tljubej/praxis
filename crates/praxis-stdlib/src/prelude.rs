//! The Praxis prelude (§16.1): symbols automatically available in every
//! program, with no `use` required.
//!
//! Kept as data so the type checker, the LSP completion table, and the
//! documentation generator all read the same single list.

use crate::abi::RuntimeSymbol;

/// The names automatically imported into every Praxis program (§16.1).
///
/// Sorted by category so the LSP can present them sensibly; within a category
/// the order is alphabetical. The list is checked for emptiness and duplicates
/// by the unit test.
pub const PRELUDE: &[PreludeEntry] = &[
    // Output / control
    PreludeEntry::new("out", "Write one value followed by a newline."),
    PreludeEntry::new("dbg", "Print to stderr and return the value."),
    PreludeEntry::new(
        "panic",
        "Stop with an explicit message and enter the crash debugger.",
    ),
    PreludeEntry::new("assert", "Stop if a condition is false."),
    // Numeric helpers
    PreludeEntry::new("abs", "Absolute value of an integer."),
    PreludeEntry::new("sign", "Sign of an integer: -1, 0, or 1."),
    PreludeEntry::new("min", "Smaller of two orderable values."),
    PreludeEntry::new("max", "Larger of two orderable values."),
    PreludeEntry::new("clamp", "Clamp a value into an inclusive range."),
    PreludeEntry::new("gcd", "Greatest common divisor of two integers."),
    PreludeEntry::new("lcm", "Least common multiple of two integers."),
    PreludeEntry::new("pi", "The constant π as a Float."),
    PreludeEntry::new("e", "Euler's number as a Float."),
    // Collections
    PreludeEntry::new("Vec", "Grow, iterate, and pipeline over an ordered list."),
    PreludeEntry::new("Deque", "Double-ended queue."),
    PreludeEntry::new("Map", "Hash map."),
    PreludeEntry::new("Set", "Hash set."),
    PreludeEntry::new("Counter", "Map whose absent values read as zero."),
    PreludeEntry::new(
        "MinHeap",
        "Priority queue yielding the smallest element first.",
    ),
    PreludeEntry::new(
        "MaxHeap",
        "Priority queue yielding the largest element first.",
    ),
    PreludeEntry::new("Grid", "2D grid with rectangular indexing."),
    PreludeEntry::new("BitSet", "Compact set of non-negative integers."),
    // Optionality (M9). Option[T] is a polymorphic enum: Some(T) carries a
    // value, None marks absence. Returned by the `optional(P)` parser and by
    // `find`/`position` on a miss.
    PreludeEntry::new("Option", "Optional value: Some(T) or None."),
    PreludeEntry::variant("Some", "Wrap a value in an Option."),
    PreludeEntry::variant("None", "The absent Option value."),
    // Graph algorithms
    PreludeEntry::new("bfs", "Breadth-first traversal."),
    PreludeEntry::new("bfs_distance", "Breadth-first shortest distance."),
    PreludeEntry::new("dfs", "Depth-first traversal."),
    PreludeEntry::new("dijkstra", "Dijkstra shortest path."),
    PreludeEntry::new("a_star", "A* search."),
    PreludeEntry::new("flood_fill", "Flood fill from a starting cell."),
];

/// The §16.1 numeric prelude helpers: the free functions that are neither
/// output/control names nor collection constructors.
///
/// Every one of them is monomorphic on `Int` (ADR-058), so a row needs only the
/// name and the wrapper it lowers to — the **arity** the type checker gives the
/// name is [`RuntimeSymbol::arity`], read off the F4 manifest rather than
/// restated here. That is what makes "a prelude name whose signature disagrees
/// with the wrapper it calls" unrepresentable; before this the names had no
/// wrapper at all and lowered as calls to functions nobody defined (TY-33).
///
/// `Float`'s counterparts are *methods* (`0.5.abs()`, `x.min(y)` — §4.12), not
/// entries here: a genuinely polymorphic `abs` would have to carry a numeric
/// capability on its own binder and pick a lowering per instantiation, and
/// nothing needs that yet.
///
/// `pi` and `e` are not here either. They are `Float` constants, not `Int`
/// functions, and they already had schemes and dispatch.
pub const NUMERIC_HELPERS: &[NumericHelper] = &[
    NumericHelper::new("abs", RuntimeSymbol::IntAbs),
    NumericHelper::new("sign", RuntimeSymbol::IntSign),
    NumericHelper::new("min", RuntimeSymbol::IntMin),
    NumericHelper::new("max", RuntimeSymbol::IntMax),
    NumericHelper::new("clamp", RuntimeSymbol::IntClamp),
    NumericHelper::new("gcd", RuntimeSymbol::IntGcd),
    NumericHelper::new("lcm", RuntimeSymbol::IntLcm),
];

/// One numeric prelude helper: the source name and the wrapper it lowers to.
#[derive(Clone, Copy, Debug)]
pub struct NumericHelper {
    pub name: &'static str,
    pub symbol: RuntimeSymbol,
}

impl NumericHelper {
    const fn new(name: &'static str, symbol: RuntimeSymbol) -> NumericHelper {
        NumericHelper { name, symbol }
    }

    /// How many `Int` parameters the source-level function takes, which is the
    /// wrapper's arity: every helper takes `Int`s and returns one, so the two
    /// counts are the same number and only the manifest states it.
    #[inline]
    pub const fn arity(&self) -> usize {
        self.symbol.arity()
    }
}

/// The numeric helper `name` denotes, or `None` for any other name.
///
/// The one lookup from a source name to a numeric helper. Both consumers use
/// it — inference for the scheme's arity, MIR for the call target — so neither
/// carries its own list.
pub fn numeric_helper(name: &str) -> Option<NumericHelper> {
    NUMERIC_HELPERS.iter().copied().find(|h| h.name == name)
}

/// One prelude symbol: its name and a one-line description.
#[derive(Clone, Copy, Debug)]
pub struct PreludeEntry {
    pub name: &'static str,
    pub doc: &'static str,
    /// Whether this name is an **enum variant constructor** rather than an
    /// ordinary value. `Some` and `None` are `Option`'s two variants, declared
    /// by the prelude rather than by an `enum` item — and a consumer that has
    /// to tell a constructor from a binding cannot do it from the type
    /// (`let A = None` has `Option`'s type too), so the declaration says
    /// (HIR-03).
    pub is_variant_ctor: bool,
}

impl PreludeEntry {
    pub const fn new(name: &'static str, doc: &'static str) -> PreludeEntry {
        PreludeEntry {
            name,
            doc,
            is_variant_ctor: false,
        }
    }

    /// An entry that constructs an enum variant.
    pub const fn variant(name: &'static str, doc: &'static str) -> PreludeEntry {
        PreludeEntry {
            name,
            doc,
            is_variant_ctor: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn prelude_is_non_empty() {
        assert!(!PRELUDE.is_empty());
    }

    #[test]
    fn prelude_names_are_unique() {
        let mut seen = HashSet::new();
        for e in PRELUDE {
            assert!(seen.insert(e.name), "duplicate prelude name {}", e.name);
        }
    }

    #[test]
    fn prelude_includes_design_canonical_entries() {
        let names: HashSet<_> = PRELUDE.iter().map(|e| e.name).collect();
        for required in ["out", "dbg", "panic", "abs", "Vec", "Map", "bfs_distance"] {
            assert!(
                names.contains(required),
                "missing prelude entry {required:?}"
            );
        }
    }

    /// Every numeric helper is a prelude name, and every name the design's
    /// numeric line lists is a numeric helper. The two lists are written
    /// separately — one by category for the LSP, one by lowering — and a name in
    /// only one of them is either a phantom (TY-33's shape: resolves, then has
    /// nowhere to go) or an unreachable wrapper.
    #[test]
    fn every_numeric_helper_is_a_prelude_name() {
        let prelude: HashSet<_> = PRELUDE.iter().map(|e| e.name).collect();
        for h in NUMERIC_HELPERS {
            assert!(prelude.contains(h.name), "{} is not in PRELUDE", h.name);
        }
        // §16.1's numeric line, verbatim. `pi`/`e` are on it in `PRELUDE` but
        // are Float constants with their own dispatch, not Int functions.
        for required in ["abs", "sign", "min", "max", "clamp", "gcd", "lcm"] {
            assert!(
                numeric_helper(required).is_some(),
                "§16.1 lists {required:?} and it has no helper row"
            );
        }
        assert!(numeric_helper("pi").is_none());
        assert!(numeric_helper("out").is_none());
        assert!(numeric_helper("bfs").is_none());
    }

    /// A helper's source arity is its wrapper's arity, because the row does not
    /// state one. This is the property the row's shape buys: `min(a)` cannot
    /// typecheck against a two-operand wrapper, and `clamp(v, lo)` cannot
    /// either, without anyone maintaining a second number.
    #[test]
    fn a_helpers_arity_is_the_wrappers_arity() {
        assert_eq!(numeric_helper("abs").unwrap().arity(), 1);
        assert_eq!(numeric_helper("sign").unwrap().arity(), 1);
        assert_eq!(numeric_helper("min").unwrap().arity(), 2);
        assert_eq!(numeric_helper("max").unwrap().arity(), 2);
        assert_eq!(numeric_helper("gcd").unwrap().arity(), 2);
        assert_eq!(numeric_helper("lcm").unwrap().arity(), 2);
        assert_eq!(numeric_helper("clamp").unwrap().arity(), 3);
        // Every helper's wrapper takes only `Gc` operands after the context and
        // returns one — the uniform shape the MIR lowering relies on to be one
        // path rather than seven.
        for h in NUMERIC_HELPERS {
            let sig = h.symbol.sig();
            assert_eq!(sig.params[0], crate::abi::AbiKind::Ctx, "{}", h.name);
            assert!(
                sig.params[1..]
                    .iter()
                    .all(|k| *k == crate::abi::AbiKind::Gc),
                "{} takes a non-Gc operand",
                h.name
            );
            assert_eq!(sig.ret, crate::abi::AbiRet::Gc, "{}", h.name);
        }
    }

    /// No two helpers share a wrapper. A copy-pasted row that named an
    /// already-used symbol would make one of the two names compute the other's
    /// answer, and nothing else would notice.
    #[test]
    fn each_helper_has_its_own_wrapper() {
        let mut seen = HashSet::new();
        for h in NUMERIC_HELPERS {
            assert!(seen.insert(h.symbol), "{} reuses a wrapper", h.name);
        }
    }
}
