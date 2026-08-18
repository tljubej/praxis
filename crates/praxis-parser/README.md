# praxis-parser

Lexer and parser for the [Praxis](https://github.com/tljubej/praxis) language.

- `lex` turns source text into a lossless token stream, trivia included, plus
  `T0xx` diagnostics.
- `parse` runs the lexer and then a recursive-descent parser with a Pratt
  expression layer, producing a [`rowan`](https://crates.io/crates/rowan)-backed
  lossless tree plus `P0xx` diagnostics.

Parsing does not stop at the first error, and the tree it returns is complete
whether or not one was reported: an editor asking for hover types in a file with
a syntax error still gets a tree to ask about. Because trivia is retained, a
code action can rewrite one span and leave every comment where the author put
it.

`praxis-ast` is the typed view over the result; `praxis-syntax` owns the node
vocabulary.

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
