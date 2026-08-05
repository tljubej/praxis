# ADR-130: A match's coverage is analysis's answer, and the pattern shape is built once

**Date:** 2026-08-05
**Status:** accepted
**Milestone:** 12

## Context

§15.2 lists **exhaustiveness errors** among the diagnostics the language server
must publish, and §19.12's third acceptance criterion asks for a code action
that adds missing match arms. Neither was reachable.

`exhaustive::check` — the usefulness matrix, `Y120`/`Y121` — was called from
exactly one place: `Lowerer::lower_match`, in typed-HIR lowering. The language
server does not lower, and neither does `praxis check`: both stop at
`analyze_root`, which is name resolution plus inference. So a non-exhaustive
match was **clean under `praxis check` and an error under `praxis run`** —
REP-12's asymmetry, the one this repository has spent several milestones
removing everywhere else — and the editor was silent about it entirely.

The obstacle was not the checker. It was its input: `check` consumes
`TypedMatchArm`s, and the only thing that could build a `TypedPattern` was
`Lowerer::lower_pattern`, a private method reachable only where MIR was being
built.

Three ways out were available:

1. **A second pattern builder in inference.** ~150 lines of near-duplicate, and
   the two would be free to disagree about the one question `Y120` turns on:
   whether a bare `Name` in a pattern is a variable binding or a payload-less
   variant.
2. **Recompute coverage in the language server**, from the arms' name tokens and
   the scrutinee's enum. A second opinion about exhaustiveness, in the component
   least able to test it.
3. **Extract the builder and move the call.**

## Decision

**Pattern shape is built in one place, and coverage is decided at the end of
analysis.**

- `praxis_hir::pattern::PatternBuilder` is that place. Its context is four
  fields — the file, the type arena, the declarations resolution minted, and a
  diagnostic sink — which is the measurement that made the extraction possible:
  pattern shape depends on the scrutinee's type and on nothing lowering knows.
- `exhaustive::check_matches` walks every `MATCH_EXPR` in the file after
  inference and calls the checker. `analyze` runs it, so `praxis check`, the
  language server and `praxis run` all see the same `Y120`/`Y121`.
- `Lowerer::lower_match` **no longer asks**. Lowering is reached only for a
  program analysis accepted, so a match that reaches it has already been found
  exhaustive.

Two consequences, both stated where they are made:

- **The pass runs after inference, not inside `infer_match`.** A scrutinee's
  type is not final while inference is on the stack: `match e { … }` on an
  unannotated parameter is pinned by a call further down the file. A coverage
  answer given against a type variable is a `Y120` demanding a `_` arm the
  program does not need — a false positive, which is worse than the silence this
  replaces.
- **The builder's diagnostics are discarded in this pass.** Inference has
  already walked these patterns and reported the shape mistakes (`Y122`,
  `Y123`); lowering reports the ones only it can see. A third report of the same
  mistake under the same caret is not new information.

The `Y120` also carries its fix: a machine-applicable suggestion inserting the
arms for the witnesses the message names (ADR-132). The witnesses are computed
once and both the message and the fix read them, so the two cannot name
different sets — including under `MAX_WITNESSES`, which bounds both.

## Consequences

- `praxis check` now reports non-exhaustive matches. **This is a behaviour
  change for existing programs**: a file that checked clean and failed to run
  now fails to check, at the same span with the same message.
- The editor underlines the `match` and offers the arms.
- `check` gained a `MatchToCheck` parameter object rather than a ninth argument.
- The coverage pass costs one extra pattern build per match — patterns only, not
  bodies. At AoC file sizes it is not measurable against the ~4 ms front end.

## Gates

`crates/praxis-hir/src/coverage_tests.rs`. The load-bearing ones are
`it_is_reported_once` (both builders exist; only one reports),
`a_scrutinee_pinned_later_in_the_file_is_not_reported` (why the pass is not
inside `infer_match`), and `applying_the_suggested_arms_makes_the_file_clean`,
which applies the fix and re-analyzes — the only test that catches an arm
rendered in a shape the grammar would refuse.
