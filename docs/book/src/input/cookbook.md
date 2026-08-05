# Cookbook: input shapes

One recipe per input shape, each a complete program with the input it reads.
Every one of them runs: the programs and the outputs below are the files under
`docs/book/examples/input-b/`, and the numbers are what the compiler printed.
Each recipe is three blocks — the input, the program, and what it printed.

| Shape | Parser |
|---|---|
| one number per line | `lines(int)` |
| two columns | ``lines(`{left:int} {right:int}`)`` |
| one comma-separated line | `csv(int)` |
| blank-line groups | `sections(lines(int))` |
| two named sections | `sections(rules: …, updates: …)` |
| a header and N boards | `sections(draws: …, boards: repeated(matrix(int)))` |
| a map and a command stream | `sections(map: grid(char), moves: chars(…))` |
| repeated labelled blocks | `sections(…, maps: repeated(block(…)))` |
| instructions in noise | `scan(choice(…))` |
| a name, an arrow, a list | ``lines(`{from:word} -> {to:ws(word)}`)`` |
| mixed instruction lines | `lines(choice(…))` |

Run any of them with:

```console
$ praxis run c-one-per-line.px --input c-one-per-line.in
```

## One number per line

The base case. `lines(P)` splits on line endings and applies `P` to each line,
and every line has to be consumed — a stray non-numeric line is a fault, not a
silently skipped element.

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

```praxis
// Shape: one number per line.
//
// 199
// 200
// 208
var depths = read lines(int)

out(depths.len())
out(depths.sum())
out(depths.max())

// How many readings are larger than the one before.
var increases = depths
    .zip(depths.skip(1))
    .count(|(a, b)| b > a)
out(increases)
```

```text
10
2256
269
7
```

The trailing newline needs no special handling. `int` cannot read it, and a run
of whitespace the parser offered it does not read is not data.

## Two columns

Two numbers per line, aligned with however many spaces the puzzle felt like.
A named-capture template gives a record per line, so the columns have names
rather than indices.

```text
3   4
4   3
2   5
1   3
3   9
3   3
```

```praxis
// Shape: two columns of numbers, aligned with a variable run of spaces.
//
// 3   4
// 4   3
//
// A run of ordinary spaces in a template matches one or more spaces or tabs,
// so the alignment does not have to be exact.
var pairs = read lines(`{left:int} {right:int}`)

var left = pairs.map(|p| p.left).sorted()
var right = pairs.map(|p| p.right).sorted()

var distance = left
    .zip(right)
    .map(|(a, b)| abs(a - b))
    .sum()

var counts = right.frequencies()
var similarity = left
    .map(|value| value * counts[value])
    .sum()

out(distance)
out(similarity)
```

```text
11
31
```

`lines(ws(int))` would also read this file, as a `Vec[Vec[Int]]`. The template
is better here because it says there are exactly two columns and what they are
called; the mismatch is then a fault at the line that broke the shape rather
than a short inner `Vec` discovered later.

## One comma-separated line

A single line of comma-separated integers, read as a `Vec[Int]` and then
mutated in place.

```text
1,9,10,3,2,3,11,0,99,30,40,50
```

```praxis
// Shape: one line of comma-separated numbers.
//
// 1,9,10,3,2,3,11,0,99,30,40,50
//
// `csv` splits on commas and nothing else. Space around a comma is left in
// the field, and `int` does not read it, so no trimming is needed.
var program = read csv(int)

out(program.len())
out(program.max())

// Run it as a two-opcode machine: 1 adds, 2 multiplies, 99 halts.
var pc = 0
while pc < program.len() {
    var op = program[pc]
    if op == 99 {
        break
    }
    var a = program[program[pc + 1]]
    var b = program[program[pc + 2]]
    var dst = program[pc + 3]
    if op == 1 {
        program[dst] = a + b
    } else {
        program[dst] = a * b
    }
    pc = pc + 4
}
out(program[0])
```

```text
12
99
3500
```

For comma-separated values on *many* lines, nest: `lines(csv(int))` is a
`Vec[Vec[Int]]`. `csv(int)` on a multi-line file is a mismatch, because `csv`
splits on commas and nothing else, so a field straddles the line ending: over
`1,2\n3,4\n` the middle field is `2\n3`, and what `int` leaves of it is not
whitespace. The file's own final newline is not what breaks it — the last field
above ends in one too, and a run of whitespace nobody read is not data.

## Blank-line groups

Groups of numbers separated by blank lines. `sections` splits on the blank
lines, `lines` splits each section, and the two constructors nest in the order
they are written.

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

```praxis
// Shape: blank-line separated groups of numbers.
//
// 1000
// 2000
//
// 4000
//
// `sections` splits on blank lines; `lines` splits each section.
var groups = read sections(lines(int))

out(groups.len())

var totals = groups.map(|g| g.sum()).sorted()
out(totals)
out(totals.max())
out(totals.skip(totals.len() - 3).sum())
```

```text
5
[4000, 6000, 10000, 11000, 24000]
24000
45000
```

A trailing blank line at the end of the file is not a section: `sections` never
produces an empty one.

## Two named sections

Ordering rules, a blank line, then the updates to check against them. The two
sections have different shapes, so they get named arguments and the result is a
record.

```text
47|53
97|13
97|61
97|47
75|29
61|13
75|53
29|13
97|29
53|29
61|53
97|53
61|29
47|13
75|47
97|75
47|61
75|61
47|29
75|13
53|13

75,47,61,53,29
97,61,53,29,13
75,29,13
75,97,47,61,53
61,13,29
97,13,75,29,47
```

```praxis
// Shape: two heterogeneous sections — ordering rules, then updates.
//
// 47|53
// 97|13
//
// 75,47,61,53,29
//
// Named arguments parse the sections in order and give the result one field
// per name.
var data = read sections(
    rules: lines(`{before:int}|{after:int}`),
    updates: lines(csv(int)),
)

out(data.rules.len())
out(data.updates.len())

var forbidden = Set[(Int, Int)]()
for r in data.rules {
    forbidden.insert((r.after, r.before))
}

var ordered = 0
var middles = 0
for update in data.updates {
    var ok = true
    for i in 0..update.len() {
        for j in (i + 1)..update.len() {
            if forbidden.contains((update[i], update[j])) {
                ok = false
            }
        }
    }
    if ok {
        ordered = ordered + 1
        middles = middles + update[update.len() / 2]
    }
}
out(ordered)
out(middles)
```

```text
21
6
3
143
```

The sections are matched positionally, in the order they are named. Fewer
sections in the file than fields in the parser is a fault (`expected section
header`); more sections are simply not read unless the last field is a
`repeated(...)` tail.

## A header and an unknown number of boards

One header section, then as many boards as the file happens to contain.
`repeated(P)` is the final named argument and takes every section that is left.

```text
7,4,9,5,11,17,23,2,0,14,21,24,10,16,13,6,15,25,12,22,18,20,8,19,3,26,1

22 13 17 11  0
 8  2 23  4 24
21  9 14 16  7
 6 10  3 18  5
 1 12 20 15 19

 3 15  0  2 22
 9 18 13 17  5
19  8  7 25 23
20 11 10 24  4
14 21 16 12  6

14 21 17 24  4
10 16 15  9 19
18  8 23 26 20
22 11 13  6  5
 2  0 12  3  7
```

```praxis
// Shape: one header section, then an unknown number of boards.
//
// 7,4,9,5,11
//
// 22 13 17 11  0
//  8  2 23  4 24
//
// `repeated(P)` is the final named argument: it takes every section that is
// left. `matrix` splits each row into whitespace-separated tokens itself, so
// the ragged column alignment does not matter.
var bingo = read sections(
    draws: csv(int),
    boards: repeated(matrix(int)),
)

out(bingo.draws.len())
out(bingo.boards.len())
out(bingo.boards.get(0).width())

// When does each board first complete a row or a column?
fn wins_at(board: Grid[Int], order: Map[Int, Int]) -> Int {
    var best = -1
    for y in 0..board.height() {
        var turn = board.row(y).map(|n| order[n]).max()
        if best < 0 || turn < best {
            best = turn
        }
    }
    for x in 0..board.width() {
        var turn = board.column(x).map(|n| order[n]).max()
        if best < 0 || turn < best {
            best = turn
        }
    }
    best
}

var order = Map[Int, Int]()
for (turn, n) in bingo.draws.enumerate() {
    order[n] = turn
}

var turns = bingo.boards.map(|b| wins_at(b, order))
out(turns)

var first = turns.min()
match turns.position(|t| t == first) {
    Some(index) => {
        var winner = bingo.boards.get(index)
        var drawn = bingo.draws.take(first + 1).to_set()
        out(winner.cells().filter(|n| !drawn.contains(n)).sum() * bingo.draws[first])
    }
    None => out("no board wins")
}
```

```text
27
3
5
[13, 14, 11]
4512
```

The boards are padded to align their columns, and `matrix` tokenizes a row on
whitespace itself, so `22 13 17 11  0` and ` 8  2 23  4 24` are both five
tokens. `grid(int)` reads this file identically — `int` reads a whole token and
skips the space in front of it — but `matrix` is the constructor that says
"whitespace-separated" out loud rather than leaning on the cell parser's
whitespace rule to get there.

## A map and a command stream

A character map, a blank line, then a stream of movement characters that the
puzzle wrapped across lines for no reason of its own.

```text
#######
#.....#
#..#..#
#.....#
#..#..#
#######

>>vv<^^
>>>vv
```

```praxis
// Shape: a character map, a blank line, then a stream of movement characters
// wrapped across lines for no reason of its own.
//
// #######
// #.....#
//
// >>vv<^^
//
// `chars(P, skip: newlines)` folds the command stream back into one sequence:
// `newlines` is the broader policy — it passes over spaces, tabs and line
// endings alike. There is no character literal, so `"#"[0]` is how a program
// names one.
var data = read sections(
    map: grid(char),
    moves: chars(one_of("^v<>"), skip: newlines),
)

out(data.map.width())
out(data.map.height())
out(data.moves.len())

var wall = "#"[0]
var x = 1
var y = 1
var blocked = 0
for move in data.moves {
    var nx = x
    var ny = y
    if move == ">"[0] { nx = x + 1 }
    if move == "<"[0] { nx = x - 1 }
    if move == "v"[0] { ny = y + 1 }
    if move == "^"[0] { ny = y - 1 }
    if data.map[nx, ny] == wall {
        blocked = blocked + 1
    } else {
        x = nx
        y = ny
    }
}
out(x)
out(y)
out(blocked)
```

```text
7
6
12
5
3
4
```

`skip: newlines` is what folds the wrapped stream back into one sequence. The
default is `skip: whitespace`, which is horizontal whitespace only and would
fault at the first line ending inside the section.

## Repeated labelled blocks

A header section, then any number of sections that each begin with a label and
continue with lines of numbers. `block` sequences the label template and the
body parser inside one section; `repeated(block(...))` applies that to every
section that is left.

```text
seeds: 79 14 55 13

seed-to-soil map:
50 98 2
52 50 48

soil-to-fertilizer map:
0 15 37
37 52 2
39 0 15

fertilizer-to-water map:
49 53 8
0 11 42
42 0 7
57 7 4

water-to-light map:
88 18 7
18 25 70

light-to-temperature map:
45 77 23
81 45 19
68 64 13

temperature-to-humidity map:
0 69 1
1 0 69

humidity-to-location map:
60 56 37
56 93 4
```

```praxis
// Shape: a header section, then any number of labeled blocks whose bodies are
// lines of numbers.
//
// seeds: 79 14 55 13
//
// seed-to-soil map:
// 50 98 2
// 52 50 48
//
// `block` sequences a header template and a body parser inside one section;
// `repeated(block(...))` applies that to every section that is left. The
// header's captures are flattened into the same record as the named body.
var almanac = read sections(
    seeds: block(`seeds: {values:ws(int)}`),
    maps: repeated(block(
        `{source:word}-to-{destination:word} map:`,
        ranges: lines(`{destination:int} {source:int} {length:int}`),
    )),
)

out(almanac.seeds.values)
out(almanac.maps.len())
out(almanac.maps.get(0).source)
out(almanac.maps.get(0).destination)

// An anonymous record type has no name to write in an annotation, so the
// mapping is done in place rather than in a helper that would have to name it.
var locations = almanac.seeds.values.map(|seed| {
    var v = seed
    for m in almanac.maps {
        for r in m.ranges {
            if v >= r.source && v < r.source + r.length {
                v = r.destination + (v - r.source)
                break
            }
        }
    }
    v
})
out(locations)
out(locations.min())
```

```text
[79, 14, 55, 13]
7
seed
soil
[82, 43, 86, 35]
35
```

The header's captures are flattened into the same record as the named
`ranges:` item, so a map is `{ source, destination, ranges }` and not
`{ header: { … }, ranges: … }`. Put the `lines(...)` item last: it is offered
the rest of the region, so anything after it would have nothing left.

## Instructions embedded in noise

The data is buried in text that is deliberately corrupt. `scan` looks for its
parser at every position, keeps what matches in source order and ignores the
rest.

```text
xmul(2,4)&mul[3,7]!^don't()_mul(5,5)+mul(32,64](mul(11,8)undo()?mul(8,5))
```

```praxis
// Shape: instructions embedded in text that is otherwise noise.
//
// xmul(2,4)&mul[3,7]!^don't()_mul(5,5)+mul(32,64](mul(11,8)undo()?mul(8,5))
//
// `scan` looks for its parser at every position, keeps the matches in source
// order and ignores everything else. Nothing bounds the root, so a scan that
// matches nothing is an empty Vec rather than a fault.
var program = read scan(choice(
    Multiply: `mul({left:int},{right:int})`,
    Enable: `do()`,
    Disable: `don't()`,
))

out(program.len())

var all = 0
var enabled = 0
var on = true
for step in program {
    match step {
        Multiply(p) => {
            all = all + p.left * p.right
            if on {
                enabled = enabled + p.left * p.right
            }
        }
        Enable(_) => { on = true }
        Disable(_) => { on = false }
    }
}
out(all)
out(enabled)
```

```text
6
161
48
```

Nothing bounds the root, so a `scan` that matches nothing is an empty `Vec`
rather than a fault. That is the same rule that lets a root-level `choice` match
a prefix: requiring a region to be filled is the decision of whoever computed
the bound, and nobody bounded the root.

## A name, an arrow, and a list

One line, two shapes. The template names the two halves; `sep` splits the same
line on the exact arrow string.

```text
jqt -> rhn xhk nvd
rsh -> frs pzl lsr
cmg -> qnr nvd lhk bvb
```

```praxis
// Shape: a name, an arrow, then a space-separated list.
//
// jqt -> rhn xhk nvd
//
// `sep` splits on the exact string and trims nothing, so the arrow's spaces
// are part of the separator rather than of the words around it. A capture
// whose parser is `ws(word)` takes the whole tail of the line.
var edges = read lines(`{from:word} -> {to:ws(word)}`)

out(edges.len())
for e in edges {
    out(e.from)
    out(e.to)
}

// The same line read the other way: two fields split on the arrow.
var halves = read lines(sep(" -> ", rest))
out(halves.get(0))
```

```text
3
jqt
[rhn, xhk, nvd]
rsh
[frs, pzl, lsr]
cmg
[qnr, nvd, lhk, bvb]
[jqt, rhn xhk nvd]
```

Both parsers read the same input: a `read` expression always parses the whole
process input from the beginning, so a second `read` is not a second half of a
stream. That is what makes repeated reads deterministic — and it is also why a
normal program only does it once.

## Instruction lines of different shapes

One instruction per line, and the instructions do not all look alike. `choice`
inside `lines` gives one anonymous enum variant per case, and `match` covers
them.

```text
noop
addx 3
addx -5
noop
addx 11
```

```praxis
// Shape: one instruction per line, and the instructions have different
// shapes.
//
// noop
// addx 3
// addx -5
//
// `choice` inside `lines` gives one anonymous enum variant per case. The
// first case that matches wins, and `lines` then requires the line to be
// consumed — so a case that matches a prefix of a longer line is caught by
// `lines`, not silently accepted.
var program = read lines(choice(
    Addx: `addx {value:int}`,
    Noop: `noop`,
))

out(program.len())

var x = 1
var cycle = 0
for instruction in program {
    match instruction {
        Noop(_) => { cycle = cycle + 1 }
        Addx(p) => {
            cycle = cycle + 2
            x = x + p.value
        }
    }
}
out(cycle)
out(x)
```

```text
5
8
10
```

Case order matters: the first case that matches wins, and `choice` itself does
not require the region to be consumed. Here `Noop` cannot match an `addx 3` line
at all, so either order works. Where one case *is* a prefix of another, put the
longer one first — otherwise the shorter one matches, and `lines` faults on the
bytes it left behind.

## Choosing between them

Three questions settle most inputs.

**What separates the records?** A line ending is `lines`, a blank line is
`sections`, a comma is `csv`, any run of whitespace is `ws`, anything else is
`sep`. `chars` is the one that has no separator: it applies its parser again and
again and says what it skips in between.

**Does a record have a shape inside it?** If so, write a template and name the
captures; the fields come out named. If the parts are homogeneous, nest a
constructor instead — `lines(csv(int))` rather than a template with N captures.

**Are the sections different from each other?** Then name them, and put a
`repeated(...)` last if there is an unknown number of the final kind. A section
whose parts are heterogeneous *within* the section is a `block`.

What is left after that is `grid` and `matrix`, which are the two-dimensional
answers: `grid` lets its cell parser decide how far a cell reaches, `matrix`
splits a row into whitespace-separated tokens itself.

The constructors are described one by one in [structural
parsers](structural.md), the types they produce in [how a parser gets its
type](type-derivation.md), and what a mismatch looks like in [when a parse
fails](faults.md).
