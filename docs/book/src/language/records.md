# Records

A record is a fixed set of named fields. You declare one with `struct`, build one
with a brace literal, and read a field with a dot. There is no `impl` block and
no method syntax: a record is data, and the operations on it are the functions
you write.

```praxis
struct Point {
    x: Int
    y: Int
}

var p = Point { x: 3, y: 4 }
out(p)
out(p.x)

// A field is an assignable place, in every spelling an assignment has.
p.x = 5
p.y += 1
out(p)

// Field punning: `x` and `y` are already the names the fields want.
var x = 10
var y = 20
out(Point { x, y })
```

```text
{ x: 3, y: 4 }
3
{ x: 5, y: 5 }
{ x: 10, y: 20 }
```

Fields are separated by a comma **or** a line break, so `struct Point { x: Int,
y: Int }` on one line and the four-line form above are the same declaration. A
trailing comma closes the list.

Note what `out` prints: `{ x: 3, y: 4 }`, with no `Point` in front of it. A
record formats as its fields.

## Fields are places, and a record is an object

`p.x = 5` writes the field. The binding is not what is written — the *object*
is — so the receiver need not be a name you reassign, and every other reference
to that record sees the write.

```praxis
struct Point { x: Int, y: Int }

// A record value is an object. A binding names it; it does not own it, so a
// write through one name is visible through every other.
var nodes = [Point { x: 0, y: 0 }]
nodes[0].x += 1
out(nodes)

var alias = nodes[0]
alias.y = 9
out(nodes[0])
```

```text
[{ x: 1, y: 0 }]
{ x: 1, y: 9 }
```

`nodes[0].x += 1` evaluates `nodes[0]` once, reads the field and writes it back.
`min=` and `max=` are not among the spellings a field accepts: those are map
updates, and what they mean — "an absent entry accepts the first value" — is
about an entry that might not be there. A field always is.

## Equality, hashing and nesting come for free

Two records of the same type are equal when their fields are equal, and they hash
consistently with that, so a record is a map key or a set element with no
declaration on your part. Records nest.

```praxis
struct Point { x: Int, y: Int }
struct Segment { label: Text, from: Point, to: Point }

// Two records of the same type with equal fields are equal, and hash alike.
var a = Point { x: 1, y: 2 }
var b = Point { x: 1, y: 2 }
out(a == b)

var seen = Set()
seen.insert(a)
seen.insert(b)
out(seen.len())

var owner = Map()
owner[a] = "north"
out(owner[b])

// Records nest, and a nested field is read through the outer one.
var s = Segment { label: "edge", from: a, to: Point { x: 4, y: 6 } }
out(s)
out(s.to.y)
```

```text
true
1
north
{ label: edge, from: { x: 1, y: 2 }, to: { x: 4, y: 6 } }
6
```

A `struct` is **nominal**: it is the same type as itself and nothing else, so two
declarations with identical fields are two types. `Point { x: 1, y: 2 } ==
Vector { x: 1, y: 2 }` does not compare unequal — it is `Y001`, `expected Point,
found Vector`.

You cannot derive or implement any of this, and there is nothing to opt out of:
equality, hashing and formatting are decided by the compiler from the field
types.

Records are **not** ordered. Nothing says which field decides, so `<` and
`sorted()` refuse them:

```praxis
struct Point { x: Int, y: Int }

// Records are equatable and hashable. They are not ordered: nothing says which
// field decides, so `<` and `sorted()` refuse them. Sort by a key instead.
var ps = [Point { x: 2, y: 0 }, Point { x: 1, y: 9 }]
out(ps.sorted())
```

```console
$ praxis check docs/book/examples/records-enums/record-cannot-be-ordered.px
error[Y006]: values of type `Point` cannot be ordered

  record-cannot-be-ordered.px:6:8
  6 | out(ps.sorted())
    |        ^^^^^^ values of type `Point` cannot be ordered

praxis: 1 error(s)
```

Name the field that decides and it sorts:

```praxis
struct Point { x: Int, y: Int }

var ps = [Point { x: 2, y: 0 }, Point { x: 1, y: 9 }]
out(ps.sorted_by_key(|p: Point| p.x))
```

```text
[{ x: 1, y: 9 }, { x: 2, y: 0 }]
```

## A record literal must not be mistaken for a block

`if p { … }` is genuinely ambiguous: `p { … }` is a well-formed record literal,
and `p` followed by a block is a well-formed `if`. The rule is that four keyword
heads — `if` and `while`'s conditions, `for`'s iterator, `match`'s scrutinee —
claim the brace as their block, and everywhere else a literal is legal.

```praxis
struct Point { x: Int, y: Int }

var p = Point { x: 1, y: 2 }

// `if`, `while`, `for` and `match` claim the brace that follows their head, so
// a record literal there needs parentheses to say it is not the block.
if (Point { x: 1, y: 2 } == p) {
    out("same")
}

// Inside a bracket the grammar knows what closes it, so a literal is legal at
// any depth: an argument list, a match arm body, a block, a closure.
out(match p {
    Point { x: 1, y } => Point { x: 9, y: y }
    _ => p
})
out([Point { x: 0, y: 0 }].map(|q: Point| Point { x: q.y, y: q.x }))
```

```text
same
{ x: 9, y: 2 }
[{ x: 0, y: 0 }]
```

Suppression follows the head's operands and stops at brackets: a parenthesized
expression, an argument list, a block, a record body and a match arm all re-enter
with literals allowed. Writing `if p == Point { x: 1, y: 2 } { … }` without the
parentheses is not a tidy error — the `{` becomes the `if`'s block, the field
list becomes statements, and one line yields a dozen diagnostics. The fix is the
parentheses.

The **anonymous** literal has no such ambiguity to suppress, and none of this
applies to it. `p { … }` is ambiguous because the name could be the whole head;
a `{` where an operand is still required cannot be a keyword's block, because
that block comes after a complete head. So `if { hit: true }.hit { … }` needs no
parentheses. What that form has instead is its own tie with the block —
`{ x: 1 }` versus `{ x }` — which
[Records without names](../types/structural-records.md#the-one-brace-that-stays-a-block)
covers.

## Anonymous records

A record literal with no name in front of it — `{ x: 1, y: 2 }` — builds a
record whose type *is* its field set, and a named-capture template derives the
same kind with no declaration anywhere. Between them this is where most records
in a puzzle-shaped program come from.

```praxis
// A named-capture template derives a record type with no declaration anywhere.
var points = read lines(`{x:int},{y:int}`)
out(points)
out(points[0].x)

// The derived record is an ordinary value: its fields are read by name, and a
// function that takes one need not name the type.
fn area(r) -> Int { r.x * r.y }
for r in points {
    out(area(r))
}
```

Given this input:

```text
3,4
10,20
```

```text
[{ x: 3, y: 4 }, { x: 10, y: 20 }]
3
12
200
```

An anonymous record is a type of its own. It *prints* as `{ x: Int, y: Int }`,
but that is a spelling diagnostics use and not one you can write as an
**annotation**: the type grammar has no record form, so
`var p: { x: Int, y: Int }` does not parse and an anonymous record only ever
gets its type from inference — including from a `{ x: 1, y: 2 }` literal, which
is the *value* form and does parse. Two anonymous records are the same type when
their field names match and their field types unify. A `struct` is not one of
them, however alike the two look.

```praxis
struct Point { x: Int, y: Int }

// An anonymous record is a type of its own. It is the same type as another
// anonymous record with the same field names and types, and it is never the
// same type as a `struct` that happens to look like it.
var rows = read lines(`{x:int},{y:int}`)
var p: Point = rows[0]
out(p)
```

```console
$ praxis check docs/book/examples/records-enums/record-anonymous-is-not-nominal.px
error[Y001]: expected Point, found { x: Int, y: Int }

  record-anonymous-is-not-nominal.px:7:16
  7 | var p: Point = rows[0]
    |                ^^^^^^^ expected Point, found { x: Int, y: Int }

praxis: 1 error(s)
```

To cross the line, build the `struct` from the fields: `Point { x: rows[0].x,
y: rows[0].y }`. Most programs never need to — the anonymous record already has
the fields, and a function that takes one need not name its type. The type
system's side of it is [Records without names](../types/structural-records.md).

## A bare `.name` is a field; a zero-argument accessor is a call

`p.x` reads a field: it lowers to a slot index taken from the record's
definition. `v.len()` calls a method: it looks the name up in the catalog. The
two are different syntax on purpose, and there is no property form of a method.

```praxis
struct Row { len: Int }

var r = Row { len: 4 }
out(r.len)

var v = [1, 2, 3]
out(v.len())
out(v.len)
```

```console
$ praxis check docs/book/examples/records-enums/record-field-is-not-a-call.px
error[Y112]: no field `len` on type `Vec[Int]`

  record-field-is-not-a-call.px:8:7
  8 | out(v.len)
    |       ^^^ no field `len` on type `Vec[Int]`

praxis: 1 error(s)
```

A record field may be called `len` and is unaffected: `r.len` reads it, `r.len()`
looks for a method. `v.len()`, `grid.width()` and `grid.height()` all take their
parentheses, and a bare one of those is `Y112` naming the type it was asked of.
The rule exists because a receiver whose type inference has not pinned yet cannot
tell a field read from a nullary call, and picking by "whichever the receiver
happens to have" would mean adding a field could silently change what an existing
expression does.

## What the compiler reports

A literal must supply every field, exactly once, and only fields the record has.

```praxis
struct Point { x: Int, y: Int }

var missing = Point { x: 1 }
var extra = Point { x: 1, y: 2, z: 3 }
var twice = Point { x: 1, x: 2, y: 3 }
out(missing.z)
```

```console
$ praxis check docs/book/examples/records-enums/record-literal-fields.px
error[Y113]: `Point` literal is missing a field: y

  record-literal-fields.px:3:15
  3 | var missing = Point { x: 1 }
    |               ^^^^^^^^^^^^^^ `Point` literal is missing a field: y

error[Y114]: `Point` has no field `z`

  record-literal-fields.px:4:33
  4 | var extra = Point { x: 1, y: 2, z: 3 }
    |                                 ^ `Point` has no field `z`

error[Y115]: field `x` is initialized more than once

  record-literal-fields.px:5:27
  5 | var twice = Point { x: 1, x: 2, y: 3 }
    |                           ^ field `x` is initialized more than once

error[Y112]: no field `z` on type `Point`

  record-literal-fields.px:6:13
  6 | out(missing.z)
    |             ^ no field `z` on type `Point`

praxis: 4 error(s)
```

## A type cannot refer to itself

There are no recursive types. A declaration that reaches itself through its own
annotations is `N006`, and a mutual pair is named through the member that closes
the cycle.

```praxis
struct Node {
    next: Node
    value: Int
}

struct A { b: B }
struct B { a: A }

out(1)
```

```console
$ praxis check docs/book/examples/records-enums/record-self-referring.px
error[N006]: `Node` refers to itself, and a self-referring type is not supported

  record-self-referring.px:1:8
  1 | struct Node {
    |        ^^^^ `Node` refers to itself, and a self-referring type is not supported

error[N006]: `A` refers to itself through `B`, and a self-referring type is not supported

  record-self-referring.px:6:8
  6 | struct A { b: B }
    |        ^ `A` refers to itself through `B`, and a self-referring type is not supported

error[N006]: `B` refers to itself through `A`, and a self-referring type is not supported

  record-self-referring.px:7:8
  7 | struct B { a: A }
    |        ^ `B` refers to itself through `A`, and a self-referring type is not supported

praxis: 3 error(s)
```

Indirection does not help: `struct Node { children: Vec[Node] }` is the same
`N006`, reported at `Node`. The message says the *feature* is missing rather than
that the values are impossible, and that wording is deliberate — every field
holds a reference, so a tree is a perfectly ordinary runtime shape. What is
absent is recursive types in the type system. Model the tree with node ids
instead: a `Map[Int, Vec[Int]]` of children.

One report is emitted per cycle member, and a declaration that merely sits
*behind* a cycle is not the mistake and is not reported — in `struct C { a: A }`
above `struct A { b: B }` and `struct B { a: A }`, only `A` and `B` are named.

## What is not here

There is no generic `struct`: `struct Box[T] { … }` does not parse, and
`Option[T]` is the one generic definition in the language — an
[enum](enums.md), built in. There are no defaulted fields, no positional
construction, no visibility modifiers, and no traits or interfaces to implement.

Taking a record apart in a pattern — including without naming its type — is
[pattern matching](pattern-matching.md).
