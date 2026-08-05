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

There is a real gap behind that second error and it is worth stating plainly:
**`Int` has no `to_text()`**, `Char` has none either, and string interpolation
is specified in the design document and not implemented. `Float.to_text()` is
the only method in the catalog that answers a `Text` at all, so the nearest an
`Int` gets is `n.to_float().to_text()`, which answers `3.0` for `3`. A program
that wants a number in its output calls `out` on it directly.

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
    t: Text = ab
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

Because there is no character literal, `"#"[0]` is how a program names a
particular character. Comparing a `Char` with a one-character `Text` is a type
error rather than a convenience — `c == "a"` is
`error[Y001]: expected Char, found Text`, and the fix is `c == "a"[0]`.

## `Char` and `Int`

Two conversions, and they are the whole `Char` surface.

- `Char.to_int()` answers the Unicode scalar value. It never faults.
- `Int.to_char()` answers the `Char` with that scalar value. It is the narrowing
  half, so it **faults** on a negative value, on anything above `0x10FFFF`, and
  on a surrogate.

```praxis
// Char and Int convert in both directions, and nothing else converts.
var digits = "2026"
var value = 0
for c in digits {
    value = value * 10 + (c.to_int() - "0"[0].to_int())
}
out(value)
out("A"[0].to_int())
out(65.to_char())
out(233.to_char())
```

```text
2026
65
A
é
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
`replace`, `starts_with`, `repeat`, `join`, `chars`, `to_upper` — is absent.
That is not an oversight so much as a division of labour: the work those
functions do is what [the `read` expression](../input/read.md) is for, and it
does it while parsing rather than afterwards. The
[method catalog](method-catalog.md) is the authoritative list.

`Text` is equatable and orderable, so it works as a `Map` key, a `Set` element
and a sort key. Comparison is lexicographic over UTF-8 bytes, which for UTF-8 is
code-point order.

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
therefore quadratic; there is no builder type that avoids it.

## Where to go next

- [Scalars](scalars.md) — literals, escapes, and why there is no `'a'`.
- [The `read` expression](../input/read.md) — how text turns into structure.
- [Pipelines](pipelines.md) — the combinators a `Text` accepts.
