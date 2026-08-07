# Text and Char

A `Text` is an immutable UTF-8 string. A `Char` is one Unicode scalar value.
They are two types with a small, deliberate surface between them: `+`
concatenates two `Text`s, `t[i]` reads a `Char` out of one, a `for` walks the
same `Char`s, and `Char` and `Int` convert in both directions. That is nearly
everything there is.

```praxis
// `+` builds a Text; `len`, `is_empty` and `[i]` take one apart.
var greeting = "héllo" + ", " + "world"
out(greeting)
out(greeting.len())
out(greeting.is_empty())
out(greeting[1])

var vowels = 0
for c in greeting {
    if c == "o"[0] || c == "e"[0] { vowels += 1 }
}
out(vowels)
```

```text
héllo, world
12
false
é
2
```

`len()` is 12 and `greeting[1]` is `é`, not half of it: both count and index by
**Unicode scalar value, never by byte**.

## `+` is the only arithmetic operator

`Text + Text` is concatenation and produces a new `Text` — a `Text` is
immutable, so neither operand is touched. `s += "x"` is the same operator, since
a compound assignment types its right-hand side against the binding.

Everything else is refused. `-`, `*`, `/` and `%` report `Y016`, and `+` does
**not** stringify its other operand — there is no implicit conversion to `Text`
in any direction:

```praxis
// `+` is Text's only arithmetic operator, and it does not stringify its operand.
out("ab" * 3)
out("count: " + 3)
```

```console
$ praxis check text-operators.px --color never
error[Y016]: `*` is not defined for `Text`

  text-operators.px:2:5
  2 | out("ab" * 3)
    |     ^^^^^^^^ `*` is not defined for `Text`

error[Y001]: expected Text, found Int

  text-operators.px:2:12
  2 | out("ab" * 3)
    |            ^ expected Text, found Int

error[Y001]: expected Text, found Int

  text-operators.px:3:17
  3 | out("count: " + 3)
    |                 ^ expected Text, found Int

praxis: 3 error(s)
```

`"ab" * 3` is repetition in some languages. It is not a spelling here, which
keeps it free to mean that later. The reasoning for both halves is
[ADR-085](../../../decisions/085-text-concatenation-is-plus-and-nothing-else-is.md).

The conversion the second error asks for is written, and it is written
explicitly:

```praxis
out("count: " + (3).to_text())
```

`Int`, `Float` and `Char` each have a `to_text()`, and each answers exactly the
characters `out` writes — the method and the printer share one renderer, so they
cannot disagree
([ADR-143](../../../decisions/143-the-to-text-family-is-int-float-and-char.md)).
`Bool` has none, and there is no universal `T.to_text()`.

For a labelled line you usually want the next section instead.

## Interpolation: `{…}` renders a value

A `{` inside a text literal opens a **hole**. The expression in it is evaluated
and rendered into the surrounding text:

```praxis
{{#include ../../examples/scalars/text-interpolation.px}}
```

```text
{{#include ../../examples/scalars/text-interpolation.out}}
```

Two things about a hole are worth stating plainly, because both are decisions
rather than conveniences.

**A hole may hold any type**, and it renders exactly what `out` renders — the
same `[10, 20, 30]`, the same `(1, x)`, the same shortest round-tripping float.
That is not a coincidence checked by a test: a hole and `out` call the same
renderer through the value's type descriptor, so a type that prints is a type
that interpolates, and the two cannot drift apart. There is no list of
interpolable types to fall out of.

**A hole holds a full expression**, not just a name. `{a + b}`, `{p.0}`,
`{xs.len()}`, `{m["k"]}` and even `{if c { "yes" } else { "no" }}` are all holes,
and a `"` inside one opens a literal of its own. The expression is parsed
exactly as it would be anywhere else, which is why a name in a hole resolves,
renames, reports `N001` when it does not exist, and is captured when the hole is
inside a closure.

A literal brace is `\{`, joining the escape table in
[Scalars](scalars.md#escapes). A `}` on its own closes nothing, so it needs no
escape — `"a } b"` is three characters and a pair of spaces.

`{{` is **not** an escape, and it is refused rather than left to mean something
else. It is the escape in Rust, C# and Python, so it is the first thing most
readers try — and here it would parse: `{` opens the hole, `{}` is an empty
block, `}` closes it, so `"a{{}}b"` would quietly print `aUnitb`. Rather than
let a doubled brace mean a block nobody wanted, the compiler names the spelling
that works:

```praxis
{{#include ../../examples/scalars/text-interpolation-doubled-brace.px}}
```

```text
{{#include ../../examples/scalars/text-interpolation-doubled-brace.err}}
```

### A hole is part of the program

The expression in a hole is an ordinary subtree, not text re-read later, and
everything that follows from that is the point of the design. A name in a hole
is a real reference: it is captured by an enclosing closure, it renames with
every other occurrence, and your editor colours and hovers it as the binding it
is rather than as string.

```praxis
{{#include ../../examples/scalars/text-interpolation-captures.px}}
```

```text
{{#include ../../examples/scalars/text-interpolation-captures.out}}
```

`describe` prints `<closure:2>` because it captured two bindings — `label` and
`n` — and it named both of them only inside a hole.

This does **not** change what `+` does. `"n = " + n` is still `Y001`, and
deliberately so: a hole is a rendering site the program wrote, where `+`
coercing its operand would render values nobody asked to render. The two rules
are complements, and
[ADR-147](../../../decisions/147-a-hole-renders-anything-because-the-program-wrote-the-hole.md)
is where they are reconciled.

## Indexing answers a `Char`

`t[i]` and `t.get(i)` are one row with two spellings, and both answer a `Char`.
Indexing is by Unicode scalar value, and an index past the end faults:

```praxis
fn fourth(t) {
    t[3]
}

out(fourth("ab"))
```

```console
$ praxis run text-index-fault.px --debug never
error: program faulted: index out of bounds

Backtrace:
#0   fourth
#1   <entry>

  locals:
    t: Text = "ab"
  temps:
    <tmp#2: Int> @ "3" = 3
    <tmp#3: Char> @ "t[3]" = <uninit>
```

There is **no store**: `t[0] = c` is `Y020 values of type Text cannot be
assigned through 1 index(es)`, because a `Text` is immutable. `Text` is the one
subscriptable type that reads and does not write.

There is also **no slicing**. `t[1..3]` is a type error — a subscript takes an
`Int`, and a `Range` is not one. To take a piece of a line, either walk it or
let [the `read` expression](../input/read.md) cut it up as it parses.

A character the program chooses is written as a [character literal](scalars.md),
`'#'`; `"#"[0]` is the other spelling, for a character read out of a text the
program did not write down. Comparing a `Char` with a one-character `Text` is a
type error rather than a convenience — `c == "a"` is
`error[Y001]: expected Char, found Text`, and the fix is `c == 'a'`.

## `Char` and `Int`

Three rows, and they are the whole `Char` surface.

- `Char.to_int()` answers the Unicode scalar value. It never faults.
- `Int.to_char()` answers the `Char` with that scalar value. It is the narrowing
  half, so it **faults** on a negative value, on anything above `0x10FFFF`, and
  on a surrogate.
- `Char.to_text()` answers the one-character `Text` holding it — the same
  character `out` writes. It never faults, because a `Char` is a validated
  scalar value by construction.

```praxis
// Char and Int convert in both directions, and each renders as a Text.
var digits = "2026"
var value = 0
for c in digits {
    value = value * 10 + (c.to_int() - "0"[0].to_int())
}
out(value)
out("A"[0].to_int())
out(65.to_char())
out(233.to_char())
out("A"[0].to_text())
out("count: " + value.to_text())
```

```text
2026
65
A
é
A
count: 2026
```

```praxis
fn as_char(n) {
    n.to_char()
}

out(as_char(55296))
```

```console
$ praxis run int-to-char-fault.px --debug never
error: program faulted: not a Unicode scalar value

Backtrace:
#0   as_char
#1   <entry>

  locals:
    n: Int = 55296
  temps:
    <tmp#2: Char> @ "n.to_char()" = <uninit>
```

A `Char` is not an arithmetic type: `c - 48` does not compile. `c.to_int() - 48`
does, and that round trip is why `to_int()` exists at all. There is deliberately
no `is_digit`, `is_alpha`, `to_upper` or `to_lower` — `to_int()` expresses every
one of them, and inventing four rows to save a comparison was refused
([ADR-086](../../../decisions/086-a-text-subscript-answers-a-char.md)).

## A `Text` is iterable

`for c in t` walks the characters, and the `Char` it binds is the same one
`t[i]` answers — one runtime function answers both, so the two cannot disagree
about what the *i*th character is, including about counting scalars rather than
bytes.

There is no `Text.chars()`. The `for` is the spelling, and two spellings for one
question is what the catalog refuses. `Char` itself is not iterable: it is what
iterating a `Text` produces.

A `Text` is also a full [pipeline](pipelines.md) receiver — the tenth one, and
the only one that is not a collection:

```praxis
// A Text is a pipeline receiver, and its item is the Char `t[i]` answers.
var line = "a1b2c3"
out(line.count())
out(line.count(|c| c >= "0"[0] && c <= "9"[0]))
out(line.filter(|c| c >= "a"[0]).to_vec().len())
out(line.map(|c| c.to_int()).sum())
```

```text
6
3
3
444
```

Both the list literal and the iterable `Text` arrived together in
[ADR-099](../../../decisions/099-a-list-literal-is-a-vec-and-a-text-is-iterable.md),
which is also where the `Char` item type is argued for.

## The methods

`Text`'s own catalog is three rows plus the subscript:

| Method | Result | Notes |
|---|---|---|
| `t.len()` | `Int` | number of Unicode scalars |
| `t.is_empty()` | `Bool` | true iff no scalars |
| `t.get(i)` | `Char` | faults if out of range |
| `t[i]` | `Char` | the same row, the same answer |

Everything a bigger standard library would offer — `split`, `trim`, `lines`,
`replace`, `starts_with`, `repeat`, `chars`, `to_upper` — is absent. That is not
an oversight so much as a division of labour: the work those functions do is
what [the `read` expression](../input/read.md) is for, and it does it while
parsing rather than afterwards. The [method catalog](method-catalog.md) is the
authoritative list.

### The two routes back into a `Text`

Those rows go the other way — a `Text` taken apart — and there are two that put
one back together
([ADR-144](../../../decisions/144-a-sequence-of-text-joins-and-a-sequence-of-char-becomes-one.md)):

| Written | Answers |
|---|---|
| `seq.join(sep)` | the `Text` items with `sep` between them |
| `chars.to_text()` | the `Char`s of a `Vec` as one `Text`, nothing between |

`join` is a [pipeline](pipelines.md) row, so it works on any of the ten
receivers; its items must be `Text`, and it renders nothing — `[1, 2].join(",")`
is `expected Text, found Int`, and the spelling is
`[1, 2].map(|n| n.to_text()).join(",")`. The separator is required, so `join("")`
says at the call site that nothing goes between.

`to_text()` on a `Vec[Char]` is the inverse of [walking a
`Text`](#a-text-is-iterable), and it is how a `Grid` row is drawn back as the
line it was read from:

```praxis
for y in 0..g.height() { out(g.row(y).to_text()) }
```

`Text` is equatable, which is what makes it a `Map` key and a `Set` element, and
separately orderable, which is what makes it a sort key — `Bool` and tuples are
keys without being orderable, so the two properties are worth keeping apart.
Comparison is lexicographic over UTF-8 bytes, which for UTF-8 is code-point
order, and it is the order a `Set[Text]` walks in as well as the order
`sorted()` gives.

## What a `Text` costs

A `Text` has two representations and the language never says which one you have.
A literal, a concatenation and the whole of the program's input are **owned**
payloads. Every capture the input parser hands back is a **slice** — a view into
the buffer it was parsed from, one level deep, with no copy
([ADR-022](../../../decisions/022-source-slice-text.md)). Parsing a hundred
thousand fields therefore allocates a hundred thousand views and copies nothing.

What that buys, and what it does not, is visible only as complexity. An owned
text counts its own scalars once, lazily, and caches the count; a slice inherits
that answer from its owner. When every scalar in the owner is one byte — which
is every ASCII input, and so nearly every input — indexing is a byte offset:

| the text | `t.len()` | `t[i]` | `for c in t` |
|---|---|---|---|
| owned, all ASCII | O(1) after the first call | O(1) | O(n) |
| owned, has a multi-byte scalar | O(1) after the first call | O(i) | O(n²) |
| slice of an ASCII owner | O(1) | O(1) | O(n) |
| slice of a multi-byte owner | O(its own length) | O(i) | O(n²) |

Rows two and four are the honest residual, and neither the `for` nor the
subscript escapes it: there is no random access into a variable-width encoding
without a wider representation, and Praxis does not build one. A text with one
non-ASCII character in it costs O(n) per character to walk, whichever spelling
you use. The measurements and the three rejected alternatives are in
[ADR-115](../../../decisions/115-a-text-counts-itself-once-and-the-count-is-the-licence.md).

Concatenation always allocates a fresh owned payload, because a new `Text` has
no single owner to point into. Building a long string with `+=` in a loop is
therefore quadratic, and there is no builder type that avoids it — but there is
`join`, which walks the sequence once and allocates once, so a line assembled
from parts does not have to pay for it.

## Where to go next

- [Scalars](scalars.md) — literals, escapes, and why there is no `'a'`.
- [The `read` expression](../input/read.md) — how text turns into structure.
- [Pipelines](pipelines.md) — the combinators a `Text` accepts.
