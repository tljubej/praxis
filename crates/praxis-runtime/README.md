# praxis-runtime

The GC heap, type descriptors and runtime context for
[Praxis](https://github.com/tljubej/praxis).

This crate is the contract between JIT-generated code and the Rust runtime, and
it holds the collector.

- Every language value is a uniform `GcRef` — a `#[repr(transparent)]` pointer
  that generated code treats as opaque.
- Every generated function follows one calling convention:
  `fn(RuntimeContext*, GcRef...) -> GcRef`.
- A Rust panic never unwinds across the ABI. The runtime wrappers set
  `pending_fault` and return a defined sentinel instead, which is what turns an
  index out of bounds into a debugger session rather than an abort.
- A `TypeDescriptor` centralizes every operation on a value's payload, so the
  compiler never scatters type switches across the backend.
- Collection is precise, non-moving mark-and-sweep over size-class pages, rooted
  through a shadow stack the generated code maintains.

`RUNTIME_ABI_VERSION` is checked at startup: a compiler and a runtime that
disagree about the layout of a heap object refuse to run together rather than
reading the wrong bytes.

## What it provides

- `GcRef`, `Heap`, `RuntimeContext`, `TypeDescriptor` — the core vocabulary.
- The built-in collections: `Vec`, `Deque`, `Map`, `Set`, `Counter`, `MinHeap`,
  `MaxHeap`, `BitSet`, `Grid`, `Range`, and `Text`.
- `parser` — the interpreter for the plans `praxis-input-parser` compiles.
- `crash_snapshot`, `debug`, `breakpoint` — the per-frame state capture the
  crash debugger renders.
- `abi` — the `praxis_*` symbols the code generator links against.

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
