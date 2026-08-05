# Enums and Option

An enum is a closed set of named variants, each optionally carrying a payload. A
value is one of them and knows which. You take one apart with
[`match`](pattern-matching.md), and the checker knows when you have missed a
case.

```praxis
enum Tile {
    Empty
    Wall
    Number(Int)
    Portal(Text)
}

// A variant is constructed by naming it. One with a payload is called.
out(Empty)
out(Number(7))
out(Portal("ab"))

var tiles = [Empty, Wall, Number(3), Portal("z")]
for t in tiles {
    out(match t {
        Empty => 1
        Wall => 0
        Number(n) => n
        Portal(_) => 100
    })
}
```

```text
Empty
Number(7)
Portal(ab)
1
0
3
100
```

Variants are separated by a comma **or** a line break, so `enum Tile { Empty,
Wall }` and the multi-line form are the same declaration. A constructor is used
bare, not qualified: `Empty`, not `Tile::Empty` — there is no path syntax.

## Payloads

A payload is a parenthesized list of types. There may be more than one, and the
pattern names them by position.

```praxis
enum Move {
    Step(Int, Int)
    Stay
}

// A payload may hold more than one value, and the pattern names them by
// position.
var m = Step(1, 2)
out(m)
out(match m { Step(dx, dy) => dx * 10 + dy, Stay => 0 })

// A variant named without its payload means "any payload": `Step`, `Step(_)`
// and `Step(_, _)` are one test.
out(match m { Step => 1, Stay => 0 })

var at = (0, 0)
for step in [Step(1, 0), Step(0, 2), Stay, Step(3, 4)] {
    at = match step {
        Step(dx, dy) => match at { (x, y) => (x + dx, y + dy) }
        Stay => at
    }
}
out(at)
```

```text
Step(1, 2)
12
1
(4, 6)
```

Naming fewer sub-patterns than the payload has is not an error: the rest are
wildcards. Naming *more* is `Y124`:

```praxis
enum Wrapped { Wrap(Int) }

// Naming *more* sub-patterns than the payload has is `Y124`. It comes from
// lowering rather than from analysis, so `praxis check` is silent about it and
// `praxis run` reports it.
fn value(w: Wrapped) -> Int {
    match w { Wrap(a, b) => a + b }
}

out(value(Wrap(1)))
```

```console
$ praxis check docs/book/examples/records-enums/match-too-many-sub-patterns.px
$ praxis run docs/book/examples/records-enums/match-too-many-sub-patterns.px
error[Y124]: `Wrap` in `Wrapped` holds 1 value(s), but this pattern names 2

  match-too-many-sub-patterns.px:7:15
  7 |     match w { Wrap(a, b) => a + b }
    |               ^^^^ `Wrap` in `Wrapped` holds 1 value(s), but this pattern names 2

praxis: 1 error(s)
```

That silence from `praxis check` is what "emitted by lowering" means: the
message names the variant and the enum it belongs to, but nothing sees it until
the file runs. `Y124` is not the only pattern mistake in that position — `Y125`,
a pattern that can fail where the language needs one that always fits, is
lowering's too. Both are
[at the end of the pattern chapter](pattern-matching.md#where-the-check-runs).

An enum declaration is not generic — a variant's payload types are concrete —
and a declaration that reaches itself through a payload type is the same `N006`
a [self-referring record](records.md#a-type-cannot-refer-to-itself) gets.

## An enum value records its type

Equality is same type, same variant, equal payloads, and hashing agrees with it,
so an enum value is a map key or a set element like any other.

```praxis
enum Tile { Empty, Wall, Number(Int) }

// Equality is same variant and equal payloads; hashing follows it, so an enum
// value is a map key or a set element like any other.
out(Number(7) == Number(7))
out(Number(7) == Number(8))
out(Empty == Wall)

var seen = Set()
seen.insert(Number(1))
seen.insert(Number(1))
seen.insert(Empty)
out(seen.len())
```

```text
true
false
false
2
```

"Same type" is real, not a tag comparison: a value carries a schema naming the
enum it belongs to and the shape of each variant, so two enums whose variants
line up are still two types. That is what makes `Some(3)` print as `Some(3)`
rather than as a bare tag, what keeps an `Option[Int]` the runtime built from an
`Option[Int]` the compiler built, and what lets the debugger say what it is
looking at. See
[ADR-074](../../../decisions/074-an-enum-value-records-which-enum-type-it-is.md).

Statically the type checker usually gets there first. Two enums may declare the
same variant name; in **expression** position the name resolves like any other
name, so a later declaration shadows an earlier one:

```praxis
enum Colour { Red, Green }
enum Light { Red, Amber }

// An enum value records which enum type it is, so a `Colour` and a `Light` are
// never the same value — and here they are not even the same type. In
// *expression* position `Red` is an ordinary name, and the later declaration
// shadows the earlier one, so this `Red` is `Light`'s.
var c: Colour = Red
out(c)
```

```console
$ praxis check docs/book/examples/records-enums/enum-value-knows-its-type.px
error[Y001]: expected Colour, found Light

  enum-value-knows-its-type.px:8:17
  8 | var c: Colour = Red
    |                 ^^^ expected Colour, found Light

praxis: 1 error(s)
```

In **pattern** position there is no such problem: a variant pattern's enum is the
scrutinee's, so `match c { Red => … }` and `match l { Red => … }` each read their
own. That is [in the pattern chapter](pattern-matching.md#the-scrutinee-decides-which-enum-a-variant-pattern-names).

Enums are not ordered, for the same reason records are not — nothing says which
variant or which payload decides:

```praxis
enum Tile { Empty, Number(Int) }

// Like records, enums are equatable and hashable but not ordered.
out([Number(1), Empty].sorted())
```

```console
$ praxis check docs/book/examples/records-enums/enum-cannot-be-ordered.px
error[Y006]: values of type `Tile` cannot be ordered

  enum-cannot-be-ordered.px:4:24
  4 | out([Number(1), Empty].sorted())
    |                        ^^^^^^ values of type `Tile` cannot be ordered

praxis: 1 error(s)
```

## Option

`Option[T]` is the one generic definition in the language and it is an enum:
`Some(T)` and `None`. It is what the standard library answers when a value may
legitimately be absent. It is not an error channel — a program that runs out of
budget or indexes past the end [faults](../debugger/faults.md); a lookup that
finds nothing answers `None`.

```praxis
// `Option[T]` is an ordinary enum with two variants, `Some(T)` and `None`. It
// is what a library answers when a value may legitimately be absent.
var counts = Map()
counts["a"] = 1

out(counts.get("a"))
out(counts.get("z"))
out(match counts.get("z") { Some(n) => n, None => 0 })

var words = ["alpha", "beta"]
out(words.find(|w| w == "beta"))
out(words.find(|w| w == "gamma"))
out(words.position(|w| w == "beta"))

out(9000000000000000000.checked_add(9000000000000000000))
out(2.checked_add(3))

// A closure may build one: `filter_map` keeps the `Some`s and drops the `None`s.
out([1, 2, 3].filter_map(|n| if n % 2 == 1 { Some(n * 10) } else { None }))

// And a function may declare one.
fn first_big(v: Vec[Int]) -> Option[Int] {
    v.find(|n| n > 1)
}
out(first_big([1, 2, 3]))
```

```text
Some(1)
None
0
Some(beta)
None
Some(1)
None
Some(5)
[10, 30]
Some(2)
```

`Some` and `None` are ordinary constructors: you build them, annotate with
`Option[T]`, store them in collections, and match them. There are no methods on
an `Option` — no `unwrap`, no `is_some`, no `?` operator. A `match` is how you
get the value out, and it is two tokens more than an unwrap would be.

### What answers an `Option`

| Signature | Absent means |
|---|---|
| `Map[K, V].get(K) -> Option[V]` | the key is not in the map |
| `Vec[T].find((T) -> Bool) -> Option[T]` | nothing matched — the **element**, not its index |
| `Vec[T].position((T) -> Bool) -> Option[Int]` | nothing matched — the index |
| `Grid[T].find(T) -> Option[(Int, Int)]` | the value is nowhere in the grid |
| `Int.checked_add/sub/mul(Int) -> Option[Int]` | the result overflowed |

`filter_map`'s closure returns `Option[U]`, which is how it drops elements.

Three near neighbours deliberately answer something else. `Counter[T].get`
answers a plain count, because a counter's absent value is zero rather than
absent. `v.min()` and `v.max()` on an empty sequence **fault**: an empty minimum
is a mistake in the program, not domain-level absence, and making it an `Option`
would force an unwrap at every call site for a case the caller has already ruled
out. `Grid.find_all` answers a `Vec`, which already encodes "nothing matched" as
emptiness.

### An `Option` is not the value

`.get` answers an `Option[V]`, so it does not do arithmetic, index or compare as
a `V`. This is the most common first surprise:

```praxis
var counts = Map()
counts["a"] = 1

// `.get` answers an `Option`, so it is not an `Int` until a `match` takes it
// apart. Where the key is known to be present, index instead: `counts["a"]`.
out(counts.get("a") + 1)
```

```console
$ praxis check docs/book/examples/records-enums/option-is-not-the-value.px
error[Y001]: expected Int, found Option[Int]

  option-is-not-the-value.px:6:5
  6 | out(counts.get("a") + 1)
    |     ^^^^^^^^^^^^^^^ expected Int, found Option[Int]

praxis: 1 error(s)
```

There are two spellings and you pick between them: `counts.get(k)` is explicit
absence, and `counts[k]` is assertion-like access that faults on a miss. Where
the key was just inserted three lines up, index. Where it might not be there,
match. See
[ADR-076](../../../decisions/076-absence-is-an-option-and-an-empty-min-is-a-fault.md)
and
[ADR-082](../../../decisions/082-find-answers-the-element-and-a-miss-is-none.md).

## Anonymous enums

The input parser's `choice` constructor derives an enum with no declaration, one
variant per case, each carrying the case's own payload. It renders as its
variants — `{ Mul({ a: Int, b: Int }) | Do(Unit) | Dont(Unit) }` — and it behaves
like a declared enum in every way except that it has no name to write in an
annotation. Matching one is
[in the pattern chapter](pattern-matching.md#a-record-pattern-needs-no-head).
