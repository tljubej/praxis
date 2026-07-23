# ADR-005: Formatter skeleton lives in `praxis-parser`

**Date:** 2026-07-23 · **Status:** accepted

## Context

Milestone 1 (§19) requires "a basic formatter skeleton," and the formatter
acceptance criterion is idempotency on the milestone syntax. The design's
crate layout (§14) does not list a dedicated `praxis-fmt` crate.

## Decision

Implement the formatter skeleton inside `praxis-parser` (module `fmt`). It
reads the lossless syntax tree (ADR-003) and emits formatted source; it never
re-lexes raw text.

## Reason

- §14 lists no separate formatter crate, so adding one now would be premature.
- The formatter is driven entirely by the lossless tree, which is produced in
  `praxis-parser`; co-locating it avoids a new crate and a new dependency edge
  for a small module.
- The skeleton is small in M1 (deterministic spacing/indentation for the parsed
  subset, byte-for-byte preservation of backtick-template contents per §15.2).

## Consequences

- If the formatter grows substantially in later milestones, it can be split
  into its own `praxis-fmt` crate depending on `praxis-ast`/`praxis-syntax`
  without disturbing the rest of the DAG.
- Until then, `praxis-parser` exposes both `parse` and `format_syntax`, and
  the LSP formatting request (§15.2) will call into it.
