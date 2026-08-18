# praxis-repr

The bridge between a static [Praxis](https://github.com/tljubej/praxis) type and
its runtime type descriptor.

One small crate holding both directions, because they have to be inverses of
each other and two independently written halves are each locally plausible while
failing to compose. Keeping them side by side makes the round trip a test rather
than a hope, and the matches on both sides are exhaustive — a new built-in type,
scalar or collection constructor is a compile error here until it is handled.

The mapping is **total** in the sense that matters: `descriptor_for_type`
returns a `Result`. A type with no runtime representation — `Range`, the
compiler-internal `Seq`, `UInt`, `Never`, an unresolved type variable — yields
`NoRuntimeRepr` rather than a descriptor that names some other type. Each of
those is an upstream bug at a descriptor-producing site, and the JIT refusing to
emit is how it becomes visible, instead of becoming a wrong payload read at
runtime.

## What it provides

- `descriptor_for_type` — `praxis_typeck::Type` to
  `praxis_runtime::TypeDescriptor`.
- The inverse, for rendering a live value's type in the debugger.
- `NoRuntimeRepr`, `NoReprCause` — why a type has no descriptor, in a form fit
  for a diagnostic.

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
