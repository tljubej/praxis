# The Praxis Book

Praxis is a small, statically typed, garbage-collected programming language for
Advent of Code-style puzzle solving. It is procedural and expression-oriented,
it infers essentially every type, and it is built around three things a puzzle
solver spends the evening doing: reading a strange input format, manipulating
data, and finding out why the program just fell over.

A whole program can be three lines:

```praxis
var numbers = read lines(int)

out(numbers.sum())
```

```console
$ praxis run sum.px --input sum.in
60
```

There is no `main`, no imports, no annotations. `read lines(int)` is not a
library call — `read` is an expression in the language and `lines(int)` is a
parser written in a small DSL the compiler checks and compiles alongside the
rest of your program. `numbers` is a `Vec[Int]` because that is what the parser
produces, and the compiler worked that out rather than being told.

## The three things this book is mostly about

**Reading input.** Puzzle input is never a data format anybody would choose.
Praxis makes the shape of the file the shape of the code. Structure is written
outside backticks, where whitespace does not matter; the literal text of the
input is written inside them, where it does:

```praxis
var moves = read lines(`{dir:word} {amount:int}`)

var horizontal = 0
var depth = 0

for move in moves {
    match move.dir {
        "forward" => { horizontal = horizontal + move.amount }
        "down" => { depth = depth + move.amount }
        "up" => { depth = depth - move.amount }
        _ => {}
    }
}

out(horizontal * depth)
```

```console
$ praxis run dive.px --input dive.in
150
```

`moves` is a `Vec` of records with a `dir` field and an `amount` field. Nothing
declared that record; the template's captures are where it came from.
[Reading input](input/read.md) is the part of the book that covers this, and
[the cookbook](input/cookbook.md) has a recipe for every input shape Advent of
Code has thrown so far.

**Type inference.** Praxis is statically typed and almost none of the types are
written down. Inference runs over your whole program including the parsers, so a
mistake about the shape of the input is a compile error rather than a surprise
at line 400. The editor shows you what it concluded: `fn foo(a, b)` reads as
`fn foo(a: Int, b: Int)` in VS Code, and you can accept the hint to write the
annotation into the file. [Type inference](types/model.md) covers the model,
what generalizes, and how to read the errors when the compiler disagrees with
you.

**The crash debugger.** Praxis has no exceptions and no error handling. An index
out of bounds, a missing key, an integer overflow, a parse mismatch or a failed
assertion stops the program and hands you the wreckage:

```text
error: program faulted: index out of bounds

Backtrace:
#0   window_sum
#1   <entry>

  locals:
    values: Vec[Int] = [12, 7, 41]
    start: Int = 1
  temps:
    <tmp#3: Int> @ "values[start]" = 7
    <tmp#5: Int> @ "start + 1" = 2
    <tmp#6: Int> @ "values[start + 1]" = 41
    <tmp#9: Int> @ "start + 2" = 3
```

Those are not only the locals — they are the intermediate values of the
expression that faulted, each labelled with the source text it came from. In a
terminal you get a prompt instead of an exit code, and can walk the frames,
print expressions against the captured state, look at the input near the
parser's cursor, then fix the file and `reload` without losing your input.
[The crash debugger](debugger/faults.md) is the part of the book that covers it.

## What Praxis is not

It does not build standalone binaries; the compiler and the runtime are one
executable and your program is JIT-compiled every time you run it. There is no
ownership, no lifetimes, no manual memory management. There are no user-visible
traits, no operator overloading, no macros, and no exceptions. There is no
concurrency. There is no package registry.

These are deliberate, and when two of the goals conflict there is an order that
settles it: correctness and diagnostics first, then input ergonomics, then
edit-run-debug speed, then language simplicity, and only then runtime
performance.

## How this book is arranged

- **[Getting started](getting-started/install.md)** — build the compiler, run a
  program, learn the command surface.
- **[The language](language/program-structure.md)** — the whole surface, from
  bindings to pattern matching to the collection set, with the prelude and the
  method catalog as reference tables at the end.
- **[Reading input](input/read.md)** — the `read` expression and its DSL.
- **[Type inference](types/model.md)** — what the compiler works out, and why it
  sometimes will not.
- **[The crash debugger](debugger/faults.md)** — the fault model, and the
  prompt you get instead of an exit code.
- **[Tooling](tooling/editors.md)** — the language server, the VS Code
  extension, and an index of every diagnostic code.
- **[Under the hood](internals/pipeline.md)** — for the reader who wants to
  change the compiler rather than use it.
- **[Appendix A](appendix/programs.md)** has complete programs;
  **[Appendix B](appendix/grammar.md)** has the grammar.

## Every example here was run

No code block in this book that shows a program and its output was written by
hand. Each one is a real file under `docs/book/examples/`, and

```bash
docs/book/examples/verify.sh
```

re-runs every one of them against the compiler in this repository and diffs the
result against the output printed in the chapter. That includes the programs
that are supposed to fail: the diagnostics and the debugger transcripts are
captured the same way. If the language changes, this book breaks loudly.
