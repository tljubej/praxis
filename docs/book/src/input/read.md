# The `read` expression

`read PARSER` applies a parser to the whole process input and gives you back a
value whose type the compiler already knows. There is no scanner object, no line
iterator and no `Result`: the shape of the input is written once, in a small
sublanguage, and everything after it is ordinary Praxis.

```praxis
// One `read` at the top, ordinary code underneath. The parser expression is
// broken across lines because whitespace outside the backticks is not input.
var segments = read lines(
    `{x1:int},{y1:int} -> {x2:int},{y2:int}`
)
var total = 0
for s in segments {
    total = total + abs(s.x2 - s.x1) + abs(s.y2 - s.y1)
}
out(segments.len())
out(total)
```

Given

```text
0,9 -> 5,9
8,0 -> 0,8
9,4 -> 3,4
```

it prints

```text
3
27
```

`segments` is a `Vec[{ x1: Int, y1: Int, x2: Int, y2: Int }]`, and it is that
type before the program runs — `s.x3` is a compile error, not a runtime
surprise. [How a parser gets its type](type-derivation.md) is the whole rule.

## Where the input comes from

`praxis run` reads the process input from standard input, or from the file named
by `--input`. By the time a program sees it, the two are the same input.

```console
$ praxis run read-shape.px --input read-shape.in
3
27
$ praxis run read-shape.px < read-shape.in
3
27
```

Standard input is read **lazily**. The CLI installs a reader rather than the
bytes, and that reader is called by the first `read` a program evaluates, so a
program with no `read` in it never touches standard input and does not sit
waiting on an open pipe. `--input FILE` is the eager half: the file is read
before the program starts, so an unreadable one is reported — with exit code 2,
before any output — whether the program `read`s or not.

## `read` is an expression

It is a prefix expression, so it goes wherever a value goes. Store it in a
rebindable variable:

```praxis
var values = read lines(int)
values = values.filter(|value| value > 1)
```

or pass it straight into a call:

```praxis
out(solve(read grid(char)))
```

What it is **not** is a stream. Every `read` parses the same immutable buffer
from its first byte, so a second one is not a continuation of the first.

```praxis
// `read` is not a consuming stream. Both expressions parse the same buffer
// from its first byte, so the second one still sees all six bytes.
var numbers = read lines(int)
var whole = read rest
out(numbers)
out(whole.len())
```

Over `1\n2\n3\n`:

```text
[1, 2, 3]
6
```

That makes repeated reads deterministic. It is also why most programs have
exactly one: a second `read` is a second description of the same bytes, which is
usually a sign the first one wanted to be a
[`sections`](structural.md) or a [`block`](structural.md).

## The two parser modes

The operand of `read` is not an ordinary expression. It is a parser expression,
written in a sublanguage with two visual modes, and the backtick is the border
between them.

**Parser-expression mode** is everything outside backticks. It is a grammar of
constructor calls and atomic names, and its own whitespace means nothing —
newlines, indentation and comments are ignored, because none of it describes
input.

```praxis
read lines(
    // a comment here is a comment, not input
    `{x1:int},{y1:int} -> {x2:int},{y2:int}`,
)
```

**Template mode** is everything between backticks. There, every character is
about the input: `,` matches a comma, a space matches a run of horizontal
whitespace, and `{...}` is a capture. [Templates and captures](templates.md)
covers the syntax, and [Whitespace, lines and positions](whitespace.md) covers
what each kind of space matches.

The border is enforced in both directions. A labelled argument such as
`skip: whitespace` or `ranges: lines(int)` belongs to the parser-expression
grammar and is a syntax error in an ordinary call. A backtick template outside
`read`/`parse` is [an error rather than a `Text`](templates.md#a-template-is-a-parser-expression-everywhere-or-nowhere).

A parser is not a value either: there is no `var p = int`. `int` inside a parser
expression is an atomic parser; `int` in an ordinary expression is an undefined
name.

## Empty input is input

A reader that answers zero bytes has given **empty input**. There is no separate
"no input" state a program can be in: `--input /dev/null`, a closed pipe and a
terminal all produce a zero-length buffer, and the parser runs against it.

```praxis
// Empty input is input. `read-empty.in` is a zero-byte file, and
// `lines` over nothing is an empty Vec — an answer, not a fault.
out(read lines(int))
out((read rest).len())
```

```text
[]
0
```

That is the right answer and not a special case: splitting nothing into lines
gives no lines. A parser that *requires* content still fails, and says so at
offset zero — which is a sentence you can act on:

```praxis
// The other half of the rule: a program that requires content still gets a
// fault over empty input, and the fault says where it looked and what for.
out(read int)
```

```text
error: program faulted: input parse mismatch
       at input offset 0..0: expected int

Backtrace:
#0   <entry>

  temps:
    <tmp#1> = ""
    <tmp#2: Int> = 1
    <tmp#4: Unit> @ "out(read int)" = <uninit>
```

"You forgot to pipe your input" is a thing that report tells you.
[When a parse fails](faults.md) reads the rest of it.

## Parsing a `Text` you already have

`parse(text, PARSER)` runs the same sublanguage against a `Text` instead of the
process input. It is syntax, not a function — its second argument is a parser
expression, which is not something an ordinary call could take.

```praxis
// `parse(text, PARSER)` runs the same sublanguage against a `Text` you already
// have. Nothing is trimmed off the root, so `parse(t, rest)` is the identity.
var sample = "1,2,3"
out(parse(sample, csv(int)))
out(parse("ab\ncd\n", rest) == "ab\ncd\n")
```

```text
[1, 2, 3]
true
```

The second line is a property worth relying on: a root parse runs against the
whole buffer with nothing trimmed off it, so `rest` at the root really is
everything, terminator included. There is no hidden newline handling anywhere in
the parser — the way a file's own trailing newline stops mattering is
[the whitespace rule](whitespace.md#trailing-whitespace-belongs-to-nobody), not a
trim.

`parse` is how you try a parser against a literal without a file, and how you
re-parse a field you first captured as `text`. Every example in these chapters
that shows its input inline is using it.

## Shaping a program around one read

The shape that works is: one `read` that produces the whole puzzle, then code
that never looks at a byte again.

```praxis
var data = read sections(
    rules: lines(`{before:int}|{after:int}`),
    updates: lines(csv(int)),
)
```

`data.rules` and `data.updates` are typed collections of records. Nothing
downstream splits a string, and nothing downstream can be wrong about what the
input looked like, because the description is in one place and the compiler
checked it.

Where to go from here:

- [Atomic parsers](atoms.md) — the ten leaves every parser is built from.
- [Templates and captures](templates.md) — backtick syntax and what it produces.
- [Structural parsers](structural.md) — `lines`, `sections`, `grid`, `block`,
  `choice` and the rest of the constructors.
- [Whitespace, lines and positions](whitespace.md) — the rules that decide which
  bytes are data.
- [Cookbook: input shapes](cookbook.md) — the shapes puzzle input actually comes
  in.
