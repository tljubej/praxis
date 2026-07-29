//! The Praxis prelude (§16.1): symbols automatically available in every
//! program, with no `use` required.
//!
//! Kept as data so the type checker, the LSP completion table, and the
//! documentation generator all read the same single list.

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
}
