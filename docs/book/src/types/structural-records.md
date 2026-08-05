# Records without names

There are two kinds of record type in Praxis. A `struct` declaration makes a
**nominal** one: `Point` is `Point` because it is that declaration, and a second
declaration with identical fields is a different type. The input parser makes
**anonymous** ones: `{ x: Int, y: Int }` is that field set and nothing else, and
any two of them with the same fields are the same type (§5.6).

The anonymous kind is not something you can write. There is no record literal
without a name in front of it: a `{ … }` in expression position is a block, so
`{ x: 1, y: 2 }` is read as one and rejected. Every anonymous record in a
program comes from a named capture in a parser expression:

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

## Same fields, same type

Two anonymous records are the same type when their field-name sets match and
their field types unify. Identity is established *by unification* rather than by
a lookup at construction, which is what makes the field types get checked rather
than assumed
([ADR-025](../../../decisions/025-typedata-record-enum-defid.md)).

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

## Field order does not decide the type

`{ w: Int, h: Int }` and `{ h: Int, w: Int }` are one type. Unification matches
fields by name, not by position. What *is* preserved is display and
construction: a value prints in the order its own parser wrote the captures.

**One caveat, and it is a real one.** The runtime layout also follows source
order, and a field read compiles to a slot index taken from whichever definition
unification made canonical. So if two anonymous records of the same type were
written in different field orders and are then used together, the field read
takes the wrong slot:

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
{ h: 4, w: 3 }
3
4
```

`b.w` is 3, and `width_of(b)` answers 4. This is a compiler bug, not a language
rule; until it is fixed, write the captures of a shared shape in one order. A
program with a single spelling of each shape — which is nearly all of them —
cannot reach it.

## Nominal identity is a definition applied to arguments

A `struct` or `enum` type is a **definition** plus its type arguments, and its
identity is the definition — not its name and not its shape
([ADR-048](../../../decisions/048-nominal-identity-is-a-definition-applied-to-arguments.md)).
Two declarations with identical fields are two definitions and therefore two
types:

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

Use what the parser derives when the shape appears once and is used near where
it was read. That is most of a puzzle solution, and declaring a `struct` to hold
what ``lines(`{x:int},{y:int}`)`` already produces buys nothing.

Declare a `struct` when you want one of these three things:

**A name in every diagnostic.** `expected (Point) -> Int` reads better than
`expected ({ x: Int, y: Int }) -> Int`, and the difference grows with the field
count.

**Two shapes kept apart.** This is the real one. Two anonymous records with the
same fields are the *same type*, so nothing stops a `{ x: Int, y: Int }` meant
as a position being passed where one meant as a velocity is expected. Two
`struct`s make that a compile error, as above.

**Fields the parser did not give you.** A record you build yourself can carry a
computed field, a default, or a value from somewhere else in the program.

The conversion is a loop and a literal:

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
