# praxis-source

Source files, spans, line maps and diagnostics for the [Praxis](https://github.com/tljubej/praxis)
compiler.

This is the leaf crate of the workspace: every other compiler crate reads source
through a `SourceMap` and reports problems as a `Diagnostic`. Nothing here
depends on anything else in Praxis.

The types are built so that illegal states cannot be constructed through the
public API — an inverted span, a span with no file, or a diagnostic with no
primary span are all unrepresentable rather than merely discouraged.

## What it provides

- `SourceMap`, `SourceFile`, `FileId` — the files a compilation reads.
- `Span`, `FileSpan`, `BytePos` — byte offsets into one of them.
- `LineMap`, `LineCol` — the byte-offset-to-line-and-column conversion, computed
  once per file.
- `Diagnostic`, `DiagCode`, `Severity`, `Suggestion` — the error format the
  whole compiler shares: a stable code, a primary span, notes, and where the
  compiler is sure of the fix, a machine-applicable suggestion an editor applies
  as a quick fix.
- `Renderer` — the terminal rendering of all of the above, with the offending
  line underlined.
- `nearest` — the one "did you mean" threshold, so every near-miss suggestion in
  the compiler agrees about when a near miss is near enough.

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
