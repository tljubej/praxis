# praxis-syntax

Token and syntax-node definitions for the [Praxis](https://github.com/tljubej/praxis)
language.

The syntax tree is [`rowan`](https://crates.io/crates/rowan)-backed and lossless
— trivia and all — so the language server can rewrite a span without disturbing
the comments and whitespace around it. This crate contributes the vocabulary
that tree is made of; `praxis-parser` is what builds one.

## What it provides

- `SyntaxKind` — the single enum covering tokens, trivia and tree nodes.
- `PraxisLanguage`, `SyntaxNode`, `SyntaxToken`, `SyntaxElement` — the rowan
  `Language` impl and its node aliases.
- `span_bridge` — the only place `praxis_source::Span` and `rowan::TextRange`
  meet. The Praxis `Span` stays the source of truth for diagnostics.

It also holds the scanners that more than one consumer needs to agree about, in
one copy each: the identifier character class, the text-literal decoder, the
digit-separator rule, the rule for where an interpolated `"…"` literal ends, and
the rule for where a backtick template ends. The lexer and the input parser both
read the last of those, which is what stops one of them from accepting a
template the other cannot.

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
