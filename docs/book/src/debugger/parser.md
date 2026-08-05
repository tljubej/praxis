# Inspecting the input parser

The most common way a puzzle program fails is not a bug in the program. It is
that the input did not look the way you thought it did — a semicolon where a
comma was promised, a stray letter in a column of digits, a blank line that is
not blank. Praxis turns that into a fault of its own kind, `input parse
mismatch`, and the debugger has two commands for it: `input`, which shows the
input around the byte where the parser stopped, and `parser`, which shows what
the parser wanted there.

Both are read-only and neither needs a frame selected; they read a record the
runtime keeps of the parse, not the stack.

## The fault carries the offset

Here is a program declaring a format that one line of its input does not have.

```praxis
// Every line of the input is meant to be `x,y`. One of them is not.
var points = read lines(`{x:int},{y:int}`)
out(points.len())
```

```text
12,7
5,3
9;1
4,4
```

The fault line already tells you most of it, before any command is typed:

```text
error: program faulted: input parse mismatch
       at input offset 10..11: expected literal ","
       actual: 12,7⏎5,3⏎9;1⏎4,4⏎
```

Three facts, and they are the three you need. **Offset 10..11** is where in the
input the parser stopped, in bytes from the start — byte 10 is the `;` on the
third line. **`expected literal ","`** is what the template wanted at that
point. And the `actual` line is a preview of the input around the offset, with
newlines drawn as `⏎` so the whole thing stays on one line.

The two commands print the same information one piece at a time, which is what
you want once you have scrolled past the banner:

```text
Praxis crash> input
input at offset 10..11:
  12,7⏎5,3⏎9;1⏎4,4⏎
Praxis crash> parser
expected: literal ","
parser expression: <unknown parser>
Praxis crash> bt
#0   <entry>
  (frame 0 selected)
Praxis crash> quit
```

`parser expression: <unknown parser>` is what that command prints for every
parse failure. §9.4 describes `parser` as showing the active input parser near
the fault, and the machinery to carry a parser expression's source span into the
failure exists — but nothing in the input-parser interpreter fills it in, so the
span is always absent. What `parser` actually gives you is the `expected`
description, which is the same one the fault line printed. Treat it as a
shorthand, not as a second source of information.

## Walking it back to the byte

An offset is not a line and a column, and for a real puzzle input you will want
one. The preview helps you recognise the neighbourhood; the offset is what
locates it exactly.

```praxis
// One integer per line, twenty of them.
var depths = read lines(int)
out(depths.len())
```

The input is twenty three-digit numbers, one of which contains a capital `O`
instead of a zero. Nothing about the fault says which:

```text
Praxis crash> input
input at offset 49..51:
  06⏎107⏎108⏎109⏎110⏎111⏎1O2⏎113⏎114⏎115⏎116⏎117⏎1
Praxis crash> parser
expected: the rest of the line
parser expression: <unknown parser>
Praxis crash> quit
```

Read that carefully, because both halves of it are informative.

The **preview is a window**, not the input. It is at most 24 bytes on each side
of the failure offset, clipped at the ends of the buffer, which is why it begins
mid-number at `06`. Do not count characters from the left of the preview to find
your line; the window's own start is arbitrary.

The **span starts where the parser gave up**, and that start is the fact to rely
on. Here it is `49..51`, two bytes wide, with `expected: the rest of the line` —
the shape of a `lines(...)` element that matched something and then found the
line was not over. `int` read the `1`, wanted the line to end, and found `O2`
still sitting there, so the span covers what was left over.

The width means something different in each case, so do not read it as "how much
matched". It is zero when the parser matched nothing at all; it is the width of
the expected text when a template literal did not match (`expected literal ","`
at `10..11` above is one byte because `,` is one byte); it is the width of the
unconsumed remainder when a line or region was not used up. Only the start is
uniformly "the byte the parser was looking at when it stopped".

To turn the offset into a line, count the newlines before it:

```console
$ head -c 49 docs/book/examples/debugger-b/parse-depths.in | wc -l
      12
$ sed -n 13p docs/book/examples/debugger-b/parse-depths.in
1O2
```

Twelve newlines precede byte 49, so byte 49 is on line 13, and line 13 is
`1O2` — the `O` is a letter. That is the whole bug, and the parser was right.

The raw input is also in the frame, if you would rather look at it than at a
window. The temp that holds the input buffer is listed by `locals`, and its
value is the entire text:

```text
  temps:
    <tmp#1> = 100
101
102
103
104
105
106
107
108
109
110
111
1O2
113
114
115
116
117
118
119
```

For a real puzzle input that is thousands of lines and you will not want it. For
a fixture you are debugging by hand, it is often quicker than switching windows.

## The deepest failure is the one reported

A structural parser fails at several levels at once. `sections(lines(csv(int)))`
can fail because a section did not end, because a line did not split, or because
a token was not an integer — and the outer failures are always less informative
than the inner one. The runtime keeps the failure whose input offset is
**furthest into the buffer**, on the argument that the point at which parsing
genuinely broke is the deepest point it reached.

```praxis
// Blank-line-separated sections of comma-separated integers.
var groups = read sections(lines(csv(int)))
out(groups.len())
```

```text
1,2,3
4,5,6

7,8,x
```

```text
Praxis crash> input
input at offset 17..17:
  1,2,3⏎4,5,6⏎⏎7,8,x⏎
Praxis crash> parser
expected: int
parser expression: <unknown parser>
Praxis crash> quit
```

`expected: int` at offset 17 — the `x`, the third field of the second section's
only line — and not "expected a section" at offset 0. The span is zero-width
here because `int` found no digits at all at that byte, which is the clearest
kind of parse failure there is: the innermost parser, at the exact byte, saying
what it wanted.

## Locals during a parse failure

A frame that faulted inside `read` has a distinctive shape. The binding the
`read` was going to fill is `<uninit>`, because the parse never produced a value
to assign:

```text
  locals:
    points: Vec[{ x: Int, y: Int }] = <uninit>
```

The type is still there, and it is worth reading. `Vec[{ x: Int, y: Int }]` is
what the template `` `{x:int},{y:int}` `` derives — a vector of anonymous
two-field records — and if that is not the type you expected, the parser you
wrote is not the parser you meant, whatever the input says. See [how a parser
gets its type](../input/type-derivation.md).

## When there is nothing to inspect

The two commands are only meaningful for a parse failure. Every other fault kind
gets a note saying so, including in a program that reads its input successfully
and then fails at something else:

```praxis
// The input parses. The fault comes later, from the program.
var depths = read lines(int)
out(depths[99])
```

```text
Praxis crash> input
(no input context — not a parse failure)
Praxis crash> parser
(no parser context — not a parse failure)
Praxis crash> quit
```

That is a useful negative result and worth typing early: it tells you the input
matched the parser, so whatever went wrong is in the program. The rest of the
debugger — `bt`, `locals`, [`p`](expressions.md) — is where you go next.

## What the fault does not tell you

Three limits, stated plainly so you do not go looking.

There is no partial value. §7.11 reserves a slot for the deepest sub-value the
parser assembled before failing, and the runtime has a field for it, but nothing
in the interpreter ever fills it and no command prints it. You cannot ask for
the two lines that did parse.

There is no caret. The compiler underlines a span in your source; `input`
prints an offset and a preview, and you locate the column yourself.

And there is no parser span, as above — `parser expression: <unknown parser>`,
every time. For a program with one `read` this costs nothing. For a program with
several, the `expected` description is what tells them apart.
