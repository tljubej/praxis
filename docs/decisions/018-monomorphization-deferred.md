# ADR-018: Monomorphization deferred — M4 is monomorphic

**Date:** 2026-07-23 · **Status:** superseded (monomorphization landed in M7-WS8, 2026-07-25)

## Context

The contract pipeline (§10.1) places monomorphization (§13.6) between typed HIR
and MIR: "Inferred polymorphic functions are instantiated for concrete use
sites." M2's inference *does* infer polymorphic functions (e.g. `fn id(x) { x }`
generalizes to `a -> a`). The M4 acceptance criteria (§19), however, are entirely
monomorphic `Int`-typed programs: boxed arithmetic, branches, loops, recursive
calls, faults.

## Decision

M4 handles **monomorphic code only**. The typed-HIR lowering (ADR-014) rejects
any function whose inferred `Scheme` is polymorphic with a `Y100` diagnostic
("`<name>` is generic; monomorphization is not supported yet (M4 is
monomorphic)"). The CLI's honesty gate surfaces this before any JIT compilation.

A real monomorphization pass (instantiating polymorphic functions at each call
site, caching by `FunctionId + canonical type arguments`, §13.6) is a future
milestone.

## Reason

- The M4 acceptance corpus contains no polymorphic code, so a monomorphization
  pass would be untested by the acceptance criteria — rule 20.2 (vertical slices
  with executable behavior) argues against shipping it unused.
- Rejecting generics *honestly* (a clear diagnostic) is better than silently
  miscompiling (e.g. treating a type variable as `Int`).
- Monomorphization interacts with the eventual record/enum/closure types (M7);
  designing it before those exist risks rework.

## Consequences

- Generic user code fails to run in M4 with `Y100`; this is documented, not a
  silent gap.
- When monomorphization lands, it inserts between `lower` (typed HIR) and
  `lower_module` (MIR), instantiating clones of generic functions. The typed-HIR
  tree is shaped to make this straightforward (each `fn` is self-contained).

## Supersession (M7-WS8, 2026-07-25)

Monomorphization landed in M7 Part 3 (WS8) exactly where this ADR predicted:
between `lower` and `lower_module`, as `praxis-hir/src/mono.rs`. The `Y100` gate
was removed (no test asserted it). The pass:

- Captures each call site's concrete argument types during inference
  (`Analysis.call_sites`, keyed by the callee name token's range), since
  `infer_call` instantiated and unified but previously discarded them.
- Walks the typed tree; for each `Call` to a polymorphic callee, canonicalizes the
  arg types (rendered structural strings — not type ids, so two call sites with
  the same concrete type share one clone even when inference gave them distinct
  arena slots), clones the callee `TypedFn`, specializes it (instantiate scheme,
  unify params with arg types to pin the quantified vars, then resolve every
  `Type` via `db.follow`), mangles the name (e.g. `id__Int`), and rewrites the
  call site to target it. Reaches a fixpoint for transitive instantiation.
- Drops the original generic fns (only clones survive); monomorphic fns pass
  through unchanged.

Generic user code now compiles and runs; this ADR's "deferred" stance is
superseded.
