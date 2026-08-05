# Grids and graphs

Two things live in this chapter. `Grid[T]` is the rectangular two-dimensional
collection: fixed width and height, one `T` per cell, indexed `grid[x, y]`. And
the prelude has six graph walks — `bfs`, `bfs_distance`, `dfs`, `dijkstra`,
`a_star`, `flood_fill` — which take a start state and a closure that answers
"what is next to here?". They are related because most puzzle graphs *are* a
grid, but neither needs the other: a walk never sees a grid, and a grid knows
nothing about search.

## Coordinates

`(x, y)`, with `x` increasing rightward and `y` increasing downward. `x` is the
column and `y` is the row, so `grid[0, 2]` is the leftmost cell of the third
line of input. Every position a grid method hands back is an `(Int, Int)` tuple
in that order.

## Where a grid comes from

From the input parser, in practice. `grid(P)` reads one cell per character of
each line; `matrix(P)` reads one cell per whitespace-separated token. Both are
covered in [structural parsers](../input/structural.md).

`Grid()` exists as a prelude name and builds a 0×0 grid, which is not useful for
much: it takes no arguments, so there is no way to ask for a sized one, and
there is no `to_vec`-style `to_grid` on any sequence. A grid is something you
read, and then index, mutate and rotate.

```praxis
// `read grid(char)` makes every character of every line a cell.
var map = read grid(char)
var rock = "#"[0]

out(map.width())
out(map.height())

// `map[x, y]` is column `x` of row `y`: x rightward, y downward.
// `map.get(x, y)` is the same read spelled as a method.
out(map[0, 0])
out(map.get(3, 1))

// Whether a position is inside is a question, not a fault.
out(map.contains(3, 1))
out(map.contains(9, 0))

// A row, a column, and the whole thing flattened in row-major order.
out(map.row(1))
out(map.column(0))
out(map.cells())

// A `for` walks the cells in that same order.
var rocks = 0
for cell in map {
    if cell == rock { rocks = rocks + 1 }
}
out(rocks)

// A grid is mutable in place; `map.set(x, y, v)` is the same store.
map[0, 0] = rock
map.set(0, 2, rock)
out(map.row(0))
out(map.row(2))
```

On the input

```text
..#.
#...
.#.#
```

that prints

```text
4
3
.
.
true
false
[#, ., ., .]
[., #, .]
[., ., #, ., #, ., ., ., ., #, ., #]
4
[#, ., #, .]
[#, #, ., #]
```

There is no `Char` literal, which is why the wall is written `"#"[0]` — a
one-character `Text` subscripted at 0
([ADR-107](../../../decisions/107-a-small-char-is-one-object-and-there-is-no-char-literal.md),
[ADR-086](../../../decisions/086-a-text-subscript-answers-a-char.md)). See
[Text and Char](text.md).

Note what a grid does *not* have. There is no `len()`: "how many" would have to
choose between cells and rows, and §6.4's method list does not pick one, so
`map.len()` is a `Y110` — no such method on `Grid[Char]`. A grid is
also not one of the ten [pipeline](pipelines.md) receivers, so `map.map(f)` and
`map.filter(p)` do not resolve either — `map.cells()` is the bridge, and
`map.cells().filter(p).count()` is one fused loop over a `Vec[T]`
([ADR-127](../../../decisions/127-a-pipelines-source-is-the-for-loops-and-a-collection-converts-by-naming-what-it-becomes.md)).
A bare `for` over the grid itself works and yields cells in row-major order,
which is the case that mattered.

## Positions and neighbours

```praxis
var g = read grid(char)
var blank = "."[0]

// Every position, row-major, as `(x, y)` tuples.
out(g.positions())

// Where a value is: the first one as `Option[(Int, Int)]`, or all of them.
out(g.find(blank))
out(g.find_all(blank))
out(g.find("z"[0]))

// The in-bounds neighbours of a point. Both take one `(Int, Int)` and
// answer a `Vec[(Int, Int)]`, already clipped to the grid.
out(g.neighbors4((0, 0)))
out(g.neighbors8((0, 0)))
out(g.neighbors4((1, 1)))

// A pattern opens a tuple by binding both elements at once.
for (x, y) in g.neighbors4((1, 1)) {
    out(g[x, y])
}

// `p.0` and `p.1` select one element by position.
var p = g.neighbors4((1, 1))[0]
out(p.0)
out(p.1)
```

On the input

```text
ab.
cde
.fg
```

that prints

```text
[(0, 0), (1, 0), (2, 0), (0, 1), (1, 1), (2, 1), (0, 2), (1, 2), (2, 2)]
Some((2, 0))
[(2, 0), (0, 2)]
None
[(0, 1), (1, 0)]
[(1, 0), (0, 1), (1, 1)]
[(1, 0), (1, 2), (0, 1), (2, 1)]
b
f
c
e
1
0
```

The neighbour lists are already clipped, so a corner gets two entries and not
four — no bounds test of your own. `neighbors4` runs up, down, left, right;
`neighbors8` runs the 3×3 block row-major with the centre skipped. `find`
answers `Option[(Int, Int)]`, so a value that is not there is `None` rather than
a position nobody wrote; `find_all` needs no `Option` because an empty `Vec`
already says it.

There are two ways to take a `(Int, Int)` apart. `p.0` and `p.1` select an
element by position, and an index past the end is a `Y019` at check time rather
than a run-time surprise. A pattern takes the whole tuple at once — in a `for`
binding as above, or in a [`match`](pattern-matching.md). If you want names, use
a [record](records.md) as your position instead; both work everywhere a tuple
does.

## Turning a grid

```praxis
var g = read grid(char)

fn show(name: Text, g: Grid[Char]) -> Unit {
    out(name)
    for y in 0..g.height() {
        out(g.row(y))
    }
}

show("original", g)
show("transpose", g.transpose())
show("rotate_right", g.rotate_right())
show("rotate_left", g.rotate_left())

// Each answers a new grid; the receiver is untouched.
show("still original", g)
```

On `abc` / `def`:

```text
original
[a, b, c]
[d, e, f]
transpose
[a, d]
[b, e]
[c, f]
rotate_right
[d, a]
[e, b]
[f, c]
rotate_left
[c, f]
[b, e]
[a, d]
still original
[a, b, c]
[d, e, f]
```

`out(grid)` prints the cells flat, in row-major order, with no row structure —
printing `grid.row(y)` in a loop, as `show` does, is how you look at the shape.

## `matrix(P)` is a `Grid[T]`

There is no separate `Matrix` type. `grid` and `matrix` differ only in how each
cuts a row into cells, and both answer `Grid[T]`
([ADR-030](../../../decisions/030-matrix-is-grid.md)).

```praxis
// The only difference between the two constructors is how a row is cut up.
// `grid` takes one cell per character; `matrix` takes one per whitespace-
// separated token. Both answer a `Grid[T]`.
var heights = parse("123\n456\n", grid(digit))
var readings = parse("12 3\n45 6\n", matrix(int))

out(heights.width())
out(heights.cells())

out(readings.width())
out(readings.cells())

// Same type, so the same methods.
out(heights.row(1))
out(readings.row(1))
```

```text
3
[1, 2, 3, 4, 5, 6]
2
[12, 3, 45, 6]
[4, 5, 6]
[45, 6]
```

Both constructors require every row to have the same cell count; a short row is
a parse fault unless you ask for `grid(P, ragged, fill: "X")`. That is the
[input parser](../input/structural.md)'s business, not the grid's.

## When an index is off the grid

`grid[x, y]`, `get`, `set`, `row` and `column` fault when the position is not
there. `contains` is the way to ask first.

```praxis
var g = read grid(char)
out(g[4, 0])
```

```text
error: program faulted: index out of bounds

Backtrace:
#0   <entry>

  locals:
    g: Grid[Char] = [a, b, c, d]
  temps:
    <tmp#1> = abcd

    <tmp#2: Int> = 1
    <tmp#3: Grid[Char]> = [a, b, c, d]
    <tmp#5: Int> @ "4" = 4
    <tmp#6: Int> @ "0" = 0
    <tmp#7: Char> @ "g[4, 0]" = Unit
    <tmp#8: Unit> @ "out(g[4, 0])" = <uninit>
    <tmp#9: Unit> @ "var g = read grid(char) out(g[4, 0])" = <uninit>
```

## The `Grid[T]` method surface

That is all of it — eighteen rows, including the two subscript forms.

| Call | Answers | Notes |
|---|---|---|
| `grid[x, y]` | `T` | faults off the grid |
| `grid[x, y] = v` | `Unit` | faults off the grid |
| `grid.get(x, y)` | `T` | the same read as the subscript |
| `grid.set(x, y, v)` | `Unit` | the same store as the subscript |
| `grid.width()` | `Int` | columns |
| `grid.height()` | `Int` | rows |
| `grid.contains(x, y)` | `Bool` | never faults |
| `grid.row(y)` | `Vec[T]` | faults off the grid |
| `grid.column(x)` | `Vec[T]` | faults off the grid |
| `grid.cells()` | `Vec[T]` | row-major |
| `grid.positions()` | `Vec[(Int, Int)]` | row-major |
| `grid.neighbors4(p)` | `Vec[(Int, Int)]` | `p` is `(Int, Int)`; clipped |
| `grid.neighbors8(p)` | `Vec[(Int, Int)]` | `p` is `(Int, Int)`; clipped |
| `grid.find(v)` | `Option[(Int, Int)]` | first match, row-major |
| `grid.find_all(v)` | `Vec[(Int, Int)]` | every match |
| `grid.transpose()` | `Grid[T]` | a new grid |
| `grid.rotate_left()` | `Grid[T]` | 90° counter-clockwise, a new grid |
| `grid.rotate_right()` | `Grid[T]` | 90° clockwise, a new grid |

§6.4 of the design document also lists `grid.map(fn)`. It is not implemented:
`grid.map(f)` is a `Y110`. Map over `grid.cells()` instead, and index back with
`grid.positions()` if you need to know where each cell was.

## The graph helpers

There is no graph object, no adjacency type and no node type. Every helper takes
a start **state** and then only **functions of it** — the graph *is* the closure
([ADR-060](../../../decisions/060-the-graph-helpers-are-closure-driven-walks.md)).
A state is any value you can put in a `Set`, so an `Int` node id, a `Text`, an
`(Int, Int)` grid position and a record all work, and the walk never learns
where they came from.

| Helper | Signature |
|---|---|
| `bfs` | `forall T. (T, (T) -> Vec[T]) -> Vec[T]` |
| `dfs` | `forall T. (T, (T) -> Vec[T]) -> Vec[T]` |
| `flood_fill` | `forall T. (T, (T) -> Vec[T]) -> Set[T]` |
| `bfs_distance` | `forall T. (T, (T) -> Vec[T], (T) -> Bool) -> Option[Int]` |
| `dijkstra` | `forall T. (T, (T) -> Vec[T], (T, T) -> Int) -> Map[T, Int]` |
| `a_star` | `forall T. (T, (T) -> Vec[T], (T, T) -> Int, (T) -> Int, (T) -> Bool) -> Option[Int]` |

The order is always start, neighbours, then weight, heuristic and goal where
they apply. The goal is a **predicate**, not a value: `|p| p == exit` for a
specific square, `|s| s.depth == 26` for a property. `a_star`'s five arguments
are the honest count — a start, a graph, a cost, an estimate and a goal, none of
which has a default worth guessing.

### A maze

A grid on stdin, a shortest path, an answer.

```praxis
// A maze on stdin: `#` is a wall, `S` the start, `E` the exit.
// How few steps is the exit?
var maze = read grid(char)
var wall = "#"[0]

// The graph is this function. `bfs_distance` never sees the grid: it only
// ever asks "what is next to here?" and "is this the exit?".
fn open_neighbours(maze: Grid[Char], wall: Char, p: (Int, Int)) -> Vec[(Int, Int)] {
    var open = Vec()
    for (x, y) in maze.neighbors4(p) {
        if maze[x, y] != wall {
            open.push((x, y))
        }
    }
    open
}

fn square(maze: Grid[Char], mark: Char) -> (Int, Int) {
    match maze.find(mark) {
        Some(p) => p,
        None => panic("the maze is missing a marker"),
    }
}

var start = square(maze, "S"[0])
var exit = square(maze, "E"[0])

var steps = bfs_distance(
    start,
    |p| open_neighbours(maze, wall, p),
    |p| p == exit,
)

match steps {
    Some(n) => out(n),
    None => out("walled in"),
}
```

The input is nine columns by five rows, with a sealed pocket on the right edge:

```text
S..#....#
.#.#.#..#
.#...#.#.
.#.#.#..#
...#...E#
```

```text
11
```

`bfs_distance` counts *steps*, one per edge, and stops at the first state its
predicate accepts — which is a shortest one, because breadth-first reaches every
state by its shortest path. `is_goal` is asked about the start first, so a
search that begins at its goal answers `Some(0)`. A goal nothing satisfies is
`None`, not `-1` and not a fault; see [Option](enums.md).

### Traversals

`bfs`, `dfs` and `flood_fill` do not look for anything. They answer everything
reachable — the first two in the order they reached it, the third as a `Set`.
All three contain the state you started from, which is why none of them needs an
`Option`.

```praxis
var maze = read grid(char)
var wall = "#"[0]

// The same graph as the maze, written as one closure: the in-bounds
// neighbours, minus the walls.
var step = |p| maze.neighbors4(p).filter(|q| match q { (x, y) => maze[x, y] != wall })

// `bfs` and `dfs` answer every reachable state, in the order they reached it.
// Both start with the state you gave them.
var breadth = bfs((0, 0), step)
out(breadth.len())
out(breadth[0])
out(breadth[1])
out(breadth[2])

var depth = dfs((0, 0), step)
out(depth[1])
out(depth[2])

// `flood_fill` asks the same reachability question and drops the order.
var filled = flood_fill((0, 0), step)
out(filled.len())

// The pocket at (8, 2) is open but walled off, so no walk reaches it.
out(maze[8, 2] != wall)
out(filled.contains((8, 2)))
```

On the same maze:

```text
29
(0, 0)
(0, 1)
(1, 0)
(0, 1)
(0, 2)
29
true
false
```

Thirty squares are open and twenty-nine are reachable; the pocket at `(8, 2)` is
on the right edge with a wall on each of its three neighbours, so it appears in
nothing. `dfs` descends into the
*first* neighbour a state reports, so its second and third states are `(0, 1)`
and `(0, 2)` — straight down the left edge — while `bfs` takes `(0, 1)` then
`(1, 0)`.

A closure stored in a variable, as `step` is here, is an ordinary value and can
be handed to as many helpers as you like. So can a top-level `fn` by name
([ADR-061](../../../decisions/061-a-fn-name-in-value-position-is-a-closure.md)):
`bfs(0, steps)` and `bfs(0, |n| steps(n))` are the same walk. The neighbour
result is an ordinary `Vec`, so a [pipeline](pipelines.md) is a fine way to
build it, as `step` does here.

### Costs: `dijkstra` and `a_star`

`dijkstra` answers a whole `Map[T, Int]` — the least cost from the start to
every state it reaches. `a_star` answers one `Option[Int]` for one goal, and
takes an estimate of the remaining cost to steer with.

```praxis
// A cost field: entering a square costs the number written on it.
var cost = read matrix(int)
var goal = (cost.width() - 1, cost.height() - 1)

var step = |p| cost.neighbors4(p)
var price = |a, b| match b { (x, y) => cost[x, y] }

// `dijkstra` answers a table: the least cost from the start to every state it
// reaches. The start is in it at 0, and a state it cannot reach is absent
// rather than infinite — which is why this one needs no `Option`.
var table = dijkstra((0, 0), step, price)
out(table.len())
out(table[(0, 0)])
out(table[(2, 0)])
out(table[goal])
out(table.contains((99, 99)))

fn manhattan(a: (Int, Int), b: (Int, Int)) -> Int {
    match a {
        (ax, ay) => match b {
            (bx, by) => abs(ax - bx) + abs(ay - by),
        },
    }
}

// `a_star` answers one number, and wants an estimate that never overshoots.
// Every step here costs at least 1, so Manhattan distance is admissible.
out(a_star((0, 0), step, price, |p| manhattan(p, goal), |p| p == goal))

// A zero estimate is admissible too; it turns A* back into Dijkstra.
out(a_star((0, 0), step, price, |p| 0, |p| p == goal))

// A goal nothing satisfies is `None`, not a fault and not a sentinel.
out(a_star((0, 0), step, price, |p| 0, |p| p == (99, 99)))
```

On

```text
1 1 9 1
1 9 1 1
1 1 1 9
9 1 1 1
```

it prints

```text
16
0
10
6
false
Some(6)
Some(6)
None
```

The weight function is `(T, T) -> Int` — the two endpoints of the edge, in that
order. Here only the destination matters, because the cost is a property of the
square you enter; a graph whose edges carry their own cost would use both.

A\*'s contract is that the estimate never exceeds the true remaining cost.
`|p| 0` always satisfies it, and turns the search into Dijkstra with one goal.

### What a state has to be

Every walk keeps the states it has seen in a set, and the weighted ones key a
cost table on them, so the state type has to be one that cannot change after it
is stored — the same `HashStable` rule a `Map` key follows
([capabilities](../types/capabilities.md)).

```praxis
// Every walk keeps the states it has seen in a set, and the weighted ones key
// a cost table on them. So a state has to be a value that cannot change after
// it is stored — and a `Vec` can.
fn steps(v: Vec[Int]) -> Vec[Vec[Int]] {
    Vec()
}

fn main() -> Int {
    bfs(Vec(), steps).len()
}
```

```console
$ praxis check unstable-state.px
error[Y014]: a value of type `Vec[Int]` can change after it is stored, so it cannot be used as a key

  unstable-state.px:9:5
  9 |     bfs(Vec(), steps).len()
    |     ^^^^^^^^^^^^^^^^^ a value of type `Vec[Int]` can change after it is stored, so it cannot be used as a key

help: use a value that cannot change — a number, `Text`, or a tuple of those

praxis: 1 error(s)
```

The requirement is reported at the call, not inside the helper. If the state
type is still a variable — `fn walk(start, step) { bfs(start, step) }` — it is
deferred onto `walk`'s own signature and answered at each call to `walk`.

The neighbour function's result shape is written into the helper's signature, so
handing it the wrong container is a type error and not a run-time surprise:

```praxis
// The neighbour function's shape is written into the helper's own signature,
// so a `Set` of neighbours is reported at the call rather than at run time.
fn steps(n: Int) -> Set[Int] {
    Set()
}

fn main() -> Int {
    bfs(0, steps).len()
}
```

```console
$ praxis check neighbours-must-be-a-vec.px
error[Y001]: expected (Int, (Int) -> Vec[Int]) -> Vec[Int], found (Int, (Int) -> Set[Int]) -> ?T

  neighbours-must-be-a-vec.px:8:5
  8 |     bfs(0, steps).len()
    |     ^^^^^^^^^^^^^ expected (Int, (Int) -> Vec[Int]) -> Vec[Int], found (Int, (Int) -> Set[Int]) -> ?T

praxis: 1 error(s)
```

### What stops a walk

An answer the walk cannot compute is a fault, not a wrong number. A negative
edge weight is the one to know about: Dijkstra and A\* settle a state the first
time they pop it and never reconsider, so a negative edge makes the answer
quietly too large. It stops instead.

```praxis
fn steps(n: Int) -> Vec[Int] {
    var v = Vec()
    if n < 3 { v.push(n + 1) }
    v
}

fn main() -> Int {
    dijkstra(0, steps, |a, b| -1).len()
}
```

```text
error: program faulted: an argument this algorithm has no answer for

Backtrace:
#0   main

  temps:
    <tmp#1: Int> @ "0" = 0
    <tmp#2: (Int) -> Vec[Int]> @ "steps" = <closure:0>
    <tmp#3: (Int, Int) -> Int> @ "|a, b| -1" = <closure:0>
    <tmp#4: Map[Int, Int]> @ "dijkstra(0, steps, |a, b| -1)" = Unit
    <tmp#5: Int> @ "dijkstra(0, steps, |a, b| -1).len()" = <uninit>
```

The full list of stops:

| Cause | What the program prints |
|---|---|
| a negative edge weight | `an argument this algorithm has no answer for` |
| a negative heuristic, which makes `g + h` fall along a path | `an argument this algorithm has no answer for` |
| a path cost or step count that leaves `Int` | `integer overflow` |
| a fault raised inside one of your closures | that fault, with your function on the backtrace |

The last row is worth stating plainly: a walk calls back into your code, and a
division by zero inside a neighbour function stops the walk rather than letting
it continue over garbage. See [the fault model](../debugger/faults.md).

An estimate that merely *overshoots* is not on that list, and that is the one
place in this family where you are on your own: A\* cannot detect it without
computing the answer first, so an inadmissible heuristic is a wrong number
rather than a stop.
