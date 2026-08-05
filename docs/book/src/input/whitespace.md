# Whitespace, lines and positions

Puzzle input is full of whitespace that means nothing and whitespace that means
everything, often in the same file. Praxis answers the question in one sentence
and applies it everywhere:

> **A run of whitespace the parser offered it does not read is not data and not
> a mismatch.**

There is one question — *does the parser offered these bytes read them?* — so
there is one answer, and the half of the machinery that can ask it is the half
that decides
([ADR-078](../../../decisions/078-a-parser-position-is-absolute-and-a-region-only-narrows.md)).
No constructor has a trailing-newline special case, and none may grow one.

Inside a template the question is different, because there whitespace is
something you wrote on purpose. That half comes first.

## What a space in a template matches

| written | matches |
|---|---|
| a run of spaces | one or more spaces or tabs |
| `\s*` | zero or more whitespace characters, line endings included |
| `\s+` | one or more whitespace characters, line endings included |
| `\x20` | exactly one space |
| `\t` | exactly one tab |
| `\n` | one line ending, CRLF included |
| nothing | nothing — the literal must start right here |

The plain space run is the flexible one, and it is flexible on purpose: puzzle
input aligns columns with variable spacing, and a template that had to count
spaces would be unusable.

```praxis
// A run of ordinary spaces in a template matches one or more spaces or tabs.
// That is the flexible rule column-aligned puzzle input needs: one space in
// the template accepts any horizontal run in the input, but not none.
fn main() {
    out(parse("1 2", `{a:int} {b:int}`))
    out(parse("1     2", `{a:int} {b:int}`))
    out(parse("1\t2", `{a:int} {b:int}`))
}
```

```text
{ a: 1, b: 2 }
{ a: 1, b: 2 }
{ a: 1, b: 2 }
```

"One or more" means one or more. A template that writes a space requires a
space:

```praxis
// A space run requires a space. A template written ` -> ` does not match
// `1->2`, and the mismatch names the position where the run was expected.
fn main() -> Int {
    var pair = read `{a:int} -> {b:int}`
    pair.a + pair.b
}
```

```text
error: program faulted: input parse mismatch
       at input offset 1..1: expected whitespace
       actual: 1->2⏎

Backtrace:
#0   main

  locals:
    pair: { a: Int, b: Int } = <uninit>
  temps:
    <tmp#1> = 1->2

    <tmp#2: Int> = 1
    <tmp#3: { a: Int, b: Int }> = Unit
    <tmp#7: Int> @ "pair.a + pair.b" = <uninit>
```

The other direction is the same rule. A literal with *no* run written in front of
it consumes nothing before matching: the cursor must already be sitting on it.

```praxis
// A literal with no whitespace run written in front of it consumes none: the
// cursor must already be sitting on it, so `x:` does not skip an indent.
fn main() -> Int {
    (read `x:{a:int}`).a
}
```

Over ` x:1`:

```text
error: program faulted: input parse mismatch
       at input offset 0..2: expected literal "x:"
       actual:  x:1⏎

Backtrace:
#0   main

  temps:
    <tmp#1> =  x:1

    <tmp#2: Int> = 1
    <tmp#3: { a: Int }> = Unit
```

That does *not* mean a bare `,` refuses every space near it, and the reason is
worth having straight before the second half of this chapter:

```praxis
// A bare `,` consumes no whitespace of its own, and this still matches: the
// capture's region ends at the comma, `int` reads `1` and leaves the space
// behind, and whitespace a child declined is nobody's.
fn main() {
    out(parse("1 ,2", `{a:int},{b:int}`))
    out(parse("1, 2", `{a:int},{b:int}`))
}
```

```text
{ a: 1, b: 2 }
{ a: 1, b: 2 }
```

The comma consumed nothing, both times. On the first line the space fell inside
`a`'s region and `int` declined it; on the second it fell in front of `b` and
`int` skipped it. Neither is the literal's doing — which is the whole of the
second half.

### A run at either end of a literal is a policy

A whitespace run attached to a literal is not text the following capture
inherits. It is a requirement on the input, and satisfying it consumes the run.

```praxis
// A whitespace run at either end of a template literal is a policy the input
// must satisfy, and the policy consumes it. `a` starts after the space,
// however many spaces the input wrote there.
fn main() {
    out(parse("x: hello", `x: {a:rest}`))
    out(parse("x:   hello", `x: {a:rest}`))
    out(parse("1 -> 2", `{a:int} -> {b:int}`))
}
```

```text
{ a: hello }
{ a: hello }
{ a: 1, b: 2 }
```

`a` is `hello` and not `" hello"`, and it is the same `hello` whether the input
wrote one space or three. The run at the *end* of a literal is a policy exactly
like the run at its start, so the mirror image of the fault above is a fault too:

```praxis
// The mirror of `ws-space-required.px`: a run at the *end* of a literal is a
// policy too, so a template written `-> ` does not match `1->2` either.
fn main() -> Int {
    var pair = read `{a:int}-> {b:int}`
    pair.a + pair.b
}
```

```text
error: program faulted: input parse mismatch
       at input offset 3..3: expected whitespace
       actual: 1->2⏎

Backtrace:
#0   main

  locals:
    pair: { a: Int, b: Int } = <uninit>
  temps:
    <tmp#1> = 1->2

    <tmp#2: Int> = 1
    <tmp#3: { a: Int, b: Int }> = Unit
    <tmp#7: Int> @ "pair.a + pair.b" = <uninit>
```

Note the offsets: `1..1` for the leading spelling and `3..3` for the trailing
one. Each names the byte where the run was looked for.

### The escapes

`\s*` and `\s+` are the explicit forms, `\x20` and `\t` are the exact ones, and
`\n` matches a line ending.

```praxis
// The escapes §7.2 gives a template: `\s*` zero or more, `\s+` one or more,
// `\x20` exactly one space, `\t` exactly one tab, `\n` one line ending.
fn main() {
    out(parse("1  ,2", `{a:int}\s*,{b:int}`))
    out(parse("1,2", `{a:int}\s*,{b:int}`))
    out(parse("1  ,2", `{a:int}\s+,{b:int}`))
    out(parse("1  2", `{a:int}\x20{b:rest}`))
    out(parse("1  2", `{a:int} {b:rest}`))
    out(parse("1\t2", `{a:int}\t{b:rest}`))
}
```

```text
{ a: 1, b: 2 }
{ a: 1, b: 2 }
{ a: 1, b: 2 }
{ a: 1, b:  2 }
{ a: 1, b: 2 }
{ a: 1, b: 2 }
```

Line four is the point of `\x20`: exactly one space is consumed, so `b` gets
`" 2"` where the flexible run on line five leaves it `"2"`. Reach for `\x20`
when indentation is data — a puzzle where two leading spaces mean something
different from four.

`\s*` and `\s+` are also **broader than spaces and tabs**: they match line
endings.

```praxis
// `\s*` and `\s+` match line endings too, where a plain space run does not.
// §7.2 says "spaces or tabs" for all three; the compiler is broader for the
// two escapes, and this is what it actually accepts.
fn main() {
    out(parse("1\n2", `{a:int}\s+{b:int}`))
    out(parse("1\n2", `{a:int}\s*{b:int}`))
}
```

```text
{ a: 1, b: 2 }
{ a: 1, b: 2 }
```

That is worth knowing in both directions. It makes `\s+` a way to join two lines
without writing `\n`; it also means `\s+` is not a drop-in for a plain space run
when a record must not run past its line. Inside a `lines(...)` it makes no
difference, because the region ends at the line ending anyway.

### A capture is not bounded by its own leading whitespace

A capture is offered the bytes at the cursor, its own leading whitespace
included — [whether to skip them is the child's decision](atoms.md#leading-spaces-and-tabs).
What that leading run does *not* do is decide where the capture ends.

```praxis
// A capture is offered the bytes at the cursor, its own leading whitespace
// included — the child decides. What the leading run does *not* do is bound
// the capture, or `{a:text}` would stop at byte 0 on an indented line.
fn main() {
    out(parse("  foo 3", `{a:text} {v:int}`))
    out(parse("  foo 3", `{a:word} {v:int}`))
}
```

```text
{ a:   foo, v: 3 }
{ a: foo, v: 3 }
```

Same template, same bytes, two children, two answers — and both are the child's
own rule from [Atomic parsers](atoms.md). If the leading run bounded the capture
instead, `{a:text}` would stop at byte 0 on every indented line, because a space
run matches the indent itself.

## Trailing whitespace belongs to nobody

Outside a template, whitespace is not something you wrote — it is something the
input has. The rule at the top of this chapter is what decides it, and it has
two halves.

**The bound half asks the child.** Wherever a construct requires its child to
consume a region exactly — a line, a section, a CSV field, a `ws` or `sep`
token, a matrix cell, a template capture — what the child leaves over is
forgiven if it is whitespace and is a mismatch otherwise.

**The extent half asks nobody.** A construct that splits a region into lines
never hands out a trailing *empty* one: the split drops the run of lines holding
no bytes at all — the file's own terminator, the `"\n\n"` an editor leaves
behind, any number of them. That happens before any parser runs, which is why it
is restricted to lines with nothing in them to decide about. The region itself is
not trimmed; only the split is.

```praxis
// Trailing whitespace belongs to nobody when no parser reads it — at the end
// of a line, of a region, or of the file. `int` makes nothing of a space or of
// a line of spaces, so both are padding rather than data or a mismatch.
fn main() {
    out(parse("1 \n2 \n", lines(int)))
    out(parse("1 2 3\n\n", ws(int)))
    out(parse("1\n2\n  \n", lines(int)))
}
```

```text
[1, 2]
[1, 2, 3]
[1, 2]
```

Every one of those would break under a different rule. Line one is two elements
because `int` cannot read the space after the digit; a construct that required
its line to be filled byte for byte would fault. Line two is three tokens
because a `ws` token contains no whitespace at all — a rule that trimmed a fixed
number of terminators off the buffer would hand `int` the token `3\n`. Line
three is two elements because `int` makes nothing of a line of spaces, so it is
nobody's.

There is no trim anywhere in this. A root parse runs against the whole buffer
with its terminator inside it, which is why `parse(t, rest)` is
[the identity on `t`](read.md#parsing-a-text-you-already-have).

### The same rule, a different child

Whitespace a parser **can** read is data. Change the child and the same bytes
come out the other way — which is what says this is one rule and not a file
convention.

```praxis
// The same rule, the other child: `char` reads a space, so the trailing run is
// a cell and the trailing line of spaces is a row. `grid` complains about the
// data, not about a file convention.
fn main() {
    out(parse("ab\ncd\n  \n", grid(char)).height())
    out(parse("  \n  \n", grid(char)).width())
    out(parse("ab\ncd\n  \n", lines(rest)).len())
    out(parse("1 2\n3 4\n  \n", lines(ws(int))))
    out(parse("1 2\n3 4\n  \n", matrix(int)))
}
```

```text
3
2
3
[[1, 2], [3, 4], []]
[1, 2, 3, 4]
```

`char` reads a space as a cell, so a trailing line of spaces is a row and
`grid(char)` over `"  \n  \n"` is a 2×2 grid of spaces. `lines(rest)` is lossless
for the same reason. And the last two lines are why
[`matrix(P)` is not a synonym for `lines(ws(P))`](structural.md): a child that
succeeds *vacuously* has made something of the line — `ws` answers an
all-whitespace region with an empty collection — where `matrix` has no
zero-token row to make and drops it.

The one place this bites is `grid(char)` over a file whose last row alone ends in
a space: that is a genuinely ragged grid and it says so. Put the space on every
row and the grid is one column wider. Compare `grid(int)`, where the run is
padding, because `int` reads no cell there.

## An interior blank line is structure

Only a **trailing** run is forgiven. An interior blank line is data about the
shape of the input, and no constructor skips one.

```praxis
// Only a *trailing* run is forgiven. An interior blank line is structure: it
// is a zero-element line, and `lines(int)` says so where it stands.
fn main() -> Int {
    (read lines(int)).len()
}
```

Over `1\n  \n2\n`:

```text
error: program faulted: input parse mismatch
       at input offset 4..4: expected int
       actual: 1⏎  ⏎2⏎

Backtrace:
#0   main

  temps:
    <tmp#1> = 1
  
2

    <tmp#2: Int> = 1
    <tmp#3: Vec[Int]> = Unit
    <tmp#4: Int> @ "(read lines(int)).len()" = <uninit>
```

Offset 4 is the end of the blank line, not its start: `int` skipped the two
spaces looking for a digit and ran out of line. `grid(digit)` and `matrix(int)`
fault on the same shape, by the same rule — a blank line is a zero-cell,
zero-token row and the count check rejects it like any other wrong-sized row.
The messages differ; the rule does not.

[`sections`](structural.md) is the one construct for which a blank line is *its
own* separator, interior or trailing. That is its definition, not an exception:
`sections` is defined on blank lines the way `csv` is defined on commas.

An interior run of anything else is data too, and always was. `lines(int)` over
`12junk` is a mismatch, `chars(digit, skip: none)` over `1\n2` is a mismatch, and
`sep(",", int)` over `1,2\n3,4\n` is a mismatch because the second field really
is `2\n3` — the multi-line spelling is `lines(sep(...))`. "Trailing" is
load-bearing.

## Positions are absolute and regions only narrow

A parser position is a byte offset into the whole input. A construct that
narrows — `lines` to a line, `sections` to a section, a capture to its bound —
gives its child a **narrower window on the same buffer**, never a fresh buffer
starting at zero. A window can only get smaller.

That is invisible until something goes wrong, and then it is the whole
diagnostic:

```praxis
// A parser position is absolute (ADR-078). The mismatch is on the second line
// of the second section — what `word` left of it — and the offset it reports
// counts from the first byte of the input, not from the start of that line.
fn main() -> Int {
    (read sections(lines(word))).len()
}
```

Over

```text
alpha
beta

gamma
has space
```

it reports

```text
error: program faulted: input parse mismatch
       at input offset 21..27: expected the rest of the line
       actual: alpha⏎beta⏎⏎gamma⏎has space⏎

Backtrace:
#0   main

  temps:
    <tmp#1> = alpha
beta

gamma
has space

    <tmp#2: Int> = 1
    <tmp#3: Vec[Vec[Text]]> = Unit
    <tmp#4: Int> @ "(read sections(lines(word))).len()" = <uninit>
```

`21..27` is ` space` counted from the first byte of the file, two levels of
narrowing down. `word` read `has` and stopped there; what faulted is the `lines`
inside the `sections` requiring its child to fill the line, which is why the
report says *the rest of the line* rather than *word*. The failing line is the
fifth in the file — and the offset is still an offset you can find in the input
with any tool you like.

Two consequences fall out of the same design:

- **Every captured `Text` is a slice of the one input buffer**, with the right
  offset. A `word` in the second section names its own bytes and not the bytes
  at the start of the file.
- **A root parse requires nothing.** Requiring a region to be filled is a
  *parent's* decision, made by whoever computed the bound. Nobody bounded the
  root, so `scan(...)` and a root-level `choice(...)` may match a fragment and
  stop — and so may a root-level template, which is why one does not fault on
  the file's trailing newline.

The debugger reads the same positions back: in a crash session `input` shows the
failing offset in its input context and `parser` shows the parser expression that
reached it. See [Inspecting the input parser](../debugger/parser.md) and
[When a parse fails](faults.md).

## If you are adding a constructor

The corollary, stated for anyone extending the parser: **do not write a
trailing-newline or blank-line special case.** A construct that tokenizes to the
end of its region and bounds its children exactly has already inherited the
rule; a construct that splits lines drops a trailing blank line only when its
parser made nothing of it. Anything that forgives whitespace per constructor is
fixing this in the wrong place, N times, and will end up disagreeing with
itself — which is exactly how `csv` and `matrix` each became the one constructor
that behaved differently from all the others, twice, before the rule was stated
once.
