# Command reference

Fifteen commands, and `help` lists all of them:

```text
Crash debugger commands (§9.4):
  bt              show the numbered backtrace
  frame N         select frame N
  up              move the selection toward the caller
  down            move the selection toward the callee
  locals          show the selected frame's locals
  p EXPR          evaluate a read-only expression
  type EXPR       show the inferred expression type
  heap EXPR       inspect a value with its type
  source [N]      show the selected (or Nth) frame's source
  input           show the input near the active parser cursor
  parser          show the active input parser near the fault
  restart         rerun the program with the same input
  reload          recompile source and rerun with the same input
  help            show this message
  quit            exit the debugger
```

A command is one line: the first word is the command and the rest is its
argument, whitespace-trimmed at both ends. There is no history, no completion,
no multi-line input and no abbreviation — `b` is not `bt`. A blank line does
nothing. A word that is not a command gets

```text
unknown command `frobnicate`. Type `help` for the list.
```

Three commands have aliases: `bt`/`backtrace`, `help`/`?`, `quit`/`exit`/`q`.
End-of-file is `quit`.

Almost every transcript below is against the same three-frame program, which
divides by the difference between two equal readings:

```praxis
var depths = read lines(int)

fn ratio(a, b) {
    a / (b - a)
}

fn step(values, i) {
    ratio(values[i], values[i + 1])
}

var total = 0
for i in 0..depths.len() - 1 {
    total = total + step(depths, i)
}
out(total)
```

Its input is `10`, `20`, `20`. The fault banner each session opens with is the
same one every time, so it is cut from most of the transcripts below; each block
starts at `Entered crash debugger.`. See [entering the debugger](entering.md) for
what the banner says.

---

## `bt`, `backtrace`

Print every frame in the snapshot, innermost first, then a line naming the
selected one. The number sits in a three-column field so the names line up.

```text
#{N}   {function name}
  (frame {selected} selected)
```

## `frame N`

Select frame `N`. Prints `frame N: name`. An `N` past the end is an error and
leaves the selection alone; an `N` that is not a number is a usage line.

## `up`, `down`

`up` moves one frame toward the caller (a *higher* number), `down` one frame
toward the callee. Each prints the newly selected frame the way `frame N` does,
or refuses at the end of the chain:

```text
already at the outermost frame
already at the innermost frame
```

All four navigation commands in one session:

```text
Entered crash debugger. 3 frame(s). Type `help` for commands.
Praxis crash> bt
#0   ratio
#1   step
#2   <entry>
  (frame 0 selected)
Praxis crash> frame 1
frame 1: step
Praxis crash> bt
#0   ratio
#1   step
#2   <entry>
  (frame 1 selected)
Praxis crash> up
frame 2: <entry>
Praxis crash> up
already at the outermost frame
Praxis crash> down
frame 1: step
Praxis crash> down
frame 0: ratio
Praxis crash> down
already at the innermost frame
Praxis crash> frame 9
error: frame 9 out of range (0..=2)
Praxis crash> frame x
usage: frame N
Praxis crash> quit
```

## `locals`

Print the selected frame's slots, in two labeled sections.

```text
  locals:
    {name}: {Type} = {value}
  temps:
    <tmp#{id}: {Type}> @ "{expression}" = {value}
```

`locals:` is the bindings you wrote. `temps:` is the compiler's intermediates,
each tagged with its per-frame id, its static type, and — this is the useful
part — the source expression it materialized. A value is rendered through the
same descriptor `out` uses. A slot nothing was written into is `<uninit>`.

Either section is omitted when it is empty; a frame with no slots at all prints
`  (no locals in this frame)`. Temps that hold nothing and explain nothing (no
value *and* no source span) are dropped rather than shown as noise.

Unlike the banner, `locals` has no cap: it prints the whole frame.

```text
Entered crash debugger. 3 frame(s). Type `help` for commands.
Praxis crash> locals
  locals:
    a: Int = 20
    b: Int = 20
  temps:
    <tmp#3: Int> @ "b - a" = <uninit>
    <tmp#4: Int> @ "a / (b - a)" = <uninit>
Praxis crash> frame 1
frame 1: step
Praxis crash> locals
  locals:
    values: Vec[Int] = [10, 20, 20]
    i: Int = 1
  temps:
    <tmp#3: Int> @ "values[i]" = 20
    <tmp#4: Int> @ "1" = 1
    <tmp#5: Int> @ "i + 1" = 2
    <tmp#6: Int> @ "values[i + 1]" = 20
    <tmp#7: Int> @ "ratio(values[i], values[i + 1])" = <uninit>
Praxis crash> quit
```

Frame 1's temps read as a small trace of the call that faulted: `values[i]` was
20, `values[i + 1]` was 20, and the call whose result they were arguments to
never returned a value.

Two known rough edges in this output. The first: a `for` loop's binding reaches
the frame without a name, so it prints as `? = 1` and `p` will not bind it.

The second is shadowing. Two bindings that shadow each other print as two lines
with the same name, in declaration order, and nothing distinguishes them:

```praxis
var count = 1
if count > 0 {
    var count = count + 40
    var zero = 0
    out(count / zero)
}
```

```text
Entered crash debugger. 1 frame(s). Type `help` for commands.
Praxis crash> locals
  locals:
    count: Int = 1
    count: Int = 41
    zero: Int = 0
  temps:
    <tmp#1: Int> @ "1" = 1
    <tmp#3: Int> @ "0" = <uninit>
    <tmp#4: Bool> @ "count > 0" = true
    <tmp#6: Int> @ "40" = 40
    <tmp#7: Int> @ "count + 40" = 41
    <tmp#9: Int> @ "0" = 0
    <tmp#11: Int> @ "count / zero" = <uninit>
    <tmp#12: Unit> @ "out(count / zero)" = <uninit>
    <tmp#14: Unit> @ "var count = 1 if count > 0 { var count = count + 40 var zero = 0 out(count / zero) }" = <uninit>
Praxis crash> p count
41
Praxis crash> quit
```

The frame's metadata does carry a distinct symbol id for each `count`, and this
renderer does not print it — so the order of the lines is what tells you which
is which, and `p count` answers for the inner one only. Read the outer one off
`locals`. See [bindings and shadowing](../language/bindings.md).

## `p EXPR`

Evaluate `EXPR` against the selected frame and print the result. The expression
is ordinary Praxis; the frame's named locals are in scope; the result is printed
through its descriptor, one line, no type.

`p` compiles and runs real code — it synthesizes a function whose parameters are
the locals your expression mentions, type-checks it, JIT-compiles it, and calls
it against the live heap. What it will not do is change anything: a mutating
method or a call to one of your own functions is refused before it runs. Empty
argument prints `usage: p EXPR`.

```text
Entered crash debugger. 3 frame(s). Type `help` for commands.
Praxis crash> p a
20
Praxis crash> p b - a
0
Praxis crash> type a
Int
Praxis crash> frame 1
frame 1: step
Praxis crash> p values
[10, 20, 20]
Praxis crash> type values
Vec[Int]
Praxis crash> p values.len() * 2
6
Praxis crash> p values.push(9)
error: method `push` is impure (may mutate state) — `p` rejects mutating expressions
Praxis crash> quit
```

`p b - a` answering `0` is the whole diagnosis of this crash in one line.

Errors come back as `error: ` and a message. A name the frame does not bind
gives:

```text
error: type error: `xs` is not defined
```

[Evaluating expressions](expressions.md) covers what `p` accepts, what the
purity gate rejects, and where the limits are.

## `type EXPR`

The same pipeline as `p`, stopped after type-checking: it prints the inferred
type of `EXPR` and never JIT-compiles or runs it. Usage line is
`usage: type EXPR`.

Because nothing runs, `type` also skips the purity gate — `type values.push(9)`
answers `Unit` where `p values.push(9)` refuses. It is the safe way to ask what
a method would give you back.

## `heap EXPR`

`p` with the type in front, separated by `: `.

```text
{Type}: {value}
```

The value part is the same text `p` prints, through the same recursive
descriptor — a map of vectors comes back with the vectors in it either way. What
`heap` adds is the type, which is what you want when the question is what shape
a structure has rather than what number it holds. Against a different program:

```praxis
var counts = Map[Text, Vec[Int]]()
counts["ada"] = [1, 2, 3]
counts["alan"] = [4, 5]
out(counts["ada"].sum() + counts["turing"].sum())
```

```text
Entered crash debugger. 1 frame(s). Type `help` for commands.
Praxis crash> p counts
{ada: [1, 2, 3], alan: [4, 5]}
Praxis crash> heap counts
Map[Text, Vec[Int]]: {ada: [1, 2, 3], alan: [4, 5]}
Praxis crash> heap counts["ada"]
Vec[Int]: [1, 2, 3]
Praxis crash> type counts
Map[Text, Vec[Int]]
Praxis crash> quit
```

`heap` is `p` and `type` in one line. It runs the expression, so the purity gate
applies to it too. Usage line is `usage: heap EXPR`.

## `source [N]`

Print the selected frame's function — the function's name, a `file:line:column`
header, and the source lines with a caret rule under the extent the frame
covers. With an argument, print frame `N` instead, without changing the
selection.

```text
Entered crash debugger. 3 frame(s). Type `help` for commands.
Praxis crash> source
ratio:
  <debug>:3:1
  3 | fn ratio(a, b) {
    | ^^^^^^^^^^^^^^^^...
  4 |     a / (b - a)
    | ^^^^^^^^^^^^^^^...
  5 | }
    | ^
Praxis crash> source 1
step:
  <debug>:7:1
  7 | fn step(values, i) {
    | ^^^^^^^^^^^^^^^^^^^^...
  8 |     ratio(values[i], values[i + 1])
    | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^...
  9 | }
    | ^
Praxis crash> quit
```

Three things about that output are worth knowing before they surprise you.

The filename is literally `<debug>`, not your file's name: the snippet is
rendered against a throwaway source map built from the text the session is
holding. The line numbers are real.

The carets cover the *frame's* extent, which is the whole function, not the
faulting line. Each line is underlined to its own end and a `...` marks that the
span continues onto the next one. To find the faulting subexpression, read the
`@ "expr"` annotations in `locals`.

An out-of-range or non-numeric `N` is not an error — `source 99` and `source zz`
both fall back to the selected frame. And the entry frame's extent is the whole
file, so `source` on `<entry>` prints your program, entire.

A frame with no recorded span prints `  (no source span recorded for this
frame)`.

## `input`

Show the input around the point an input parser stopped. It answers for an
`input parse mismatch` fault and says so for any other kind:

```text
(no input context — not a parse failure)
```

## `parser`

Show what the parser wanted at that point. Same rule: it answers for a parse
fault and otherwise prints `(no parser context — not a parse failure)`.

Both against the parse-fault program from [the fault model](faults.md):

```praxis
var rows = read lines(`{name:word} {score:int}`)
out(rows.len())
```

```text
Entered crash debugger. 1 frame(s). Type `help` for commands.
Praxis crash> input
input at offset 12..12:
  ada 36⏎alan oops⏎
Praxis crash> parser
expected: int
parser expression: <unknown parser>
Praxis crash> quit
```

`parser expression: <unknown parser>` is what you get when the failing parser's
source span was not threaded through to the runtime, which is the common case
today; when it is, the line is replaced by `parser expression (source A..B):`
and the parser's own source text. [Inspecting the input parser](parser.md) goes
into the detail.

## `restart`

Re-run the *same compiled code* against the *same input*. No recompilation. The
fault, snapshot and parse detail are cleared first, the original input bytes are
re-installed, and the program's entry point is called again.

If the re-run faults, the new snapshot replaces the old one and the frame cursor
resets to 0:

```text
program faulted: {kind}
{N} frame(s); frame 0 selected.
```

If it completes, the debugger prints `program completed: {value}` and stays at
the prompt.

## `reload`

Re-read the source file from disk, recompile it, and then do what `restart`
does. The input bytes and the input filename are retained; only the code
changes. This is the edit-and-retry loop: leave the debugger open in one window,
fix the source in another, type `reload`.

A compilation that fails leaves everything as it was:

```text
error: {diagnostic}
(session unchanged — the old snapshot is still active)
```

The old JIT code and the old snapshot are only discarded once the new
compilation has succeeded, so a typo in your fix costs you nothing.

```text
Entered crash debugger. 3 frame(s). Type `help` for commands.
Praxis crash> restart
program faulted: division by zero
3 frame(s); frame 0 selected.
Praxis crash> bt
#0   ratio
#1   step
#2   <entry>
  (frame 0 selected)
Praxis crash> reload
program faulted: division by zero
3 frame(s); frame 0 selected.
Praxis crash> quit
```

Nothing changed on disk between those two commands, so the program faulted
identically twice. That is the point of the guarantee: a `restart` is
reproducible, and a `reload` that reports a different fault reports it because
*you* changed something.

One caveat about "the same input". What is retained is what the program actually
read. A program that faulted before its first `read` recorded nothing, so a
`reload` that moves the `read` earlier in the program sees empty input rather
than the original standard input — that stream is gone. Running with
`--input FILE` avoids the question entirely.

## `help`, `?`

Print the command list at the top of this chapter.

## `quit`, `exit`, `q`

Leave the debugger. The heap and the JIT are torn down in order, and `praxis
run` exits 1, because the program still faulted.

The full set of error and usage lines, in one session:

```text
Entered crash debugger. 3 frame(s). Type `help` for commands.
Praxis crash> help
Crash debugger commands (§9.4):
  bt              show the numbered backtrace
  frame N         select frame N
  up              move the selection toward the caller
  down            move the selection toward the callee
  locals          show the selected frame's locals
  p EXPR          evaluate a read-only expression
  type EXPR       show the inferred expression type
  heap EXPR       inspect a value with its type
  source [N]      show the selected (or Nth) frame's source
  input           show the input near the active parser cursor
  parser          show the active input parser near the fault
  restart         rerun the program with the same input
  reload          recompile source and rerun with the same input
  help            show this message
  quit            exit the debugger
Praxis crash> frobnicate
unknown command `frobnicate`. Type `help` for the list.
Praxis crash> p
usage: p EXPR
Praxis crash> input
(no input context — not a parse failure)
Praxis crash> quit
```

## What is not here

There is no `continue`, no `step`, no `next`, and no breakpoint. The debugger is
reached by a fault and only by a fault: there is nothing to resume, because the
faulting operation has no answer to resume with. `restart` and `reload` are the
only ways to run anything again, and both start from the top.

There is also no way to change a value. `p` is read-only by construction, and
the reason is the same one — a state that cannot be resumed cannot usefully be
edited.
