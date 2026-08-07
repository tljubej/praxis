# Atomic parsers

An atomic parser is a leaf: it reads one run of bytes starting at the cursor and
produces one value. Everything else in the input sublanguage — templates,
`lines`, `grid`, `choice` — exists to decide *which* bytes an atomic is handed.
There are ten of them and the list is closed.

| parser | what it reads | type |
|---|---|---|
| `int` | an optional `-`, then decimal digits | `Int` |
| `uint` | decimal digits; a leading `-` is refused | `Int` |
| `float` | an optional sign, digits, an optional `.` fraction, an optional exponent | `Float` |
| `byte` | a decimal integer in `0..=255` | `Byte` |
| `char` | one Unicode scalar value, whatever it is | `Char` |
| `digit` | one decimal digit | `Int` |
| `word` | a non-empty run up to a space, tab, comma, CR or LF | `Text` |
| `identifier` | an identifier, by the language's own identifier rule | `Text` |
| `text` | the region it is given | `Text` |
| `rest` | the region it is given | `Text` |

```praxis
// Every atomic parser §7.4 has, once each, against text chosen so the value is
// visible. `text` and `rest` are in `atom-text-rest.px`.
fn main() {
    out(parse("-42", int))
    out(parse("007", uint))
    out(parse("-2.5e3", float))
    out(parse("255", byte))
    out(parse("é", char))
    out(parse("7", digit))
    out(parse("a-b:c d", word))
    out(parse("count_2 = 3", identifier))
}
```

```text
-42
7
-2500.0
255
é
7
a-b:c
count_2
```

An atomic name is spelled in lower case, and it is only a name inside a parser
expression. `int` in ordinary code is an undefined identifier; there is no value
of "parser" type to bind.

`one_of("LR")` is a leaf too, but it takes an argument, so it lives with
[the constructors](structural.md) rather than here. Its result is `Char`.

## Leading spaces and tabs

Seven of the ten skip a leading run of spaces and tabs before they look at
anything. Three do not, and it matters: a space is a character, and leading
whitespace is part of a text.

- **Skips it:** `int`, `uint`, `float`, `byte`, `digit`, `word`, `identifier`.
- **Reads it:** `char`, `text`, `rest`.

```praxis
// Which atomics skip leading spaces and tabs, and which do not. The numeric
// and word-shaped ones do; `char`, `text` and `rest` read the byte at the
// cursor, because a space is a character and leading space is part of a text.
fn main() {
    out(parse("   42", int))
    out(parse("   9", digit))
    out(parse("   hi", word))
    out(parse("   x", char) == parse(" ", char))
    out(parse("   ab", rest))
}
```

```text
42
9
hi
true
   ab
```

Only *leading* horizontal whitespace, and only spaces and tabs — a line ending
is never skipped by an atomic. What happens to whitespace an atomic leaves
behind is the caller's business, and there is one rule for that:
[trailing whitespace belongs to nobody](whitespace.md#trailing-whitespace-belongs-to-nobody).

## The numbers

`int` takes an optional `-` and then decimal digits. It does **not** take a
leading `+`: `parse("+1", int)` is a mismatch at offset 0. `uint` is the same
run with the sign refused — the type is still `Int`, and the non-negativity is
enforced by the parse rule rather than by a separate integer type.

`byte` is a decimal integer in `0..=255` producing a `Byte`. It is a number, not
a raw input byte: `parse("255", byte)` reads three characters. `300` is a
mismatch (`expected byte`), not a wraparound.

`digit` is exactly one decimal digit, and its type is `Int`, not `Byte` or
`Char`. It exists so a dense digit grid has a cell parser:
[`grid(digit)`](structural.md) is one digit per cell where `grid(int)` would be
one whole number per cell.

`float` takes an optional `-` or `+`, then digits, then a fraction only if there
are digits after the `.`, then an exponent only if it is complete. So a trailing
`.` or `e` is not part of the number — it is left for whatever follows.

```praxis
// `float`'s run takes an optional sign, digits, a fraction only when there are
// digits after the `.`, and an exponent only when it is complete. So `1.` is a
// `1` and a literal dot, and `1e` is a `1` and a literal `e`.
fn main() {
    out(parse("+4", float))
    out(parse("1.", `{v:float}{tail:rest}`))
    out(parse("1e", `{v:float}{tail:rest}`))
}
```

```text
4.0
{ v: 1.0, tail: . }
{ v: 1.0, tail: e }
```

## `char`

One Unicode scalar value, taken at the cursor with nothing skipped. A space is a
`Char`, a tab is a `Char`, and `é` is one `Char` and not two bytes. That is what
makes a character grid positional: `grid(char)` counts cells, so a row with a
space in the middle is three columns wide and a row that ends in a space is one
column wider than its neighbours.

`char` fails only when there is nothing left in the region: `expected char` at
the region's end.

## `word` and `identifier`

`word` reads a non-empty run and stops on a space, a tab, a comma, CR or LF.
That list is deliberately short — it does **not** include `-`, `:`, `|`, `>` or
anything else a template might use as punctuation.

```praxis
// `word` stops on a space, a tab, a comma, CR or LF, and on nothing else. It
// runs straight through `-` and `:`, which is what makes `-to-` templates work
// — the literal that follows the capture is what stops it there.
fn main() {
    out(parse("a-b:c d", word))
    out(parse("seed-to-soil map:", `{source:word}-to-{destination:word} map:`))
    out(parse("hello,world", csv(word)))
}
```

```text
a-b:c
{ source: seed, destination: soil }
[hello, world]
```

The second line is the reason the delimiter set stays small. A bare `word` swallows
`seed-to-soil` whole; a `word` capture inside a template stops at the literal
that follows it, because [every capture is bounded](templates.md#a-capture-is-bounded-by-what-follows-it).
Growing `word`'s own delimiter set to cover `-` would have broken the bare case
to fix a case the bound already fixes
([ADR-079](../../../decisions/079-a-grid-cell-is-what-its-cell-parser-reads.md)).

An empty run is a failure: `word` at a comma reports `expected word` and reads
nothing.

`identifier` reads a run that starts with an identifier-start character and
continues with identifier-continue characters — the *language's* identifier
class, the same one that decides what a Praxis binding may be called, not a
narrower ASCII copy of it. Use it when the input's names are genuinely
identifiers and you want `x2` but not `x-2`.

## `text` and `rest`

Both take the region they are given, whole, leading whitespace included. In the
implementation they are the same parser, and the difference §7.4 describes
between them lives one level up: a capture is bounded by whatever follows it in
the template, and that bound applies to **every** capture, not only the `text`
ones.

```praxis
// `text` and `rest` are one parser: both take the whole region they are given.
// What makes a `text` capture stop early is the bound a template puts on
// *every* capture, so `rest` in the same position stops in the same place.
fn main() {
    out(parse("prefoopost", `pre{body:text}post`))
    out(parse("prefoopost", `pre{body:rest}post`))
    out(parse("a b\nc\n", text) == parse("a b\nc\n", rest))
    out(parse("Card 1: 41 48 83", `Card {id:int}: {body:rest}`))
}
```

```text
{ body: foo }
{ body: foo }
true
{ id: 1, body: 41 48 83 }
```

Write `text` where a capture has something after it and `rest` where it does
not; the two words then say what you meant, even though the compiler cannot tell
them apart. Neither ever fails.

The `Card` line is worth reading twice: the space after `:` is part of the
template's literal run, so the run consumes it and `body` starts at `4`. A
template's trailing whitespace is a policy the input must satisfy, not text the
capture inherits — see
[whitespace](whitespace.md#a-run-at-either-end-of-a-literal-is-a-policy).

## How an atomic fails

Every atomic fails the same way: a parse mismatch carrying the byte offset it
was looking at and the name of the parser that was looking. There is no
`Result`, no `Option` and nothing to check.

```praxis
// `uint` refuses a leading `-`. Every atomic fails the same way: a mismatch
// naming the byte offset it looked at and the parser that looked.
fn main() {
    out(read uint)
}
```

```text
error: program faulted: input parse mismatch
       at input offset 0..1: expected uint
       actual: -5⏎

Backtrace:
#0   main

  temps:
    <tmp#1> = "-5\n"
    <tmp#2: Int> = 1
    <tmp#4: Unit> @ "out(read uint)" = <uninit>
```

The `expected` word is the atomic's own keyword, so the report names the leaf
that disagreed rather than the constructor that called it. The offset is
absolute — a byte index into the whole input, not into the line or field the
atomic was handed
([ADR-078](../../../decisions/078-a-parser-position-is-absolute-and-a-region-only-narrows.md)).
[When a parse fails](faults.md) covers the rest of the report and what the crash
debugger does with it.

An atomic that succeeds but does not fill the region it was given is a different
question, and its answer belongs to whoever computed the region: `lines(int)`
over `12junk` is a mismatch, and `lines(int)` over `12  ` is not.
[Whitespace, lines and positions](whitespace.md) is that rule.
