# ADR-031: Fixed-width diagram helper deferred

**Date:** 2026-07-27
**Status:** Deferred (with rationale)
**Milestone:** M9 (Input parser v2, §19.9)

## Context

The M9 deliverable list includes "Fixed-width diagram helper *if corpus evidence
still justifies it*" — explicitly conditional. During M9 we evaluated the AoC
corpus examples (Appendix C) and the implemented constructor set against the
kinds of inputs a fixed-width helper would address.

## Decision

**Defer the fixed-width helper.** No fixture in the M9 acceptance set (C.5–C.9)
or the broader corpus requires column-anchored fixed-width parsing that isn't
already expressible with the constructors M9 delivered:

- Regular character grids → `grid(char)`.
- Whitespace-tokenized grids → `matrix(int)`.
- Ragged grids → `grid(P, ragged, fill:)` (runtime complete; `fill:` value
  grammar is a small follow-up).
- Column-aligned numeric records → `lines(`{a:int} {b:int}`)` with the flexible
  ordinary-space rule (§7.2), which already absorbs variable column spacing.

A dedicated fixed-width helper would add a constructor (`fixed_width` or a
diagram literal) whose entire purpose — rigid column positions — is already
covered by the template literal's exact-space escape `\x20` (§7.2) for the rare
case where flexible spacing is wrong.

## Consequences

- No new constructor in M9. The §19.9 deliverable's conditional ("if corpus
  evidence still justifies it") is satisfied: the evidence does not justify it.
- Revisit if a future corpus fixture needs rigid column anchoring that
  `\x20`-spaced templates cannot express cleanly (e.g. multi-byte column
  alignment, or a diagram literal that would be materially more readable than a
  template). Until then, templates + `\x20` suffice.
