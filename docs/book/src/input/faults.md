# When a parse fails

There is no `ParseError` and no `Result`. A parser that does not match the input
raises a runtime fault, the program stops, and you get the position in the
input, what the parser expected there, and a window of the bytes around it. If
the run is interactive you land in the crash debugger with the same information
under the `input` and `parser` commands.

That is the run-time half. The other half never gets that far: a parser
expression that is *malformed* — a constructor that does not exist, an argument
of the wrong kind, a template that mixes capture styles — is a compile error
with an `I0xx` code, reported by `praxis check` before anything runs.

## The shape of a parse fault

Each example below is three blocks: the program, the input it was run against,
and the standard error of `praxis run … --debug never`.

```praxis
// The third line is not an integer.
fn total() -> Int {
    read lines(int).sum()
}

out(total())
```

```text
1
2
three
4
```

```console
$ praxis run f-lines-int.px --input f-lines-int.in --debug never
```

```text
error: program faulted: input parse mismatch
       at input offset 4..4: expected int
       actual: 1⏎2⏎three⏎4⏎

Backtrace:
#0   total
#1   <entry>

  temps:
    <tmp#1> = 1
2
three
4

    <tmp#2: Int> = 1
```

Four things are on that first block and each is worth naming.

**The fault kind** is `input parse mismatch`. It is one of the runtime fault
kinds and it means exactly this: a parser was applied to bytes it could not
read. Division by zero, an out-of-range subscript and an integer overflow are
different kinds with their own messages — see [the fault
model](../debugger/faults.md).

**The input offset** is a byte range into the input, absolute and not relative
to whatever region the failing parser had been handed. `4..4` is where `three`
starts: bytes 0–3 are `1\n2\n`. A zero-width range means the parser expected
something at that point and found something else; a non-empty one means the
range itself is the complaint.

**The expectation** is what the failing parser wanted, in its own words:
`expected int`. The failure reported is the **deepest** one — the one furthest
into the input — because that is the most specific point at which parsing broke.
Offset decides that and not nesting: at equal offsets the inner parser's
specific complaint is the one kept, and an outer constructor that failed further
into the input is reported over the inner one — which is the next example.

**The preview** is a bounded window of the input around that offset, with line
endings rendered as `⏎` so it stays on one line.

Below that is the ordinary noninteractive crash report: the backtrace, and the
locals and temporaries of each frame. The temporary holding the raw input buffer
shows up there, which is why the report contains a copy of the input.

## Reading the position

The position is where parsing broke, and for a bounded construct that is not
always where you would have pointed.

### A child that stops short

```praxis
// `int` reads `12` and stops. `lines` requires the line to be consumed, and
// what is left is not whitespace.
fn values() -> Int {
    read lines(int).len()
}

out(values())
```

```text
12junk
```

```text
error: program faulted: input parse mismatch
       at input offset 2..6: expected the rest of the line
       actual: 12junk⏎

Backtrace:
#0   values
#1   <entry>

  temps:
    <tmp#1> = 12junk

    <tmp#2: Int> = 1
    <tmp#4: Int> @ "read lines(int).len()" = <uninit>
```

`int` succeeded. `lines` is what failed, and it says so: `expected the rest of
the line`, spanning `junk` — the bytes nobody read. Every bounded construct has
its own wording, and the wording tells you which one bounded the region:

| Message | The construct that bounded the region |
|---|---|
| `expected the rest of the line` | `lines` |
| `expected the rest of the section` | `sections` |
| `expected the rest of the field` | `csv` |
| `expected the rest of the token` | `ws`, `sep`, `matrix` |
| `expected the rest of the capture` | a template capture |
| `expected a grid row of the same cell count as the first` | `grid` |
| `expected rectangular matrix row` | `matrix`'s width check |
| `expected section header` | a named `sections` with too few sections |
| ``expected 6 sections for `shapes` `` | a `repeated(P, 6)` group with too few sections |

Whitespace is the exception, and it is the one rule the whole parser shares: a
leftover run the child could not read is forgiven, so `lines(int)` over `"1 \n"`
is fine. See [whitespace, lines and positions](whitespace.md).

### A row that breaks the shape

```praxis
// The second row is one cell short. The fault names that row, not the file.
fn heights() -> Int {
    read grid(digit).width()
}

out(heights())
```

```text
123
45
678
```

```text
error: program faulted: input parse mismatch
       at input offset 4..6: expected a grid row of the same cell count as the first
       actual: 123⏎45⏎678⏎

Backtrace:
#0   heights
#1   <entry>

  temps:
    <tmp#1> = 123
45
678

    <tmp#2: Int> = 1
    <tmp#4: Int> @ "read grid(digit).width()" = <uninit>
```

`4..6` is `45` — the offending row, not the whole grid and not the file. The
complaint is about the data ("a row of the same cell count as the first"), not
about a file convention, which is what makes it actionable: either the row is
short or the grid is ragged and wants `grid(P, ragged, fill: v)`.

### A group that came up short

```praxis
// The count is a promise about the input, so two sections where the program
// said three is a parse fault — not a `Vec` of two. The message names the
// group that came up short, because the number is written in the program and
// "which one" is the only thing left to say.
var data = read sections(
    shapes: repeated(lines(int), 3),
    regions: lines(int),
)

out(data.shapes.len())
```

```text
1

2
```

```text
error: program faulted: input parse mismatch
       at input offset 0..5: expected 3 sections for `shapes`
       actual: 1⏎⏎2⏎

Backtrace:
#0   <entry>

  locals:
    data: { shapes: Vec[Vec[Int]], regions: Vec[Int] } = <uninit>
  temps:
    <tmp#1> = 1

2

    <tmp#2: Int> = 1
    <tmp#6: Int> @ "data.shapes.len()" = <uninit>
    <tmp#7: Unit> @ "out(data.shapes.len())" = <uninit>
    <tmp#8: Unit> @ "// The count is a promise about the input, so two sections where the program // said three is a parse fault — not a `Vec` of two. The message names the // group that came up short, because the number is written in the program and // "which one" is the only thing left to say. var data = read sections( shapes: repeated(lines(int), 3), regions: lines(int), ) out(data.shapes.len())" = <uninit>
```

`repeated(P, N)` is the one place the program states a *number* of sections, so
the fault states which group the number belonged to rather than the generic
`expected section header` a fixed field gets. Two sections cannot be three, and
answering with a `Vec` of two would be the one outcome the program could not
notice.

### A literal that was looked for and not found

```praxis
// The second line writes `=>` where the template writes `->`.
fn pairs() -> Int {
    read lines(`{from:int} -> {to:int}`).len()
}

out(pairs())
```

```text
1 -> 2
3 => 4
```

```text
error: program faulted: input parse mismatch
       at input offset 9..11: expected literal "->"
       actual: 1 -> 2⏎3 => 4⏎

Backtrace:
#0   pairs
#1   <entry>

  temps:
    <tmp#1> = 1 -> 2
3 => 4

    <tmp#2: Int> = 1
    <tmp#4: Int> @ "read lines(`{from:int} -> {to:int}`).len()" = <uninit>
```

The span is where the literal was looked for, which is after the template's
whitespace policy has run: `3` at offset 7, the space run at 8, and the literal
expected at 9.

### The case that got furthest

`choice` tries its cases in order and keeps the failure that reached furthest
into the input, so a failed `choice` names the case the input was *trying* to
be, not the choice itself.

```praxis
// Neither case matches. The failure reported is the one that got furthest
// into the input — `Multiply` reached the second argument, `Enable` failed at
// the first byte.
fn program() -> Int {
    read lines(choice(
        Multiply: `mul({left:int},{right:int})`,
        Enable: `do()`,
    )).len()
}

out(program())
```

```text
mul(2,x)
```

```text
error: program faulted: input parse mismatch
       at input offset 6..6: expected int
       actual: mul(2,x)⏎

Backtrace:
#0   program
#1   <entry>

  temps:
    <tmp#1> = mul(2,x)

    <tmp#2: Int> = 1
    <tmp#4: Int> @ "read lines(choice( Multiply: `mul({left:int},{right:int})`, Enable: `do()`, )).len()" = <uninit>
```

`expected int` at offset 6 is `Multiply`'s second capture. `Enable` failed at
offset 0 and is not mentioned, which is the point.

## In the crash debugger

Run without `--debug never` on a terminal — or with `--debug always` anywhere —
and the same fault opens the crash debugger. Two commands are about the parse:
`input` prints the offset and the preview, `parser` prints the expectation.

```praxis
fn shipping(limit: Int) -> Int {
    // The third line of the input is not an integer.
    var values = read lines(int)
    values.filter(|v| v < limit).sum()
}

out(shipping(25))
```

```text
1
2
three
4
```

```console
$ praxis run f-debugger.px --input f-debugger.in --debug always
```

```text
error: program faulted: input parse mismatch
       at input offset 4..4: expected int
       actual: 1⏎2⏎three⏎4⏎

Backtrace:
#0   shipping
#1   <entry>

  locals:
    limit: Int = 25
    values: Vec[Int] = <uninit>
  temps:
    <tmp#2> = 1
2
three
4

    <tmp#3: Int> = 1
    <tmp#7: (Int) -> Bool> @ "|v| v < limit" = <uninit>
Entered crash debugger. 2 frame(s). Type `help` for commands.
Praxis crash> bt
#0   shipping
#1   <entry>
  (frame 0 selected)
Praxis crash> input
input at offset 4..4:
  1⏎2⏎three⏎4⏎
Praxis crash> parser
expected: int
parser expression: <unknown parser>
Praxis crash> p limit
25
Praxis crash> quit
```

`values` is `<uninit>`: the binding never took a value, because the parse it was
waiting on is what failed. Everything bound *before* the `read` is live and
readable — `p limit` answers `25` — which is often enough to tell whether the
parser is wrong or the input is.

`parser expression: <unknown parser>` is not a defect in your program. The
design document reserves a parser-expression source span on every parse failure,
so that the debugger can underline the `lines(int)` that failed; the runtime
does not populate it, so the command reports the expectation and says the span
is unavailable. The full command list is in [inspecting the input
parser](../debugger/parser.md).

## Compile-time errors

A malformed parser expression never reaches the interpreter. `praxis check`
reports it with an `I0xx` code, and it reports *every* problem in the call
rather than the first.

### A call that is not the shape the constructor has

```praxis
// A constructor call is a shape, and the shape is checked before anything is
// built. None of these five reaches the parser interpreter.
var a = read frobnicate(int)
var b = read optional(int, word)
var c = read choice(int)
var d = read sep(int, int)
var e = read chars(digit, skip: wihtespace)
```

```console
$ praxis check f-err-shape.px --color never
```

```text
error[I013]: unknown parser constructor `frobnicate` (§7.5)

  f-err-shape.px:3:14
  3 | var a = read frobnicate(int)
    |              ^^^^^^^^^^ unknown parser constructor `frobnicate` (§7.5)

error[I022]: `optional` expects 1 argument, got 2

  f-err-shape.px:4:14
  4 | var b = read optional(int, word)
    |              ^^^^^^^^^^^^^^^^^^^ `optional` expects 1 argument, got 2

error[I022]: `choice` expects at least 1 named argument(s), got 0

  f-err-shape.px:5:14
  5 | var c = read choice(int)
    |              ^^^^^^^^^^^ `choice` expects at least 1 named argument(s), got 0

error[I014]: `choice` argument 1 is a parser, but every argument must be `Name: parser`

  f-err-shape.px:5:14
  5 | var c = read choice(int)
    |              ^^^^^^^^^^^ `choice` argument 1 is a parser, but every argument must be `Name: parser`

error[I014]: `sep` argument 1 is a parser, but the separator must be a string literal

  f-err-shape.px:6:14
  6 | var d = read sep(int, int)
    |              ^^^^^^^^^^^^^ `sep` argument 1 is a parser, but the separator must be a string literal

error[I014]: `skip: wihtespace` is not a skip policy — `none` (skips nothing), `whitespace` (skips spaces and tabs) or `newlines` (skips spaces, tabs and line endings) (§7.5)

  f-err-shape.px:7:14
  7 | var e = read chars(digit, skip: wihtespace)
    |              ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ `skip: wihtespace` is not a skip policy — `none` (skips nothing), `whitespace` (skips spaces and tabs) or `newlines` (skips spaces, tabs and line endings) (§7.5)

praxis: 6 error(s)
```

`choice(int)` gets two errors from one call, which is deliberate: the arity is
wrong *and* the argument kind is wrong, and reporting only one would send you
round the loop twice. Note also that the misspelt `skip:` policy is an error and
not a silent fall back to the default.

### Values that have no representation, and a marker in the wrong place

```praxis
// `repeated(...)` is a marker on a named argument of a `sections` call, not a
// parser, and the uncounted form is greedy so it must be last; `sep` needs a
// separator that advances; `grid`'s ragged form is written with both `ragged`
// and `fill:`.
var a = read sections(boards: repeated(matrix(int)), draws: csv(int))
var b = read repeated(int)
var c = read sep("", int)
var d = read grid(char, fill: ".")
```

```console
$ praxis check f-err-marker.px --color never
```

```text
error[I028]: an unbounded `repeated(...)` tail may appear only as the final named argument (§7.5): it consumes every remaining section, so nothing can follow it — write `repeated(P, N)` for a group of N sections, which can

  f-err-marker.px:5:14
  5 | var a = read sections(boards: repeated(matrix(int)), draws: csv(int))
    |              ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ an unbounded `repeated(...)` tail may appear only as the final named argument (§7.5): it consumes every remaining section, so nothing can follow it — write `repeated(P, N)` for a group of N sections, which can

error[I028]: `repeated(...)` is only a named argument of a `sections` call (§7.5)

  f-err-marker.px:6:14
  6 | var b = read repeated(int)
    |              ^^^^^^^^^^^^^ `repeated(...)` is only a named argument of a `sections` call (§7.5)

error[I023]: `sep` needs a non-empty separator: an empty one never advances

  f-err-marker.px:7:14
  7 | var c = read sep("", int)
    |              ^^^^^^^^^^^^ `sep` needs a non-empty separator: an empty one never advances

error[I014]: `grid`'s ragged form is written `grid(P, ragged, fill: value)` — `ragged` and `fill:` come together or not at all (§7.5)

  f-err-marker.px:8:14
  8 | var d = read grid(char, fill: ".")
    |              ^^^^^^^^^^^^^^^^^^^^^ `grid`'s ragged form is written `grid(P, ragged, fill: value)` — `ragged` and `fill:` come together or not at all (§7.5)

praxis: 4 error(s)
```

Two of those refuse something meaningless rather than something misspelled. An
empty separator never advances a cursor, so `sep("", P)` would loop forever, and
it is refused where it is written rather than discovered at run time; an empty
`fill:` is refused by the same rule, because a pad of no characters pads
nothing. And `grid(char, fill: ".")` without `ragged` is not a shorthand: it
used to *become* the ragged parser on the strength of the `fill:` alone, which
is a different parser than the one written.

### Templates and block items

```praxis
// Template and block errors: captures may not mix naming styles, a capture
// name is used once, and a positional `block` item that produces a scalar has
// no field name to contribute.
var a = read lines(`{x:int},{int}`)
var b = read lines(`{x:int},{x:int}`)
var c = read block(int)
```

```console
$ praxis check f-err-template.px --color never
```

```text
error[I020]: named and anonymous captures may not be mixed in one template (§7.3)

  f-err-template.px:4:20
  4 | var a = read lines(`{x:int},{int}`)
    |                    ^^^^^^^^^^^^^^^ named and anonymous captures may not be mixed in one template (§7.3)

error[I021]: duplicate capture name `x` in template

  f-err-template.px:5:20
  5 | var b = read lines(`{x:int},{x:int}`)
    |                    ^^^^^^^^^^^^^^^^^ duplicate capture name `x` in template

error[I026]: a positional `block` item returning a scalar must be named

  f-err-template.px:6:14
  6 | var c = read block(int)
    |              ^^^^^^^^^^ a positional `block` item returning a scalar must be named

praxis: 3 error(s)
```

Every one of these is a type that could not be built: a template with mixed
capture styles has no shape, two `x` captures have no record, and a positional
scalar block item has no field name. The rule holds across the whole
sublanguage — the errors are about the type the parser would have produced, and
they are listed with the rest of the codes in [diagnostic
codes](../tooling/diagnostics.md).

## The codes

| Code | Meaning |
|---|---|
| `I000` | a parser expression the lowerer cannot read at all (`read 42` is one) |
| `I001` | a parser AST that could not be converted to a type or a plan |
| `I010` | an atomic parser name that does not exist |
| `I011` | an invalid capture name in a template |
| `I012` | a capture kind that does not exist |
| `I013` | a parser constructor that does not exist |
| `I014` | a constructor argument that is invalid or in excess |
| `I020` | named and anonymous captures mixed in one template |
| `I021` | one capture name used twice in a template |
| `I022` | a constructor called with the wrong number of arguments |
| `I023` | an empty separator, which cannot advance a cursor |
| `I024` | a section or block field declared twice |
| `I025` | a `sections` or `choice` with no field or case at all |
| `I026` | a positional `block` item returning a scalar with no name |
| `I027` | a `choice` case declared twice |
| `I028` | a misplaced or duplicated **unbounded** `repeated(...)` tail |
| `I030` | a backtick template the scanner could not read |
