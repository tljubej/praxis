# ADR-036: `p EXPR` evaluates a synthesized `__p_expr` function

**Date:** 2026-07-27
**Status:** Accepted
**Milestone:** M10b-WS4 (Read-only `p EXPR` / `type EXPR` evaluator, §9.5)
**Builds on:** ADR-032 (debugger expressions on the main heap),
ADR-034 (read-only purity gate), ADR-035 (static type id threading)

## Context

§9.5's `p EXPR` pipeline is: parse → resolve against snapshot locals →
type-check with captured local types → JIT a synthetic read-only function →
execute against snapshot slots → format. The hard parts are (a) name
resolution against the snapshot's locals (which are *values*, not source
bindings), and (b) the calling convention for executing against those values
(the original native stack is gone).

## Decision

1. **Synthesize a one-function source module.** For the selected frame's named
   locals, emit `fn __p_expr(<typed params>) { EXPR }`, where each parameter's
   annotation is the local's rendered type (`xs: Vec[Int]`, `n: Int`, …). Run
   the standard pipeline (parse → analyze → lower → mono → MIR) on it. Name
   resolution + typing then fall out for free: `EXPR`'s identifiers resolve to
   the synthesized params, and the captured local types are the param
   annotations. No bespoke resolver is needed.

2. **The parameter types come from the runtime descriptors**
   (`descriptor_to_type`, ADR-035), not the static `type_id` — the inference
   gap leaves collection element types unresolved, but the runtime descriptor
   carries the real shape.

3. **JIT into a fresh `Jit` per `p EXPR` call.** Cranelift's `JITModule` does
   not support ad-hoc late compilation into an existing module cleanly, and
   multiple `Jit`s coexist (the jit.rs suite creates one per test). Each `p` /
   `heap` call builds its own short-lived `Jit`, dropped when the call returns.
   The returned `GcRef` lives on the *main* heap (ADR-032), not the JIT's, so
   it outlives the `Jit`'s drop.

4. **Call with snapshot locals as ABI arguments, rooted for the call.** The
   uniform calling convention is `fn(*mut RuntimeContext, GcRef…) -> GcRef`
   (one `GcRef` per MIR param). The evaluator dispatches on arity (0–6;
   extendable) and transmutes the entry pointer to the matching fixed-arity
   function-pointer type. A `RootScope` chained off the snapshot roots every
   argument for the duration of the call (§12.3 "active debugger-expression
   arguments") so a GC inside `__p_expr` cannot collect them. The synthetic
   function's own prologue pushes a shadow frame (the primary root set); the
   scope is the safety net for the args before the prologue spills them.

5. **Clear the stale fault before the call.** The original crash left
   `pending_fault` set; `__p_expr`'s safepoints (`CheckFault`) would otherwise
   bail immediately. `take_fault` clears it (the snapshot is rooted
   separately). If `__p_expr` itself faults (div0, OOB), the evaluator reports
   the fault kind.

## Consequences

- Each `p EXPR` recompiles a one-function module — measurable but acceptable
  for an interactive debugger. A persistent `Jit` that accepts late
  compilation is a follow-up if latency matters.
- The arity dispatch supports up to 6 named locals per frame (extendable in
  `call_with_arity`). Frames with more locals get a clear error.
- `type EXPR` reuses the synthesis + pipeline but stops before JIT, rendering
  the inferred type with the *fresh* `TypeDb` (type ids are positional and do
  not cross dbs).
- The `__p_expr` function is span-less (`(0, 0)`); the `source` command
  degrades for it, which is fine (the user inspects the faulting frame, not
  the evaluator).
