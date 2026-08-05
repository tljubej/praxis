# Pattern matching

`match` takes a value apart and picks the first arm whose pattern fits. Arms are
separated by a comma or a line break, an arm body is any expression, and the
whole `match` is an expression — so it is either the value of a binding or a
statement, depending on where you put it.

The checker requires that the arms cover the type, and that no arm is dead. Both
are the same question asked twice, and both are answered by `praxis check`.

```praxis
struct Point { x: Int, y: Int }
enum Tile { Empty, Number(Int) }

// A literal pattern tests the value; `_` matches anything and binds nothing.
fn word(n: Int) -> Text {
    match n {
        0 => "zero"
        1 => "one"
        _ => "many"
    }
}
out(word(0))
out(word(9))

// A variant pattern tests the tag and takes the payload apart. A bare name that
// is a variant of the scrutinee's enum is that variant, not a binding.
fn cost(t: Tile) -> Int {
    match t {
        Empty => 1
        Number(n) => n
    }
}
out(cost(Empty))
out(cost(Number(6)))

// A record pattern names fields in any order, and a field it does not name is
// left alone. A punned field binds under its own name.
fn quadrant(p: Point) -> Int {
    match p {
        Point { x: 0, y: 0 } => 0
        Point { x: 0, y } => y
        Point { y: 0, x } => x * 10
        Point { x, y } => x * 100 + y
    }
}
out(quadrant(Point { x: 0, y: 0 }))
out(quadrant(Point { x: 0, y: 7 }))
out(quadrant(Point { x: 7, y: 0 }))
out(quadrant(Point { x: 1, y: 2 }))

// A tuple pattern binds by position, and patterns nest inside one another.
out(match (1, 2, 3) { (a, _, c) => a + c })
out(match Some((4, 5)) { Some((a, b)) => a * b, None => 0 })

// `true` and `false` are literal patterns, and they are the whole of `Bool`.
out(match 3 > 2 { true => "yes", false => "no" })
```

```text
zero
many
1
6
0
7
70
102
4
20
yes
```

## The pattern forms

```text
pattern := "_"                                            // wildcard
         | literal                                        // Int, Text, true, false
         | Ident                                          // binding, or payload-less variant
         | Ident "(" [pattern ("," pattern)*] ")"         // enum variant
         | Ident "{" [pattern_field ("," pattern_field)*] "}"  // record
         |       "{"  pattern_field ("," pattern_field)*  "}"  // headless record
         | "(" pattern ("," pattern)* ")"                 // tuple
pattern_field := Ident [":" pattern]
```

That is the whole grammar. Some notes on the edges:

- **Literals** are integers, text, `true` and `false`. There is no float pattern,
  no character pattern (the language has no character literal), and no negative
  literal — `-1` in pattern position is a parse error, because `-` is an operator
  and a pattern has no operators.
- **`_` binds nothing.** It is not an identifier named `_`: it declares no
  symbol, so two `_` arms are not a duplicate declaration (the second is merely
  unreachable), and `_` has no expression form — reading one is `P001: expected
  an expression`.
- **A record pattern may name fewer fields than the record has.** The rest are
  wildcards. Naming a field the record does not have is `Y114`; naming one twice
  is `Y115`, *field `x` is matched more than once*, because the second binding
  would silently replace the first.
- **`P {}` is a record pattern, not a binding.** Bare `P` binds the whole value
  under the name `P`; `P {}` names the record and matches on it, naming none of
  its fields — and since a record has one constructor, that covers the type, so
  an arm after it is `Y121`.
- **A headless `{}` is a parse error.** It would bind nothing and test nothing,
  which is what `_` is for; a second spelling of "matches everything" is how a
  half-written pattern becomes an irrefutable arm by accident.
- **Parentheses in pattern position are always a tuple.** There is no grouping
  form, because a pattern has no precedence to override, so `(p)` is a
  one-element tuple pattern and `Y123` reports it: `a tuple pattern names two
  elements or more`. `()` gets the same `Y123`, and a parse error after it.

**There are no guards.** `A if n > 3 => …` does not parse. Put the condition in
the arm body, or match on the condition.

## Patterns are not only for `match`

The pattern grammar is one production, so a `for` header and a closure parameter
take the same shapes an arm does. Neither has a second arm to fall through to,
so both must be given a shape that always fits.

```praxis
struct Point { x: Int, y: Int }

// The pattern grammar is one production, so a `for` header and a closure
// parameter take the same shapes a match arm does.
var points = [Point { x: 1, y: 2 }, Point { x: 3, y: 4 }]
for { x, y } in points {
    out(x * y)
}

var pairs = [(1, 10), (2, 20)]
var sum = 0
for (weight, value) in pairs {
    sum = sum + weight * value
}
out(sum)

// A closure parameter is a pattern too. This one is headless, so it needs the
// annotation to say which record it takes apart.
out(points.map(|{ x, y }: Point| x + y))
```

```text
2
12
50
[3, 7]
```

A shape that can fail is `Y125`, and the message says which of the two positions
it is in. Lowering is where it is caught, not analysis, so `praxis check` passes
the file and `praxis run` is what reports it:

```praxis
var xs = [Some(1), None]

// A `for` binding has no second arm for an item that does not fit, so a pattern
// that can fail is `Y125`. This one is lowering's too: `praxis check` is silent
// and `praxis run` reports it.
for Some(n) in xs {
    out(n)
}
```

```console
$ praxis check docs/book/examples/records-enums/match-refutable-binding.px
$ praxis run docs/book/examples/records-enums/match-refutable-binding.px
error[Y125]: a `for` binding must match every item, and a variant pattern does not

  match-refutable-binding.px:6:5
  6 | for Some(n) in xs {
    |     ^^^^^^^ a `for` binding must match every item, and a variant pattern does not

praxis: 1 error(s)
```

A closure parameter gets the same code and a message that says `a closure
parameter must match every argument`. A literal is refutable too, so
`for 1 in [1, 2, 3]` is `Y125` as well.

## The scrutinee decides which enum a variant pattern names

A variant pattern's enum is the **scrutinee's**, not whatever the constructor
name happens to resolve to elsewhere in the file. Two enums may share a variant
name and each `match` still reads its own.

```praxis
enum Colour { Red, Green }
enum Light { Red, Amber }

// A variant pattern's enum is the *scrutinee's*. Both enums have a `Red`, and
// each `match` reads its own — no annotation on the pattern, and no ambiguity.
fn colour_code(c: Colour) -> Int {
    match c { Red => 1, Green => 2 }
}

fn light_code(l: Light) -> Int {
    match l { Red => 10, Amber => 20 }
}

out(colour_code(Green))
var stop: Light = Amber
out(light_code(stop))
```

```text
2
20
```

That is what makes the next diagnostic possible: once the enum comes from the
scrutinee, a name the enum has not is a mistake with nothing else it could mean.

```praxis
enum Tile { Empty, Wall, Number(Int) }

// A misspelling with a payload has nothing else it could be, so it is reported.
fn cost(t: Tile) -> Int {
    match t { Empty => 1, Wall => 2, Numbr(n) => n }
}

out(cost(Empty))
```

```console
$ praxis check docs/book/examples/records-enums/match-unknown-variant.px
error[Y122]: `Tile` has no variant `Numbr`

  match-unknown-variant.px:5:38
  5 |     match t { Empty => 1, Wall => 2, Numbr(n) => n }
    |                                      ^^^^^ `Tile` has no variant `Numbr`

praxis: 1 error(s)
```

### The one that bites: a bare name is a binding

A bare `Ident` is a variant only if the scrutinee's enum has one by that name.
Otherwise it is a binding — and a binding matches everything.

```praxis
enum Tile { Empty, Wall, Number(Int) }

// `Wal` is not a variant of `Tile`, so it is not a variant pattern — it is a
// binding, and a binding matches everything. The match is exhaustive, nothing
// is reported, and every tile that is not `Empty` takes the second arm.
fn cost(t: Tile) -> Int {
    match t {
        Empty => 1
        Wal => 2
    }
}

out(cost(Empty))
out(cost(Wall))
out(cost(Number(9)))
```

```text
1
2
2
```

Nothing is reported, because nothing is wrong: `Wal` is a legal catch-all
binding. If a `match` you expect to be exhaustive compiles without the arm you
thought you needed, look for a misspelt payload-less variant. Writing `Wal(_)`
instead would have been `Y122`.

See
[ADR-091](../../../decisions/091-a-variant-patterns-enum-is-the-scrutinees.md).

## A record pattern needs no head

The head of a record pattern is optional. A headless `{ a, b }` pins its record
from the scrutinee the way a tuple pattern always has, which is the only spelling
available when the record is anonymous — a `choice` template's payloads have no
name a head could write.

```praxis
// A `choice` template derives an anonymous enum whose payloads are anonymous
// records. Neither has a name, so a pattern that wants the fields cannot write
// a head — a headless `{ a, b }` pins its record from the scrutinee instead.
var instructions = read scan(choice(
    Mul: `mul({a:int},{b:int})`,
    Do: `do()`,
    Dont: `don't()`,
))

var on = true
var total = 0
for m in instructions {
    match m {
        Mul({a, b}) => { if on { total = total + a * b } }
        Do(_) => { on = true }
        Dont(_) => { on = false }
    }
}
out(total)

// The same walk, binding the whole payload and reading its fields instead.
var sum = 0
for m in instructions {
    match m {
        Mul(p) => { sum = sum + p.a * p.b }
        Do(_) => {}
        Dont(_) => {}
    }
}
out(sum)
```

Given this input:

```text
xmul(2,3)don't()mul(4,5)do()mul(6,7)
```

```text
48
68
```

A headless pattern needs a record it can *see*. Field names alone do not
determine a record type — the language has no row variables — so a scrutinee
nothing has pinned is reported rather than silently guessed at:

```praxis
// A headless record pattern needs a record it can see. Field names alone do not
// determine a record type, so a scrutinee nothing has pinned is reported.
fn total(p) -> Int {
    match p { {x, y} => x + y }
}
out(total(1))
```

```console
$ praxis check docs/book/examples/records-enums/match-headless-needs-a-record.px
error[Y123]: `{ … }` cannot tell which record it matches here; name the record (`P { … }`) or annotate the value

  match-headless-needs-a-record.px:4:15
  4 |     match p { {x, y} => x + y }
    |               ^^^^^^ `{ … }` cannot tell which record it matches here; name the record (`P { … }`) or annotate the value

praxis: 1 error(s)
```

The message names the two ways out and both work when the record has a name:
write the head (`match p { Point { x, y } => … }`) or annotate the value
(`fn total(p: Point)`). Neither is available for an *anonymous* record. It has
no name for a head, and the type grammar has no record form to annotate with —
`{ x: Int, y: Int }` is a spelling diagnostics print, not one you can write. That
is why the payload of a `choice` is matched at the scrutinee that already knows,
as above.

## Exhaustiveness and reachability

A match must cover every value its scrutinee can take, and every arm must match
something the arms above it do not. These are one question — *is this pattern
useful against the ones before it?* — and it is asked at every position a value
has, not only at the top level.

A type has a **closed** signature when its values can be enumerated: an enum's
variants, `Bool`'s `true`/`false`, and the single constructor a record or a tuple
each have. Everything else — `Int`, `Float`, `Text`, `Char`, `Unit`, functions,
and a type inference could not pin — is **open** and needs a `_`.

A missing case is `Y120`, and the message names the shapes that are missing,
up to three of them, with the arms to add:

```praxis
enum Tile {
    Empty
    Wall
    Number(Int)
    Portal(Text)
}

fn cost(t: Tile) -> Int {
    match t {
        Empty => 1
        Number(n) => n
    }
}

out(cost(Empty))
```

```console
$ praxis check docs/book/examples/records-enums/match-non-exhaustive.px
error[Y120]: non-exhaustive match: missing `Wall`, `Portal(_)`

  match-non-exhaustive.px:9:5
  9 |     match t {
     |     ^^^^^^^^^ non-exhaustive match: missing `Wall`, `Portal(_)`...
  10 |         Empty => 1
     | ^^^^^^^^^^^^^^^^^^...
  11 |         Number(n) => n
     | ^^^^^^^^^^^^^^^^^^^^^^...
  12 |     }
     | ^^^^^

help: add the missing match arms
              Wall => panic("todo")
              Portal(_) => panic("todo")

praxis: 1 error(s)
```

The `help:` text is a machine-applicable suggestion: an editor offers it as a
quick fix, and the arms it writes compile, because `panic` fits whatever type the
other arms produced. When the scrutinee has no signature to enumerate the message
says `missing a `_` catch-all arm` instead, because there is no shape to name.

Coverage goes *through* constructors, not just up to them. A one-variant enum
does not make every match on it exhaustive:

```praxis
enum Flag { On, Off }
enum Wrapped { Wrap(Flag) }

// Coverage is asked at every position a value has, not only at the top level:
// `Wrap` is named, and the `Off` inside it is not.
fn on(w: Wrapped) -> Int {
    match w { Wrap(On) => 1 }
}

out(on(Wrap(On)))
```

```console
$ praxis check docs/book/examples/records-enums/match-non-exhaustive-payload.px
error[Y120]: non-exhaustive match: missing `Wrap(Off)`

  match-non-exhaustive-payload.px:7:5
  7 |     match w { Wrap(On) => 1 }
    |     ^^^^^^^^^^^^^^^^^^^^^^^^^ non-exhaustive match: missing `Wrap(Off)`

help: add the missing match arms
          Wrap(Off) => panic("todo")

praxis: 1 error(s)
```

An arm that can never run is `Y121`. The obvious case is an arm after a
catch-all:

```praxis
enum Tile { Empty, Wall, Number(Int) }

fn cost(t: Tile) -> Int {
    match t {
        Empty => 1
        _ => 0
        Number(n) => n
    }
}

out(cost(Empty))
```

```console
$ praxis check docs/book/examples/records-enums/match-unreachable.px
error[Y121]: unreachable match arm

  match-unreachable.px:7:9
  7 |         Number(n) => n
    |         ^^^^^^^^^^^^^^ unreachable match arm

praxis: 1 error(s)
```

The less obvious ones are a repeated constructor (`A => 1, A => 2`) and a nested
pattern an earlier arm already subsumed (`Some(n)` followed by `Some(_)`) — the
same walk finds all three.

The two halves meet at a record: a record has exactly one constructor, so naming
it covers the type, and a `_` after it is dead.

```praxis
struct Point { x: Int, y: Int }

// A record has one constructor, so naming it covers the type: the `_` below can
// never run, and an arm that can never run is an error.
fn sum(p: Point) -> Int {
    match p {
        Point { x, y } => x + y
        _ => 0
    }
}

out(sum(Point { x: 1, y: 2 }))
```

```console
$ praxis check docs/book/examples/records-enums/match-record-needs-no-catch-all.px
error[Y121]: unreachable match arm

  match-record-needs-no-catch-all.px:8:9
  8 |         _ => 0
    |         ^^^^^^ unreachable match arm

praxis: 1 error(s)
```

`match p { Point { x: 0, y } => y }` is the other half of the same fact: the
constructor is covered, the `0` inside it is not, and the witness names the
shape — `` missing `Point { x: _, y: _ }` ``.

An unreachable arm still *covers* what it names, whether or not it can run, so
`{ _ => 1, A => 2 }` does not then report a missing `B` on account of the arm it
has just rejected. The reasoning for the whole check is in
[ADR-055](../../../decisions/055-exhaustiveness-and-reachability-are-one-usefulness-question.md).

## Where the check runs

`Y120` and `Y121` come from analysis, which means `praxis check`, `praxis run`
and the language server all see them at the same place with the same message. The
editor underlines the `match` and offers the arms.

The check runs *after* inference rather than inside it, because a scrutinee's
type is not final while inference is still on the stack: a `match` on an
unannotated parameter can be pinned by a call further down the file, and a
coverage answer given against a type variable would be a `Y120` demanding a `_`
the program does not need. See
[ADR-130](../../../decisions/130-a-matchs-coverage-is-analysis-answer-and-the-pattern-is-built-once.md).

Two pattern mistakes are still lowering's alone and so are invisible to `praxis
check`: naming more sub-patterns than a variant's payload has (`Y124`), and a
pattern that can fail in a `for` header or a closure parameter (`Y125`). Both
report when you run the file.
