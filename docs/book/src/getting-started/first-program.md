# Your first program

A Praxis program is one file with a `.px` extension. It has no imports, no
`main` you are obliged to write and no module declaration — the top-level
statements in the file *are* the program.

```praxis
out("Hello, Praxis!")
```

Save that as `hello.px` and run it:

```console
$ praxis run hello.px
Hello, Praxis!
```

`out` is the print function; it writes its argument and a newline to standard
output. `run` parses, type-checks, lowers, JIT-compiles and executes in one
step — there is no separate build product and nothing lands on disk.

## A program that reads its input

Praxis exists to solve puzzles that arrive as a text file, so the interesting
first program reads one. Here is the sonar-sweep problem from Advent of Code
2021 day 1: given a list of depth measurements, count how many are larger than
the one before.

The input is a file of integers, one per line — call it `sonar.in`:

```text
199
200
208
210
200
207
240
269
260
263
```

And the first draft of the program:

```praxis
var depths = read lines(int)

var increases = 0
for i in 1..depths.length() {
    if depths.get(i) > depths.get(i - 1) {
        increases = increases + 1
    }
}

out(increase)
```

Three things are worth naming before we run it. `var` is the only binding form,
and every binding is assignable; there is no `let`, and no `mut` either — a
parameter, a `for` variable and a name bound by a pattern are all writable too.
`read lines(int)` is an input parser, not a library call — `lines(int)` is a
shape the compiler understands, and the type of `depths` is derived from it as
`Vec[Int]` rather than declared. And `1..depths.length()` is a half-open range,
so it stops one short of the end, which is what a loop that looks backwards
wants.

## Getting it wrong

```console
$ praxis run sonar-draft.px --input sonar.in
error[Y110]: no method `length` on type `Vec[Int]` taking 0 argument(s)

  sonar-draft.px:4:20
  4 | for i in 1..depths.length() {
    |                    ^^^^^^ no method `length` on type `Vec[Int]` taking 0 argument(s)

error[N001]: `increase` is not defined

  sonar-draft.px:10:5
  10 | out(increase)
     |     ^^^^^^^^ `increase` is not defined

help: did you mean `increases`?
      increases

praxis: 2 error(s)
```

Two mistakes, both reported. That is the normal case: analysis does not stop at
the first error, so one run tells you everything the front end knows.

Read one diagnostic and you can read all of them. It opens with a severity and a
code — `Y110` is a type error, `N001` a name-resolution error, and the letter
says which phase found it. Then the file, line and column, then the source line
with the exact span underlined. `help:` is a suggestion, and where the compiler
is confident enough to write the replacement — as it is for `increase` here — the
same suggestion is the quick fix your editor offers. Every code is listed in
[Diagnostic codes](../tooling/diagnostics.md).

The compiler declined to run the program at all. Nothing was JIT-compiled and
nothing was executed: a file with an error in it never reaches the back end, so
you cannot get partial output from a program that does not type-check. `praxis
check sonar-draft.px` prints exactly the same report and skips even trying.

Note also what the *first* error did not say. There is no "did you mean `len`?"
under `length`. A near miss is offered when it is within an edit distance of
`max(1, n / 3)` for a name of `n` characters — so `length`, at six characters,
gets a budget of two, and `len` is three edits away. A suggestion that fires too
eagerly is worse than none, because an editor that offers to rewrite `x` as `y`
teaches you to stop reading the quick-fix list.

## Getting it right

`length` is spelled `len`, and the variable is `increases`:

```praxis
var depths = read lines(int)

var increases = 0
for i in 1..depths.len() {
    if depths.get(i) > depths.get(i - 1) {
        increases = increases + 1
    }
}

out(increases)
```

```console
$ praxis run sonar.px --input sonar.in
7
```

`--input FILE` is one of two ways to feed a program. The other is standard
input, which is what you get when you leave the flag off:

```console
$ praxis run sonar.px < sonar.in
7
```

They differ in one respect that matters when something goes wrong: `--input` is
read up front, so an unreadable file is reported before your program starts,
while standard input is not read until the program's first `read` actually
evaluates. A program with no `read` in it never touches stdin at all.

## The shorter way

The loop above is the one you would write in any language. Praxis would rather
you wrote a pipeline:

```praxis
var depths = read lines(int)

out(depths.zip(depths.skip(1)).count(|pair| pair.1 > pair.0))
```

```console
$ praxis run sonar-pipeline.px --input sonar-pipeline.in
7
```

`skip(1)` drops the first measurement and `zip` pairs the two sequences
positionally, stopping at the shorter — so each pair is a measurement and the
one after it. `|pair| pair.1 > pair.0` is a closure over the resulting tuple,
and `count` answers how many satisfy it. There is no `.collect()` at the end
because there is nothing to collect: every stage materializes, so the chain
already *is* a value. See
[Pipelines](../language/pipelines.md).

## When a program compiles and still goes wrong

A clean `praxis check` means the types work out, not that the program does.
Division by zero, an index past the end of a `Vec`, integer overflow and a
`read` that does not match its input are all runtime faults:

```praxis
var total = 10
var n = 0

out(total / n)
```

Run that on a terminal and Praxis drops you into an interactive debugger at the
faulting instruction, with the locals still alive. Run it anywhere else — a
pipe, a CI job, a `--debug never` — and it prints the same state
noninteractively and exits `1`:

```console
$ praxis run divide-by-zero.px --debug never
error: program faulted: division by zero

Backtrace:
#0   <entry>

  locals:
    total: Int = 10
    n: Int = 0
  temps:
    <tmp#1: Int> @ "10" = 10
    <tmp#3: Int> @ "0" = 0
    <tmp#5: Int> @ "total / n" = <uninit>
    <tmp#6: Unit> @ "out(total / n)" = <uninit>
```

The `temps` are the compiler's own intermediate values, each labelled with the
expression that produced it, and `<uninit>` marks the ones the fault stopped
from ever being assigned — which is how you find the instruction that failed.
[The fault model](../debugger/faults.md) explains the fault kinds, and
[Entering the debugger](../debugger/entering.md) explains when you get a prompt.

## Where to go next

The three commands you will use are `run`, `check` and — through your editor —
`lsp`; [The command line](cli.md) is the complete surface, including the exit
codes. [A file is a program](../language/program-structure.md) explains what the
top level really is and when you would write `fn main` instead. And [The `read`
expression](../input/read.md) is the part of Praxis that most repays reading
early: `lines(int)` is the simplest shape it has, and the puzzle input you are
about to paste in is probably not that shape.
