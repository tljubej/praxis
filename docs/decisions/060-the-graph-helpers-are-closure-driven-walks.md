# ADR-060: The graph helpers are closure-driven walks, and their state is a value that can be remembered

**Date:** 2026-07-30
**Status:** Accepted — implemented
**Milestone:** Repair (stage S17 — TY-33, unit 3 of 4)

## Context

§16.1's last prelude line is `bfs bfs_distance dfs dijkstra a_star flood_fill`.
All six were strings and nothing else: the name resolved (it is in `PRELUDE`),
inference gave it a **fresh type variable**, and the call then lowered as
`CallTarget::User("bfs")` — a direct call to a function nobody defined. That is
TY-33, and these were the last six names in that state. It had the two symptoms
the other nine had:

```praxis
fn main() -> Text { bfs("start") }     // accepted: one argument, any type
```

```text
$ praxis run walk.px
error: JIT compilation failed: Cranelift error: unresolved user function `bfs`
```

D5 answered "implement all fifteen" against its own recommendation to delete
these six, and then said the thing that made this a unit of its own: **"They
need a graph *representation* decision first — what a caller passes as the
neighbour function, and whether `dijkstra`/`a_star` take edge weights as a
closure or a `Map`. That decision is not in this plan and is owed before the
unit starts."**

It is owed no longer, and the answer was already written down. §6.5 states it in
one sentence and one example.

## Decision 1: the graph is a closure, and there is no graph object

§6.5: "Provide closure-based algorithms that do not require materializing a
graph object", followed by

```praxis
let distance = bfs_distance(
    start,
    |state| neighbors(state),
    |state| state == goal,
)
```

So every helper takes a **start state** and then only **functions of it**. That
is the shape, and it settles both of D5's questions at once: the neighbour
function is a closure `(T) -> Vec[T]`, and `dijkstra`/`a_star` take their edge
weights as a closure too.

The two alternatives were considered and are worse *today*, not in principle:

- **A `Map[T, Vec[T]]` adjacency table** needs `Map.get`'s contract settled
  first, which is **D1** and is still open (S18). A helper that read an
  adjacency map would have to decide what an absent key means before the
  language has.
- **A `Grid` plus an implicit 4-/8-neighbourhood** needs one name to accept two
  argument shapes, which is arity- or type-based overloading. The language does
  not have it — the same wall ADR-056 hit when `assert` could not take an
  optional message — and a `Grid` convenience is a *second* name, not a second
  signature, whenever a milestone wants one. `grid.neighbors4(p)` already
  exists, so `bfs(start, |p| grid.neighbors4(p))` is what that program writes
  meanwhile.

The closure form also needs nothing new: §4.10's closures, HIR-08's captures and
`Inst::CallIndirect` all work today, and a closure value is the only runtime
representation a `Func` has.

| Name | Scheme |
|---|---|
| `bfs` | `forall T. (T, (T) -> Vec[T]) -> Vec[T]` |
| `dfs` | `forall T. (T, (T) -> Vec[T]) -> Vec[T]` |
| `flood_fill` | `forall T. (T, (T) -> Vec[T]) -> Set[T]` |
| `bfs_distance` | `forall T. (T, (T) -> Vec[T], (T) -> Bool) -> Option[Int]` |
| `dijkstra` | `forall T. (T, (T) -> Vec[T], (T, T) -> Int) -> Map[T, Int]` |
| `a_star` | `forall T. (T, (T) -> Vec[T], (T, T) -> Int, (T) -> Int, (T) -> Bool) -> Option[Int]` |

The goal is a **predicate**, not a goal value, because §6.5's own example writes
one — and because a search that stops on a property (`|s| s.x == n`) is the
common case, while a search for a specific value is `|s| s == goal`.

`a_star`'s five arguments are the honest count: A\* is a start, a graph, a cost,
an estimate and a goal, and none of them has a default the caller would not have
to think about. The order is the same as `bfs_distance`'s with the extra two
inserted where they belong — start, neighbours, [weight], [heuristic], goal — so
the family reads as one family.

`praxis-stdlib::GRAPH_HELPERS` states each row as *shapes* (`GraphParam`,
`GraphResult`) rather than as types, because that crate cannot name a `Type`.
Inference matches on them exhaustively, so a new shape is a compile error at the
one place that builds the scheme, and the arity that follows from `params` is
checked against the wrapper's own arity by a unit test — as close to ADR-058's
"the arity is the wrapper's arity" as a family with five distinct signatures can
get.

## Decision 2: a search that finds nothing answers `Option[Int]`

"No path" is an ordinary outcome of a search, not a fault and not a number. The
two goal-directed helpers answer `Option[Int]`; the three exhaustive walks need
no `Option` at all because they always contain the state they started from, and
`dijkstra` needs none because an unreachable state is simply **absent** from its
table.

The alternative was a `-1` sentinel, and ADR-058 already ruled on that shape: a
number nobody wrote is worse than a stop. `Option[T]` is real (F12), `Some`/`None`
are seeded, and S16 made matching on them exhaustive-checked — so the honest
signature costs a caller one `match` and buys a checked one.

This does **not** pre-empt D1. D1 asks whether `Map.get`/`Grid.find` should
*change* from `V`-with-`Unit` to `Option[V]`, which breaks every program that
uses them. These six names are new: there is nothing to break, and picking the
answer D1 recommends for a name that has no callers yet is the cheap direction
to be wrong in.

The `Option` the runtime builds is an ordinary one. `praxis_alloc_enum` writes
the same `EnumPayload` the codegen writes for a `Some(x)` a program spells, at
the same variant tags `TypeDb::new` declares (`Some` = 0, `None` = 1), so it
matches against the same arms. Those two tags are now written down in
`praxis-runtime` as well as in `praxis-types`, which is a duplication with no
shared crate to hold it; the JIT gate that matches a runtime-built `Option`
against `Some`/`None` arms is what keeps the two honest.

## Decision 3: the state type must be one the walk can remember

Every walk keeps a visited set, and the weighted ones keep a cost table keyed on
the state. So a state must be a `Set` element and a `Map` key, which is
`CapKind::HashStable` — and D4's rule reaches these six unchanged: a value that
can change after it is stored cannot be found again, so the walk would visit it
forever.

The requirement is emitted **at the call site**, through `Inferer::require_cap`,
rather than claimed by the helper's own scheme. Both routes ride F10's channel;
the difference is the diagnostic. A constraint claimed by a *prelude name's*
scheme has no source span of its own to point its "this is the operation that
requires it" note at — the requirement is not written anywhere in the user's
file. Emitted at the call, it reports at the call and needs no note.

It is still deferred when it has to be. `fn walk(start, step) { bfs(start, step) }`
constrains a variable, so the requirement goes on the channel, is claimed by
`walk`'s own scheme, and is answered at each call to `walk` — which is the whole
of F10 and is what D5 meant by "a graph helper's signature is where the channel
gets its hardest test".

`Iterable` and `HasMethod` are **not** emitted. The signature says `Vec[T]`
outright, so nothing about the neighbour function's result is deferred, and no
helper calls a method on its state.

## Decision 4: the walks do not call closures; an oracle does

`praxis_runtime::graph` owns the six algorithms and never touches a closure. It
asks a `GraphOracle` — `neighbours`, `weight`, `heuristic`, `is_goal`, plus
`retain` and `abort` — and `praxis_runtime::abi::ClosureOracle` is the one
implementation that transmutes a JIT'd function pointer.

That split is what makes the algorithms testable. Calling a closure needs a
compiler, a JIT and a live context; a table-backed oracle needs none, so
"`dijkstra` settles a state once", "a descending stack visits the first
neighbour first" and "a negative weight faults" are unit tests over real
`GcRef`s with real descriptors. The end-to-end gates in `jit.rs` then cover the
half the oracle abstracts away: that the wrapper behind the name really calls
the program's closure and reads the `Vec` it hands back.

Calling convention: a closure's entry point is `fn(ctx, closure_self, params…)`
(§4.10, Approach B), which the runtime reaches by `transmute` — the same thing
the debugger's `call_with_arity` does for a synthetic `__p_expr` entry, and the
same thing the CLI does for `main`. The descriptor is checked first: the
alternative to a `TypeMismatch` fault is transmuting whatever the payload's
first word happens to be and jumping to it.

**A fault raised inside a closure stops the walk.** Every oracle call checks
`praxis_check_fault` afterwards and returns `Aborted`; the wrapper then returns
the Unit sentinel and the call site's own `CheckFault` branches to its fault
path. Without it the walk would keep going over a graph of Unit sentinels.

## Decision 5: an answer the walk cannot compute is a fault, not a wrong number

| Refusal | Kind | Why |
|---|---|---|
| a negative edge weight | `InvalidSize` | Dijkstra and A\* settle a state the first time they pop it and never reconsider, so a negative edge makes the answer quietly too *large* |
| a negative heuristic | `InvalidSize` | it makes `f = g + h` decrease along a path, which is the ordering the search is built on |
| a path cost with no `Int` | `IntOverflow` | ADR-058's rule at the one place a walk does arithmetic the program did not write |
| a `Func` operand that is not a closure | `TypeMismatch` | the alternative is jumping to a word that is not a function pointer |
| a closure result of the wrong type | `TypeMismatch` | reading an `Int` payload out of something else |

An *inadmissible but non-negative* heuristic is the caller error A\* cannot see,
and it is not diagnosed. That is stated rather than hidden: the search's contract
is that the estimate never exceeds the true remaining cost, and checking it would
mean computing the answer twice.

> **Paid in S18 (ADR-075).** Both rows are now `FaultKind::NoAnswer`, the kind
> this section's own heading asked for.

`InvalidSize` is borrowed twice more here, and the debt ADR-058 recorded grows
rather than shrinking. Its doc already covers "an argument the runtime cannot
honour"; a negative weight and a negative heuristic are that, and S17 has no ABI
bump left (H17 — ADR-056 spent it). **The next stage that spends a bump should
give this family a kind of its own** alongside the empty-range kind ADR-058 and
ADR-059 both owe.

## Consequences

- **`RUNTIME_ABI_VERSION` stays 13.** Six new symbols are additive — a row in the
  manifest and an arm in `praxis_runtime::abi::address`, both exhaustive
  matches — and no `#[repr(C)]` layout changed.
- **`seed_builtin_schemes` now covers every prelude name that denotes a value.**
  A name absent from it gets a fresh variable, which is the bug and not the
  default; there are none left absent. TY-33 is closed.
- **`praxis_runtime::graph` is a new module and the first place the runtime calls
  *back* into generated code.** Everything before it was a leaf. The two
  consequences are recorded where they live: the native root frame is what keeps
  a Rust-held state alive across a closure's allocations, and a pending fault
  after a call back is what stops the walk.
- **A walk's working set is rooted for the walk's duration.** Every state ever
  seen is in the `NativeScope`, which is the same set the algorithm holds
  anyway — so the rooting costs a `Vec<GcRef>` alongside the visited set and
  nothing asymptotic.
- **A tuple state cannot be *used* yet, though it is a legal one.** The type
  system accepts `(Int, Int)` as a state (it is `HashStable`), and the runtime
  keys on it correctly, but the language has no tuple element syntax — `p.0` is
  a parse error — so a neighbour function cannot read one. A record of scalars
  is what a grid position is written as meanwhile, and the gates use one. That
  gap is not this unit's and no finding covers it.
- **A top-level `fn` used as a value still crashes the host**, and a graph helper
  is a new way to reach it. Inference accepts `bfs(1, steps)` — a `fn`'s type
  *is* a `Func` — and lowering then evaluates the bare name to **`Unit`**, not to
  a closure. Through `Inst::CallIndirect` (`apply(double, 3)`) that Unit's
  payload is read as a function pointer and the host takes a SIGBUS; through a
  graph helper the descriptor check turns it into a `TypeMismatch` fault with the
  program's own backtrace. That is containment, not a fix — the bug is that the
  name evaluates to `Unit` at all. No finding covers it; see the progress note.
