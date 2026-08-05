# ADR-133: Every diagnostic a well-formed program can earn is analysis's

**Date:** 2026-08-05
**Status:** accepted
**Milestone:** 12

## Context

[ADR-097](./097-the-shared-query-layer-lives-in-praxis-lsp.md) made `praxis check` and
the language server run the same query, and
[ADR-130](./130-a-matchs-coverage-is-analysis-answer-and-the-pattern-is-built-once.md)
moved a match's coverage answer into analysis so `Y120`/`Y121` would reach an
editor at all. ADR-130 stated the problem it was closing in one sentence: *"a
file could check clean and fail to run."*

That sentence is still true, for every code ADR-130 did not move.
`praxis_lsp::query::Snapshot::diagnostics` runs parse and analyze and never runs
HIR lowering, so a diagnostic lowering is the sole emitter of is invisible to
`check` **and to the editor**. Handover 30 §1 reproduces four:

| Code | Program | `check` | `run` |
|---|---|---|---|
| `Y013` | `var x = 99999999999999999999999` | exit 0 | error |
| `Y013` | `match n { 99999999999999999999 => … }` | exit 0 | error |
| `Y124` | `match bla { A(i, j) => … }` on `A(Int)` | exit 0 | error |
| `Y125` | `for Point { x: 0, y } in pts { … }` | exit 0 | error |

The last two were reported by a user, who found them in the order that makes the
shape obvious: a program the compiler refuses, in an editor that says nothing is
wrong.

The pattern builder is already extracted (ADR-130) and already runs during
analysis — the coverage pass builds every match arm's pattern with it. **It runs
it with a sink it throws away.** The comment explaining why says inference has
already reported the same mistakes from its own walk, and that is true of `Y122`
and `Y123` and false of the other two: inference never decodes an integer
literal and never counts a payload.

## Decision

**Every diagnostic a well-formed program can earn is decided during analysis.**
Lowering may still hold assertions about its own invariants; it may not be the
only place a user's mistake is named.

Concretely:

- **The pattern builder's sink is kept.** `exhaustive::check_matches` merges it
  into the analysis diagnostics through `pattern::merge_pattern_diagnostics`,
  which drops anything already reported *under the same code at the same caret*.
  Two passes that agree on both have made one report, not two.
- **The other two pattern positions are walked too.**
  `pattern::check_binding_patterns` builds the pattern of every `for` header and
  every destructuring closure parameter, keeps the builder's diagnostics, and
  raises `Y125` when the pattern can fail. Match arms stay with the coverage
  pass, which already builds them.
- **`Y013` is inference's.** `infer_literal` decodes the token and reports the
  range failure; nothing about the range needs the typed tree.
- **The decode is one function.** `praxis_syntax::numeric::parse_int_literal` is
  what all three readers call, so "out of range" cannot come to mean two things.

`check` does **not** lower. That was the other available fix and it is the wrong
one: ADR-097's whole point is that `check` and the editor run the same query,
and a fix that only helps the CLI reintroduces exactly the divergence ADR-097
removed.

`Y099` is deliberately left where it is. It says inference recorded no type for a
node lowering reached — a compiler bug, not a program mistake — and there is no
well-formed program that earns it.

## Consequences

**What is bought.** The four programs above report from `check` and in the
editor, with the same text and the same span `run` gave them. `praxis check`
exiting 0 is now a claim about the program rather than about which passes ran.

**What it costs.** `check_binding_patterns` is a second walk over the file's
descendants, and it builds patterns the lowerer will build again. Both are
proportional to the source and neither allocates per node beyond the pattern
itself; the measured `just ci` time did not move.

**The gate.** `coverage_tests::a_program_run_refuses_is_a_program_check_refuses`
asserts each of the five shapes reports *from `analyze` alone*.
`a_diagnostic_two_passes_agree_on_is_reported_once` is the other half — the merge
must not double `Y122`. And
`every_pattern_position_is_checked_by_analysis` asserts that the three positions
walked here are every position the grammar puts a top-level `PATTERN` in, so a
fourth one added to the parser turns red instead of quietly going back to being
lowering's alone.

**The book's run-only table is deleted** (`tooling/diagnostics.md`), along with
`docs/book/examples/tooling/lowering-only.px`, which existed only to document
this defect.
