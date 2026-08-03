# ADR-110: A `Pure` boxing wrapper is a load, not a call

**Date:** 2026-08-03
**Status:** Accepted
**Milestone:** post-M11 performance (handover 23, item P-1 — the part of it that
does not depend on Q-1)
**Amends:** nothing. ADR-040 Decision 4 made these wrappers pure; this stops
calling them.

## Context

Handover 21 §3.4 inlined three of four rows of its table; the fourth —
`Inst::Alloc` / `Inst::Materialize` — was deferred and handover 23 carries it as
P-1, described as an inline bitmap claim against the page allocator.

Mapping P-1 against the current tree found the starting point had moved twice
since that description was written, and one of the moves makes a piece of it
free.

**`praxis_alloc_bool` and `praxis_alloc_unit` do not allocate, and have not since
ADR-040.** Decision 4 of that record stopped twenty-four wrappers minting a fresh
immortal per call — a `Bool` per comparison, per `contains`, per `is_empty` —
because an immortal is invisible to sweep and to `Heap`'s `Drop`, so a
per-call mint is storage no collection can reclaim (RT-03). They read
`ctx.true_ref` / `ctx.false_ref` / `ctx.unit_ref` instead. Their manifest rows
have said `Effect::Pure` ever since, which is ADR-040's own Consequence: *"The
`Effect::Pure` rows for the predicate wrappers became honest: they now really do
allocate nothing, so their call sites really are not safepoints."*

What did not follow is that the backend stopped emitting calls to them. Every
`AllocKind::Bool`, every `Materialize { Bool }` and every `AllocKind::Unit` was
still a `bl`, an `abi_guard!` `catch_unwind` landing pad, and a return — in front
of a load of a field the caller already has a pointer to.

## Decision: box a `Bool` with a `select` and a `Unit` with a load

`AllocKind::Unit` lowers to `load_unit_sentinel`, the same load every fault
epilogue in the backend already emits.

`AllocKind::Bool` and `Materialize { Bool }` lower to `emit_inline_bool`: two
loads and a `select`.

**No branch and no cold block**, which is where this differs from ADR-102's
`emit_scalar_load` and from the interned-`Int` probe that P-1a still owes. Those
have a proof that can fail, so they need a slow path. There are exactly two
`Bool`s, both always present in the context, so there is nothing to prove — and a
branchless `select` over two unconditional loads is cheaper than the `brif` a
two-block form would emit.

The test is `!= 0`, not `== 1`, which is the same test `praxis_alloc_bool`
applies and deliberately not the tighter one: a byte that is neither is an
invalid `bool`, and `ScalarLoad::BoolByte` already records why the runtime never
materializes one from an untrusted byte.

### The safepoint spill above these arms stays

`Inst::Alloc` and `Inst::Materialize` are unconditional safepoints in MIR, so
`spill.spill_roots` runs before the match that this decision changes. It would be
sound to skip it for a row the manifest calls `Pure` — that is what
`Effect::Pure` *means* — but that is a MIR-level property about which
instructions are safepoints, not a backend arm's to narrow. Narrowing it here
would put the manifest's answer and the backend's answer in two places, which is
the failure mode MIR-10 exists to prevent.

So this change removes the call and not the spill. Removing the spill is a
separate change, and it belongs in `praxis-mir`.

## Consequences

- **Three call sites in the language become loads**, and with them three
  `catch_unwind` landing pads per evaluation. `Bool` boxing in particular is on
  every comparison a program stores or passes.
- **Q-1 does not touch this.** Only `RuntimeContext` offsets are baked, and
  `true_ref` / `false_ref` / `unit_ref` are fields ADR-103 and ADR-109 leave
  where they are. That is what made this the half of P-1 that could land before
  the page-segregation question was answered — and it stays true now that
  ADR-109 has answered it.
- **P-1 is not finished.** The interned-`Int` probe and the bitmap claim both
  remain, and both need something this change did not: ADR-040's `Safepoint`
  token has to acquire a spelling in generated code. The argument is available —
  the inline fast path is exactly the branch on which `maybe_collect` returns
  `false`, so it forges no token, because the token is permission to *collect* —
  but it wants a decision record and a mechanism that checks it rather than a
  comment. Handover 24 §3 carries the shape.
- **No ABI change.** The three wrappers still exist, still have their manifest
  rows and their address arms, and are still what a debugger-compiled generation
  or a future non-inlining path would call. ADR-080's source-scanning property
  over the set of entry points is untouched.
