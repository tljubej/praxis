# praxis-codegen-cranelift

Cranelift JIT code generation and ABI lowering for
[Praxis](https://github.com/tljubej/praxis).

Lowers MIR to Cranelift IR, registers the `praxis_*` runtime symbols and
finalizes the JIT module. Nothing lands on disk: `praxis run` lexes, infers,
lowers, compiles and executes in one step, and startup is milliseconds. Because
code generation is Cranelift, building the compiler needs a stable Rust
toolchain and nothing else — there is no LLVM to install.

Every generated function follows the uniform
`fn(RuntimeContext*, GcRef...) -> GcRef` convention. `GcRef` is
`#[repr(transparent)]` over a pointer and crosses the ABI as a pointer-sized
integer, opaque to generated code. MIR locals become Cranelift `Variable`s and
Cranelift builds SSA from them. GC roots go through a generated shadow-stack
frame, whose per-safepoint root set is what MIR's liveness pass computed.

## Reading the emitted code

Two environment variables dump what the backend emitted, on stderr, from the
real compile path. Each takes `1`, `all`, or a comma-separated list of function
names, and each dump is headed by its instruction count per block:

```console
$ PRAXIS_DUMP_CLIF='<entry>' praxis run loop.px    # Cranelift IR
$ PRAXIS_DUMP_VCODE=main praxis run loop.px        # machine-level listing
```

They are permanent rather than debug scaffolding, because an instruction count
is the deterministic result for a change that removes a few instructions from a
loop, and the clock is not.

## Feature flags

The `*-arm-a` and `unfolded-*` features are measurement toggles, not options.
Each reverts exactly one change in this crate so that change can be priced
against this tree rather than against an older commit. Several make the compiler
emit worse code on purpose and fail the tests that pin the better shape. Nothing
in the workspace enables any of them, and the crate is built exactly once with
every one of them compiled out.

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
