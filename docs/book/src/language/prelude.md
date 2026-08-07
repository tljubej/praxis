# The prelude

Thirty-one names are bound in every Praxis file before the first line runs.
There is no import, no `use`, and no way to get more: a program is one file, and
the prelude is the whole free-function surface of the language. Everything else
is a method, and those are in [the method catalog](method-catalog.md).

The list lives in `crates/praxis-stdlib/src/prelude.rs`, which is the same table
the type checker and the editor read. It falls into five groups.

## Output and control

| Name | Signature | What it does |
|---|---|---|
| `out` | `(T) -> Unit` | Write one value to stdout, followed by a newline. |
| `dbg` | `(T) -> T` | Write one value to **stderr** and return it unchanged. |
| `panic` | `(T) -> Never` | Stop with an explicit message and raise a fault. |
| `assert` | `(Bool) -> Unit` | Stop if the condition is false. |

`out`, `dbg` and `panic` take any type and render it through the value's own
formatter, so `panic(candidate)` says what the candidate was:

```praxis
out(42)
out("a line")
out([1, 2, 3])
out((1, "x"))
out(Some(3))

var squares = Map()
squares.insert(2, 4)
squares.insert(3, 9)
out(squares)
```

```text
42
a line
[1, 2, 3]
(1, x)
Some(3)
{2: 4, 3: 9}
```

`dbg` is the identity on types, which is what lets it wrap any subexpression
without changing what the program computes — including one whose value you then
`panic` on. Both halves of this go to stderr:

```praxis
var doubled = dbg(21) * 2
panic(doubled)
```

```text
21
error: program faulted: panic: 42

Backtrace:
#0   <entry>

  locals:
    doubled: Int = 42
  temps:
    <tmp#1: Int> @ "21" = 21
    <tmp#2: Int> @ "dbg(21)" = 21
    <tmp#3: Int> @ "2" = 2
    <tmp#4: Int> @ "dbg(21) * 2" = 42
    <tmp#6> @ "panic(doubled)" = <uninit>
```

`panic`'s result type is `Never`, so a function can end on one and still satisfy
a declared result type: `fn pick(v: Vec[Int]) -> Int { if v.is_empty() {
panic("no candidates") }; v[0] }` type-checks, and so does `fn boom() -> Int {
panic("x") }`.

`assert` is the one name here that is monomorphic. It takes a `Bool` and nothing
else, so `assert(1)` is a type error rather than a call that silently accepts
anything, and it takes exactly one argument — a message parameter has no
spelling, because a name in Praxis has exactly one signature
([ADR-089](../../../decisions/089-a-name-has-one-signature.md)).

```praxis
assert(1 + 1 == 3)
```

```text
error: program faulted: assertion failed

Backtrace:
#0   <entry>

  temps:
    <tmp#1: Int> @ "1" = 1
    <tmp#2: Int> @ "1" = 1
    <tmp#3: Int> @ "1 + 1" = 2
    <tmp#4: Int> @ "3" = 3
    <tmp#5: Bool> @ "1 + 1 == 3" = false
    <tmp#6: Unit> @ "assert(1 + 1 == 3)" = <uninit>
```

Both `panic` and `assert` raise ordinary faults, which is why the output above
carries a backtrace and the locals. Under the default `--debug auto` — stdin and
stdout both a terminal — they drop you into [the crash
debugger](../debugger/entering.md) instead of printing. That is the reason they
are faults rather than a write to stderr followed by an exit
([ADR-056](../../../decisions/056-the-prelude-control-names-are-real-functions.md)).

## Numeric helpers

Seven functions on `Int`, and two nullary `Float` functions.

| Name | Signature | What it does |
|---|---|---|
| `abs` | `(Int) -> Int` | Absolute value. Faults on `Int`'s minimum, which has no positive counterpart. |
| `sign` | `(Int) -> Int` | `-1`, `0` or `1`. Total. |
| `min` | `(Int, Int) -> Int` | The smaller of two. |
| `max` | `(Int, Int) -> Int` | The larger of two. |
| `clamp` | `(Int, Int, Int) -> Int` | `clamp(value, low, high)`. Faults if `low > high`. |
| `gcd` | `(Int, Int) -> Int` | Non-negative greatest common divisor. `gcd(0, 0)` is `0`. |
| `lcm` | `(Int, Int) -> Int` | Non-negative least common multiple; `0` if either operand is `0`. Faults if the result leaves `Int`. |
| `pi` | `() -> Float` | π. |
| `e` | `() -> Float` | Euler's number. |

`pi` and `e` are nullary functions, not bare constants: `pi()`, not `pi`.

```praxis
out(abs(-7))
out(sign(-7))
out(min(3, 9))
out(max(3, 9))
out(clamp(12, 0, 10))
out(gcd(12, 18))
out(lcm(4, 6))
out(pi())
out(e())
```

```text
7
-1
3
9
10
6
12
3.141592653589793
2.718281828459045
```

**All seven are `Int` functions and none of them is generic**
([ADR-058](../../../decisions/058-the-numeric-prelude-helpers-are-int-functions.md)).
`Float` carries its own `abs`, `sign`, `min` and `max` as methods — `x.abs()`,
`x.min(y)` — so the free function never has to choose a lowering per
instantiation. `clamp`, `gcd` and `lcm` have no `Float` counterpart at all;
`(2.5).clamp(0.0, 1.0)` is a `Y110`. Handing a `Float` to one of the free
functions is an ordinary type error:

```console
$ praxis check prelude-min-is-int.px
error[Y001]: expected (Int, Int) -> Int, found (Float, Float) -> ?T

  prelude-min-is-int.px:1:5
  1 | out(min(1.0, 2.0))
    |     ^^^^^^^^^^^^^ expected (Int, Int) -> Int, found (Float, Float) -> ?T

praxis: 1 error(s)
```

## Collection constructors

Nine names. Called with no arguments, each builds an empty collection and the
element type comes from what you then put in. Two of them — `Vec` and `Grid` —
also take a **size and a fill**, and the argument count is what chooses between
the two shapes
([ADR-146](../../../decisions/146-a-collection-constructors-arity-is-its-shape.md)).

| Name | Signature | Notes |
|---|---|---|
| `Vec` | `() -> Vec[T]` | Ordered, growable, indexed from 0. |
| `Vec` | `(Int, T) -> Vec[T]` | `n` slots, every one the fill. |
| `Deque` | `() -> Deque[T]` | Double-ended queue. |
| `Map` | `() -> Map[K, V]` | Hash map. |
| `Set` | `() -> Set[T]` | Hash set. |
| `Counter` | `() -> Counter[T]` | A map whose absent values read as zero. |
| `MinHeap` | `() -> MinHeap[T]` | Priority queue, smallest first. |
| `MaxHeap` | `() -> MaxHeap[T]` | Priority queue, largest first. |
| `Grid` | `() -> Grid[T]` | 2D grid. `Grid()` is the empty 0 × 0 one. |
| `Grid` | `(Int, Int, T) -> Grid[T]` | A `w` × `h` board, every cell the fill. |
| `BitSet` | `() -> BitSet` | Compact set of non-negative integers. Takes no type argument. |

**Only those two are sized**, and the rest of the language has no arity
overloading at all — `Set(3, 0)` is an error that says the function takes zero
arguments. The two exceptions are the collections whose contents are addressed
by position, which is what makes "n of them" mean something;
[ADR-146](../../../decisions/146-a-collection-constructors-arity-is-its-shape.md)
records why that is a deliberate narrowing of
[ADR-089](../../../decisions/089-a-name-has-one-signature.md) rather than a hole
in it.

```praxis
var v = Vec()
v.push(1)

var d = Deque()
d.push_front("front")

var m = Map()
m.insert("k", 1)

var s = Set()
s.insert(3)

var c = Counter()
c.inc("x")

var lo = MinHeap()
lo.push(5)

var hi = MaxHeap()
hi.push(5)

var g = Grid()

var sized = Vec(3, 0)
var board = Grid(3, 2, '.')

var b = BitSet()
b.insert(3)

out(v)
out(d)
out(m)
out(s)
out(c)
out(lo.peek())
out(hi.peek())
out(g.width())
out(b)
out(sized)
out(board)
out(board.width())
```

```text
[1]
[front]
{k: 1}
{3}
{x: 1}
5
5
0
{3}
[0, 0, 0]
[., ., ., ., ., .]
3
```

`[1, 2, 3]` is a `Vec` literal, so `Vec()` is only needed when you want an empty
one. The tenth collection, `Range`, has **no constructor**: a range is written
`0..n` or `0..=n`, and `Range` is a type name rather than a value — `Range()` is
`N001: 'Range' is not defined`.

A `Map` key, a `Set` element and a `Counter` key have to be usable as keys, and a
heap element has to be orderable — but the constructor is not where that is
asked. The requirement is checked at the first method call on the collection,
because that is where a program actually puts a value into one, so the
construction below is accepted and the `len()` on the next line is what is
refused:

```praxis
var seen: Set[Vec[Int]] = Set()
out(seen.len())
```

```console
$ praxis check prelude-key-bound.px
error[Y014]: a value of type `Vec[Int]` can change after it is stored, so it cannot be used as a key

  prelude-key-bound.px:2:10
  2 | out(seen.len())
    |          ^^^ a value of type `Vec[Int]` can change after it is stored, so it cannot be used as a key

help: use a value that cannot change — a number, `Text`, or a tuple of those

praxis: 1 error(s)
```

See [capabilities](../types/capabilities.md).

## Optionality

| Name | Signature | What it is |
|---|---|---|
| `Option` | — | The type name. `Option[T]` is a legal annotation. |
| `Some` | `(T) -> Option[T]` | Wrap a value. |
| `None` | `Option[T]` | The absent value. Not a call — `None`, never `None()`. |

`Option[T]` is domain-level absence and not an error channel. It is what
`Map.get`, `Grid.find`, the sequence `find`/`position`, the `checked_*`
arithmetic methods and the goal-directed graph walks answer with.

```praxis
var seen = Map()
seen.insert("a", 1)

var found: Option[Int] = seen.get("a")
var missing = seen.get("z")

out(found)
out(missing)

match missing {
  Some(n) => out(n)
  None => out("nothing there")
}
```

```text
Some(1)
None
nothing there
```

`Some` and `None` are the only enum variants the prelude declares; everything
else about them is what [enums](enums.md) says about any variant.

## Graph helpers

Six closure-driven walks. None of them takes a graph object — there is no graph
type — so a program describes its graph by giving a start state and a function
from a state to its neighbours
([ADR-060](../../../decisions/060-the-graph-helpers-are-closure-driven-walks.md)).

| Name | Signature | Answers |
|---|---|---|
| `bfs` | `(T, (T) -> Vec[T]) -> Vec[T]` | Every state reached, in breadth-first order. |
| `dfs` | `(T, (T) -> Vec[T]) -> Vec[T]` | Every state reached, in depth-first order. |
| `flood_fill` | `(T, (T) -> Vec[T]) -> Set[T]` | Every state reached, unordered. |
| `bfs_distance` | `(T, (T) -> Vec[T], (T) -> Bool) -> Option[Int]` | Steps to the first goal state, or `None`. |
| `dijkstra` | `(T, (T) -> Vec[T], (T, T) -> Int) -> Map[T, Int]` | Least cost to each reachable state. An unreachable state is simply absent. |
| `a_star` | `(T, (T) -> Vec[T], (T, T) -> Int, (T) -> Int, (T) -> Bool) -> Option[Int]` | Cost of the cheapest path to a goal, or `None`. |

The first parameter is always the start state and every other parameter is a
function of it. The weight function takes two adjacent states, the heuristic
takes one state and estimates the remaining cost, and the goal is a *predicate*
rather than a value, so a search can stop on a property.

Only the two goal-directed helpers answer with an `Option`, and that pairing is
the rule: a walk that always reaches at least its own start cannot fail, and
`dijkstra` needs no `Option` because "unreachable" is absence from its table.

```praxis
fn steps(n: Int) -> Vec[Int] {
  var next = Vec()
  if n * 2 <= 20 { next.push(n * 2) }
  if n + 1 <= 20 { next.push(n + 1) }
  next
}

out(bfs(1, |n| steps(n)).len())
out(dfs(1, |n| steps(n)).len())
out(flood_fill(1, |n| steps(n)).len())
out(bfs_distance(1, |n| steps(n), |n| n == 20))
out(dijkstra(1, |n| steps(n), |a, b| b - a).len())
out(a_star(1, |n| steps(n), |a, b| b - a, |n| 20 - n, |n| n == 20))
```

```text
20
20
20
Some(5)
20
Some(19)
```

Every walk remembers where it has been, so the **state type has to be usable as
a key**: a number, a `Text`, a `Char`, a tuple of those, or a record or enum of
those. A `Vec` state is refused at the call site, with the reason rather than
the rule:

```console
$ praxis check prelude-graph-state.px
error[Y014]: a value of type `Vec[Int]` can change after it is stored, so it cannot be used as a key

  prelude-graph-state.px:7:5
  7 | out(bfs([1], |s| step(s)).len())
    |     ^^^^^^^^^^^^^^^^^^^^^ a value of type `Vec[Int]` can change after it is stored, so it cannot be used as a key

help: use a value that cannot change — a number, `Text`, or a tuple of those

praxis: 1 error(s)
```

## Which of these can fault

Most prelude names cannot fail. The ones that can:

| Name | Fault | When |
|---|---|---|
| `panic` | panic | Always. That is the point. |
| `assert` | assertion failed | The condition was false. |
| `abs` | integer overflow | The argument is `Int`'s minimum. |
| `clamp` | empty range | `low > high`. There is no value to answer with, and inventing one would be a guess. |
| `gcd` | integer overflow | Only for `Int`'s minimum with itself, whose answer is 2⁶³. |
| `lcm` | integer overflow | The multiple does not fit an `Int`, which happens easily. |
| the six graph walks | whatever the closures raise | Your neighbour, weight, heuristic or goal function faulted. |
| `Vec(n, fill)` | size or extent out of range | `n` is negative, or larger than the runtime will allocate at a stroke (2²⁸). |
| `Grid(w, h, fill)` | size or extent out of range | An extent is negative, or `w × h` is past 2²⁸ cells. |

`out`, `dbg`, `sign`, `min`, `max`, `pi`, `e` and `Some` cannot fault, and
neither can any collection constructor called with **no arguments** — there is
nothing you gave it for it to refuse. The two sized forms are the exception, and
the size is the reason: it is an ordinary `Int` computed at run time, so a
negative or absurd one cannot be caught at `praxis check` and is a fault instead
([ADR-041](../../../decisions/041-bounded-extents-fault-instead-of-aborting.md)).
See [the fault model](../debugger/faults.md) for what happens after a fault.

## They are ordinary bindings

A prelude name is a normal binding in the file's root scope, so a `var` of the
same name shadows it for the rest of the file, exactly as any other shadow
works:

```praxis
var max = 10
out(max + 1)
```

```text
11
```

Worth knowing mostly so that "`max` is not a function" stops being mysterious
once you have used the name for something else.

## What is not here

The type names `Int`, `Text`, `Bool`, `Char`, `Float`, `Unit` and `Never` are
also in scope, as annotations. `UInt` and `Byte` are named in the design
document and are **not** implemented: `var x: UInt = 1` is `N002: unknown type
'UInt'`.

The design document's prelude list is the four control names, the seven numeric
helpers, the nine collection constructors and the six graph walks. The
implementation adds `pi`, `e`, `Option`, `Some` and `None`, and takes nothing
away.
