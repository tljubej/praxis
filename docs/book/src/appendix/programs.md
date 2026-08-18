# Appendix A: Complete programs

Seven programs, in rough order of size. Every one is a real file under
`docs/book/examples/appendix/`, re-run by `docs/book/examples/verify.sh` against
the input and the output printed here. So this is not a sketch of what a Praxis
program might look like; it is what `target/release/praxis` does with that text.

Run any of them:

```console
$ praxis run docs/book/examples/appendix/depths.px --input docs/book/examples/appendix/depths.in
```

The first four are puzzle-sized. The fifth is a whole puzzle in thirty lines.
The sixth is a breadth-first search written twice, and the seventh is a bytecode
interpreter that retires 1.15 million instructions on the input shown.

## `depths.px` — one integer per line

The floor of the language. A whole day's input is `lines(int)`, and the type
`Vec[Int]` comes from the parser expression rather than from an annotation. The
three lines of output are `len`, `sum`, and a
[pipeline](../language/pipelines.md) that zips the vector with itself offset by
one and counts the pairs that increase.

This program has no `fn main`, and there is none to write: a file is a program
and its top-level statements run in order
([A file is a program](../language/program-structure.md)).

```praxis
// The smallest complete program that reads input: one integer per line.
//
// `read lines(int)` is the whole parser. Its type — `Vec[Int]` — is derived
// from the parser expression, so nothing is annotated. What follows is three
// pipelines over that vector.

var depths = read lines(int)

out(depths.len())
out(depths.sum())
// A window over consecutive pairs: how many readings are larger than the one
// before. `zip` pairs the vector with itself offset by one, and `count` takes
// the predicate.
out(depths.zip(depths.skip(1)).count(|p| p.1 > p.0))
```

`depths.in`:

```text
199
200
208
210
200
207
240
269
260
263
```

Output:

```text
10
2256
7
```

Explained in [The `read` expression](../input/read.md),
[Atomic parsers](../input/atoms.md) and [Pipelines](../language/pipelines.md).

## `calories.px` — blank-line-separated sections

The other structural shape every puzzle set contains: groups separated by blank
lines. `sections(lines(int))` nests two structural parsers, and the nesting *is*
the type — `Vec[Vec[Int]]`, one inner vector per group.

`sorted_by_key(|t| 0 - t)` is how this program writes a descending sort. There
is a second spelling, `sorted().reversed()`, and the example keeps the first:
negating the key is one pass over the group where sorting and then reversing is
two.

```praxis
// A sections day: blank-line-separated groups of integers.
//
// `sections(lines(int))` nests two structural parsers, so the result is
// `Vec[Vec[Int]]` — one inner vector per group. Nothing in the program says
// that type; it is read off the parser expression.

var groups = read sections(lines(int))

out(groups.len())
// The largest group total, and the sum of the three largest.
var totals = groups.map(|g| g.sum())
out(totals.max())
out(totals.sorted_by_key(|t| 0 - t).take(3).sum())
```

`calories.in`:

```text
1000
2000
3000

4000

5000
6000

7000
8000
9000

10000
```

Output:

```text
5
24000
45000
```

Explained in [Structural parsers](../input/structural.md) and
[How a parser gets its type](../input/type-derivation.md).

## `toboggan.px` — a character grid

A grid day. `read grid(char)` yields a `Grid[Char]`, indexed `map[x, y]` — a
subscript taking two arguments, which is why a subscript's index list is an
argument list rather than a single expression.

Two details here are worth naming. `'#'` is how a program writes a character it
chose, and `"#"[0]` — subscripting a one-character `Text` — still names the same
`Char`, which is what a program reaches for when the character came out of text
it did not write down. And `trees_on_slope` takes the grid as a *parameter*
even though `map` is in scope at the file level — a `fn` does not capture the
bindings around it, and reading one from inside a function is `N007` with a
message telling you to pass it in (and, when the function is recursive, saying
why a closure is not an option).

```praxis
// A grid day: count the trees hit by descending a slope of (right 3, down 1).
//
// `read grid(char)` yields a `Grid[Char]`, indexed `map[x, y]` — a subscript
// with two arguments, which is why a subscript's index list is an argument list
// and not a single expression. The map repeats horizontally forever, so the
// column wraps with `%`.

var map = read grid(char)

fn trees_on_slope(map, right, down) {
    var x = 0
    var y = 0
    var hits = 0
    while y < map.height() {
        if map[x % map.width(), y] == "#"[0] {
            hits = hits + 1
        }
        x = x + right
        y = y + down
    }
    hits
}

out(map.width())
out(map.height())
out(trees_on_slope(map, 3, 1))
// Part two multiplies five slopes together.
var product = 1
for slope in [(1, 1), (3, 1), (5, 1), (7, 1), (1, 2)] {
    product = product * trees_on_slope(map, slope.0, slope.1)
}
out(product)
```

`toboggan.in`:

```text
..##.......
#...#...#..
.#....#..#.
..#.#...#.#
.#...##..#.
..#.##.....
.#.#.#....#
.#........#
#.##...#...
#...##....#
.#..#...#.#
```

Output:

```text
11
11
7
336
```

Explained in [Grids and graphs](../language/grid-and-graphs.md),
[Text and Char](../language/text.md) and
[Functions and closures](../language/functions.md).

## `pipeline.px` — records from a template, then closures

A named-capture template turns each line into a record: `{name:word}: {score:int}`
produces `{name: Text, score: Int}`, and the whole read is a vector of those.
Field access is `e.name`, and the record type was never declared.

`above` is a function that *returns a closure*. `cut` is captured and the
closure outlives the call that built it — which is the difference between a
closure and a `fn`, and the reason `trees_on_slope` above had to take its grid
as a parameter.

```praxis
// A closure pipeline over records read from a template.
//
// The template `{name:word}: {score:int}` names its captures, so each line
// parses into a record `{name: Text, score: Int}` and the whole read is a
// `Vec[{name: Text, score: Int}]`. Everything after that is pipeline
// combinators and closures — including one closure returned from a function,
// which captures the parameter it was built with.

var entries = read lines(`{name:word}: {score:int}`)

// A function that returns a closure. `cut` is captured by value; the closure
// outlives the call that made it.
fn above(cut) {
    |e| e.score > cut
}

var passing = above(50)

out(entries.len())
out(entries.filter(passing).map(|e| e.name))
out(entries.map(|e| e.score).fold(0, |a, s| a + s))
out(entries.sorted_by_key(|e| 0 - e.score).take(2).map(|e| e.name))
// `frequencies` counts, and a `Counter` reads absent keys as zero.
var initials = entries.map(|e| e.name[0]).frequencies()
out(initials["a"[0]])
out(initials["z"[0]])
```

`pipeline.in`:

```text
ada: 91
alan: 47
grace: 88
alonzo: 63
edsger: 12
```

Output:

```text
5
[ada, grace, alonzo]
301
[ada, grace]
3
0
```

The last two lines are the `Counter` rule: a key that was counted reads its
count, and a key that was never inserted reads `0` instead of faulting.

Explained in [Templates and captures](../input/templates.md),
[Records without names](../types/structural-records.md),
[Functions and closures](../language/functions.md) and
[Collections](../language/collections.md).

## `segments.px` — a whole puzzle in thirty lines

What a finished puzzle solution looks like at full size, top-level statements
and all: the template read, a function over the records it produced,
`Counter[(Int, Int)]()` with an explicit type argument, `counts[point] += 1`
storing through a subscript, `0..=distance`, a trailing comma in the `max(…)`
call, and two `out` calls at file scope.

The one type argument is not forced by anything: `Counter()` on its own infers
`(Int, Int)` from `counts[point] += 1` and prints the same two answers. It is
written out because this is the only shape a type argument has — a
compiler-owned constructor name, a bracket list, and then the call. Nothing else
in the program is annotated.

```praxis
// segments — the shape of a whole puzzle in thirty lines: a template read, a
// function over the records it produced, a `Counter` keyed by a tuple, a
// compound assignment through a subscript, an inclusive range, and two calls
// printing the two parts. Nothing in it is annotated.
//
// Input: line segments, `x1,y1 -> x2,y2`. Output: the number of points covered
// by two or more segments, first ignoring diagonals and then including them.
var segments = read lines(`{x1:int},{y1:int} -> {x2:int},{y2:int}`)

fn overlaps(segments, diagonals) {
    var counts = Counter[(Int, Int)]()

    for segment in segments {
        var dx = sign(segment.x2 - segment.x1)
        var dy = sign(segment.y2 - segment.y1)

        if !diagonals && dx != 0 && dy != 0 {
            continue
        }

        var distance = max(
            abs(segment.x2 - segment.x1),
            abs(segment.y2 - segment.y1),
        )

        for step in 0..=distance {
            var point = (
                segment.x1 + dx * step,
                segment.y1 + dy * step,
            )
            counts[point] += 1
        }
    }

    counts.values().count(|n| n >= 2)
}

out(overlaps(segments, false))
out(overlaps(segments, true))
```

`segments.in`:

```text
0,9 -> 5,9
8,0 -> 0,8
9,4 -> 3,4
2,2 -> 2,1
7,0 -> 7,4
6,4 -> 2,0
0,9 -> 2,9
3,4 -> 1,4
0,0 -> 8,8
5,5 -> 8,2
```

Output:

```text
5
12
```

`!diagonals && dx != 0 && dy != 0` leans on the precedence table: `!` binds
tighter than every infix operator, `&&` binds looser than `!=`, and `&&` is
left-associative — so it reads as `((!diagonals) && (dx != 0)) && (dy != 0)`.
The whole table is in [Appendix B](grammar.md#operator-precedence).

Explained in [Templates and captures](../input/templates.md),
[Collections](../language/collections.md) and
[Bindings and shadowing](../language/bindings.md).

## `maze.px` — breadth-first search, twice

The same search written both ways. The first is the loop every puzzle starts
with: a `Deque` as the FIFO frontier, a `Set` of visited cells, a `Map` of
distances. The second is the prelude's `bfs_distance`, which takes a start
state, a closure answering a state's neighbours, and a closure saying whether a
state is the goal — the graph is never built.

A cell is a `(Int, Int)` tuple. That is what lets it be a `Set` member and a
`Map` key: a key has to be hashable *and* unable to change after it is stored,
and a tuple of scalars is both. A `Vec` is not: a `Vec[Int]` key is `Y014` — "a
value of type `Vec[Int]` can change after it is stored, so it cannot be used as
a key". Hashable is not orderable: a tuple has no `<` — though the collections
above still walk and print their tuple keys element-wise, because a container
needs a reproducible order whatever the source language permits.

`bfs_distance` answers `Option[Int]`, so the third line of output is `Some(22)`
and not `22`: a goal that cannot be reached has no distance.

```praxis
// Breadth-first search over a maze read as a character grid.
//
// Two ways to write the same search. The first is the loop every puzzle starts
// with: a `Deque` as the FIFO frontier, a `Set` of visited cells, a `Map` of
// distances. The second is the prelude's `bfs_distance`, which takes the start,
// a closure answering the neighbours of a state, and a closure saying whether a
// state is the goal — the graph is never materialized.
//
// A cell is a `(Int, Int)` tuple, which is what lets it be a `Set` member and a
// `Map` key: tuples are values compared by their elements.

var maze = read grid(char)

fn open_cell(maze, p) {
    p.0 >= 0 && p.1 >= 0 && p.0 < maze.width() && p.1 < maze.height()
        && maze[p.0, p.1] != "#"[0]
}

fn neighbours(maze, p) {
    var found = Vec()
    for step in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        var q = (p.0 + step.0, p.1 + step.1)
        if open_cell(maze, q) { found.push(q) }
    }
    found
}

var start = (0, 0)
var goal = (maze.width() - 1, maze.height() - 1)

// The explicit loop.
var frontier = Deque()
frontier.push_back(start)
var seen = Set()
seen.insert(start)
var dist = Map()
dist[start] = 0
var answer = -1
while frontier.len() > 0 {
    var here = frontier.pop_front()
    if here == goal {
        answer = dist[here]
        break
    }
    for next in neighbours(maze, here) {
        if !seen.contains(next) {
            seen.insert(next)
            dist[next] = dist[here] + 1
            frontier.push_back(next)
        }
    }
}
out(answer)
out(seen.len())

// The same answer from the prelude helper. It answers an `Option[Int]`,
// because a goal that is not reachable has no distance.
out(bfs_distance(start, |p| neighbours(maze, p), |p| p == goal))
```

`maze.in`:

```text
.....#....
.###.#.##.
.#...#..#.
.#.#####.#
.#.......#
.#.#####.#
...#...#..
.###.#.##.
.....#....
.#####....
```

Output:

```text
22
51
Some(22)
```

Explained in [Grids and graphs](../language/grid-and-graphs.md),
[Collections](../language/collections.md) and
[Enums and Option](../language/enums.md).

## `vm.px` — a stack bytecode interpreter

The one program here that is not puzzle-sized. Ten opcodes as an `enum` with
payloads, one `match` per executed instruction, an operand stack in a `Deque`,
four registers in plain bindings, and a hand-assembled program in a `Vec`. On
the input below it retires 1,150,005 instructions and finishes in about thirty
milliseconds, compilation included.

It is copied from `benchmarks/praxis/vm.px`, where it is the dispatch
benchmark — the closest thing in the benchmark set to a "simulate this machine"
puzzle part.

Three rules are load-bearing. The `match` over `Op` is checked for
exhaustiveness, so an opcode added to the enum and forgotten in the loop is a
compile error rather than a runtime surprise. `Push(k)` in a pattern binds the
payload. And `prog[pc]` is a bounds-checked subscript: a jump target past the
end of the program faults with `index out of bounds` and enters the crash
debugger, rather than reading whatever is there.

```praxis
// vm — a stack bytecode interpreter.
//
// Ten opcodes as an `enum` with payloads, one `match` per executed instruction,
// an operand stack in a `Deque`, and four registers held in plain bindings.
// This is the "simulate this machine" half of a puzzle at full size: the loop
// below retires 1.15 million instructions on the input in `vm.in`, and the
// exhaustiveness check on the `match` is what says no opcode was forgotten.
//
// It is copied from `benchmarks/praxis/vm.px`, where it is the dispatch
// benchmark.
//
// The interpreted program computes a rolling modular hash over `0..limit`; the
// interpreter reports its result and the number of instructions it retired.
//
// Input: the interpreted loop's iteration count, as one integer on stdin.
// Output: the interpreted program's result, then the instruction count.

enum Op {
    Push(Int)
    Load(Int)
    Store(Int)
    Add
    Mul
    Mod
    Lt
    JmpZ(Int)
    Jmp(Int)
    Halt
}

var limit = read int

// The program, hand-assembled. Register 0 is the loop counter, register 1 the
// accumulator; the loop head is instruction 4 and the exit target is 27.
var prog = Vec()
prog.push(Push(0))      //  0
prog.push(Store(0))     //  1   i = 0
prog.push(Push(1))      //  2
prog.push(Store(1))     //  3   acc = 1
prog.push(Load(1))      //  4   <- loop head
prog.push(Push(31))     //  5
prog.push(Mul)          //  6   acc * 31
prog.push(Load(0))      //  7
prog.push(Push(7))      //  8
prog.push(Mul)          //  9   i * 7
prog.push(Push(13))     // 10
prog.push(Add)          // 11   i * 7 + 13
prog.push(Push(1000003)) // 12
prog.push(Mod)          // 13   (i * 7 + 13) % 1000003
prog.push(Add)          // 14   acc * 31 + that
prog.push(Push(1000003)) // 15
prog.push(Mod)          // 16
prog.push(Store(1))     // 17   acc = ...
prog.push(Load(0))      // 18
prog.push(Push(1))      // 19
prog.push(Add)          // 20
prog.push(Store(0))     // 21   i = i + 1
prog.push(Load(0))      // 22
prog.push(Push(limit))  // 23
prog.push(Lt)           // 24   i < limit
prog.push(JmpZ(27))     // 25
prog.push(Jmp(4))       // 26
prog.push(Load(1))      // 27   <- exit
prog.push(Halt)         // 28

var stack = Deque()
var pc = 0
var r0 = 0
var r1 = 0
var r2 = 0
var r3 = 0
var steps = 0
var running = true

while running {
    var op = prog[pc]
    pc = pc + 1
    steps = steps + 1
    match op {
        Push(k) => { stack.push_back(k) }
        Load(k) => {
            if k == 0 { stack.push_back(r0) }
            else if k == 1 { stack.push_back(r1) }
            else if k == 2 { stack.push_back(r2) }
            else { stack.push_back(r3) }
        }
        Store(k) => {
            var v = stack.pop_back()
            if k == 0 { r0 = v }
            else if k == 1 { r1 = v }
            else if k == 2 { r2 = v }
            else { r3 = v }
        }
        Add => {
            var b = stack.pop_back()
            var a = stack.pop_back()
            stack.push_back(a + b)
        }
        Mul => {
            var b = stack.pop_back()
            var a = stack.pop_back()
            stack.push_back(a * b)
        }
        Mod => {
            var b = stack.pop_back()
            var a = stack.pop_back()
            stack.push_back(a % b)
        }
        Lt => {
            var b = stack.pop_back()
            var a = stack.pop_back()
            if a < b { stack.push_back(1) } else { stack.push_back(0) }
        }
        JmpZ(t) => {
            var v = stack.pop_back()
            if v == 0 { pc = t }
        }
        Jmp(t) => { pc = t }
        Halt => { running = false }
    }
}

out(stack.pop_back())
out(steps)
```

`vm.in`:

```text
50000
```

Output:

```text
990539
1150005
```

The enum's variants are separated by line breaks rather than commas. Either
works: a comma **or** a line break separates the members of a `struct` or `enum`
body, and a trailing comma closes the list either way — the same rule that
separates statements and match arms.

Explained in [Enums and Option](../language/enums.md),
[Pattern matching](../language/pattern-matching.md) and
[The fault model](../debugger/faults.md).

## Where the rest of the corpus lives

These seven are a selection. The repository carries larger sets, run by the test
suite rather than by this book:

| directory | what is in it |
|---|---|
| `tests/aoc-corpus/` | 31 fixtures, one per input shape and per language feature, each with its `.out` and with an `.in` when it reads input |
| `tests/input-parsers/` | 12 fixtures for the `read` DSL: one constructor, template or whitespace rule each |
| `benchmarks/praxis/` | eight larger programs: `bfs`, `collatz`, `hashwork`, `mandelbrot`, `pipeline`, `primes`, `tree`, `vm` |
| `crates/praxis-cli/tests/fixtures/` | programs whose *diagnostics* are the fixture, and `run/`, whose programs are driven end to end |

None of those directories is documentation. Where one of them disagrees with a
chapter of this book, the chapter is the one that was checked against a running
compiler.
