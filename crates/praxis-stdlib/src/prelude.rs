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
    // value, None marks absence (§4.7 — "normal domain-level absence… not an
    // error channel"). Returned by the `optional(P)` parser, by `Map.get` and
    // `Grid.find` (D1), and by the graph walks that may not reach their goal.
    //
    // NOT by `find`/`position` on a sequence, which this used to claim: those
    // answer an `Int` index with a `-1` miss sentinel. That is a separate
    // question from D1's and is not S18's to change.
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

/// §6.5's graph helpers: the closure-driven algorithms that walk a graph the
/// program never materializes (ADR-060).
///
/// A row is the source name, the wrapper it lowers to, the **shape** of each
/// parameter and the shape of the result. The shapes are what inference reads
/// to build the scheme, so a helper's signature and the wrapper it becomes are
/// written down once; the arity that follows from `params` is checked against
/// [`RuntimeSymbol::arity`] by a unit test, which is as close to ADR-058's
/// "the arity is the wrapper's arity" as a family with six different shapes
/// can get.
///
/// Every helper's first parameter is the start state and every other one is a
/// function of it, which is §6.5's own spelling:
///
/// ```text
/// var distance = bfs_distance(start, |s| neighbors(s), |s| s == goal)
/// ```
///
/// Before this, all six names resolved (they are in [`PRELUDE`]) and then had
/// no scheme, no lowering and no implementation: inference handed out a fresh
/// variable that unified with anything and the call lowered as a direct call to
/// a function nobody defined. That is TY-33, and these were the last six names
/// in that state.
pub const GRAPH_HELPERS: &[GraphHelper] = &[
    GraphHelper::new(
        "bfs",
        RuntimeSymbol::Bfs,
        &[GraphParam::Start, GraphParam::Neighbours],
        GraphResult::VisitOrder,
    ),
    GraphHelper::new(
        "bfs_distance",
        RuntimeSymbol::BfsDistance,
        &[GraphParam::Start, GraphParam::Neighbours, GraphParam::Goal],
        GraphResult::Distance,
    ),
    GraphHelper::new(
        "dfs",
        RuntimeSymbol::Dfs,
        &[GraphParam::Start, GraphParam::Neighbours],
        GraphResult::VisitOrder,
    ),
    GraphHelper::new(
        "dijkstra",
        RuntimeSymbol::Dijkstra,
        &[
            GraphParam::Start,
            GraphParam::Neighbours,
            GraphParam::Weight,
        ],
        GraphResult::CostTable,
    ),
    GraphHelper::new(
        "a_star",
        RuntimeSymbol::AStar,
        &[
            GraphParam::Start,
            GraphParam::Neighbours,
            GraphParam::Weight,
            GraphParam::Heuristic,
            GraphParam::Goal,
        ],
        GraphResult::Distance,
    ),
    GraphHelper::new(
        "flood_fill",
        RuntimeSymbol::FloodFill,
        &[GraphParam::Start, GraphParam::Neighbours],
        GraphResult::Reached,
    ),
];

/// One parameter of a graph helper, as a shape rather than as a type.
///
/// `praxis-stdlib` cannot name a `Type` — that is `praxis-types`, which depends
/// on this crate — so the signature is written as the shapes inference then
/// builds the types from. The match on this in `seed_builtin_schemes` is
/// exhaustive, so a new shape is a compile error there rather than a parameter
/// that silently gets the wrong type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GraphParam {
    /// The state the walk starts from: a `T`.
    Start,
    /// `(T) -> Vec[T]` — the states reachable in one step from a given one.
    /// Returning a `Vec` rather than a lazy sequence is what makes the helper
    /// callable from the runtime, which has no way to drive a pipeline.
    Neighbours,
    /// `(T, T) -> Int` — the cost of the edge between two adjacent states.
    Weight,
    /// `(T) -> Int` — the estimated remaining cost from a state to a goal.
    Heuristic,
    /// `(T) -> Bool` — whether a state is a goal. A predicate rather than a
    /// goal *value* so a search can stop on a property (`|s| s.0 == n`), which
    /// is what §6.5's own example writes.
    Goal,
}

/// What a graph helper answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GraphResult {
    /// `Vec[T]` — every state reached, in the order the walk reached it.
    VisitOrder,
    /// `Set[T]` — every state reached, without an order.
    Reached,
    /// `Map[T, Int]` — the least cost from the start to each reachable state.
    /// A state that is not reachable is simply absent, which is why this needs
    /// no `Option`.
    CostTable,
    /// `Option[Int]` — the cost of the cheapest path to a goal, or `None` when
    /// no goal is reachable. "Unreachable" is an ordinary outcome of a search,
    /// not a fault, and a sentinel `-1` would be a number nobody wrote.
    Distance,
}

/// One graph prelude helper: the source name, the wrapper it lowers to, and the
/// shape of its signature.
#[derive(Clone, Copy, Debug)]
pub struct GraphHelper {
    pub name: &'static str,
    pub symbol: RuntimeSymbol,
    pub params: &'static [GraphParam],
    pub result: GraphResult,
}

impl GraphHelper {
    const fn new(
        name: &'static str,
        symbol: RuntimeSymbol,
        params: &'static [GraphParam],
        result: GraphResult,
    ) -> GraphHelper {
        GraphHelper {
            name,
            symbol,
            params,
            result,
        }
    }

    /// How many arguments the source-level function takes.
    #[inline]
    pub const fn arity(&self) -> usize {
        self.params.len()
    }
}

/// The graph helper `name` denotes, or `None` for any other name.
///
/// The one lookup from a source name to a graph helper. Both consumers use it —
/// inference for the scheme, MIR for the call target — so neither carries its
/// own list.
pub fn graph_helper(name: &str) -> Option<GraphHelper> {
    GRAPH_HELPERS.iter().copied().find(|h| h.name == name)
}

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
    /// (`var A = None` has `Option`'s type too), so the declaration says
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
        let mut seen = HashSet::new();
        for h in GRAPH_HELPERS {
            assert!(seen.insert(h.symbol), "{} reuses a wrapper", h.name);
        }
    }

    /// Every graph helper is a prelude name, and every name §6.5 lists is a
    /// graph helper. Same property as the numeric line's, for the same reason: a
    /// name in only one list is either a phantom (resolves, then has nowhere to
    /// go — TY-33's shape) or an unreachable wrapper.
    #[test]
    fn every_graph_helper_is_a_prelude_name() {
        let prelude: HashSet<_> = PRELUDE.iter().map(|e| e.name).collect();
        for h in GRAPH_HELPERS {
            assert!(prelude.contains(h.name), "{} is not in PRELUDE", h.name);
        }
        // §6.5's six algorithms, verbatim. (Its list also names connected
        // components and topological sort; neither is a `PRELUDE` name, so
        // neither is one of TY-33's phantoms.)
        for required in [
            "bfs",
            "bfs_distance",
            "dfs",
            "dijkstra",
            "a_star",
            "flood_fill",
        ] {
            assert!(
                graph_helper(required).is_some(),
                "§6.5 lists {required:?} and it has no helper row"
            );
        }
        assert!(graph_helper("abs").is_none());
        assert!(graph_helper("out").is_none());
    }

    /// A helper's source arity is its wrapper's arity. `params` states the
    /// *shape* of each argument because six helpers have five different
    /// signatures and the manifest cannot say which is which — but the count
    /// still has one authority, so a row that grew a parameter without growing
    /// its wrapper is a failure here rather than a call that passes garbage in
    /// the slot nobody filled.
    #[test]
    fn a_graph_helpers_arity_is_the_wrappers_arity() {
        for h in GRAPH_HELPERS {
            assert_eq!(
                h.arity(),
                h.symbol.arity(),
                "{}'s signature and its wrapper disagree on arity",
                h.name
            );
            // Every wrapper takes only `Gc` operands after the context and
            // returns one — the uniform shape that makes the MIR lowering one
            // path for all six rather than six branches.
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

    /// Every helper starts from a state and every other parameter is a function
    /// of it. That is §6.5's shape — "closure-based algorithms that do not
    /// require materializing a graph object" — and it is what lets one runtime
    /// calling convention serve all six: the first operand is a value, the rest
    /// are closures.
    #[test]
    fn a_graph_helper_takes_a_start_state_and_then_only_functions() {
        for h in GRAPH_HELPERS {
            assert_eq!(
                h.params.first(),
                Some(&GraphParam::Start),
                "{} does not start from a state",
                h.name
            );
            assert!(
                h.params[1..].iter().all(|p| *p != GraphParam::Start),
                "{} takes a second bare state",
                h.name
            );
            assert!(
                h.params.contains(&GraphParam::Neighbours),
                "{} has no way to reach a second state",
                h.name
            );
        }
    }

    /// A search that can fail to find anything answers with an `Option`, and a
    /// walk that always reaches at least its own start does not. The pairing is
    /// the rule: `dijkstra` needs no `Option` because an unreachable state is
    /// *absent* from its table, and `bfs`/`dfs`/`flood_fill` always contain the
    /// start.
    #[test]
    fn only_a_goal_directed_helper_can_answer_with_nothing() {
        for h in GRAPH_HELPERS {
            let goal_directed = h.params.contains(&GraphParam::Goal);
            let optional = h.result == GraphResult::Distance;
            assert_eq!(
                goal_directed, optional,
                "{} looks for a goal but cannot say it found none (or vice versa)",
                h.name
            );
        }
    }
}
