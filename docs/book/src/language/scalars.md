# Scalars

Praxis has six scalar types: `Int`, `Float`, `Bool`, `Char`, `Text` and `Unit`.
They are the leaves of every value the language builds — the elements of a
`Vec[Int]`, the keys of a `Map[Text, Int]`, the fields of a record. All six have
a literal you can write.

| Type | Payload | Written as |
|---|---|---|
| `Int` | signed 64-bit | `42`, `1_000_000` |
| `Float` | IEEE-754 binary64 | `3.5`, `1e10`, `2e-3` |
| `Bool` | true or false | `true`, `false` |
| `Char` | one Unicode scalar value | `'p'` |
| `Text` | immutable UTF-8 | `"praxis"` |
| `Unit` | nothing | `()` |

```praxis
// One value of every scalar type the language has.
var n: Int = 42
var f: Float = 3.5
var b: Bool = true
var t: Text = "praxis"
var c: Char = 'p'
var u: Unit = ()

out(n)
out(f)
out(b)
out(t)
out(c)
out(u)
```

```text
42
3.5
true
praxis
p
Unit
```

The annotations are optional — each of those types is inferred from the
initializer. See [Bindings and shadowing](bindings.md).

One further name is legal in type position. `Never` is the type of an
expression that produces no value (`panic(...)`, `return`, `break`); see
[Control flow](control-flow.md).

## Literals

```praxis
// Int literals, with `_` allowed between digits.
out(42)
out(1_000_000)

// Float literals: a fraction, an exponent, or both. `.5` is not one.
out(0.5)
out(3.141_592)
out(1.5e3)
out(2e-3)

// Bool, Unit, Text.
out(true)
out(())
out("praxis")
```

```text
42
1000000
0.5
3.141592
1500.0
0.002
true
Unit
praxis
```

An underscore may appear **between** digits of any run — the integer part, the
fraction and the exponent each accept them. A trailing `_` is not part of the
literal. A float needs a digit on both sides of its point: `0.5` is a float and
neither `.5` nor `2.` is one. That is also what keeps `1..5` a
[range](control-flow.md) instead of a malformed number — a `.` joins a numeric
literal only when a digit follows it.

A literal is typed by its syntax and by nothing else: `42` is an `Int`, `42.0`
is a `Float`, and the two do not mix. That rule and everything that follows from
it is [Numbers](numbers.md).

An integer literal outside the signed 64-bit range is
``error[Y013]: `9223372036854775808` is outside the range of `Int` ``. It is
raised while lowering, which `praxis check` does not run, so this is one of the
few diagnostics a clean `check` will not show you and `praxis run` will.

`-9223372036854775808` is therefore not a way to write the smallest `Int`: the
`-` is a unary operator applied to a literal that is itself out of range, and
the literal is what gets reported. `0 - 9223372036854775807 - 1` computes that
value instead.

A text literal is a double-quoted run of UTF-8. Eight escapes are decoded:
`\n`, `\t`, `\r`, `\"`, `\\`, `\0`, `\{` and `\}`. The last two exist because a `{`
opens an [interpolation hole](text.md#interpolation--renders-a-value), so a
literal brace needs a spelling; a `}` closes nothing outside a hole and so needs
no escape, but `\}` is accepted anyway to let a pair be written symmetrically.
Anything else after a backslash is `T005 invalid escape in text literal` and
stops compilation, with one exception: `` \` `` is accepted by the lexer —
backticks delimit [parser templates](../input/templates.md) — and is *not*
decoded, so it stays in the text as two characters. There is no `\u{...}`
escape.

```praxis
// The eight escapes a text literal decodes.
out("a\tb")
out("line\nbreak")
out("quote: \" backslash: \\")
out("a\rb".len())
out("a\0b".len())

// `\{` and `\}` are literal braces: a bare `{` opens an interpolation hole.
out("a hole is \{expr\}")

// A `\`` is accepted and left alone: two characters, not one.
out("a\`b".len())
```

```text
a	b
line
break
quote: " backslash: \
3
3
a hole is {expr}
4
```

## A character literal

A `Char` is written in single quotes: `'#'`, `'a'`, `' '`. It holds exactly one
Unicode scalar value, and the escapes are a text literal's plus `\'` for the
quote itself — `\'`, `\\`, `\n`, `\r`, `\t`, `\0`, `\"`, `\{` and `\}`. There are
no `\x` or `\u{…}` escapes, in a character literal or in a text one.

```praxis
// A character literal is one Unicode scalar in single quotes.
var wall = '#'
out(wall)
out(wall == "#"[0])

// The escapes are a text literal's, plus `\'` for the quote itself.
out('\n'.to_int())
out('\t'.to_int())
out('\''.to_int())
out('\\'.to_int())

// One scalar, not one byte: `é` is a single character.
out('é'.to_int())

// And a `Char` can be matched on, which is what the literal is for.
fn cell(c: Char) -> Text {
    match c {
        '#' => "wall"
        '.' => "open"
        _ => "something else"
    }
}

for c in "#.x" {
    out(cell(c))
}
```

```text
#
true
10
9
39
92
233
wall
open
something else
```

Exactly one character is the whole rule, and the lexer holds it. `''` names no
character and `'ab'` names two, so both are refused where they are written
rather than becoming something the program did not mean:

```praxis
// A character literal names exactly one character. These are lexical errors,
// which is the whole point: `"##"[0]` was a well-typed program that quietly
// meant `#`, and `""[0]` was a fault at run time.
var two = '##'
var none = ''
out(two)
out(none)
```

```text
error[T007]: a character literal holds exactly one character

  char-literal-not-one-character.px:4:11
  4 | var two = '##'
    |           ^^^^ a character literal holds exactly one character

help: write it as a text literal
      "##"

error[T007]: empty character literal: `''` names no character

  char-literal-not-one-character.px:5:12
  5 | var none = ''
    |            ^^ empty character literal: `''` names no character

praxis: 2 error(s)
```

That is the point of the literal rather than a convenience. `"##"[0]` was a
well-typed program that quietly meant `#`, and `""[0]` was a fault at run time;
neither is expressible as a literal.

`"#"[0]` still works and still means what it did. It is the spelling for a
character read out of a text the program did not write down — a line it just
parsed, a name it was given — and the literal is the spelling for one the
program chose. `t[i] == '#'` is the common shape, with one of each.

The literal is also a *load* rather than a call. An `Int` literal is two loads
out of an interned table, and an ASCII `Char` literal is the same; `"#"[0]` is a
runtime call that re-evaluates every time it is reached.

The literal is what makes a `Char` matchable, which is the part that is not
cosmetic — see [pattern matching](pattern-matching.md).

## Every value is an object

All runtime values are garbage-collected objects reached through a handle,
including an `Int`. No storage location a program can name holds an unboxed
scalar: a variable, a field, a tuple element, an enum payload, a captured
binding and a collection slot all hold a reference.

You cannot observe this. Scalars and `Text` are immutable, so aliasing one is
indistinguishable from copying it, and the language has no identity comparison
— `==` always asks about values. What the uniform model buys is that
[the crash debugger](../debugger/faults.md) can print every live binding with
its type, and that no generic function needs a boxing rule of its own.

What it does not cost is an allocation per number. The runtime interns `Int`
values from `-256` to `1024` and `Char` values from `0` to `127` into immortal
tables, so the loop counters and ASCII characters a puzzle program actually
handles are a table read rather than a heap block.

## Equality

`==` and `!=` are defined for every scalar, and they compare values rather than
addresses. Two `Text`s built in different ways are equal when their characters
are:

```praxis
// `==` works on every scalar. `<` works on Int, Float, Char and Text.
out(42 == 42)
out(3.5 != 3.6)
out(true == true)
out(() == ())
out("abc" == "ab" + "c")
out('x' == 'x')

out(1 < 2)
out(1.5 <= 1.5)
out("Z" < "a")
out('a' < 'b')
```

```text
true
true
true
true
true
true
true
true
true
true
```

`Float` equality is IEEE-754, so `NaN == NaN` is `false` and `0.0 == -0.0` is
`true`. Both are in [Numbers](numbers.md).

Equality extends structurally to tuples, records, enums and collections built
out of equatable types. Function values are the one thing that is never
equatable. See [Capabilities](../types/capabilities.md).

## Ordering

`<`, `>`, `<=` and `>=` are defined for exactly four types: `Int`, `Float`,
`Char` and `Text`.

- `Int` compares as a signed 64-bit number.
- `Float` compares by IEEE-754, so any comparison involving `NaN` is `false`.
- `Char` compares by Unicode scalar value.
- `Text` compares lexicographically by UTF-8 bytes, which for UTF-8 is exactly
  code-point order.

`Bool`, `Unit`, tuples, records, enums, collections and functions have no order.
Using one where an order is required is `Y006`, at check time:

```praxis
// Bool and Unit have no order. Only Int, Float, Char and Text do.
var ready = true
var done = false
out(ready < done)
```

```console
$ praxis check bool-order.px --color never
error[Y006]: values of type `Bool` cannot be ordered

  bool-order.px:4:13
  4 | out(ready < done)
    |             ^^^^ values of type `Bool` cannot be ordered

praxis: 1 error(s)
```

The same rule governs `sorted()` and heap elements: a `Vec[(Int, Int)]` cannot
be sorted and a `MinHeap[(Int, Int)]` cannot be pushed to, because a tuple has
no order. A lexicographic order over composites is conventional elsewhere and is
not defined here: ordering a composite means choosing a semantics for it, and
rejecting the program is the honest answer until one is chosen.

Ordering *inside a container* is a separate question, with one deliberate
difference. A container needs a total order or it corrupts its own invariants,
so the ordering a heap or a sort uses places a `Float` `NaN` after every number
and ties it with itself. The source-level `<` is untouched and stays IEEE-754.

## Where to go next

- [Numbers](numbers.md) — checked `Int` arithmetic, `Float` semantics, and the
  full operator and precedence table.
- [Text and Char](text.md) — concatenation, indexing, iteration, and the two
  `Char` conversions.
- [The method catalog](method-catalog.md) — every method on every type.
