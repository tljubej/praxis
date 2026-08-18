# praxis-mir

The mid-level IR of [Praxis](https://github.com/tljubej/praxis): control-flow
graph, liveness, fault edges and GC safepoint analysis.

MIR is basic blocks with explicit branches, local slots holding a `GcRef` for
every language value, calls, allocations, bounds and overflow checks, fault
edges, safepoints and debug-local metadata. It is not SSA — the Cranelift
lowering layer builds SSA values and block parameters from the slot-based CFG.

## The passes

- `build` — typed HIR to MIR.
- `forward` — a peephole over the box/unbox pairs the builder's single return
  convention emits, leaving behind exactly the boxes that cross a block
  boundary. It also records which scalar local holds a forwarded value's word,
  so the debugger can still render a temporary whose box was elided.
- `promote` — the whole-function pass that decides those slots' representation.
- `liveness`/`annot` — the minimal root set per safepoint.
- `verify` — the rooting invariant, checked after annotation. Every host runs it
  and refuses to compile MIR that fails, so a stage that breaks the invariant is
  caught where it broke it.

`forward` runs before `promote`, and the order is load-bearing rather than
alphabetical: running `promote` first would have it price materializations
`forward` was about to delete, and decline promotions on the strength of a cost
that was never going to be paid. Both run before annotation, because each
deletes safepoints.

## Feature flags

The `*-arm-a` features are measurement toggles, not options. Each reverts one
change so that change can be priced against this tree rather than against an
older commit — a baseline taken the wrong way once reported 14.4% where the
truth was 0.8%. Several make the compiler emit worse code or accept IR it should
refuse, and the tests that pin the correct behaviour fail under them by design.
Nothing in the workspace enables any of them.

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
