# Breakpoints

Everything else in this chapter starts with a program that has already gone
wrong. A breakpoint starts with one that has not: you mark a statement, and when
the program reaches it, it stops and shows you the frame.

The mark is `:bp`, written after a statement:

```praxis
var doubled = seed * 2 :bp
```

It is syntax, not a function. There is nothing to import, nothing to call, and
no argument to pass — which also means it cannot be shadowed, passed around, or
accidentally left inside a data structure. It is a mark on a line, and it costs
one call at that line and nothing anywhere else in the program.

## Where it stops

**After** the statement it marks, not before. That is the useful order: the
statement's effect has happened, so the binding it created is in `locals` with
the value it created.

```praxis
// `:bp` marks a statement. The program runs it, stops, and shows you the frame.
fn grow(seed: Int) -> Int {
    var doubled = seed * 2 :bp
    doubled + 1
}

out(grow(20))
```

```console
$ praxis run bp-trace.px
stop: breakpoint
grow:
  <debug>:3:28
  3 |     var doubled = seed * 2 :bp
    |                            ^^^

Backtrace:
#0   grow
#1   <entry>

  locals:
    seed: Int = 20
    doubled: Int = 40
  temps:
    <tmp#2: Int> @ "2" = 2
    <tmp#3: Int> @ "seed * 2" = 40
    <tmp#5: Int> @ "1" = <uninit>
    <tmp#6: Int> @ "doubled + 1" = <uninit>
41
```

`doubled: Int = 40` is there because the stop is after the `var`. To see the
state *before* a statement, mark the one above it.

The rule holds for every statement form, including a block's trailing
expression: `m + 1 :bp` stops once `m + 1` has been computed, and the block still
yields it. What a marker never does is change what the program computes.

## Where you can write it

Anywhere a statement ends: after a `var`, after an assignment, after a bare
expression, after a block. The `:` and the `bp` must be adjacent — `: bp` with a
space is not a marker, the same rule [`min=`](../language/collections.md) lives
under.

```praxis
var xs = [1, 2, 3] :bp        // after a binding
xs.push(4) :bp                // after a call statement
total = total + n :bp         // after an assignment
counts[key] += 1 :bp          // after a store through a place
```

A marker is not an expression, so it does not go inside one: `f(x :bp)` does not
parse. Mark the statement that contains the call instead.

## Three ways a stop is served

Which surface you get follows `--debug`, exactly as
[a fault does](entering.md) — there is no second rule to learn.

| `--debug` | terminal? | at a `:bp` |
|---|---|---|
| `never` | — | nothing at all; the marker is inert |
| `auto` (default) | no | print the frame to stderr, keep running |
| `auto` / `always` | yes | the [full-screen debugger](tui.md) |
| `always` | no | the `Praxis stop>` prompt on stdin |

The second row is the one worth naming: at the default, in a pipe or a script or
CI, a marker is a **trace point**. It prints where the program is and what it
holds, and the program carries on. That is the output shown above, and it is why
`:bp` is useful in a program you are not sitting in front of.

`--debug never` makes every marker inert without touching the source, which is
what you want when the program is going somewhere else and you have not deleted
the marks yet.

## Continuing

At a prompt, `continue` (or `c`, or `cont`) lets the program go. It stops again
at the next marker it reaches — including the same one, on the next pass of a
loop, which the banner numbers for you.

```praxis
// A marker inside a loop stops on every pass, and the stop is numbered.
var total = 0
for n in [4, 7] {
    total = total + n :bp
}
out(total)
```

```console
$ printf 'bt\nlocals\ncontinue\nquit\n' | praxis run bp-loop.px --debug always
Stopped at a breakpoint. 1 frame(s). `continue` resumes; `help` lists commands.
Praxis stop> #0   <entry>
  (frame 0 selected)
Praxis stop>   locals:
    total: Int = 4
    n: Int = 4
  temps:
    <tmp#1: Int> @ "0" = 0
    <tmp#3: Vec[Int]> @ "[4, 7]" = [4, 7]
    <tmp#4: Int> @ "4" = 4
    <tmp#5: Unit> = Unit
    <tmp#6: Int> @ "7" = 7
    <tmp#7: Unit> = Unit
    <tmp#8: Int> = 0
    <tmp#9: Int> = 2
    <tmp#10: Int> = 4
    <tmp#12: Int> @ "total + n" = 4
    <tmp#14: Unit> @ "for n in [4, 7] { total = total + n :bp }" = <uninit>
    <tmp#15: Unit> @ "out(total)" = <uninit>
    <tmp#17: Int> = 0
    <tmp#18: Int> @ "var total = 0" = <uninit>
Praxis stop> continuing.
Stopped at a breakpoint (stop #2). 1 frame(s). `continue` resumes; `help` lists commands.
Praxis stop> leaving the debugger; the program runs on and will not stop again.
11
```

`quit` at a stop does not end the program — it ends the *debugging*. The program
runs to completion and no later marker takes the terminal again. There is no
command that kills a running program from a stop: a Praxis frame unwinds by
faulting, and reporting a fault the program did not have would be a lie about
what happened. Ctrl-C is what ends a run.

In the full-screen debugger the key is `c`, and the status bar leads with it.

## What a stop cannot do

A stop is the middle of a program, not the end of one, and that costs it three
commands:

- **`p EXPR` and `heap EXPR`.** Evaluating an expression means compiling a
  function and running it, and the program's own frames are still on the stack
  underneath you. `type EXPR` still works — it type-checks against the frame's
  locals and runs nothing.
- **`restart` and `reload`.** Both re-run the program from the beginning, and
  there is a program in progress. Continue to the end, and if it faults you have
  the crash debugger with both.

Everything else is the same command against the same kind of snapshot:
[`bt`](commands.md#bt-backtrace), [`frame N`](commands.md#frame-n),
[`up`/`down`](commands.md#up-down), [`locals`](commands.md#locals),
[`source`](commands.md#source-n), `help`, `quit`.

## What a marker costs

One call, at the marked line, and nothing else. The wrapper it calls allocates
nothing and cannot fault, so the compiler emits no root spill before it and no
fault check after — a marked statement is the unmarked statement plus a `call`.
A program with no marker in it emits nothing at all.

That is worth knowing because it means you can leave a marker in a loop that runs
a million times and, under `--debug never`, pay only the call. What you should
not do is leave one in and commit it: a marker in a program somebody else runs
stops *their* program.
