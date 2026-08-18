# Records without names

There are two kinds of record type in Praxis. A `struct` declaration makes a
**nominal** one: `Point` is `Point` because it is that declaration, and a second
declaration with identical fields is a different type. The other kind is
**anonymous**: `{ x: Int, y: Int }` is that field set and nothing else, and any
two of them with the same fields are the same type.

Two things produce an anonymous record, and they produce the same one. You write
a literal with no name in front of it — `{ x: 1, y: 2 }`, whose type is the
fields it just listed. Or a named capture in a parser expression derives one
with no literal at all:

```praxis
var rows = read lines(`{x:int},{y:int}`)

for r in rows {
    out(r.x * r.y)
}
out(rows)
```

Given

```text
1,2
3,4
5,6
```

it prints

```text
2
12
30
[{ x: 1, y: 2 }, { x: 3, y: 4 }, { x: 5, y: 6 }]
```

`rows` is a `Vec[{ x: Int, y: Int }]`, and it is that type before the program
runs. How a parser expression arrives at it is
[type derivation](../input/type-derivation.md).

## Writing one

A record literal with no name in front of it builds an anonymous record. Nothing
is declared first, because there is nothing to declare: the fields are the type.

```praxis
var p = { x: 1, y: 2 }
out(p.x + p.y)

p.x = 9
out(p)

var name = "origin"
var tagged = { name, pos: p }
out(tagged)
out(tagged.pos.y)
```

```text
3
{ x: 9, y: 2 }
{ name: origin, pos: { x: 9, y: 2 } }
2
```

Fields are read and assigned like any other record's, they nest, and `{ name }`
puns — it takes the field from the binding of that name, exactly as the headed
form does. The parser and the literal build the same type, so a helper written
for rows a template produced takes one you wrote by hand:

```praxis
fn area(r) { r.w * r.h }

var rooms = read lines(`{w:int}x{h:int}`)
var default_room = { w: 2, h: 5 }

for room in rooms {
    out(area(room))
}
out(area(default_room))
```

Given

```text
3x4
10x10
```

it prints

```text
12
100
10
```

### The one brace that stays a block

A `{` where an expression must begin could open either a block or a record, and
the tie is broken by **what a block cannot be**: a name followed by a `:`, or a
name followed by a `,`. So `{ x: 1 }`, `{ x: 1, y: 2 }` and `{ x, y }` are
records, and everything else at that position is the block it always was.

`{ x }` is the one case the rule cannot have both ways. It is a well-formed
block whose value is `x` *and* a well-formed one-field punned record, and blocks
had the spelling first:

```praxis
var x = 7

// A block, whose value is its last statement.
var from_block = { x }
out(from_block)

// A record with one field, which needs the field written out.
var from_record = { x: x }
out(from_record)
```

```text
7
{ x: 7 }
```

The other one is `{ x:bp }`, which is a block holding the statement `x` with a
[`:bp` marker](../debugger/breakpoints.md) on it. `{ x: bp }` with a space is the
record whose field is the binding `bp` — the same adjacency that separates
`min=` from `min =`.

A keyword head does not suppress the literal, and does not need to. What the
[record-literal suppression](../language/records.md#a-record-literal-must-not-be-mistaken-for-a-block) protects is `p { … }`, a
*name* followed by the brace that could be the `if`'s own block; a brace where
an operand is still required cannot be that block, because the block comes after
a complete condition. So `if { hit: true }.hit { … }` reads both braces the way
you would expect.

## Same fields, same type

Two anonymous records are the same type when their field-name sets match and
their field types unify. Identity is established *by unification* rather than by
a lookup at construction, which is what makes the field types get checked rather
than assumed.

The practical effect is that a helper written for one parser's rows works for
another's, with no declaration in between:

```praxis
fn area(r) {
    r.w * r.h
}

var rooms = read lines(`{w:int}x{h:int}`)
var default_room = parse("2x5", `{w:int}x{h:int}`)

for room in rooms {
    out(area(room))
}
out(area(default_room))
```

Given

```text
3x4
10x10
```

it prints

```text
12
100
10
```

`rooms`'s element type and `default_room`'s type were synthesized by two
separate walks over two separate parser expressions, and they are one type.

Disagree about a field name and they are not:

```praxis
fn area(r) {
    r.w * r.h
}

var rooms = parse("3x4", `{w:int}x{h:int}`)
var boxes = parse("3x4", `{w:int}x{d:int}`)

out(area(rooms))
out(area(boxes))
```

```console
$ praxis check different-fields.px --color never
error[Y001]: expected ({ w: Int, h: Int }) -> Int, found ({ w: Int, d: Int }) -> ?T

  different-fields.px:9:5
  9 | out(area(boxes))
    |     ^^^^^^^^^^^ expected ({ w: Int, h: Int }) -> Int, found ({ w: Int, d: Int }) -> ?T

praxis: 1 error(s)
```

The `expected` half is `area`'s inferred signature — the first call pinned its
parameter, because a field read pins its receiver the same way a method call
pins its own (see [Generalization](generalization.md)).

## An anonymous record is an ordinary record

It has fields you read and assign, it matches a record pattern, it compares
structurally, and it can be a `Map` key or a `Set` element. The only thing it
lacks is a name.

```praxis
var p = read `{x:int},{y:int}`

match p {
    { x, y } => out(x + y)
}

p.x = 9
out(p)

var seen = Set()
seen.insert(p)
seen.insert(read `{x:int},{y:int}`)
out(seen.len())
```

Given

```text
1,2
```

it prints

```text
3
{ x: 9, y: 2 }
2
```

Note the last `read`: every `read` parses the whole input from its start, so the
second one produces a fresh `{ x: 1, y: 2 }` — a different value of the same
type, which is why the set holds two.

## Field order does not decide the type, and the type decides field order

`{ w: Int, h: Int }` and `{ h: Int, w: Int }` are one type. Unification matches
fields by name, not by position.

That has a second half worth stating outright, because it is the one people do
not expect: **one type has one field order**, and it is the order the shape was
first written in anywhere in the program. Every value of it is laid out and
printed that way, whichever spelling built it.

```praxis
fn width_of(r) { r.w }

var a = parse("3x4", `{w:int}x{h:int}`)
var b = parse("4x3", `{h:int}x{w:int}`)

out(a)
out(b)
out(width_of(a))
out(width_of(b))
```

```text
{ w: 3, h: 4 }
{ w: 3, h: 4 }
3
3
```

`b`'s template writes `h` first and `b` prints `w` first, because `a` got there
first and fixed the shape's order. The *values* are still what the input said:
`b` read `4` as its `h` and `3` as its `w`, so `width_of(b)` is 3.

The order has to come from something that has seen every spelling, and within
one compile that is the type arena, which registered a definition for each. It
cannot be a property of the template or literal doing the building, because the
whole point of the rule above is that those all produce one type — and a field
read compiles to a slot index against that one type. A value laid out in its own
producer's order would put `w` in `h`'s slot for whichever spelling was not
canonical, and the read would answer the wrong field with nothing to report.

All this costs is that display order depends on where the *first* spelling
appears in the file. A program with a single spelling of each shape — which is
nearly all of them — cannot tell.

## Nominal identity is a definition applied to arguments

A `struct` or `enum` type is a **definition** plus its type arguments, and its
identity is the definition — not its name and not its shape. Two declarations
with identical fields are two definitions and therefore two types:

```praxis
struct Point { x: Int, y: Int }
struct Velocity { x: Int, y: Int }

fn magnitude(p) {
    abs(p.x) + abs(p.y)
}

out(magnitude(Point { x: 1, y: 2 }))
out(magnitude(Velocity { x: 3, y: 4 }))
```

```console
$ praxis check two-structs.px --color never
error[Y001]: expected (Point) -> Int, found (Velocity) -> ?T

  two-structs.px:9:5
  9 | out(magnitude(Velocity { x: 3, y: 4 }))
    |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ expected (Point) -> Int, found (Velocity) -> ?T

praxis: 1 error(s)
```

The same rule separates a declared record from a derived one, even when the
fields line up exactly:

```praxis
struct Room { w: Int, h: Int }

fn area(r) {
    r.w * r.h
}

var parsed = parse("3x4", `{w:int}x{h:int}`)

out(area(parsed))
out(area(Room { w: 2, h: 5 }))
```

```console
$ praxis check nominal-is-not-structural.px --color never
error[Y001]: expected ({ w: Int, h: Int }) -> Int, found (Room) -> ?T

  nominal-is-not-structural.px:10:5
  10 | out(area(Room { w: 2, h: 5 }))
     |     ^^^^^^^^^^^^^^^^^^^^^^^^^ expected ({ w: Int, h: Int }) -> Int, found (Room) -> ?T

praxis: 1 error(s)
```

There is no conversion and no coercion between the two. A parser derives an
anonymous record; a `struct` literal builds a nominal one; nothing turns one
into the other except writing the fields out.

`Option` is the one *generic* definition in the language: `Option[Int]` and
`Option[Text]` are one definition at two arguments, which is why they print
their argument and why the monomorphizer can tell them apart. There is no
`struct P[T]` syntax — a user definition has no parameters, so substitution is
free and identity is just the definition.

## When to declare a `struct` instead

Use an anonymous record — derived or written — when the shape appears once and
is used near where it was made. That is most of a puzzle solution, and declaring
a `struct` to hold what ``lines(`{x:int},{y:int}`)`` already produces buys
nothing.

Declare a `struct` when you want one of these two things:

**A name in every diagnostic.** `expected (Point) -> Int` reads better than
`expected ({ x: Int, y: Int }) -> Int`, and the difference grows with the field
count.

**Two shapes kept apart.** This is the real one, and it is the only thing a
literal cannot do for you. Two anonymous records with the same fields are the
*same type*, so nothing stops a `{ x: Int, y: Int }` meant as a position being
passed where one meant as a velocity is expected. Two `struct`s make that a
compile error, as above.

Carrying a field the parser did not produce is not a third reason: an anonymous
record literal holds whatever fields you write into it.

Converting to a declared one is a loop and a literal:

```praxis
struct Segment { x1: Int, y1: Int, x2: Int, y2: Int }

var rows = read lines(`{x1:int},{y1:int} -> {x2:int},{y2:int}`)

var segments = Vec()
for r in rows {
    segments.push(Segment { x1: r.x1, y1: r.y1, x2: r.x2, y2: r.y2 })
}

for s in segments {
    out(s)
}
```

Given

```text
0,9 -> 5,9
8,0 -> 0,8
```

it prints

```text
{ x1: 0, y1: 9, x2: 5, y2: 9 }
{ x1: 8, y1: 0, x2: 0, y2: 8 }
```

Note that a nominal record prints as its fields too — `out` shows
`{ x1: 0, … }`, with no `Segment` in front of it. The name is for the type
system and the diagnostics, not for the output. [Records](../language/records.md)
covers the declaration form in full.
