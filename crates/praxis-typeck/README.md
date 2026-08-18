# praxis-typeck

Type interning, inference and capability resolution for
[Praxis](https://github.com/tljubej/praxis).

A Hindley–Milner-inspired inference engine, extended for mutable variables,
tuples, function types and let-generalization. Praxis programs carry almost no
annotations, so this is what decides that `input` has type
`{ rules: Vec[{ before: Int, after: Int }], updates: Vec[Vec[Int]] }` when
nothing in the file said so.

Representation is an interned arena: every `Type` is a copyable `u32` handle
into a `TypeDb`, and type variables live in the arena too, so unification links
them by mutation instead of through `Rc<RefCell<…>>`.

The scalar and collection vocabulary is reused from `praxis-stdlib` rather than
restated; this crate adds the inference layer on top of it.

## What it provides

- `TypeDb`, `Type`, `VarId`, `TypeKey` — the arena and its handles.
- `unify`, `constraint`, `Capability` — unification and the constraints a type
  variable can carry.
- `Scheme`, `generalize` — let-polymorphism.
- `RecordDef`, `EnumDef`, `FieldSet`, `VariantSet`, `TupleElems` — user-declared
  and anonymous structured types.
- `pretty` — how a type is spelled in a diagnostic or a hover.

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
