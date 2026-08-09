# Noninteractive mode

A prompt is no use to a build server. When there is nobody there to type,
`praxis run` prints the same information the debugger would have shown you on
arrival — the fault, the backtrace, and the faulting frame's locals — and exits
1.

That happens in two cases: you passed `--debug never`, or you left `--debug` at
its default of `auto` and either stdin or stdout is not a terminal. A pipe, a
redirect, a CI runner and a test harness all land in the second case without
being asked to, which is the point.

| `--debug` | on a fault |
|---|---|
| `auto` (default) | enter the prompt iff stdin **and** stdout are a terminal |
| `always` | enter the prompt regardless |
| `never` | never enter the prompt; print the diagnostic and exit |

The same three rows govern a [`:bp` breakpoint](breakpoints.md), where the
declining case prints the frame and then **keeps running** — a marker in a
script is a trace point rather than a prompt nobody is there to answer.

## What gets printed

```praxis
// A fault two frames deep, with something worth reading in each frame.
fn mean(xs: Vec[Int], n: Int) -> Int {
    xs.sum() / n
}

fn summarize(xs: Vec[Int]) -> Int {
    var kept = xs.filter(|x| x > 100)
    mean(kept, kept.len())
}

var readings = [3, 9, 27]
out(summarize(readings))
```

```console
$ praxis run nonint-fault.px --debug never
error: program faulted: division by zero

Backtrace:
#0   mean
#1   summarize
#2   <entry>

  locals:
    xs: Vec[Int] = []
    n: Int = 0
  temps:
    <tmp#3: Int> = 0
    <tmp#4: Int> = 0
    <tmp#7: Int> @ "xs.sum() / n" = <uninit>
$ echo $?
1
```

Four parts, in order:

1. **The fault line.** `error:` in the same red the compiler uses for an error,
   then the fault kind. `--color never` turns the colour off, `--color always`
   forces it on; `auto` colours iff stderr is a terminal.
2. **The backtrace**, innermost first, one line per frame. `<entry>` is the
   synthetic function a file's top-level statements compile into.
3. **The innermost frame's locals**, split into the bindings you wrote and the
   compiler's temporaries. Most temps carry the source expression that produced
   them, as `@ "expr"`, alongside the value they hold; one the lowering minted
   with no expression of its own — `<tmp#3>` and `<tmp#4>` above — shows the
   value alone. `<uninit>` means the slot was never written, so the last few
   temps trace out exactly how far the faulting expression got before it
   stopped.
4. Nothing about the other frames. Only frame 0's state is printed; if you need
   frame 1's, you need the prompt.

All four go to **stderr**. Whatever the program wrote before it faulted is
already on stdout and stays there:

```praxis
// Output written before the fault is already on stdout; the diagnostic is on
// stderr. A script that captures them separately keeps both.
var totals = [10, 20, 30]
out(totals[0])
out(totals[1])
out(totals[5])
```

```console
$ praxis run nonint-partial.px --debug never
10
20
error: program faulted: index out of bounds
...
```

### `panic` and `assert`

For those two fault kinds the message is the diagnosis, so it is appended to the
fault line rather than buried:

```praxis
// `panic` and `assert` put their message on the fault line itself.
var node = "start"
var visited = [1, 2]
panic("no route from " + node)
```

```text
error: program faulted: panic: no route from start
```

An `assert` that fails prints `error: program faulted: assertion failed` and
carries no message — `assert` takes one argument.

### A parse failure adds two lines

When the fault kind is `input parse mismatch`, the input offset and the
expectation are printed under the fault line, before the backtrace:

```text
error: program faulted: input parse mismatch
       at input offset 10..11: expected literal ","
       actual: 12,7⏎5,3⏎9;1⏎4,4⏎
```

That is the whole of what the `input` and `parser` commands would have told you,
which makes a parse failure the one fault kind you can usually diagnose without
the prompt. [Inspecting the input parser](parser.md) covers reading it.

## The twelve-local cap

The printed diagnostic shows at most **twelve locals per frame**, user bindings
first, and counts the rest. It is a glance, not a dump.

```praxis
// Fourteen bindings; the noninteractive render shows twelve and counts the rest.
fn main() {
    var alpha = 1
    var bravo = 2
    var charlie = 3
    var delta = 4
    var echo = 5
    var foxtrot = 6
    var golf = 7
    var hotel = 8
    var india = 9
    var juliett = 10
    var kilo = 11
    var lima = 12
    var mike = 13
    var november = 14
    out(alpha / (mike - 13))
}
```

```text
error: program faulted: division by zero

Backtrace:
#0   main

  locals:
    alpha: Int = 1
    bravo: Int = 2
    charlie: Int = 3
    delta: Int = 4
    echo: Int = 5
    foxtrot: Int = 6
    golf: Int = 7
    hotel: Int = 8
    india: Int = 9
    juliett: Int = 10
    kilo: Int = 11
    lima: Int = 12
  …(20 more)
```

Twelve shown, twenty hidden: `mike`, `november`, and eighteen temporaries. Note
that user bindings get priority — you never lose a variable you wrote to a temp.

The cap applies to the **printed diagnostic only**. The interactive `locals`
command has no limit, which is one concrete reason to re-run a fault under
`--debug always` when the twelve were not the twelve you wanted. There is an
uncapped `locals` in the [walkthrough](walkthrough.md): frame 1 prints three
bindings and eighteen temps in one go.

Values themselves are never truncated, in either mode. A thousand-element vector
prints as a thousand elements; see [Evaluating
expressions](expressions.md#rendering-and-what-it-does-not-truncate).

Backtraces are not capped either, and one fault kind makes that visible. A
runaway recursion faults when it has spent its native-stack budget. The budget
is denominated in bytes and each call is charged for its own width, so an
ordinary recursive function gets about eight thousand frames and a function with
many live collections per frame gets fewer
([ADR-105](../../../decisions/105-the-recursion-guard-spends-a-byte-budget.md)).
Every one of those frames is in the backtrace. A function that recurses past its
budget

```praxis
fn depth(n: Int) -> Int {
    if n == 0 { 0 } else { 1 + depth(n - 1) }
}

out(depth(100000))
```

faults with `stack overflow (recursion limit)`, and the backtrace under it is
eight thousand lines long: 7999 `depth` frames and the `<entry>` beneath them.
Pipe the run through `head` and read the first few; the interesting ones are the
handful at the very bottom, where the recursion started.

## Exit codes

| code | meaning |
|---|---|
| 0 | the program ran to completion |
| 1 | a compile error, or a runtime fault |
| 2 | the source file or the `--input` file could not be read |

A fault exits 1 whether the diagnostic was printed noninteractively or the
prompt was entered and left — `--debug always` followed by `quit` (or by EOF on
stdin) still exits 1. There is no exit code that distinguishes "faulted" from
"did not compile"; if a script needs to tell them apart, run `praxis check`
first, which exits 1 only on a compile error.

## Using it in a script

The shape that works is: let the fault print, keep both streams, and let the
exit code decide.

```console
$ praxis run solve.px --input day01.txt --debug never --color never \
    > answer.txt 2> fault.txt
$ test -s fault.txt && cat fault.txt
```

`--debug never` is worth passing explicitly even though `auto` would do the same
thing under a redirect: it makes the intent local to the command, and it does
not change behaviour if somebody later runs the script from a terminal.
`--color never` keeps ANSI escapes out of the captured file.

The diagnostic is written to stderr line by line and is short — the whole thing
for a typical fault is under twenty lines — so capturing it in a CI log costs
nothing and is usually enough to identify the failure without re-running.

## `restart` and `reload`

These two are the prompt's, not the noninteractive path's, but they belong here
because they are the mechanism a faulted session uses to become a working one.

`restart` re-runs the same compiled machine code. `reload` re-reads the source
file from disk, recompiles it, and then re-runs. Both keep the input bytes the
first run was given and the path they came from, which is §9.7's guarantee.
(§9.7 also lists the debugger's display preferences among what is retained;
there are none — no command changes how a value prints.)

Both clear the fault, the crash snapshot and the parse detail before the run, so
a second fault captures a fresh frame chain and leaves you at the prompt with the
frame cursor back at 0.

```praxis
// `restart` reruns the same machine code; `reload` recompiles the file first.
// Both keep the input the first run was given.
var budget = read int
var spend = [40, 40, 40]
var left = budget
for s in spend {
    left = left - s
}
out(100 / left)
```

```text
Praxis crash> p left
0
Praxis crash> restart
program faulted: division by zero
1 frame(s); frame 0 selected.
Praxis crash> p left
0
Praxis crash> reload
program faulted: division by zero
1 frame(s); frame 0 selected.
Praxis crash> p budget
120
Praxis crash> quit
```

Both re-faulted, because nothing changed between them — the source on disk is
the same source. The line that carries information is the last one: `p budget`
answers `120`, the number the *first* run read from the input file. The input
was retained across two re-executions and re-fed to `read int` each time, which
is what makes `restart` a controlled experiment rather than a coin toss.

Three details that matter in practice.

**A clean run does not exit.** When a re-run finishes without faulting, the
debugger prints `program completed: <value>` and stays at the prompt. Its stdout
goes to stdout as usual. The [walkthrough](walkthrough.md) ends this way. The
snapshot from the run that faulted is still the one you are inspecting — there
is no new one to replace it, so `bt` and `p` keep answering about the old
frames.

**A failed `reload` changes nothing.** If the recompile produces a parse error
or a type error, the diagnostic is printed, the old JIT code and the old
snapshot stay live, and the session continues exactly as it was:

```text
Praxis crash> reload
error: parse error: expected `)`, found unexpected token
(session unchanged — the old snapshot is still active)
Praxis crash> p i
8
```

New code is swapped in only after the compile has succeeded, so there is no
window in which the session is half-reloaded.

**"Same input" means the bytes the first run read.** If the program faulted
before it ever evaluated a `read`, nothing was read, and the retained input is
empty — a `reload` whose edit moves the `read` earlier will see empty input
rather than the original stdin, which by then is at EOF. With `--input FILE`
this cannot happen: the file was read eagerly, before the program started.

`restart` and `reload` are the only commands that re-run the program, and
neither of them resumes the faulted run. There is no continue, no step and no
way to change a value and go on. A fault is terminal; what the debugger gives
you is the state it left behind, and a fast way to try again.
