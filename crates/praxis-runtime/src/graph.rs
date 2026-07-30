//! The six graph walks behind §6.5's prelude helpers (TY-33 unit 3, ADR-060).
//!
//! §6.5 asks for "closure-based algorithms that do not require materializing a
//! graph object": the caller supplies a start state and a function from a state
//! to its neighbours, and the helper walks whatever that function describes.
//! There is no graph value, no adjacency table and no node type — the graph is
//! the closure.
//!
//! # Why the walks do not call the closures themselves
//!
//! Calling a Praxis closure means transmuting a JIT'd function pointer and
//! passing it a live `RuntimeContext`, which no unit test can supply. So the
//! walks below never touch a closure: they ask a [`GraphOracle`], and
//! `praxis_runtime::abi` supplies the one implementation that calls closures.
//! A test supplies one backed by an adjacency table, which is what makes
//! "`dijkstra` relaxes an edge it has already settled" a question that can be
//! asked without a compiler in the room.
//!
//! # States are values, and the walks hold them
//!
//! A state is a `GcRef` and the walks keep every state they have seen — in a
//! visited set, in a queue, in a cost table. Those are Rust structures the
//! collector cannot see, so **the caller must root every state it hands in and
//! every state an oracle hands back** before the next call that may allocate.
//! [`GraphOracle::retain`] is where that happens: the walks call it once per
//! newly discovered state, immediately, and the ABI implementation roots it in
//! its [`NativeScope`](crate::roots::NativeScope).
//!
//! # Identity
//!
//! Two states are the same state when [`DynamicKey`] says so — the same
//! descriptor and a structural `equals` — which is exactly the rule a `Set`
//! element and a `Map` key follow. That is why inference requires
//! `CapKind::HashStable` of the state type at every call site: a state that can
//! change after the walk has stored it cannot be found again, and the walk
//! would revisit it forever.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

use crate::context::FaultKind;
use crate::dynamic_key::DynamicKey;
use crate::GcRef;

/// A walk stopped before it had an answer, because a fault is pending.
///
/// Never constructed by a walk directly: it comes back from an oracle call that
/// faulted, or from [`GraphOracle::abort`], so "a walk returned `Err` and left
/// no fault behind" is not a state a walk can produce.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Aborted;

/// What a walk asks about the graph it is walking.
///
/// Every method may fault — the closures are arbitrary Praxis code — so every
/// answer is a `Result`. An `Err(Aborted)` means a fault is already pending on
/// the context and the walk must stop; it never means "no answer".
pub trait GraphOracle {
    /// The states reachable in one step from `state`.
    fn neighbours(&mut self, state: GcRef) -> Result<Vec<GcRef>, Aborted>;

    /// The cost of the edge from `from` to `to`. Only called for a pair the
    /// oracle itself reported adjacent.
    fn weight(&mut self, from: GcRef, to: GcRef) -> Result<i64, Aborted>;

    /// The estimated remaining cost from `state` to a goal.
    fn heuristic(&mut self, state: GcRef) -> Result<i64, Aborted>;

    /// Whether `state` is a goal.
    fn is_goal(&mut self, state: GcRef) -> Result<bool, Aborted>;

    /// Keep `state` alive for the rest of the walk. Called once per state, the
    /// moment the walk decides to remember it.
    fn retain(&mut self, state: GcRef);

    /// Raise `kind` and stop the walk. The `Aborted` it returns is the only way
    /// a walk reports a fault of its own.
    fn abort(&mut self, kind: FaultKind) -> Aborted;
}

/// Remembered states, in the order they were first seen.
///
/// The visited set and the visit order are one structure because every walk
/// needs both and keeping them apart is how a state ends up in one and not the
/// other. `insert` answers whether the state was new, which is the only
/// question the walks ask.
struct Seen {
    keys: HashSet<DynamicKey>,
    order: Vec<GcRef>,
}

impl Seen {
    fn new() -> Seen {
        Seen {
            keys: HashSet::new(),
            order: Vec::new(),
        }
    }

    /// Record `state` if it is new. Returns whether it was.
    fn insert(&mut self, state: GcRef) -> bool {
        if self.keys.insert(DynamicKey::new(state)) {
            self.order.push(state);
            true
        } else {
            false
        }
    }
}

/// `bfs(start, neighbours)` — every reachable state, in breadth-first order.
///
/// The start is the first element: a walk always reaches where it began, which
/// is why this result needs no `Option`.
pub fn bfs_order(oracle: &mut dyn GraphOracle, start: GcRef) -> Result<Vec<GcRef>, Aborted> {
    let mut seen = Seen::new();
    oracle.retain(start);
    seen.insert(start);
    let mut queue = VecDeque::new();
    queue.push_back(start);
    while let Some(state) = queue.pop_front() {
        for next in oracle.neighbours(state)? {
            oracle.retain(next);
            if seen.insert(next) {
                queue.push_back(next);
            }
        }
    }
    Ok(seen.order)
}

/// `dfs(start, neighbours)` — every reachable state, in depth-first pre-order.
///
/// The neighbours are pushed in reverse so the *first* neighbour a state
/// reports is the first one descended into. Without that the order is the
/// mirror image of the one the program wrote, which is the kind of difference
/// only an end-to-end test sees.
pub fn dfs_order(oracle: &mut dyn GraphOracle, start: GcRef) -> Result<Vec<GcRef>, Aborted> {
    let mut seen = Seen::new();
    oracle.retain(start);
    let mut stack = vec![start];
    while let Some(state) = stack.pop() {
        if !seen.insert(state) {
            continue;
        }
        let next = oracle.neighbours(state)?;
        for n in next.into_iter().rev() {
            oracle.retain(n);
            stack.push(n);
        }
    }
    Ok(seen.order)
}

/// `flood_fill(start, neighbours)` — every reachable state.
///
/// The same walk as [`bfs_order`]; only the result type differs, and the ABI
/// wrapper is what turns the states into a `Set` rather than a `Vec`. Sharing
/// the walk is deliberate: "which states are reachable" has one answer, and two
/// implementations of it would eventually disagree.
pub fn reachable(oracle: &mut dyn GraphOracle, start: GcRef) -> Result<Vec<GcRef>, Aborted> {
    bfs_order(oracle, start)
}

/// `bfs_distance(start, neighbours, is_goal)` — the fewest steps from `start` to
/// a state satisfying `is_goal`, or `None` when no such state is reachable.
///
/// Every edge counts one, so the first time the walk sees a goal it has seen it
/// by a shortest path. `is_goal(start)` is asked first: a search whose start is
/// already a goal is zero steps, not one.
pub fn bfs_distance(oracle: &mut dyn GraphOracle, start: GcRef) -> Result<Option<i64>, Aborted> {
    let mut seen = Seen::new();
    oracle.retain(start);
    seen.insert(start);
    let mut queue = VecDeque::new();
    queue.push_back((start, 0_i64));
    while let Some((state, steps)) = queue.pop_front() {
        if oracle.is_goal(state)? {
            return Ok(Some(steps));
        }
        // A step count cannot overflow before the visited set exhausts memory,
        // but the addition is still checked: `saturating_add` would report a
        // distance nobody walked.
        let Some(next_steps) = steps.checked_add(1) else {
            return Err(oracle.abort(FaultKind::IntOverflow));
        };
        for next in oracle.neighbours(state)? {
            oracle.retain(next);
            if seen.insert(next) {
                queue.push_back((next, next_steps));
            }
        }
    }
    Ok(None)
}

/// `dijkstra(start, neighbours, weight)` — the least cost from `start` to every
/// reachable state, as `(state, cost)` pairs.
///
/// The start is present at cost 0. An unreachable state is simply absent, which
/// is why this answers with a table rather than with an `Option` per state.
///
/// A **negative edge weight faults**. Dijkstra settles a state the first time it
/// pops it and never reconsiders, so a negative edge makes the answer quietly
/// too large — and a cost nobody paid is worse than a stop (the rule ADR-058
/// applied to `abs(Int::MIN)`).
pub fn dijkstra_costs(
    oracle: &mut dyn GraphOracle,
    start: GcRef,
) -> Result<Vec<(GcRef, i64)>, Aborted> {
    // The heap is ordered by `(cost, sequence)` and carries the state
    // alongside: a state is not orderable — nothing requires it to be — so the
    // tie-break is insertion order, which also makes the walk deterministic.
    let mut frontier: BinaryHeap<Reverse<(i64, usize, StateEntry)>> = BinaryHeap::new();
    let mut best: HashMap<DynamicKey, i64> = HashMap::new();
    let mut settled: Vec<(GcRef, i64)> = Vec::new();
    let mut done: HashSet<DynamicKey> = HashSet::new();
    let mut seq = 0_usize;

    oracle.retain(start);
    best.insert(DynamicKey::new(start), 0);
    frontier.push(Reverse((0, seq, StateEntry(start))));
    seq += 1;

    while let Some(Reverse((cost, _, StateEntry(state)))) = frontier.pop() {
        let key = DynamicKey::new(state);
        if !done.insert(key) {
            // Already settled by a cheaper entry; this one is stale.
            continue;
        }
        settled.push((state, cost));
        for next in oracle.neighbours(state)? {
            oracle.retain(next);
            let step = oracle.weight(state, next)?;
            if step < 0 {
                return Err(oracle.abort(FaultKind::InvalidSize));
            }
            let Some(through) = cost.checked_add(step) else {
                return Err(oracle.abort(FaultKind::IntOverflow));
            };
            let next_key = DynamicKey::new(next);
            if done.contains(&next_key) {
                continue;
            }
            let improved = match best.get(&next_key) {
                Some(known) => through < *known,
                None => true,
            };
            if improved {
                best.insert(next_key, through);
                frontier.push(Reverse((through, seq, StateEntry(next))));
                seq += 1;
            }
        }
    }
    Ok(settled)
}

/// `a_star(start, neighbours, weight, heuristic, is_goal)` — the cost of the
/// cheapest path from `start` to a goal, or `None` when no goal is reachable.
///
/// The frontier is ordered by `g + h`; a state is settled when it is popped, and
/// the first goal popped is the cheapest one **provided the heuristic never
/// overestimates**. A heuristic that does is the caller's error and the search
/// cannot detect it — but a *negative* one can be detected, and is, for the same
/// reason a negative weight is: it makes `f` decrease along a path, which is the
/// condition the ordering relies on.
pub fn a_star_cost(oracle: &mut dyn GraphOracle, start: GcRef) -> Result<Option<i64>, Aborted> {
    let mut frontier: BinaryHeap<Reverse<(i64, usize, StateEntry)>> = BinaryHeap::new();
    let mut best: HashMap<DynamicKey, i64> = HashMap::new();
    let mut done: HashSet<DynamicKey> = HashSet::new();
    let mut seq = 0_usize;

    oracle.retain(start);
    let start_estimate = estimate(oracle, start, 0)?;
    best.insert(DynamicKey::new(start), 0);
    frontier.push(Reverse((start_estimate, seq, StateEntry(start))));
    seq += 1;

    while let Some(Reverse((_, _, StateEntry(state)))) = frontier.pop() {
        let key = DynamicKey::new(state);
        if !done.insert(key) {
            continue;
        }
        // `best` is the settled cost: the entry that popped is the cheapest one
        // for this state, and nothing lowers it after it is settled.
        let cost = *best.get(&key).expect("a popped state has a known cost");
        if oracle.is_goal(state)? {
            return Ok(Some(cost));
        }
        for next in oracle.neighbours(state)? {
            oracle.retain(next);
            let step = oracle.weight(state, next)?;
            if step < 0 {
                return Err(oracle.abort(FaultKind::InvalidSize));
            }
            let Some(through) = cost.checked_add(step) else {
                return Err(oracle.abort(FaultKind::IntOverflow));
            };
            let next_key = DynamicKey::new(next);
            if done.contains(&next_key) {
                continue;
            }
            let improved = match best.get(&next_key) {
                Some(known) => through < *known,
                None => true,
            };
            if improved {
                best.insert(next_key, through);
                let priority = estimate(oracle, next, through)?;
                frontier.push(Reverse((priority, seq, StateEntry(next))));
                seq += 1;
            }
        }
    }
    Ok(None)
}

/// `g + h` for a state, with the two refusals A\*'s ordering depends on: a
/// negative estimate, and a sum with no `Int`.
fn estimate(oracle: &mut dyn GraphOracle, state: GcRef, cost: i64) -> Result<i64, Aborted> {
    let h = oracle.heuristic(state)?;
    if h < 0 {
        return Err(oracle.abort(FaultKind::InvalidSize));
    }
    match cost.checked_add(h) {
        Some(f) => Ok(f),
        None => Err(oracle.abort(FaultKind::IntOverflow)),
    }
}

/// A state in a priority-queue entry, ordered as **equal to every other state**.
///
/// The queue's real key is the `(cost, sequence)` pair in front of this; a state
/// has no order of its own and requiring one would exclude every type that is a
/// legal `Map` key but not orderable — tuples and records, which is what a grid
/// position is. Making the comparison total-and-constant here is what lets the
/// tuple derive its `Ord` from the two fields that do order.
#[derive(Clone, Copy)]
struct StateEntry(GcRef);

impl PartialEq for StateEntry {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}
impl Eq for StateEntry {}
impl PartialOrd for StateEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for StateEntry {
    fn cmp(&self, _other: &Self) -> std::cmp::Ordering {
        std::cmp::Ordering::Equal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{praxis_alloc_int, praxis_int_load};
    use crate::context::{Runtime, RuntimeContext};

    /// A graph written down as a table, so a walk can be tested without a JIT.
    ///
    /// States are boxed `Int`s allocated from a real runtime — real `GcRef`s
    /// with real descriptors, so `DynamicKey` does the same structural
    /// comparison it does for a program's own states. The adjacency, weights,
    /// heuristic and goals are all keyed on the integer the state holds.
    struct Table {
        ctx: *mut RuntimeContext,
        edges: Vec<(i64, Vec<i64>)>,
        weights: Vec<((i64, i64), i64)>,
        heuristics: Vec<(i64, i64)>,
        goals: Vec<i64>,
        /// Set by `abort`; the fault the walk raised, which a real context
        /// would carry on its fault slot.
        raised: Option<FaultKind>,
        /// Every state handed to `retain`, in order — the rooting the ABI
        /// implementation performs.
        retained: Vec<i64>,
    }

    impl Table {
        fn value(&self, state: GcRef) -> i64 {
            // SAFETY: every state in these tests is an `Int` allocated below.
            unsafe { praxis_int_load(self.ctx, state) }
        }

        fn state(&self, n: i64) -> GcRef {
            // SAFETY: `ctx` is wired for the test's lifetime.
            unsafe { praxis_alloc_int(self.ctx, n) }
        }
    }

    impl GraphOracle for Table {
        fn neighbours(&mut self, state: GcRef) -> Result<Vec<GcRef>, Aborted> {
            let n = self.value(state);
            let out = self
                .edges
                .iter()
                .find(|(from, _)| *from == n)
                .map(|(_, to)| to.clone())
                .unwrap_or_default();
            Ok(out.into_iter().map(|m| self.state(m)).collect())
        }

        fn weight(&mut self, from: GcRef, to: GcRef) -> Result<i64, Aborted> {
            let pair = (self.value(from), self.value(to));
            Ok(self
                .weights
                .iter()
                .find(|(p, _)| *p == pair)
                .map(|(_, w)| *w)
                .unwrap_or(1))
        }

        fn heuristic(&mut self, state: GcRef) -> Result<i64, Aborted> {
            let n = self.value(state);
            Ok(self
                .heuristics
                .iter()
                .find(|(s, _)| *s == n)
                .map(|(_, h)| *h)
                .unwrap_or(0))
        }

        fn is_goal(&mut self, state: GcRef) -> Result<bool, Aborted> {
            Ok(self.goals.contains(&self.value(state)))
        }

        fn retain(&mut self, state: GcRef) {
            let n = self.value(state);
            self.retained.push(n);
        }

        fn abort(&mut self, kind: FaultKind) -> Aborted {
            self.raised = Some(kind);
            Aborted
        }
    }

    /// A runtime plus a leaked context, and the table over it. The runtime has
    /// to outlive every state, so both are returned together.
    ///
    /// The `Runtime` is **boxed**, and that is load-bearing: a context holds
    /// `&mut rt.heap` as a raw pointer, so returning an unboxed `Runtime` by
    /// value moves the heap out from under every context already minted from
    /// it. Boxing keeps the address stable across the move.
    fn table(edges: &[(i64, &[i64])]) -> (Box<Runtime>, Table) {
        let mut rt = Box::new(Runtime::new());
        let ctx: *mut RuntimeContext = Box::leak(Box::new(rt.context()));
        let t = Table {
            ctx,
            edges: edges
                .iter()
                .map(|(from, to)| (*from, to.to_vec()))
                .collect(),
            weights: Vec::new(),
            heuristics: Vec::new(),
            goals: Vec::new(),
            raised: None,
            retained: Vec::new(),
        };
        (rt, t)
    }

    fn values(t: &Table, states: &[GcRef]) -> Vec<i64> {
        states.iter().map(|s| t.value(*s)).collect()
    }

    /// The two orders are different walks over the same graph, and each one has
    /// to be the order it names. A diamond `1 -> {2, 3}`, both to `4`,
    /// distinguishes them: breadth-first is `1 2 3 4`, depth-first is
    /// `1 2 4 3`.
    #[test]
    fn breadth_first_and_depth_first_visit_in_the_orders_they_name() {
        let (_rt, mut t) = table(&[(1, &[2, 3]), (2, &[4]), (3, &[4]), (4, &[])]);
        let start = t.state(1);
        let bfs = bfs_order(&mut t, start).expect("no fault");
        assert_eq!(values(&t, &bfs), vec![1, 2, 3, 4]);

        let (_rt2, mut t2) = table(&[(1, &[2, 3]), (2, &[4]), (3, &[4]), (4, &[])]);
        let start2 = t2.state(1);
        let dfs = dfs_order(&mut t2, start2).expect("no fault");
        assert_eq!(values(&t2, &dfs), vec![1, 2, 4, 3]);
    }

    /// A depth-first walk descends into the *first* neighbour a state reports.
    /// The stack reverses the neighbour list, so a walk that pushed them in
    /// order would visit the last one first and still look plausible on a
    /// symmetric graph.
    #[test]
    fn a_depth_first_walk_takes_the_first_neighbour_first() {
        let (_rt, mut t) = table(&[(1, &[2, 3]), (2, &[]), (3, &[])]);
        let start = t.state(1);
        let order = dfs_order(&mut t, start).expect("no fault");
        assert_eq!(values(&t, &order), vec![1, 2, 3]);
    }

    /// A cycle terminates, and every state appears once. Without the visited
    /// set both walks run forever; with a set that is consulted but not
    /// *updated* on the queue path, a diamond enqueues its join twice.
    #[test]
    fn a_cycle_is_walked_once_and_terminates() {
        let (_rt, mut t) = table(&[(1, &[2]), (2, &[3]), (3, &[1, 2])]);
        let start = t.state(1);
        let bfs = bfs_order(&mut t, start).expect("no fault");
        assert_eq!(values(&t, &bfs), vec![1, 2, 3]);

        let (_rt2, mut t2) = table(&[(1, &[2]), (2, &[3]), (3, &[1, 2])]);
        let start2 = t2.state(1);
        let dfs = dfs_order(&mut t2, start2).expect("no fault");
        assert_eq!(values(&t2, &dfs), vec![1, 2, 3]);
    }

    /// A state with no neighbours is still reached: the walk answers with the
    /// start alone rather than with nothing.
    #[test]
    fn a_lone_state_is_its_own_walk() {
        let (_rt, mut t) = table(&[(1, &[])]);
        let start = t.state(1);
        let order = bfs_order(&mut t, start).unwrap();
        assert_eq!(values(&t, &order), vec![1]);

        let (_rt2, mut t2) = table(&[(1, &[])]);
        let start2 = t2.state(1);
        let reached = reachable(&mut t2, start2).unwrap();
        assert_eq!(values(&t2, &reached), vec![1]);
    }

    /// Identity is structural, not by pointer. Two separately allocated `Int`s
    /// holding `2` are the same state, so a graph whose neighbour function
    /// mints a fresh object per call still terminates — which is what every
    /// real neighbour closure does (`|p| [(p.0 + 1, p.1), …]` allocates).
    #[test]
    fn two_equal_states_are_one_state_however_they_were_allocated() {
        let (_rt, mut t) = table(&[(1, &[2]), (2, &[1])]);
        let start = t.state(1);
        let order = bfs_order(&mut t, start).expect("no fault");
        assert_eq!(values(&t, &order), vec![1, 2]);
        // The neighbour function allocated a fresh `1` on the second step, and
        // the walk recognized it as the state it started from.
        assert!(t.retained.len() >= 3, "the fresh states were retained");
    }

    /// Every state the walk remembers was handed to `retain` first. This is the
    /// rooting contract: a state in the visited set that the collector cannot
    /// see is a dangling reference the next allocation creates.
    #[test]
    fn every_remembered_state_was_retained_first() {
        let (_rt, mut t) = table(&[(1, &[2, 3]), (2, &[4]), (3, &[]), (4, &[])]);
        let start = t.state(1);
        let order = bfs_order(&mut t, start).expect("no fault");
        for state in &order {
            assert!(
                t.retained.contains(&t.value(*state)),
                "a visited state was never retained"
            );
        }
    }

    /// The distance is the number of *steps*, the start is zero steps away, and
    /// an unreachable goal is `None` rather than a sentinel.
    #[test]
    fn a_distance_counts_steps_and_absence_is_none() {
        let (_rt, mut t) = table(&[(1, &[2]), (2, &[3]), (3, &[]), (9, &[])]);
        t.goals = vec![3];
        let start = t.state(1);
        assert_eq!(bfs_distance(&mut t, start).unwrap(), Some(2));

        let (_rt2, mut t2) = table(&[(1, &[2]), (2, &[3]), (3, &[])]);
        t2.goals = vec![1];
        let start2 = t2.state(1);
        assert_eq!(
            bfs_distance(&mut t2, start2).unwrap(),
            Some(0),
            "a start that is already a goal is zero steps, not one"
        );

        let (_rt3, mut t3) = table(&[(1, &[2]), (2, &[])]);
        t3.goals = vec![99];
        let start3 = t3.state(1);
        assert_eq!(bfs_distance(&mut t3, start3).unwrap(), None);
    }

    /// A breadth-first distance is the *shortest* one. The long way round is
    /// enqueued first, so a walk that returned the first goal it enqueued
    /// rather than the first it dequeued would answer 3 here.
    #[test]
    fn a_distance_is_the_shortest_path_not_the_first_found() {
        let (_rt, mut t) = table(&[(1, &[2, 5]), (2, &[3]), (3, &[4]), (4, &[]), (5, &[4])]);
        t.goals = vec![4];
        let start = t.state(1);
        assert_eq!(bfs_distance(&mut t, start).unwrap(), Some(2));
    }

    /// The cost table holds the least cost to every reachable state, the start
    /// at zero, and nothing for what cannot be reached. The cheap three-hop
    /// path has to beat the expensive one-hop edge, which is the whole of
    /// Dijkstra and the half a step-counting BFS gets wrong.
    #[test]
    fn a_cost_table_prefers_a_cheap_long_path_to_an_expensive_short_one() {
        let (_rt, mut t) = table(&[(1, &[2, 4]), (2, &[3]), (3, &[4]), (4, &[]), (7, &[])]);
        t.weights = vec![((1, 4), 10), ((1, 2), 1), ((2, 3), 1), ((3, 4), 1)];
        let start = t.state(1);
        let costs = dijkstra_costs(&mut t, start).expect("no fault");
        let mut by_state: Vec<(i64, i64)> = costs.iter().map(|(s, c)| (t.value(*s), *c)).collect();
        by_state.sort_unstable();
        assert_eq!(by_state, vec![(1, 0), (2, 1), (3, 2), (4, 3)]);
        assert!(
            !by_state.iter().any(|(s, _)| *s == 7),
            "an unreachable state is absent, not present at some cost"
        );
    }

    /// A settled state is settled: a later, longer route to it does not add a
    /// second entry to the table.
    #[test]
    fn each_state_is_settled_once() {
        let (_rt, mut t) = table(&[(1, &[2, 3]), (2, &[4]), (3, &[4]), (4, &[])]);
        let start = t.state(1);
        let costs = dijkstra_costs(&mut t, start).expect("no fault");
        assert_eq!(costs.len(), 4, "one entry per reachable state");
    }

    /// A negative edge weight faults rather than answering. Dijkstra never
    /// reconsiders a settled state, so a negative edge makes the answer quietly
    /// too large — and the same refusal covers A\*, which settles the same way.
    #[test]
    fn a_negative_edge_weight_faults_rather_than_answering() {
        let (_rt, mut t) = table(&[(1, &[2]), (2, &[])]);
        t.weights = vec![((1, 2), -1)];
        let start = t.state(1);
        assert_eq!(dijkstra_costs(&mut t, start), Err(Aborted));
        assert_eq!(t.raised, Some(FaultKind::InvalidSize));

        let (_rt2, mut t2) = table(&[(1, &[2]), (2, &[])]);
        t2.weights = vec![((1, 2), -1)];
        t2.goals = vec![2];
        let start2 = t2.state(1);
        assert_eq!(a_star_cost(&mut t2, start2), Err(Aborted));
        assert_eq!(t2.raised, Some(FaultKind::InvalidSize));
    }

    /// A path whose cost leaves the `Int` range faults rather than wrapping —
    /// the rule ADR-058 applied to `abs(Int::MIN)`, at the one place a walk
    /// does arithmetic the program did not write.
    #[test]
    fn a_cost_with_no_int_faults_rather_than_wrapping() {
        let (_rt, mut t) = table(&[(1, &[2]), (2, &[3]), (3, &[])]);
        t.weights = vec![((1, 2), i64::MAX), ((2, 3), 1)];
        let start = t.state(1);
        assert_eq!(dijkstra_costs(&mut t, start), Err(Aborted));
        assert_eq!(t.raised, Some(FaultKind::IntOverflow));
    }

    /// A\* answers the cheapest cost to a goal, and the heuristic only changes
    /// the order states are examined in — not the answer. The same graph is
    /// searched twice, once with a zero heuristic (which is Dijkstra) and once
    /// with an exact one.
    #[test]
    fn a_star_finds_the_cheapest_goal_whatever_the_heuristic_estimates() {
        let edges: &[(i64, &[i64])] = &[(1, &[2, 4]), (2, &[3]), (3, &[4]), (4, &[])];
        let weights = vec![((1, 4), 10), ((1, 2), 1), ((2, 3), 1), ((3, 4), 1)];

        let (_rt, mut t) = table(edges);
        t.weights = weights.clone();
        t.goals = vec![4];
        let start = t.state(1);
        assert_eq!(a_star_cost(&mut t, start).unwrap(), Some(3));

        let (_rt2, mut t2) = table(edges);
        t2.weights = weights;
        t2.goals = vec![4];
        // An exact remaining-cost estimate: still admissible, so still 3.
        t2.heuristics = vec![(1, 3), (2, 2), (3, 1), (4, 0)];
        let start2 = t2.state(1);
        assert_eq!(a_star_cost(&mut t2, start2).unwrap(), Some(3));
    }

    /// An unreachable goal is `None`, and a start that is already a goal costs
    /// nothing.
    #[test]
    fn a_star_answers_nothing_for_an_unreachable_goal() {
        let (_rt, mut t) = table(&[(1, &[2]), (2, &[])]);
        t.goals = vec![99];
        let start = t.state(1);
        assert_eq!(a_star_cost(&mut t, start).unwrap(), None);

        let (_rt2, mut t2) = table(&[(1, &[2]), (2, &[])]);
        t2.goals = vec![1];
        let start2 = t2.state(1);
        assert_eq!(a_star_cost(&mut t2, start2).unwrap(), Some(0));
    }

    /// A negative heuristic faults. It is the one caller error A\* *can* see:
    /// an inadmissible-but-positive estimate produces a wrong answer nothing
    /// can detect, while a negative one breaks the ordering the search is built
    /// on and is one comparison away.
    #[test]
    fn a_negative_heuristic_faults_rather_than_misordering_the_search() {
        let (_rt, mut t) = table(&[(1, &[2]), (2, &[])]);
        t.goals = vec![2];
        t.heuristics = vec![(1, -5)];
        let start = t.state(1);
        assert_eq!(a_star_cost(&mut t, start), Err(Aborted));
        assert_eq!(t.raised, Some(FaultKind::InvalidSize));
    }
}
