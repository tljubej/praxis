# Structural parsers

A structural parser constructor takes a region of the input, splits it, and
applies a child parser to each piece. `lines` splits on line endings, `csv` on
commas, `sections` on blank lines, `grid` on nothing at all — it lets the cell
parser decide how far a cell reaches. Constructors nest, so
`sections(lines(csv(int)))` is a `Vec[Vec[Vec[Int]]]` and reads exactly the way
it is spelled.

There are fourteen of them and the list is closed; what goes *inside* one is an
[atomic parser](atoms.md) or a [template](templates.md). Everything below is
written as `read CONSTRUCTOR(...)`, because a parser expression only
exists after [`read`](read.md) or inside `parse(text, ...)`; that is what makes a
labelled argument such as `skip:` legal, since it belongs to the parser grammar
and has no meaning in an ordinary call.

Each example below is three blocks: the program, the input it was run against,
and what it printed.

## A call is a shape, checked before anything is built

Each constructor has a fixed argument shape, and the shape is checked before a
single parser node is constructed
([ADR-073](../../../decisions/073-a-constructor-call-is-a-shape-checked-before-it-is-built.md)).
A wrong argument is a compile error, never an argument that is quietly dropped.

| Call | Shape |
|---|---|
| `lines(P)` | one parser |
| `sections(P)` | one parser… |
| `sections(name: P, …)` | …or named arguments only |
| `csv(P)` | one parser |
| `ws(P)` | one parser |
| `sep("SEP", P)` | a string literal, then a parser |
| `grid(P)` | one cell parser… |
| `grid(P, ragged, fill: v)` | …or a cell parser with `ragged` **and** `fill:` |
| `matrix(P)` | one parser |
| `chars(P)`, `chars(P, skip: policy)` | one parser and an optional `skip:` |
| `one_of("LR")` | one string literal |
| `block(item, …)` | one or more items, positional or named |
| `choice(Name: P, …)` | named arguments only, at least one |
| `optional(P)` | one parser |
| `scan(P)` | one parser |
| `repeated(P)` | only as the final named argument of a `sections` call |

`repeated` is in the table so that misusing it is a specific complaint rather
than "unknown constructor". It is a marker, not a parser in its own right.

## `lines(P)`

Split the region into logical lines and apply `P` to each. Every line must be
consumed by `P`; what `P` leaves over is forgiven only when it is whitespace.

```praxis
// `lines(P)` splits the region into lines and applies P to each one.
var values = read lines(int)

out(values)
out(values.sum())
```

```text
10
20
30
```

```text
[10, 20, 30]
60
```

Result type: `Vec[result(P)]`.

## `sections(P)`

Split on one or more blank lines and apply `P` to every section. A blank line is
`sections`' separator the way a comma is `csv`'s, so an interior run of them is
one separator and a trailing run is none.

```praxis
// `sections(P)` splits on blank lines and applies P to each section.
var groups = read sections(lines(int))

out(groups.len())
for g in groups {
    out(g.sum())
}
```

```text
1
2

3
4
5

6
```

```text
3
3
12
6
```

Result type: `Vec[result(P)]`.

## `sections(name: P, …)` and `repeated(P)`

Named arguments parse fixed sections **in order**, and the result is a record
with one field per name.

```praxis
// Named arguments parse fixed sections in order, into a record.
var data = read sections(
    rules: lines(`{before:int}|{after:int}`),
    updates: lines(csv(int)),
)

out(data.rules.len())
out(data.rules.get(0).before)
out(data.updates)
```

```text
47|53
97|13

75,47,61
97,61,53
```

```text
2
47
[[75, 47, 61], [97, 61, 53]]
```

Fewer sections than fields is a parse fault. More sections than fields is not:
the extra ones are simply not read — unless the last field is a `repeated(P)`
tail, which takes every section that is left and produces a `Vec`.

```praxis
// `repeated(P)` is the final named argument of a `sections` call: it takes
// every section that is left, as a Vec.
var bingo = read sections(
    draws: csv(int),
    boards: repeated(matrix(int)),
)

out(bingo.draws)
out(bingo.boards.len())
out(bingo.boards.get(1))
```

```text
7,4,9

1 2
3 4

5 6
7 8
```

```text
[7, 4, 9]
2
[5, 6, 7, 8]
```

A tail may only be last, there may only be one, and its name is a field name
like any other. All three are checked, because a tail that was silently moved to
the end compiled into a different parser than the one written.

## `csv(P)`

Split on commas and apply `P` to each field. Nothing is trimmed: the field is
handed to `P` whole, `P` skips the leading horizontal space it does not read,
and a leftover run of whitespace is forgiven.

```praxis
// `csv(P)` splits on commas. The space around a comma is left in the field;
// `int` does not read it, and what a parser declines is not data.
var program = read csv(int)

out(program)
```

```text
 1, 2 ,3,4
```

```text
[1, 2, 3, 4]
```

The same rule with a different child gives the answer you would want there too:
`csv(char)` over `a, ,c` reads three characters, one of them a space, because
`char` does read a space wherever it is offered one.

`csv` always makes at least one field, so it has no answer for a blank line.
`csv(int)` over one faults — and `lines(csv(int))` never sees a *trailing* one,
because a trailing line its parser makes nothing of is nobody's.

## `ws(P)`

Split on runs of whitespace and apply `P` to each token. Every whitespace
character separates, a line ending included, so a token never spans a line: two
lines of two numbers are four tokens.

```praxis
// `ws(P)` splits on runs of whitespace, line endings included, so a token
// never spans a line: this is four tokens, not three.
var tokens = read ws(int)

out(tokens)
```

```text
1 2
3   4
```

```text
[1, 2, 3, 4]
```

## `sep(SEPARATOR, P)`

Split on an exact string, with no implicit trimming. The separator's own spaces
are part of the separator.

```praxis
// `sep(SEPARATOR, P)` splits on an exact string. Nothing is trimmed.
var chain = read sep(" -> ", word)

out(chain)
```

```text
alpha -> beta -> gamma
```

```text
[alpha, beta, gamma]
```

The separator may not be empty. An empty one never advances the cursor, so it is
refused at compile time rather than looping at run time.

`sep` splits the *whole* region, newlines and all, so the per-line spelling is
`lines(sep(",", P))` rather than `sep(",", P)`.

## `chars(P, skip: policy)` and `one_of("…")`

Apply `P` repeatedly to characters. `skip:` says what is passed over *between*
matches, and each policy is named by what it skips:

| Policy | Skips |
|---|---|
| `none` | nothing — every byte of the region belongs to `P` |
| `whitespace` | spaces and tabs (the default) |
| `newlines` | spaces, tabs **and** line endings |

`newlines` is the broader policy: it skips everything `whitespace` skips and
line endings besides. The names suggest the opposite containment, which is why
they are spelled out here.

```praxis
// `chars(P, skip: policy)` applies P repeatedly. `newlines` is the broader
// policy: it skips spaces, tabs and line endings.
var moves = read chars(one_of("^v<>"), skip: newlines)

out(moves.len())
out(moves)
```

```text
^v<>
^^
```

```text
6
[^, v, <, >, ^, ^]
```

`one_of("…")` matches one character from a literal set and is the usual child
here, but it is an ordinary parser and works anywhere: `lines(one_of("LR"))` is
a `Vec[Char]` too.

`chars` reads its whole region or fails — a child failure is a mismatch, not a
place to stop. The case that looks like an exception and is not: the file's own
trailing newline is whitespace no child read, so `skip: none` still works on a
newline-terminated file.

```praxis
// `skip: none` lets nothing through between matches: every byte of the region
// must belong to the character parser. The file's own trailing newline is
// still nobody's — no policy has to absorb it, because `one_of` declined it.
var turns = read chars(one_of("LR"), skip: none)

out(turns)
```

```text
LRLR
```

```text
[L, R, L, R]
```

Result type: `Vec[result(P)]`, derived from the child — so
`chars(int, skip: whitespace)` is a `Vec[Int]`.

## `grid(P)`

Parse rectangular lines into a `Grid`. **A cell is what its cell parser reads**
([ADR-079](../../../decisions/079-a-grid-cell-is-what-its-cell-parser-reads.md)):
`char` reads one Unicode scalar, `digit` reads one digit, `int` reads a whole
integer token. A row's width is the number of cells it produced, and every row
must have the same count.

```praxis
// `grid(P)` parses rectangular lines into a Grid. A cell is whatever the cell
// parser reads: `char` reads one scalar, so this grid is 4 wide.
var map = read grid(char)

out(map.width())
out(map.height())
out(map[2, 0])
out(map)
```

```text
..#.
#...
```

```text
4
2
#
[., ., #, ., #, ., ., .]
```

Change the cell parser and the same file is a different grid:

```praxis
// The cell parser decides how far a cell reaches. `digit` reads one digit;
// `int` reads a whole integer token, so the same file is a different grid.
var heights = read grid(digit)

out(heights.width())
out(heights)
```

```text
12
34
```

```text
2
[1, 2, 3, 4]
```

`grid(int)` over those same bytes is *one* cell per row, because `int` reads
`12` whole. That is the general rule rather than a granularity, and it is why
`digit` exists at all.

Because `char` reads a space, a row that ends in one is a wider row and
`grid(char)` says so rather than quietly aligning it. See [whitespace, lines and
positions](whitespace.md) for the whole of that rule.

## `grid(P, ragged, fill: value)`

Permit uneven rows and pad every short one to the maximum width. `ragged` and
`fill:` come together or not at all.

```praxis
// Uneven rows need the ragged form, and `ragged` and `fill:` come together.
var pad = read grid(char, ragged, fill: ".")

out(pad.width())
out(pad.height())
out(pad)
```

```text
ab
cde
f
```

```text
3
3
[a, b, ., c, d, e, f, ., .]
```

The fill value is parsed by the cell parser, so it has to be something that
parser can read: `"."` for `grid(char, …)`, `0` for `grid(int, …)`. An empty
fill is refused, for the same reason an empty separator is.

## `matrix(P)`

Parse lines of whitespace-separated elements into a `Grid`. `matrix` splits a
row into tokens itself, where `grid` lets the cell parser decide, so column
alignment does not matter.

```praxis
// `matrix(P)` splits each row into whitespace-separated tokens itself.
var board = read matrix(int)

out(board.width())
out(board.height())
out(board[2, 1])
```

```text
22 13 17
 8  2 23
```

```text
3
2
23
```

**`matrix(P)` is not `lines(ws(P))`.** The two differ exactly where a line has
no tokens: `ws` answers a line of spaces with an empty `Vec`, which is still an
element, while `matrix` makes no row of it at all.

```praxis
// `matrix(P)` is not `lines(ws(P))`. They differ exactly where a line has no
// tokens: `ws` answers a line of spaces with an empty Vec, which is an
// element; `matrix` makes no row of it at all.
var rows = read lines(ws(int))
var grid = read matrix(int)

out(rows.len())
out(rows)
out(grid.height())
out(grid)
```

```text
1 2
3 4
  
```

```text
3
[[1, 2], [3, 4], []]
2
[1, 2, 3, 4]
```

An *interior* line with no tokens is a zero-token row, and it fails the width
check like any other row of the wrong size.

## `block(item, …)`

Apply parsers in sequence inside one region. A positional template contributes
its named captures to the block's record; a named item contributes one field.

```praxis
// A `block` applies its items in sequence inside one region. A positional
// template contributes its named captures to the block's record; a named item
// contributes one field. Each item is offered its own lines: the `csv(int)`
// capture is its template's last part and still stops at the end of its line.
var monkeys = read sections(block(
    `Monkey {id:int}:`,
    `  Starting items: {items:csv(int)}`,
    `  Operation: new = old {op:char} {operand:word}`,
))

for m in monkeys {
    out(m.id)
    out(m.items)
    out(m.op)
    out(m.operand)
}
```

```text
Monkey 0:
  Starting items: 79, 98
  Operation: new = old * 19

Monkey 1:
  Starting items: 54, 65, 75
  Operation: new = old + 6
```

```text
0
[79, 98]
*
19
1
[54, 65, 75]
+
6
```

**A block item is offered its own lines**
([ADR-090](../../../decisions/090-a-block-item-is-offered-its-own-lines.md)): a
*template* item gets the line it starts on plus one more for each `\n` the
template itself writes, and every other item gets the rest of the region,
because `lines`, `sections`, `grid` and `matrix` compute their own extent.
Without that rule the `{items:csv(int)}` above — a capture that is its
template's last part — would swallow the rest of the section.

The window is a narrowing and not a requirement: an item may stop short of it,
and `block` carries the cursor on to the next item. That is what lets two items
share a line.

```praxis
// A block item is offered its own lines, but it need not fill them: `block`
// carries its cursor to the next item, so two items can share a line.
var pair = read block(`a: {a:int}`, `b: {b:int}`)

out(pair.a)
out(pair.b)
```

```text
a: 1 b: 2
```

```text
1
2
```

One shape to keep in mind: a *non-template* greedy item followed by another item
still takes the rest of the region.

```praxis
// A non-template item is offered the rest of the region, so a greedy one that
// is not last leaves nothing for the item after it.
fn parts() -> Int {
    read block(`h:`, a: csv(int), b: word).a.len()
}

out(parts())
```

```text
h:
1,2
foo
```

```text
error: program faulted: input parse mismatch
       at input offset 6..11: expected the rest of the field
       actual: h:⏎1,2⏎foo⏎

Backtrace:
#0   parts
#1   <entry>

  temps:
    <tmp#1> = h:
1,2
foo

    <tmp#2: Int> = 1
    <tmp#5: Int> @ "read block(`h:`, a: csv(int), b: word).a.len()" = <uninit>
```

`csv` is offered everything after the header, `foo` is not an `int`, and `b`
would have had nothing left in any case. Put the `lines(...)` or `csv(...)` item
last.

A positional item that produces a scalar has no field name to contribute and is
refused at compile time; name it.

## `choice(Name: P, …)`

Parse one of several alternatives into an anonymous enum, one variant per named
case, each carrying its parser's result as its payload. **The first case that
matches wins**, and `choice` itself does not require the region to be consumed —
whoever bounded the region decides that. So inside `lines`, the longer
alternative goes first.

```praxis
// `choice` parses one of several alternatives into an anonymous enum, one
// variant per named case, each carrying its parser's result as its payload.
// The first case that matches wins, so the longer alternative goes first: a
// leading `Number` would match `bbb: 2` and leave ` 3` for `lines` to reject.
var entries = read lines(choice(
    Pair: `{name:word}: {left:int} {right:int}`,
    Number: `{name:word}: {value:int}`,
))

for e in entries {
    match e {
        Number(p) => out(p.value)
        Pair({ name, left, right }) => out(left + right)
    }
}
```

```text
aaa: 1
bbb: 2 3
```

```text
1
5
```

Cases are matched with an ordinary variant pattern. `Number(p)` binds the whole
payload record and reads it with `p.value`; `Pair({ name, left, right })` takes
it apart in the pattern, and that record pattern has no head because the payload
record is anonymous. See [pattern matching](../language/pattern-matching.md).

## `optional(P)`

Return `Option[result(P)]`. A failure consumes no input — this is parser-level
optionality, not error recovery.

```praxis
// `optional(P)` returns Option[T]. A failure consumes nothing.
var maybe = read optional(int)

out(maybe)
match maybe {
    Some(n) => out(n)
    None => out("no number at the start of the input")
}
```

```text
abc
```

```text
None
no number at the start of the input
```

Because the failure consumes nothing, the next item in a `block` sees the same
bytes: `block(a: optional(int), b: word)` over `xyz` is `{ a: None, b: xyz }`.

## `scan(P)`

Find repeated matches of `P` inside text that is otherwise irrelevant, in source
order, ignoring everything that does not match. Nothing bounds the root, so a
`scan` that matches nothing is an empty `Vec` rather than a fault.

```praxis
// `scan(P)` finds repeated matches inside text it otherwise ignores, and
// returns them in source order.
var program = read scan(choice(
    Multiply: `mul({left:int},{right:int})`,
    Enable: `do()`,
    Disable: `don't()`,
))

out(program.len())
var total = 0
var on = true
for step in program {
    match step {
        Multiply(p) => { if on { total = total + p.left * p.right } }
        Enable(_) => { on = true }
        Disable(_) => { on = false }
    }
}
out(total)
```

```text
xmul(2,3)%&mul(4,5)!don't()_do()?mul(6,7)
```

```text
5
68
```

`scan` steps by Unicode scalar on a miss, so it never attempts a match at a
continuation byte. `scan_exact`, the stricter variant the design document
mentions, does not exist.

## The one rule they all inherit

Every constructor above answers the same question the same way: **a run of
whitespace the parser offered it does not read is not data and not a mismatch**.
`int` cannot read the space in `1 `, so it is padding; `char` can, so it is a
cell. That single rule decides trailing spaces, trailing blank lines and the
file's own terminator for all of them, and it is why no constructor here carries
a newline special case. It is set out in [whitespace, lines and
positions](whitespace.md), and its reasoning is
[ADR-078](../../../decisions/078-a-parser-position-is-absolute-and-a-region-only-narrows.md).

For the types these constructors produce, see [how a parser gets its
type](type-derivation.md); for what happens when the input does not match, [when
a parse fails](faults.md); for a working program per input shape, the
[cookbook](cookbook.md).
