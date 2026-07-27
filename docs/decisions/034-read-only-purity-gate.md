# ADR-034: Read-only purity gate for debugger expressions (closes §9.5, §19.10)

**Date:** 2026-07-27
**Status:** Accepted
**Milestone:** M10b-WS4 (Read-only `p EXPR` / `type EXPR` evaluator, §9.5 / §19.10)
**Builds on:** ADR-020 (method-call dispatch through the catalog),
ADR-032 (debugger expressions on the main heap)

## Context

§9.5 mandates that `p EXPR` "JIT-compile a synthetic read-only function" and
that "mutating expressions are rejected in the initial debugger. This prevents
changes to a state that cannot safely resume." The §19.10 acceptance criterion
makes this the fifth and final gate: "No command can mutate or resume a faulted
state in v1."

The challenge: Praxis has no existing purity analysis. Method calls dispatch
through the built-in catalog (ADR-020), which *tags* each entry `Pure` or
`Impure`, but that tag was not carried into the typed tree — so a tree-walk
had no way to distinguish `v.len()` (pure) from `v.push(x)` (impure). User
function calls, `read`/`parse`, closures, and diverging control flow are
further mutation/divergence hazards.

## Decision

1. **Thread the catalog's purity tag into `TypedExpr::MethodCall`.** The variant
   gains `purity: praxis_stdlib::Purity`, populated in `lower_method_call` from
   `entry.purity` (the catalog already tags every builtin). This is the single
   source of truth — no re-resolution at gate time.

2. **Reject by structural walk over the typed expression.** A new
   `praxis_debugger::purity::assert_read_only` walks the `TypedExpr` tree and
   rejects:
   - any `MethodCall` whose `purity == Impure`;
   - any user `Call` (cannot prove purity without a separate analysis);
   - `Read` / `Parse` (consume input + can fault on the cursor);
   - `Closure` literals (may capture and mutate);
   - the diverging nodes (`While` / `For` / `Loop` / `Break` / `Continue` /
     `Return` — they don't yield a value in this context);
   - statement-level `Assign` / `Var` (mutation) inside blocks.

   The accepted fragment is the pure, terminating, read-only subset: `Lit`,
   `Path`, `Bin`, `Unary`, `Paren`, `If`, `Block`, `Tuple`, `FieldGet`,
   `Match`, `RecordLit`, `EnumVariant`, and `MethodCall` only when `Pure`
   (`v.len()`, `v.get(i)`, `text.len()`, …).

3. **Assignment is statement-only** (it cannot appear inside an expression in
   Praxis), so it never reaches the expression walk — a free win that needs no
   gate.

4. **The gate runs between typing and JIT**, so a rejected expression is
   reported before any code is generated or executed.

## Consequences

- The accepted fragment is deliberately conservative. General purity analysis
  (proving a user function pure) is deferred; the safe default is to reject.
- `type EXPR` applies the same gate for consistency — the type of an impure
  expression is still a mutating expression.
- Adding a new pure builtin is one catalog tag (`Pure`) and needs no gate
  change.
- The `purity` field on `TypedExpr::MethodCall` is generally useful (future
  effect systems, optimization) — not debugger-specific.
