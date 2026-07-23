# ADR-008: let-generalization uses Pottier-style binding levels

**Date:** 2026-07-23 · **Status:** accepted

## Context

§5.3 specifies that `let` bindings (and `fn` declarations) may be generalized —
`let id = fn(x){x}` becomes `forall T. (T) -> T` — while `var` bindings are
**never** generalized (a `var` could be reassigned to a differently-shaped
value; generalizing it would be unsound). Plain HM generalization ("quantify
every unbound var not free in the environment") is unsound in the presence of
mutation and partial inference: a var created in an inner scope could be
constrained by a *later* outer binding, and naively generalizing it on scope
exit would let it escape that constraint.

## Decision

Implement let-generalization with **Pottier/Rémy binding levels** (the
"level"-based algorithm). Each type variable records the binding level at which
it was created. On `enter_level` (around a `let`/`fn` body) the counter rises;
on `exit_level` it is restored. Generalization quantifies only unbound vars
whose level is *strictly deeper* than the generalization site's level. The key
correctness rule lives in `unify`: when a younger variable is linked to a type
containing *older* variables, those older variables are **lowered** to the
younger variable's level — so they cannot be generalized out from under the
binding that will constrain them.

## Reason

- Level discipline makes generalization sound with `var` and with partial
  inference without a separate "value restriction" heuristic.
- `var` soundness falls out for free: a `var`'s RHS is inferred but the binding
  is never put through `generalize`, so it stays monomorphic regardless of level.
- The algorithm is well-understood (used in ML compilers, OCaml's inference) and
  localizes the tricky invariant to one place (`lower_levels` in `unify`).

## Consequences

- `TypeDb` carries a `level` counter; `enter_level`/`exit_level`/`scoped_return`
  bracket every `let`/`fn` inference. `exit_level` does **not** lower vars
  blanketly (that would defeat generalization); lowering happens only in `unify`.
- `fn` recursion is handled monomorphically: the name is bound to a placeholder
  var first (visible in the body for self-calls), the body is inferred, the
  placeholder is unified with the derived function type, and the result is
  generalized *after* the body. Recursive functions therefore require
  annotations (criterion 1 scopes to *non-recursive* inference).
- Full polymorphism is exercised (e.g. `out` is `forall T. (T) -> Unit` and
  accepts any argument); monomorphization of inferred polymorphism is deferred
  to M7 (§13.6), since M2 has no codegen to instantiate against.
