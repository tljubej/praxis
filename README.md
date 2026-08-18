# Praxis

Praxis is a small, statically typed, garbage-collected programming language for
solving puzzles — Advent of Code and everything shaped like it. The evening it
is built for goes: read a file in a format nobody would have chosen, push the
data around until an answer falls out, find out fast when it does not. So the
parser is part of the language, the types are inferred rather than written, and
a program that falls over hands you its state instead of a stack trace. One
`.px` file is a program: no `main`, no imports, no build step.

```praxis
// One `read` describes the whole file: two sections, each with its own shape.
var input = read sections(
    rules: lines(`{before:int}|{after:int}`),
    updates: lines(csv(int)),
)

var order = Set()
for rule in input.rules {
    order.insert((rule.before, rule.after))
}

var good = input.updates.filter(|pages| {
    (0..pages.len()).all(|i| {
        (0..i).all(|j| !order.contains((pages[i], pages[j])))
    })
})

out("in order: {good.len()}")
out("middle sum: {good.map(|pages| pages[pages.len() / 2]).sum()}")
```

The file it reads is a block of ordering rules, a blank line, then the page
lists to check against them:

```console
$ cat pages.in
47|53
97|13
75|29
29|13

75,47,61,53,29
97,61,53,29,13
75,29,13
61,13,29
$ praxis run pages.px --input pages.in
in order: 3
middle sum: 143
```

Nothing there is annotated and no record was declared: `input` has the type
`{ rules: Vec[{ before: Int, after: Int }], updates: Vec[Vec[Int]] }` because
that is what the parser produces.

## What you get

- **`read`, an input parser in the language.** `read` is an expression, and what
  follows it is a small declarative language for the shape of a file: `lines`,
  `sections`, `csv`, `ws`, `grid`, `matrix`, `chars`, and backtick templates
  whose `{name:int}` captures become record fields. Structure goes outside the
  backticks, where whitespace does not matter; the input's literal text goes
  inside, where it does. Parsers nest, and the type comes from the parser.
- **Type inference over everything.** A parameter, a collection's elements, a
  closure's argument and a parser's product are all worked out for you — so a
  mistake about the shape of the input is a compile error, not a line-400
  surprise.
- **Collections chosen for this work.** `Vec`, `Deque`, `Map`, `Set`, `Counter`,
  `MinHeap`, `MaxHeap`, `BitSet`, `Grid` and `Range` are built in and already in
  scope. Pipelines are eager — `map`, `filter`, `zip`, `windows` and the rest
  each answer a value, so a chain already *is* a `Vec`, with no `.collect()`.
- **Records, enums, matching, closures.** Named records and anonymous ones,
  enums with payloads, `Option` in the prelude, and a `match` that does not
  compile until every case is covered.
- **Diagnostics written to be read.** Every error carries a stable code, the
  exact span underlined in your source, and — where the compiler is sure of the
  fix — a suggestion your editor applies as a quick fix. Analysis does not stop
  at the first error.
- **A language server in the same binary.** `praxis lsp` serves hover types,
  completion over the method catalog, signature help, go-to-definition,
  references, rename, symbols, code actions and semantic tokens. Inlay hints
  show the types you did not write, so `fn foo(a, b)` reads as
  `fn foo(a: Int, b: Int)`.
- **JIT-compiled through Cranelift.** Nothing lands on disk: `praxis run` lexes,
  infers, lowers, compiles and executes in one step. Startup is milliseconds and
  the generated code is fast enough to stop thinking about.

## When it falls over

There are no exceptions and no error handling. An index out of bounds, a
missing key, an overflow, a mismatched `read` or a failed assertion stops the
program and hands you the wreckage:

```text
error: program faulted: index out of bounds

Backtrace:
#0   window_sum
#1   <entry>

  locals:
    values: Vec[Int] = [12, 7, 41]
    start: Int = 1
  temps:
    <tmp#6: Int> @ "values[start + 1]" = 41
    <tmp#9: Int> @ "start + 2" = 3
    <tmp#10: Int> @ "values[start + 2]" = <uninit>
```

Those are not only the locals: they are the intermediate values of the faulting
expression, each labelled with the source text it came from, and `<uninit>`
marks the one the fault stopped from being assigned. On a terminal you get a
full-screen debugger instead of an exit code — backtrace, source, locals and
transcript at once — where you walk the frames, evaluate expressions against the
captured state, then fix the file and `reload` without re-entering your input.

## Getting started

You need a stable Rust toolchain and nothing else — code generation is
Cranelift, so there is no LLVM to install. From a checkout:

```console
$ cargo install --path crates/praxis-cli
```

That builds in release mode and puts the `praxis` binary in `~/.cargo/bin`;
`cargo build --release -p praxis-cli` leaves it at `target/release/praxis`
instead. There are three commands:

```text
praxis run day05.px < input.txt        # compile and run, input on stdin
praxis run day05.px --input in.txt     # read the input from a file instead
praxis run day05.px --debug never      # print the fault report, never prompt
praxis check day05.px                  # diagnostics only; nothing is generated
praxis lsp                             # the language server, for your editor
```

`--debug <auto|always|never>` decides what a runtime fault does; the default,
`auto`, opens the debugger when stdin and stdout are both a terminal.
`--color <auto|always|never>` is global and valid on every subcommand.

## Documentation

- [**The Praxis Book**](docs/book/) is the manual: getting started, the whole
  language, the `read` DSL and a cookbook of input shapes, type inference, the
  crash debugger, editor support and an index of every diagnostic code. Every
  code block showing a program and its output is a real file under
  `docs/book/examples/`, re-run against this compiler — so the book breaks
  loudly when the language changes. `just book` renders it.
- [`editors/vscode/`](editors/vscode/) is the VS Code extension: a launcher for
  the language server, and a grammar that highlights a `.px` file before the
  server attaches.
- [`benchmarks/`](benchmarks/) writes eight programs three times each — Praxis,
  Rust, Python — and refuses to time a set whose implementations do not print
  byte-identical output; [`REPORT.md`](benchmarks/REPORT.md) has the numbers and
  the method.
- To work on the compiler itself: `just ci` is the whole quality gate, and
  [`docs/technical-design.md`](docs/technical-design.md) is the design
  document.

## Authorship

Praxis was written with large language models. The design is human — the
language itself, the decision records under `docs/decisions/`, the technical
design document, the shape of the workspace and what each crate is allowed to
know — and nearly all of the Rust implementing it was generated against that
design, then reviewed and kept or thrown away.

That is stated plainly because it bears on the license. In the EU, copyright
attaches to a human author's own intellectual creation, so how much of any
given file here is protected at all is genuinely unclear. The dual license
below is offered on its ordinary terms regardless: where there are rights to
grant, it grants them; where there are none, the code is free anyway. The
warranty disclaimer stands either way, and a contribution you send keeps its
own author's copyright under the same terms.

## License

Dual-licensed under [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your
option.
