# ADR-018: Monomorphization deferred — M4 is monomorphic

**Date:** 2026-07-23 · **Status:** accepted

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
