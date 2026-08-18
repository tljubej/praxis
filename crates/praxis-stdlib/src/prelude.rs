//! The Praxis prelude (§16.1): symbols automatically available in every
//! program, with no `use` required.
//!
//! Kept as data so name resolution and the language server's completion, hover
//! and signature tables all read the same single list.

use crate::abi::RuntimeSymbol;

/// The names automatically imported into every Praxis program (§16.1).
///
/// Sorted by category so the LSP can present them sensibly; within a category
/// the order is alphabetical. The list is checked for emptiness and duplicates
/// by the unit test.
pub const PRELUDE: &[PreludeEntry] = &[
    // Output / control
    PreludeEntry::new(
        "out",
        "Write one value to stdout, followed by a newline. Renders through the value's own formatter, so any type may be written.",
    ),
    PreludeEntry::new(
        "dbg",
        "Write one value to **stderr** and return it unchanged, so `dbg(e)` can wrap any subexpression without changing what the program computes.",
    ),
    PreludeEntry::new(
        "panic",
        "Stop with an explicit message. Raises an ordinary fault, so it enters the crash debugger on a terminal; its result is `Never`, so a function may end on one.",
    ),
    PreludeEntry::new(
        "assert",
        "Stop if a condition is false. Takes a `Bool` and nothing else — there is no message parameter.",
    ),
    // Numeric helpers. All seven are `Int` functions and none is generic
    // (ADR-058), so each doc string says `Int` rather than "a number": `Float`
    // carries its own `abs`/`sign`/`min`/`max` as *methods* and has no `clamp`,
    // `gcd` or `lcm` at all, and a reader who hovers here has already asked.
    PreludeEntry::new(
        "abs",
        "Absolute value of an `Int`. Faults on `Int`'s minimum, which has no positive counterpart. `Float` has its own `x.abs()`.",
    ),
    PreludeEntry::new(
        "sign",
        "`-1`, `0` or `1`, by the sign of an `Int`. Total. `Float` has its own `x.sign()`.",
    ),
    PreludeEntry::new(
        "min",
        "The smaller of two `Int`s. `Float` has its own `x.min(y)` method.",
    ),
    PreludeEntry::new(
        "max",
        "The larger of two `Int`s. `Float` has its own `x.max(y)` method.",
    ),
    PreludeEntry::new(
        "clamp",
        "`clamp(value, low, high)` — an `Int` held inside an inclusive range. Faults if `low > high`.",
    ),
    PreludeEntry::new(
        "gcd",
        "Non-negative greatest common divisor of two `Int`s. `gcd(0, 0)` is `0`.",
    ),
    PreludeEntry::new(
        "lcm",
        "Non-negative least common multiple of two `Int`s, or `0` if either operand is. Faults if the result leaves `Int`.",
    ),
    // Nullary **functions**, not constants: `pi()` is the value and `pi` is
    // `() -> Float` (§4.12). The doc string is what hover shows, so it must say
    // so rather than call them constants.
    PreludeEntry::new("pi", "π as a Float. A nullary function: write `pi()`."),
    PreludeEntry::new(
        "e",
        "Euler's number as a Float. A nullary function: write `e()`.",
    ),
    // Collections
    // `Vec` and `Grid` are the two constructors with a sized form, so their doc
    // strings say both — this string is what LSP hover puts in front of the one
    // reader who has already asked (ADR-146).
    PreludeEntry::new(
        "Vec",
        "Grow, iterate, and pipeline over an ordered list. `Vec()` is empty; `Vec(n, fill)` is n copies of fill.",
    ),
    PreludeEntry::new(
        "Deque",
        "Double-ended queue: push and pop at either end. `Deque()` is empty.",
    ),
    PreludeEntry::new(
        "Map",
        "Hash map from keys to values. A key must be a value that cannot change. `Map()` is empty.",
    ),
    PreludeEntry::new(
        "Set",
        "Hash set of distinct values. An element must be a value that cannot change. `Set()` is empty.",
    ),
    PreludeEntry::new(
        "Counter",
        "Map whose absent values read as zero, so `c.inc(k)` needs no first-sighting case. `Counter()` is empty.",
    ),
    PreludeEntry::new(
        "MinHeap",
        "Priority queue yielding the smallest element first. Its element must be orderable. `MinHeap()` is empty.",
    ),
    PreludeEntry::new(
        "MaxHeap",
        "Priority queue yielding the largest element first. Its element must be orderable. `MaxHeap()` is empty.",
    ),
    PreludeEntry::new(
        "Grid",
        "2D grid with rectangular indexing. `Grid()` is 0x0; `Grid(w, h, fill)` is a w-by-h board of fill.",
    ),
    PreludeEntry::new(
        "BitSet",
        "Compact set of non-negative integers. Takes no type argument.",
    ),
    // Optionality. Option[T] is a polymorphic enum: Some(T) carries a value,
    // None marks absence (§4.7 — "normal domain-level absence… not an error
    // channel"). Returned by the `optional(P)` parser, by `Map.get` and
    // `Grid.find`, and by the graph walks that may not reach their goal.
    //
    // Also by `find`/`position` on a sequence (ADR-082): `find` answers the
    // *element* as an `Option[T]` and `position` its index as an `Option[Int]`.
    // Neither uses a `-1` miss sentinel.
    PreludeEntry::new(
        "Option",
        "Optional value: `Some(T)` or `None`. Domain-level absence, not an error channel — what `Map.get`, `Grid.find`, `find`/`position` and the goal-directed graph walks answer with.",
    ),
    PreludeEntry::variant("Some", "Wrap a value in an `Option`."),
    PreludeEntry::variant(
        "None",
        "The absent `Option` value. Not a call — write `None`, never `None()`.",
    ),
    // Graph algorithms (§6.5). Each doc string writes the **call**, because the
    // one thing a reader cannot guess from the name is the shape of the closures
    // — none of these takes a graph object, so the graph *is* the neighbour
    // function (ADR-060). Every state a walk visits is remembered, so the state
    // type has to be usable as a key.
    PreludeEntry::new(
        "bfs",
        "Breadth-first walk: `bfs(start, |s| neighbors(s))` answers every state reached, in the order it was reached.",
    ),
    PreludeEntry::new(
        "bfs_distance",
        "Steps to the first state a predicate accepts, or `None` when no goal is reachable: `bfs_distance(start, |s| neighbors(s), |s| s == goal)`.",
    ),
    PreludeEntry::new(
        "dfs",
        "Depth-first walk: `dfs(start, |s| neighbors(s))` answers every state reached, in the order it was reached.",
    ),
    PreludeEntry::new(
        "dijkstra",
        "Least cost from a start state to each reachable state, as a `Map`: `dijkstra(start, |s| neighbors(s), |a, b| weight(a, b))`. An unreachable state is absent rather than `None`.",
    ),
    PreludeEntry::new(
        "a_star",
        "Cost of the cheapest path to a goal, or `None`: `a_star(start, neighbors, weight, heuristic, goal)`, where the heuristic estimates the remaining cost from one state.",
    ),
    PreludeEntry::new(
        "flood_fill",
        "Every state reachable from a start state, unordered, as a `Set`: `flood_fill(start, |s| neighbors(s))`.",
    ),
];

/// The built-in **type** names (§4.2's six scalars, `Never`, and `Range`), with
/// the one-line description the editor shows for each.
///
/// This is the table `praxis-hir`'s name resolution seeds the root scope from,
/// so a type name the checker accepts and a type name the editor can describe
/// are the same list.
///
/// `UInt` and `Byte` are deliberately absent: §4.2 reserves them and neither is
/// implemented, so either one in an annotation is an `N002` rather than a name
/// with a doc string.
pub const BUILTIN_TYPES: &[TypeEntry] = &[
    TypeEntry::seeded("Int", "Signed 64-bit integer. Written `42` or `1_000_000`."),
    TypeEntry::seeded(
        "Float",
        "IEEE-754 binary64. Written `3.5`, `1e10` or `2e-3`; `.5` is not a literal.",
    ),
    TypeEntry::seeded("Bool", "`true` or `false`."),
    TypeEntry::seeded("Char", "One Unicode scalar value. Written `'p'`."),
    TypeEntry::seeded("Text", "Immutable UTF-8 text. Written `\"praxis\"`."),
    TypeEntry::seeded("Unit", "The type with one value, written `()`."),
    TypeEntry::seeded(
        "Never",
        "The type of an expression that produces no value — `panic(...)`, `return`, `break`. It has no values, so it unifies with anything.",
    ),
    // **Not seeded**, and that is the whole of what `TypeEntry::seeded` and
    // [`TypeEntry::ctor`] distinguish. `Range` is the one built-in type with no
    // value of the same name: a range is written `0..n`, and `Range()` is
    // `N001: 'Range' is not defined`. Binding it in the root scope would make
    // that call resolve and then fail later, somewhere with less to say.
    TypeEntry::ctor(
        "Range",
        "A half-open (`0..n`) or inclusive (`0..=n`) integer range. A type name only — there is no `Range()` constructor.",
    ),
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
/// A name in [`PRELUDE`] with no row here resolves and then has no scheme, no
/// lowering and no implementation: inference hands out a fresh variable that
/// unifies with anything and the call lowers as a direct call to a function
/// nobody defined.
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
/// `praxis-stdlib` cannot name a `Type` — that is `praxis-typeck`, which depends
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
/// name is [`RuntimeSymbol::arity`], read off the ABI manifest rather than
/// restated here. That is what makes "a prelude name whose signature disagrees
/// with the wrapper it calls" unrepresentable.
///
/// `Float`'s counterparts are *methods* (`0.5.abs()`, `x.min(y)` — §4.12), not
/// entries here: a genuinely polymorphic `abs` would have to carry a numeric
/// capability on its own binder and pick a lowering per instantiation, and
/// nothing needs that yet.
///
/// `pi` and `e` are not here either. They are nullary `Float` functions rather
/// than `Int` ones, with their own schemes and dispatch.
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

/// The collection constructors that also have a **sized** form, in which the
/// argument count selects the shape: `Vec(n, fill)` and `Grid(w, h, fill)`
/// beside the nullary `Vec()` and `Grid()` (ADR-146).
///
/// This is the whole of the carve-out ADR-146 makes to
/// [ADR-089](../../../docs/decisions/089-a-name-has-one-signature.md) decision
/// 1's "a name has exactly one signature", and it is a `const` table rather
/// than a rule anywhere so that the narrowness is a fact a reader can count.
/// The other seven constructors are absent on purpose: a sized `Set` is `n`
/// copies of one element in a set, which is one element, and a sized `Map` has
/// no answer for what its keys would be. `Vec` and `Grid` are the two whose
/// contents are addressed by position, which is what makes "n of them" mean
/// something.
pub const SIZED_CTORS: &[SizedCtor] = &[
    SizedCtor::new("Vec", RuntimeSymbol::VecFilled, 1),
    SizedCtor::new("Grid", RuntimeSymbol::GridFilled, 2),
];

/// One sized collection constructor: the source name, the wrapper its sized
/// form lowers to, and how many of its leading arguments are extents.
///
/// The fill is always the last argument and always exactly one, so `extents`
/// determines the source arity — which is why nothing here states an arity
/// twice.
#[derive(Clone, Copy, Debug)]
pub struct SizedCtor {
    pub name: &'static str,
    pub symbol: RuntimeSymbol,
    /// The number of leading `Int` parameters: 1 for `Vec(n, fill)`, 2 for
    /// `Grid(w, h, fill)`.
    pub extents: usize,
}

impl SizedCtor {
    const fn new(name: &'static str, symbol: RuntimeSymbol, extents: usize) -> SizedCtor {
        SizedCtor {
            name,
            symbol,
            extents,
        }
    }

    /// The source-level arity: the extents plus the one fill.
    #[inline]
    #[must_use]
    pub const fn arity(&self) -> usize {
        self.extents + 1
    }
}

/// The sized constructor `name` denotes, or `None` for any other name —
/// including the seven collection constructors that have no sized form.
///
/// The one lookup from a source name to a sized constructor. Inference reads it
/// to build the call site's type and MIR reads it to pick the wrapper, so
/// neither carries its own list and neither can disagree with the other about
/// which names are sized.
pub fn sized_ctor(name: &str) -> Option<SizedCtor> {
    SIZED_CTORS.iter().copied().find(|c| c.name == name)
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
    /// (`var A = None` has `Option`'s type too), so the declaration says.
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

/// One built-in type name and a one-line description.
#[derive(Clone, Copy, Debug)]
pub struct TypeEntry {
    pub name: &'static str,
    pub doc: &'static str,
    /// Whether name resolution binds this name in the root scope.
    ///
    /// The scalars and `Never` are bound, which is how `var n: Nope` becomes an
    /// `N002` from a failed lookup. A **type constructor** is not: `Range`, and
    /// the collection names in [`PRELUDE`], are compiler-owned type names that
    /// annotation checking accepts without a lookup and inference turns into
    /// types. The distinction is not cosmetic — a bound name is also a *value*
    /// name, and `Range` has no value.
    pub seeded: bool,
}

impl TypeEntry {
    /// A type name bound in the root scope.
    const fn seeded(name: &'static str, doc: &'static str) -> TypeEntry {
        TypeEntry {
            name,
            doc,
            seeded: true,
        }
    }

    /// A compiler-owned type constructor, which is not bound in any scope. See
    /// [`TypeEntry::seeded`](Self::seeded)'s field documentation.
    const fn ctor(name: &'static str, doc: &'static str) -> TypeEntry {
        TypeEntry {
            name,
            doc,
            seeded: false,
        }
    }
}

/// The description of the prelude **value** `name` denotes, or `None` for any
/// other name.
///
/// The one lookup from a source name to its documentation, so the language
/// server describes the prelude the compiler actually declares. A server that
/// carried its own sentence about `bfs` would be free to describe a helper this
/// table no longer has.
///
/// Callers must satisfy themselves that the name really is the prelude's: a
/// `var out = 1` shadows it, and this function only knows the spelling.
#[must_use]
pub fn prelude_doc(name: &str) -> Option<&'static str> {
    PRELUDE.iter().find(|e| e.name == name).map(|e| e.doc)
}

/// The description of the built-in **type** `name` denotes, or `None` for any
/// other name.
///
/// Type position is a different question from value position and gets a
/// different answer: `Int` is a type and never a value, `Range` is a type whose
/// name no value shares, and the nine collection names are both — so this looks
/// in [`BUILTIN_TYPES`] first and falls back to [`PRELUDE`], where `Vec`'s row
/// already opens with what a `Vec` *is* before it says what `Vec()` builds.
///
/// The fallback is what keeps `Vec[Int]` from needing a second description of a
/// `Vec` that could drift from the first.
#[must_use]
pub fn type_doc(name: &str) -> Option<&'static str> {
    BUILTIN_TYPES
        .iter()
        .find(|e| e.name == name)
        .map(|e| e.doc)
        .or_else(|| prelude_doc(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{AbiKind, AbiRet};
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

    /// **Every name the editor can offer carries a sentence.** Both tables are
    /// read by hover, completion and signature help, and a row added later with
    /// an empty `doc` would surface as a name with a blank description rather
    /// than as a build failure.
    ///
    /// The length floor is the interesting half: `""` is the mistake a
    /// copy-pasted row makes, and `"."` is the one a placeholder makes.
    #[test]
    fn every_documented_name_has_documentation() {
        for e in PRELUDE {
            assert!(
                e.doc.len() > 10,
                "prelude entry `{}` has no real documentation",
                e.name
            );
            assert!(
                e.doc.ends_with('.'),
                "prelude entry `{}`'s doc is not a sentence",
                e.name
            );
        }
        for e in BUILTIN_TYPES {
            assert!(
                e.doc.len() > 10,
                "type entry `{}` has no real documentation",
                e.name
            );
            assert!(
                e.doc.ends_with('.'),
                "type entry `{}`'s doc is not a sentence",
                e.name
            );
        }
    }

    /// The two lookups answer for every row and for nothing else. `prelude_doc`
    /// is asked about *values* and `type_doc` about *types*, and the pair that
    /// keeps them honest is `Int` (a type, never a value) and `out` (a value,
    /// never a type).
    #[test]
    fn the_lookups_answer_for_exactly_their_own_names() {
        for e in PRELUDE {
            assert_eq!(prelude_doc(e.name), Some(e.doc), "{}", e.name);
        }
        for e in BUILTIN_TYPES {
            assert_eq!(type_doc(e.name), Some(e.doc), "{}", e.name);
        }
        // A collection name is both, and `type_doc` falls back to the prelude
        // row rather than to a second description of the same type.
        assert_eq!(type_doc("Vec"), prelude_doc("Vec"));
        assert!(type_doc("Vec").is_some());
        // `Int` is a type and not a value; `out` is a value and not a type.
        assert!(prelude_doc("Int").is_none());
        assert!(type_doc("Int").is_some());
        assert!(type_doc("out").is_none() || prelude_doc("out").is_some());
        // Neither answers for a name the language does not have.
        assert!(prelude_doc("nope").is_none());
        assert!(type_doc("nope").is_none());
        // §4.2 reserves these and neither is implemented, so neither may
        // acquire a doc string without also acquiring a type.
        assert!(type_doc("UInt").is_none());
        assert!(type_doc("Byte").is_none());
    }

    /// **Every name legal in type position has a description**, which is the
    /// property that makes "hover over an annotation says something" true by
    /// construction rather than by a list somebody kept up to date.
    ///
    /// The set is `praxis-hir`'s `is_type_ctor_name` — `Option` or a collection
    /// — plus the seeded scalars. `Seq` is excluded because it is
    /// compiler-internal and no source name reaches it (§6.3).
    #[test]
    fn every_type_position_name_is_documented() {
        for ctor in [
            crate::CollectionCtor::Vec,
            crate::CollectionCtor::Deque,
            crate::CollectionCtor::Map,
            crate::CollectionCtor::Set,
            crate::CollectionCtor::Counter,
            crate::CollectionCtor::MinHeap,
            crate::CollectionCtor::MaxHeap,
            crate::CollectionCtor::BitSet,
            crate::CollectionCtor::Grid,
            crate::CollectionCtor::Range,
        ] {
            let name = ctor.name();
            assert!(
                type_doc(name).is_some(),
                "the type name `{name}` has no description"
            );
        }
        assert!(type_doc("Option").is_some());
    }

    /// A **type constructor is not a scope symbol**, and `Range` is the row
    /// that makes the distinction load-bearing: binding it would make `Range()`
    /// resolve to a name instead of being the `N001` the book prints.
    #[test]
    fn only_a_type_that_has_no_value_is_left_unseeded() {
        let unseeded: Vec<&str> = BUILTIN_TYPES
            .iter()
            .filter(|e| !e.seeded)
            .map(|e| e.name)
            .collect();
        assert_eq!(unseeded, vec!["Range"]);
        // The seeded names are exactly §4.2's scalars plus `Never` — the set
        // `praxis-hir`'s `seed_type_names` binds.
        let seeded: Vec<&str> = BUILTIN_TYPES
            .iter()
            .filter(|e| e.seeded)
            .map(|e| e.name)
            .collect();
        assert_eq!(
            seeded,
            vec!["Int", "Float", "Bool", "Char", "Text", "Unit", "Never"]
        );
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
    /// only one of them is either a phantom (it resolves, then has nowhere to
    /// go) or an unreachable wrapper.
    #[test]
    fn every_numeric_helper_is_a_prelude_name() {
        let prelude: HashSet<_> = PRELUDE.iter().map(|e| e.name).collect();
        for h in NUMERIC_HELPERS {
            assert!(prelude.contains(h.name), "{} is not in PRELUDE", h.name);
        }
        // §16.1's numeric line, verbatim. `pi`/`e` are in `PRELUDE` too but are
        // nullary `Float` functions with their own dispatch, not `Int` ones.
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

    /// A wrapper takes the context, then `leading_ptrs` raw pointer slots, then
    /// only `Gc` operands, and returns one. That uniform shape is what lets the
    /// MIR lowering be one path per table rather than a branch per helper, and
    /// it is the same property for the numeric helpers, the graph helpers and
    /// the sized constructors — the last of which spend their one leading
    /// pointer on the static element descriptor.
    fn assert_uniform_gc_wrapper(sym: RuntimeSymbol, name: &str, leading_ptrs: usize) {
        let sig = sym.sig();
        assert_eq!(sig.params[0], AbiKind::Ctx, "{name}");
        let boxed_from = 1 + leading_ptrs;
        assert!(
            sig.params[1..boxed_from].iter().all(|k| *k == AbiKind::Ptr),
            "{name}'s first {leading_ptrs} operand(s) after the context are not raw pointers"
        );
        assert!(
            sig.params[boxed_from..].iter().all(|k| *k == AbiKind::Gc),
            "{name} takes a non-Gc operand"
        );
        assert_eq!(sig.ret, AbiRet::Gc, "{name}");
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
            assert_uniform_gc_wrapper(h.symbol, h.name, 0);
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
        let mut seen = HashSet::new();
        for c in SIZED_CTORS {
            assert!(seen.insert(c.symbol), "{} reuses a wrapper", c.name);
        }
    }

    /// A sized constructor is a prelude name, for the reason the numeric and
    /// graph helpers are: a row naming a name nothing declares is a wrapper no
    /// program can reach.
    #[test]
    fn every_sized_ctor_is_a_prelude_name() {
        let prelude: HashSet<_> = PRELUDE.iter().map(|e| e.name).collect();
        for c in SIZED_CTORS {
            assert!(prelude.contains(c.name), "{} is not in PRELUDE", c.name);
        }
    }

    /// The row's `extents` count and its wrapper's arity are one number,
    /// checked against each other. The wrapper takes a context, an element
    /// descriptor, every extent, and the fill — so the source arity plus the
    /// descriptor slot is the wrapper's arity, and a row that grew an extent
    /// without growing its wrapper would otherwise pass garbage in an unfilled
    /// slot rather than failing here.
    #[test]
    fn a_sized_ctors_arity_is_the_wrappers_arity() {
        assert_eq!(sized_ctor("Vec").expect("Vec is sized").arity(), 2);
        assert_eq!(sized_ctor("Grid").expect("Grid is sized").arity(), 3);
        for c in SIZED_CTORS {
            assert_eq!(
                c.arity() + 1,
                c.symbol.arity(),
                "{}'s row and its wrapper disagree about how many operands it takes",
                c.name
            );
            // The one leading pointer is the static element descriptor; the
            // extents and the fill after it are boxed (ADR-146 decision 7).
            assert_uniform_gc_wrapper(c.symbol, c.name, 1);
        }
    }

    /// **Only `Vec` and `Grid` are sized**, and this is the test that keeps
    /// ADR-089 decision 1 intact everywhere else. ADR-146 is a carve-out of
    /// exactly two names; a third row added without reopening that decision
    /// fails here.
    #[test]
    fn only_vec_and_grid_have_a_sized_form() {
        assert_eq!(SIZED_CTORS.len(), 2);
        for absent in [
            "Deque", "Map", "Set", "Counter", "MinHeap", "MaxHeap", "BitSet", "Range", "Option",
            "out", "abs", "bfs",
        ] {
            assert!(
                sized_ctor(absent).is_none(),
                "{absent} has no sized form and ADR-146 says why"
            );
        }
    }

    /// Every graph helper is a prelude name, and every name §6.5 lists is a
    /// graph helper. Same property as the numeric line's, for the same reason: a
    /// name in only one list is either a phantom (it resolves, then has nowhere
    /// to go) or an unreachable wrapper.
    #[test]
    fn every_graph_helper_is_a_prelude_name() {
        let prelude: HashSet<_> = PRELUDE.iter().map(|e| e.name).collect();
        for h in GRAPH_HELPERS {
            assert!(prelude.contains(h.name), "{} is not in PRELUDE", h.name);
        }
        // §6.5's six algorithms, verbatim. (Its list also names connected
        // components and topological sort; neither is a `PRELUDE` name, so
        // neither is a phantom.)
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
            assert_uniform_gc_wrapper(h.symbol, h.name, 0);
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
