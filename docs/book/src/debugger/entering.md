# Entering the debugger

When a program [faults](faults.md), `praxis run` either prints the crash report
and exits 1, or prints the crash report and then gives you a prompt at the point
of the crash. Which one you get is the `--debug` flag, and its default reads the
terminal.

```console
$ praxis run day07.px --input day07.txt              # --debug auto
$ praxis run day07.px --input day07.txt --debug always
$ praxis run day07.px --input day07.txt --debug never
```

- **`auto`** — the default. Enter the debugger if **both** standard input and
  standard output are a terminal. Anything else — a pipe, a redirect, a CI
  runner, an editor's task pane — declines.
- **`always`** — enter the debugger regardless. This is what makes the sessions
  in this book reproducible, because it lets you feed commands in on a pipe.
- **`never`** — never enter. Print the report, exit 1.

The test is `stdin && stdout`, not stderr. The report itself goes to standard
error and so does everything the debugger prints, so `2>&1` is how you capture a
session and `> out.txt` does not swallow it.

Exit is 1 on a fault either way. Quitting the debugger does not change that: a
program that faulted has still faulted.

## Driving it from a pipe

`--debug always` reads commands from standard input, one per line, and does not
echo them. That makes a session scriptable:

```console
$ printf 'bt\nlocals\nquit\n' | praxis run entering.px --input entering.in --debug always
```

The program:

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

with `10`, `20`, `20` as its input. The third reading equals the second, so
`b - a` is zero on the second iteration and the divide faults. The full session,
with the commands written back in after the prompts they were typed at:

```text
error: program faulted: division by zero

Backtrace:
#0   ratio
#1   step
#2   <entry>

  locals:
    a: Int = 20
    b: Int = 20
  temps:
    <tmp#3: Int> @ "b - a" = <uninit>
    <tmp#4: Int> @ "a / (b - a)" = <uninit>
Entered crash debugger. 3 frame(s). Type `help` for commands.
Praxis crash> bt
#0   ratio
#1   step
#2   <entry>
  (frame 0 selected)
Praxis crash> locals
  locals:
    a: Int = 20
    b: Int = 20
  temps:
    <tmp#3: Int> @ "b - a" = <uninit>
    <tmp#4: Int> @ "a / (b - a)" = <uninit>
Praxis crash> quit
```

Every transcript in these chapters was produced that way. See
[noninteractive mode](noninteractive.md) for what to do with that in a script.

## What is printed before the prompt

Everything above `Entered crash debugger.` is the same text `--debug never`
prints, in the same order, and it is printed *before* the debugger starts. You
have already been told the answer to `bt` and to `locals` by the time you get a
prompt; the prompt is for the second question.

The report is four parts:

1. **The fault line.** `error: program faulted: ` and the kind. A `panic`
   appends its message here. `error:` is colored like a compiler error when
   standard error is a terminal; `--color never` turns that off.
2. **The parse detail**, for an `input parse mismatch` only: the input offset,
   what the parser expected there, and a preview.
3. **The backtrace**, innermost first, under a `Backtrace:` header.
4. **Frame 0's locals**, split into `locals:` and `temps:`.

Part 4 is capped at twelve entries in the banner, with a `…(N more)` line if
there are more. The `locals` command at the prompt has no cap, which is the one
place the two renderings differ.

Then:

```text
Entered crash debugger. 3 frame(s). Type `help` for commands.
Praxis crash>
```

The prompt is `Praxis crash> ` with a trailing space. A blank line at it is
ignored. End-of-file has the same effect as `quit`, which is why a `.cmds` file
that forgets to end with `quit` still terminates.

## The backtrace, and what a frame is

A frame is one call that had not returned when the fault fired. `#0` is the
function that faulted; the last frame is the program's entry point.

```text
#0   ratio
#1   step
#2   <entry>
```

`<entry>` is the name of a file's top-level statements. A file whose whole
program is inside `fn main()` shows `main` there instead — the entry point is
the top-level statements when a file has any, and `main` when it has none.

There is no line number in the backtrace. The equivalent is the `source`
command, which prints the selected frame's function with a caret under the
extent the frame covers, and the `@ "expr"` annotations on the temps, which name
the exact subexpression each slot materialized.

A frame knows five things, and every debugger command is a way of asking for one
of them:

| the frame knows | the command that shows it |
|---|---|
| the function's name | `bt` |
| the caller it will return to | `up`, `down` |
| the function's source extent | `source` |
| its locals: name, static type, current value | `locals` |
| its temporaries: id, static type, materializing expression, value | `locals` |

The static type on each local is the compiler's, resolved against the same type
table the program was compiled with, which is why `locals` can print
`Vec[{ name: Text, score: Int }]` and not just "a vector".

## Selecting a frame

`frame N`, `up` and `down` move the selection. `locals`, `p`, `type`, `heap` and
`source` all act on whichever frame is selected; `bt` marks it. Here is the top
of the `frames` example — the `<entry>` frame's locals run on for another twenty
lines of temps, which are cut here:

```text
Entered crash debugger. 3 frame(s). Type `help` for commands.
Praxis crash> bt
#0   ratio
#1   step
#2   <entry>
  (frame 0 selected)
Praxis crash> frame 2
frame 2: <entry>
Praxis crash> locals
  locals:
    depths: Vec[Int] = [10, 20, 20]
    total: Int = 1
    ? = 1
```

Two things in it are worth naming.

`total: Int = 1` is the *partial* answer: one loop iteration had completed and
added its `1` before the second one faulted. That is the whole point of the
debugger — the state is the state at the moment of the fault, not a
reconstruction.

`? = 1` is the loop variable `i`. A `for` binding reaches the debugger with no
name, so `locals` prints `?` for it and `p i` reports that `i` is not defined.
The value is right and the label is not.

A pattern binding in a `match` arm loses its name the same way, and loses more:
it is not classified as a user binding at all, so it lands among the temps as a
line like `<tmp#7> = 7` with no type and no `@ "expr"`, and `p` will not bind
that name either. Function and closure parameters keep their names — `w: Int = 3`
in a `|w| …` frame — so these two are the exceptions and not the rule.

## What survives the fault

Nothing about a fault is a stack unwind in the C++ or Rust sense. Each generated
function's fault epilogue returns normally, and the **innermost** one — the first
to run, while the whole chain is still linked — deep-copies the entire frame
chain into a *crash snapshot* before it goes
([ADR-033](../../../decisions/033-crash-snapshot-rooting.md)). By the time
control is back in the host, the native frames are gone and the snapshot is
what you are talking to.

Two consequences you can see.

**The heap is still there, and the snapshot roots it.** Every value named by a
frame in the snapshot is a garbage-collection root, so a `Vec` you built ten
statements ago is still readable at the prompt, and `p` can allocate — it
compiles and runs a real function against the real heap — without the values you
are inspecting being collected out from under it.

**A value the collector already took shows as an absence, not as a lie.** A
local's debug slot keeps its value after the local's last *use*, so you can
still see it; but a collection between that last use and the fault is entitled
to reclaim the object, because nothing else refers to it. The debug slots are
the collector's one weak arm: a collection clears the slots whose objects it
reclaimed, so the snapshot copies `None` rather than a pointer into storage that
has since been handed to something else
([ADR-106](../../../decisions/106-the-debug-values-are-the-collectors-one-weak-arm.md)).

```praxis
var xs = Vec[Int]()
var i = 0
while i < 200 {
    xs.push(i + 2000)
    i = i + 1
}

var sum = xs.len()
var j = 0
while j < 40000 {
    var junk = Vec[Int]()
    junk.push(j + 2000)
    sum = sum + junk.len()
    j = j + 1
}

var ys = [sum]
out(ys[99])
```

`xs` is filled, read once into `sum`, and never touched again. The second loop
allocates forty thousand short-lived vectors, which is more than enough to
trigger a collection, and then the program faults on `ys[99]`.

```text
error: program faulted: index out of bounds

Backtrace:
#0   <entry>

  locals:
    xs: Vec[Int] = <uninit>
    i: Int = 200
    sum: Int = 40200
    j: Int = 40000
    junk: Vec[Int] = [41999]
    ys: Vec[Int] = [40200]
  temps:
    <tmp#3: Int> @ "0" = 0
    <tmp#5: Int> @ "200" = 200
    <tmp#6: Bool> @ "i < 200" = <uninit>
    <tmp#7: Int> @ "2000" = <uninit>
    <tmp#8: Int> @ "i + 2000" = <uninit>
    <tmp#9: Unit> @ "xs.push(i + 2000)" = Unit
  …(24 more)
Entered crash debugger. 1 frame(s). Type `help` for commands.
Praxis crash> p ys
[40200]
Praxis crash> p xs
error: type error: `xs` is not defined
Praxis crash> p sum
40200
Praxis crash> quit
```

`ys` is live and reads back. `xs` reads back as `<uninit>` and is not a name `p`
will bind, because the two-hundred-element vector it named no longer exists. The
alternative — and what the tree did before that fix — was to print `xs` as a
one-element vector holding a number from the *second* loop, because its memory
block had been reissued. A crash debugger that occasionally invents a plausible
value is worse than one that occasionally says nothing.

The rule to take away: **a binding you can still see in your source may read as
`<uninit>` if the program stopped using it long before the fault.** Read it as
"the collector got here first", not as "it was never assigned".
