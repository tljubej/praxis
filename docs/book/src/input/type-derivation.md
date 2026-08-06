# How a parser gets its type

A parser expression has a result type, and the compiler works it out from the
expression alone — before the program runs and without looking at any input.
`read P` is an expression of exactly that type. There is no `ParseError`, no
`Result` to unwrap and no dynamic value to inspect: if the input does not match,
the program faults, and if it does match you already have a typed value.

The derivation is a walk over the parser expression. Every rule is local:
a collection constructor's type is its child's type wrapped, a labelled argument
becomes a record field, and an atom's type is fixed.

## Asking the compiler

The quickest way to find out what you have is to annotate the binding with a
type it cannot be. The `found` half of the diagnostic is the derived type.

```praxis
// Ask the compiler for a parser's type by annotating the binding with a type
// it cannot be. The `found` half of the diagnostic is the derived type.
var a: Bool = read int
var b: Bool = read digit
var c: Bool = read uint
var d: Bool = read float
var e: Bool = read byte
var f: Bool = read char
var g: Bool = read word
var h: Bool = read one_of("LR")
```

```console
$ praxis check t-atom-types.px --color never
```

```text
error[Y001]: expected Bool, found Int

  t-atom-types.px:3:15
  3 | var a: Bool = read int
    |               ^^^^^^^^ expected Bool, found Int

error[Y001]: expected Bool, found Int

  t-atom-types.px:4:15
  4 | var b: Bool = read digit
    |               ^^^^^^^^^^ expected Bool, found Int

error[Y001]: expected Bool, found Int

  t-atom-types.px:5:15
  5 | var c: Bool = read uint
    |               ^^^^^^^^^ expected Bool, found Int

error[Y001]: expected Bool, found Float

  t-atom-types.px:6:15
  6 | var d: Bool = read float
    |               ^^^^^^^^^^ expected Bool, found Float

error[Y001]: expected Bool, found Byte

  t-atom-types.px:7:15
  7 | var e: Bool = read byte
    |               ^^^^^^^^^ expected Bool, found Byte

error[Y001]: expected Bool, found Char

  t-atom-types.px:8:15
  8 | var f: Bool = read char
    |               ^^^^^^^^^ expected Bool, found Char

error[Y001]: expected Bool, found Text

  t-atom-types.px:9:15
  9 | var g: Bool = read word
    |               ^^^^^^^^^ expected Bool, found Text

error[Y001]: expected Bool, found Char

  t-atom-types.px:10:15
  10 | var h: Bool = read one_of("LR")
     |               ^^^^^^^^^^^^^^^^^ expected Bool, found Char

praxis: 8 error(s)
```

An editor shows the same type without the deliberate error — see [inference in
the editor](../types/editor.md).

A few things are worth noting from that run. `uint` and `digit` are both `Int`:
the non-negativity of `uint` and the one-digit rule of `digit` are parse rules,
not separate types. `byte` is `Byte` — a decimal number in `0..=255`, not a raw
input byte. `word`, `identifier`, `text` and `rest` are all `Text`. The full
list is in [atomic parsers](atoms.md).

## Templates

A template's type comes from its captures, and there are exactly four cases.

| Template | Type |
|---|---|
| no captures | `Unit` |
| one anonymous capture | that capture's type |
| several anonymous captures | a tuple, in order |
| named captures | an anonymous record, one field per name, in order |

```praxis
// A template's type is decided by its captures: none is Unit, one anonymous
// capture is that capture's type, several are a tuple, and named captures are
// an anonymous record.
var a: Bool = read `hello`
var b: Bool = read `{int}`
var c: Bool = read `{int},{int}`
var d: Bool = read `{x:int},{y:int}`
var e: Bool = read `{name:word} {values:csv(int)}`
```

```console
$ praxis check t-template-types.px --color never
```

```text
error[Y001]: expected Bool, found Unit

  t-template-types.px:4:15
  4 | var a: Bool = read `hello`
    |               ^^^^^^^^^^^^ expected Bool, found Unit

help: this value is `Unit`; the binding's type annotation expected `Bool` — make the last expression produce a value, or change the declared type to `Unit`

error[Y001]: expected Bool, found Int

  t-template-types.px:5:15
  5 | var b: Bool = read `{int}`
    |               ^^^^^^^^^^^^ expected Bool, found Int

error[Y001]: expected Bool, found (Int, Int)

  t-template-types.px:6:15
  6 | var c: Bool = read `{int},{int}`
    |               ^^^^^^^^^^^^^^^^^^ expected Bool, found (Int, Int)

error[Y001]: expected Bool, found { x: Int, y: Int }

  t-template-types.px:7:15
  7 | var d: Bool = read `{x:int},{y:int}`
    |               ^^^^^^^^^^^^^^^^^^^^^^ expected Bool, found { x: Int, y: Int }

error[Y001]: expected Bool, found { name: Text, values: Vec[Int] }

  t-template-types.px:8:15
  8 | var e: Bool = read `{name:word} {values:csv(int)}`
    |               ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ expected Bool, found { name: Text, values: Vec[Int] }

praxis: 5 error(s)
```

The last one is the general rule at work: a capture's body is a whole parser
expression, so `{values:csv(int)}` contributes a `Vec[Int]` field exactly the
way a bare `csv(int)` would be a `Vec[Int]`. Naming and shape are covered in
[templates and captures](templates.md); naming styles may not be mixed in one
template, which is what keeps these four cases from overlapping.

## A collection's type is its child's

Every splitting constructor wraps its child's type, and nesting the
constructors nests the type in the same order.

| Parser | Type |
|---|---|
| `lines(P)` | `Vec[result(P)]` |
| `sections(P)` | `Vec[result(P)]` |
| `csv(P)` | `Vec[result(P)]` |
| `ws(P)` | `Vec[result(P)]` |
| `sep("s", P)` | `Vec[result(P)]` |
| `chars(P, skip: …)` | `Vec[result(P)]` |
| `scan(P)` | `Vec[result(P)]` |
| `grid(P)` | `Grid[result(P)]` |
| `grid(P, ragged, fill: v)` | `Grid[result(P)]` |
| `matrix(P)` | `Grid[result(P)]` |
| `optional(P)` | `Option[result(P)]` |
| `one_of("…")` | `Char` |

```praxis
// A collection constructor's type is its child's, wrapped: nesting the
// constructors nests the type in the same order.
var a: Bool = read lines(int)
var b: Bool = read sections(lines(csv(int)))
var c: Bool = read ws(word)
var d: Bool = read sep(" -> ", word)
var e: Bool = read grid(char)
var f: Bool = read matrix(float)
var g: Bool = read chars(one_of("^v"), skip: newlines)
var h: Bool = read chars(int, skip: whitespace)
var i: Bool = read optional(`{x:int},{y:int}`)
var j: Bool = read scan(`mul({int},{int})`)
```

```console
$ praxis check t-collection-types.px --color never
```

```text
error[Y001]: expected Bool, found Vec[Int]

  t-collection-types.px:3:15
  3 | var a: Bool = read lines(int)
    |               ^^^^^^^^^^^^^^^ expected Bool, found Vec[Int]

error[Y001]: expected Bool, found Vec[Vec[Vec[Int]]]

  t-collection-types.px:4:15
  4 | var b: Bool = read sections(lines(csv(int)))
    |               ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ expected Bool, found Vec[Vec[Vec[Int]]]

error[Y001]: expected Bool, found Vec[Text]

  t-collection-types.px:5:15
  5 | var c: Bool = read ws(word)
    |               ^^^^^^^^^^^^^ expected Bool, found Vec[Text]

error[Y001]: expected Bool, found Vec[Text]

  t-collection-types.px:6:15
  6 | var d: Bool = read sep(" -> ", word)
    |               ^^^^^^^^^^^^^^^^^^^^^^ expected Bool, found Vec[Text]

error[Y001]: expected Bool, found Grid[Char]

  t-collection-types.px:7:15
  7 | var e: Bool = read grid(char)
    |               ^^^^^^^^^^^^^^^ expected Bool, found Grid[Char]

error[Y001]: expected Bool, found Grid[Float]

  t-collection-types.px:8:15
  8 | var f: Bool = read matrix(float)
    |               ^^^^^^^^^^^^^^^^^^ expected Bool, found Grid[Float]

error[Y001]: expected Bool, found Vec[Char]

  t-collection-types.px:9:15
  9 | var g: Bool = read chars(one_of("^v"), skip: newlines)
    |               ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ expected Bool, found Vec[Char]

error[Y001]: expected Bool, found Vec[Int]

  t-collection-types.px:10:15
  10 | var h: Bool = read chars(int, skip: whitespace)
     |               ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ expected Bool, found Vec[Int]

error[Y001]: expected Bool, found Option[{ x: Int, y: Int }]

  t-collection-types.px:11:15
  11 | var i: Bool = read optional(`{x:int},{y:int}`)
     |               ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ expected Bool, found Option[{ x: Int, y: Int }]

error[Y001]: expected Bool, found Vec[(Int, Int)]

  t-collection-types.px:12:15
  12 | var j: Bool = read scan(`mul({int},{int})`)
     |               ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ expected Bool, found Vec[(Int, Int)]

praxis: 10 error(s)
```

Two of those are worth pausing on. `chars(int, skip: whitespace)` is a
`Vec[Int]` and not a `Vec[Char]` — the element type is derived from the child,
not assumed from the constructor's name. And `matrix(P)` and ragged `grid(P)`
both answer `Grid[result(P)]`: there is no separate `Matrix` type.

## Labelled arguments become record fields

Wherever the parser grammar takes a name, the name becomes a field or a variant.

- A named `sections(...)` is a record with one field per named section, in
  source order.
- A `repeated(P)` tail is one more field, holding a `Vec[result(P)]`. A counted
  `repeated(P, N)` holds the same `Vec[result(P)]`, but in the position it was
  written rather than at the end — the record's field order is the source order
  of the named arguments, so
  `sections(shapes: repeated(lines(int), 2), regions: lines(int))` is
  `{ shapes: Vec[Vec[Int]], regions: Vec[Int] }`. The count changes how many
  sections the field reads, not what it holds.
- A `block(...)` is one record: a named item contributes its own field, and a
  positional *template* contributes each of its named captures directly —
  flattened into the same record rather than nested inside one.
- A `choice(...)` is an anonymous enum with one variant per case, each carrying
  its case parser's result as its payload.

```praxis
// Named arguments become record fields. A `block` flattens a positional
// template's captures into the same record; a `repeated` tail is one field
// holding a Vec; a `choice` is an anonymous enum, one variant per case.
// Each probe is on one line so the diagnostic underlines it on one line.
var a: Bool = read sections(rules: lines(`{before:int}|{after:int}`), updates: lines(csv(int)))
var b: Bool = read sections(draws: csv(int), boards: repeated(matrix(int)))
var c: Bool = read block(`{source:word}-to-{dest:word} map:`, ranges: lines(`{a:int} {b:int}`))
var d: Bool = read choice(Number: `{name:word}: {value:int}`, Op: `{name:word}: {l:word} {r:word}`)
```

```console
$ praxis check t-record-types.px --color never
```

```text
error[Y001]: expected Bool, found { rules: Vec[{ before: Int, after: Int }], updates: Vec[Vec[Int]] }

  t-record-types.px:5:15
  5 | var a: Bool = read sections(rules: lines(`{before:int}|{after:int}`), updates: lines(csv(int)))
    |               ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ expected Bool, found { rules: Vec[{ before: Int, after: Int }], updates: Vec[Vec[Int]] }

error[Y001]: expected Bool, found { draws: Vec[Int], boards: Vec[Grid[Int]] }

  t-record-types.px:6:15
  6 | var b: Bool = read sections(draws: csv(int), boards: repeated(matrix(int)))
    |               ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ expected Bool, found { draws: Vec[Int], boards: Vec[Grid[Int]] }

error[Y001]: expected Bool, found { source: Text, dest: Text, ranges: Vec[{ a: Int, b: Int }] }

  t-record-types.px:7:15
  7 | var c: Bool = read block(`{source:word}-to-{dest:word} map:`, ranges: lines(`{a:int} {b:int}`))
    |               ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ expected Bool, found { source: Text, dest: Text, ranges: Vec[{ a: Int, b: Int }] }

error[Y001]: expected Bool, found { Number({ name: Text, value: Int }) | Op({ name: Text, l: Text, r: Text }) }

  t-record-types.px:8:15
  8 | var d: Bool = read choice(Number: `{name:word}: {value:int}`, Op: `{name:word}: {l:word} {r:word}`)
    |               ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ expected Bool, found { Number({ name: Text, value: Int }) | Op({ name: Text, l: Text, r: Text }) }

praxis: 4 error(s)
```

Note what the `block` line does *not* say: there is no
`{ header: { source: …, dest: … }, ranges: … }`. `source` and `dest` sit beside
`ranges` in one flat record, because a positional template's captures are
flattened. A named argument is what nests: in the almanac parser below, the
field `seeds` is a `block(...)` named argument and its type is the block's own
record, `{ values: Vec[Int] }`.

Duplicate names are refused: two `sections` fields, two `block` fields, two
captures in one template or two `choice` cases with the same name are all
compile errors, because the record or enum they would build cannot be written.

## Reading a nested type

Once the type is derived, it is an ordinary type. Fields are read with `.`, and
a record pattern takes one apart without naming it.

```praxis
// The derived type is an ordinary type: fields are read with `.`, and a
// record pattern takes one apart without naming it, because it has no name.
var almanac = read sections(
    seeds: block(`seeds: {values:ws(int)}`),
    maps: repeated(block(
        `{source:word}-to-{destination:word} map:`,
        ranges: lines(`{destination:int} {source:int} {length:int}`),
    )),
)

out(almanac.seeds.values)
out(almanac.maps.len())

for m in almanac.maps {
    out(m.source)
    out(m.destination)
    for { destination, source, length } in m.ranges {
        out(destination + source + length)
    }
}
```

```text
seeds: 79 14 55

seed-to-soil map:
50 98 2
52 50 48

soil-to-fertilizer map:
0 15 37
```

```text
[79, 14, 55]
2
seed
soil
150
150
soil
fertilizer
52
```

Read that type outside in and it matches the parser expression term for term:
`sections(seeds: …, maps: repeated(…))` is a record with `seeds` and `maps`;
`repeated(block(…))` makes `maps` a `Vec` of the block's record; the block's
positional template contributes `source` and `destination`, and its named
`ranges: lines(...)` contributes a `Vec` of the line template's record.

## Anonymous means anonymous

The record and enum types a parser produces have no name, and there is no syntax
for writing one down. That has one practical consequence: a helper function
cannot declare a parameter of that type, and a `struct` with the same fields is
a *different* type.

```praxis
// A parser's record type is anonymous. It is not the declared `struct` that
// has the same fields, and there is no syntax for writing it in an annotation,
// so a helper function cannot take one as a parameter.
struct Point { x: Int, y: Int }

fn total(ps: Vec[Point]) -> Int {
    ps.map(|p| p.x + p.y).sum()
}

out(total(read lines(`{x:int},{y:int}`)))
```

```console
$ praxis check t-anonymous-vs-struct.px --color never
```

```text
error[Y001]: expected (Vec[Point]) -> Int, found (Vec[{ x: Int, y: Int }]) -> ?T

  t-anonymous-vs-struct.px:10:5
  10 | out(total(read lines(`{x:int},{y:int}`)))
     |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ expected (Vec[Point]) -> Int, found (Vec[{ x: Int, y: Int }]) -> ?T

praxis: 1 error(s)
```

In practice this costs little: closures infer their parameter types, so
`.map(|p| p.x + p.y)` works without an annotation, and a loop over the value
needs none either. Where a named type is genuinely wanted, it is one `.map`
away.

```praxis
// A named type is one `.map` away: the parsed record has the fields, and a
// struct literal takes them.
struct Point { x: Int, y: Int }

fn total(ps: Vec[Point]) -> Int {
    ps.map(|p| p.x + p.y).sum()
}

var points = read lines(`{x:int},{y:int}`).map(|p| Point { x: p.x, y: p.y })

out(points.len())
out(total(points))
```

```text
1,2
3,4
```

```text
2
10
```

More on structural records and where they do and do not unify is in [records
without names](../types/structural-records.md).
