# praxis

Praxis is a small, statically typed, garbage-collected language for Advent of
Code-style puzzle solving. It is procedural and expression-oriented, it infers
essentially every type, and it is built around the three things a puzzle solver
spends the evening doing: reading a strange input format, manipulating data, and
finding out why the program just fell over.

This crate is the compiler. `praxis run` parses, type-checks, lowers,
JIT-compiles and executes a `.px` file in one step; there is no build product
and nothing lands on disk.

```console
$ cargo install praxis-cli
```

The binary it installs is called `praxis`.

## A program

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

No `main`, no imports, no annotations. `read` is an expression in the language
and the backticked template is a parser the compiler checks and compiles
alongside the rest of your program, so `moves` is a `Vec` of records with a `dir`
field and an `amount` field. Nothing declared that record; the template's
captures are where it came from.

## When it falls over

There are no exceptions and no error handling. An index out of bounds, an
integer overflow, a missing key or a template that does not match its input
stops the program and hands you the wreckage — not only the locals, but the
intermediate values of the expression that faulted, each labelled with the
source text it came from. On a terminal you get a prompt instead of an exit
code: walk the frames, print expressions against the captured state, then fix
the file and `reload` without losing your input.

`praxis check` reports the same diagnostics without running anything, and
`praxis lsp` is the language server your editor starts for you.

## More

- [The Praxis Book](https://github.com/tljubej/praxis/tree/main/docs/book/src)
  covers the language, the input DSL, type inference and the crash debugger.
- [The repository](https://github.com/tljubej/praxis).

Licensed under either of Apache License 2.0 or the MIT license, at your option.
