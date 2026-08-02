# ADR-097: The shared query layer lives in `praxis-lsp`, and `praxis check` routes through it

**Date:** 2026-08-01
**Status:** accepted
**Milestone:** 11 (language server MVP)

## Context

§14.2 is one sentence with teeth: "The CLI and LSP must share the same front-end
query API." §14.1's table already assigns `praxis-lsp` the responsibility "LSP
transport **and compiler queries**" — both halves, in one crate.

Today they do not share. `crates/praxis-cli/src/check.rs` has its own
parse → analyze → concatenate → sort-by-span sequence, written for M1 and
extended in M2. Nothing implements the §14.2 queries at all.

The risk this creates is not hypothetical and not about performance: it is that
`praxis check` and the editor can come to disagree about what a file says.
A diagnostic filtered in one path and not the other, a different sort, a
different decision about whether to analyze a tree with parse errors — each is a
one-line divergence that no test would catch, because the two paths have no test
in common.

## Decision

**No new crate.** The query layer is `praxis-lsp::query`, and
`praxis check` calls it.

- `praxis-lsp::query::Snapshot` owns `(uri, revision, text)` and memoizes
  `parse` and `analyze` for that revision.
- The public surface uses §14.2's names: `source_text`, `parse`, `analyze`,
  `type_of`, `resolve_name`, `input_parser_at`, `completion_context`.
- `check.rs`'s private pipeline is **deleted**, not left beside the new one. It
  becomes: build a snapshot, ask it for diagnostics, render them.
- `praxis-cli` gains a dependency on `praxis-lsp`. It already depends on
  `praxis-hir`, `praxis-mir`, `praxis-codegen-cranelift` and `praxis-runtime`;
  one more front-end crate is not a new kind of coupling.

**The point of it** is that a divergence between what `praxis check` prints and
what the editor underlines becomes *unrepresentable* rather than merely
unlikely. This is handover 17's "state a rule where it is enforced", applied to
a pipeline: the sort order, the "analyze even when parsing reported" decision,
and the diagnostic set are stated once, in the query, and both consumers read
them from there.

## Alternative considered: a separate `praxis-analysis` crate

Rejected. It deviates from §14.1's table to solve one problem — the CLI carrying
`praxis-lsp`'s transport dependency (`lsp-server`, `lsp-types`, `serde_json`) —
that costs nothing at this size, and it adds a crate the design document does
not name.

The mitigation is structural instead: **the transport lives in its own module**
(`praxis-lsp::server`) and the query layer depends on nothing in it. If the
dependency ever does matter, extracting `query` into its own crate is a file
move plus a manifest, not a rewrite. ADR-095's "never hand a `SyntaxNode` across
a public boundary" is the other half of keeping that true.

## Consequences

- `just ci` builds `lsp-server`/`lsp-types` for `praxis check`. Measured cost is
  a one-time dependency build; the crates are small and have no build scripts.
- WS2's characterization gate: the whole existing `crates/praxis-cli/tests/check.rs`
  suite must pass **unchanged** after the rewire. It is listed as a
  characterization gate and is deliberately *not* counted as one of the
  milestone's red-observed gates — it is proof that nothing moved, not proof
  that something new works.
- WS3's manifest gate (praxis-lsp must not reach the JIT) is what keeps this
  dependency edge one-directional in practice as well as on paper.
