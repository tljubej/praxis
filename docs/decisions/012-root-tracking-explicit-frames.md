# ADR-012: Explicit root frames for M3

**Date:** 2026-07-23 · **Status:** accepted

## Context

§12.3 offers two root-tracking strategies: "compiler-managed shadow-stack
frames **or** explicit root frames," and notes that "MIR liveness computes the
minimal root set." MIR and the JIT are M4 deliverables — in M3 there is no
generated code, so there are no generated frames to drive a shadow stack. M3
must still be testable from plain Rust, which needs a way to tell the collector
which `GcRef`s are roots.

## Decision

M3 ships **explicit root frames**: a `RootSet` trait (anything that can yield
its roots) and a RAII `RootScope` that holds a `Vec<GcRef>` and chains to an
optional parent `RootSet`. `Heap::collect` takes a `&dyn RootSet`.

## Reason

- It works today, from Rust tests, with no generated code — the shadow-stack
  option would be speculative without the M4 calling convention to anchor it.
- The `RootSet` trait is the seam the M4 shadow-stack plugs into: a generated
  frame will implement `RootSet` alongside the explicit scopes, so M3's choice
  does not constrain M4.
- Spec §12.3 explicitly permits it.

## Consequences

- M3 root sets are **over-conservative** (a scope roots everything pushed into
  it; there is no liveness analysis). This is sound; M4 tightens the root set
  via MIR liveness as the spec prescribes.
- Rooting is manual in M3 tests: a value not held by any `RootScope` (and not
  immortal) is reclaimable. Tests encode this deliberately.
