# praxis-input-parser

The `read` DSL of [Praxis](https://github.com/tljubej/praxis): template parsing,
static validation, result-type synthesis and parser plans.

In Praxis the input parser is part of the language. `read` is an expression, and
what follows it is a small declarative language for the shape of a file:

```praxis
var input = read sections(
    rules: lines(`{before:int}|{after:int}`),
    updates: lines(csv(int)),
)
```

Structure goes outside the backticks, where whitespace does not matter; the
input's literal text goes inside, where it does. Parsers nest, and **the type
comes from the parser** — `input` above is a record with a `rules` field of type
`Vec[{ before: Int, after: Int }]` and an `updates` field of type
`Vec[Vec[Int]]`, and nothing declared it. Getting the shape of the input wrong
is therefore a compile error rather than a surprise several hundred lines in.

The DSL keeps its own typed AST and is not lowered into string-splitting calls,
which is what lets it be type-checked, hovered over and completed like the rest
of the language.

## What it covers

- Atomics: `int`, `uint`, `float`, `byte`, `char`, `digit`, `word`,
  `identifier`, `text`, `rest`.
- Constructors: `lines`, `sections`, `csv`, `ws`, `sep`, `grid`, `matrix`,
  `chars`, `one_of`, `block`, `choice`, `optional`, `scan`, `repeated`.
- Backtick templates, whose `{name:int}` captures become record fields.
- `validate` — static checking, with `I0xx` diagnostics and did-you-mean
  suggestions.
- `synthesize` — the compile-time result type of a parser expression.
- `lower_to_plan`, `ParserPlan` — the plan the runtime interprets.

The interpreter that runs a plan against real bytes lives in `praxis-runtime`.

## Part of Praxis

Praxis is a small, statically typed, garbage-collected language for Advent of
Code-style puzzles: the input parser is part of the language, types are inferred
rather than written, and a program that falls over hands you its state instead
of a stack trace.

To *use* the language, install [`praxis-cli`](https://crates.io/crates/praxis-cli)
— it provides the `praxis` binary. The
[repository](https://github.com/tljubej/praxis) has the book, the design
document and the decision records.

This crate is one stage of that compiler, published so the pipeline is
inspectable and so `praxis-cli` can be built from the registry. Its API tracks
what the compiler needs and is not a stable platform for outside consumers.

Praxis was written with large language models against a human design. The
repository's README says what that means for the license.

Licensed under either of Apache License 2.0 or the MIT license, at your option.
