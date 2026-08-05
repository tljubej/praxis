# ADR-132: A code action is a diagnostic's machine-applicable suggestion

**Date:** 2026-08-05
**Status:** accepted
**Milestone:** 12

## Context

§19.12 asks for *"code actions for common mistakes"*, and its third acceptance
criterion names two: a misspelled parser constructor, and a missing match arm.
§15.3 writes the first one out in full:

```text
unknown parser constructor `line`
did you mean `lines`?
```

The obvious implementation is a table of fixes in the language server, keyed by
diagnostic code. It is also the wrong one, and for a reason this repository has
met before: the language server would then hold a second opinion about what
`lines` is, which constructors exist, and which variants a match is missing —
opinions no `praxis check` run exercises, and which drift the moment a table
changes.

Meanwhile `praxis_source::Suggestion` has carried an optional `replacement`
since M0, `Diagnostic::suggestions()` has always been part of the type, and
M11's `diagnostics.rs` already noted that the replacements "ride along
untouched. They are M12's code actions."

## Decision

**A quick fix is a diagnostic's machine-applicable suggestion, and it is written
where the mistake is detected.**

- `praxis-lsp/src/code_action.rs` turns a `Suggestion` with a `replacement` into
  a `CodeAction` with a `WorkspaceEdit`. It contains no knowledge of any
  particular diagnostic. A suggestion with **no** replacement stays advice: it
  is already in the message as a `help:` line, and an action that changes
  nothing is a menu entry that does nothing.
- The fixes are written by the pass that finds the mistake:
  - **`I013`/`I010`/`I012`** — an unknown constructor, atomic or capture parser —
    in `praxis-hir::parser_lower`, against
    `praxis_input_parser::nearest_parser_name`, which searches §7.4's and §7.5's
    two closed tables as one list because they are one thing to a user: the word
    after `read` or inside `{…}`.
  - **`N001`** — an unknown name — in the resolver, against the scope chain it
    is holding at the moment the lookup fails.
  - **`Y110`** — an unknown method — in inference, against the catalog rows
    *dispatch would have searched* (`pattern_matches` on the receiver), so the
    offered call is one that would resolve.
  - **`Y120`** — a non-exhaustive match — in `exhaustive::check`, from the same
    witnesses the message names (ADR-130).
- **One threshold decides when a near miss is near enough**, in
  `praxis_source::suggest`: an edit distance within `max(1, len / 3)`, counted in
  characters. Four passes needed the same judgement and none of them gets to
  make its own — a suggestion that fires too eagerly teaches users to stop
  reading the quick-fix list.

Two smaller decisions fall out and are worth naming:

- **`I013` now points at the constructor's name**, not at the whole call. The
  report is about the name, and a fix replaces what the report underlines.
- The action's title is the suggestion's own label, capitalized. Rewriting it in
  the language server would be a second wording of the same advice, free to
  disagree with what `praxis check` prints under `help:`.

Actions are computed from the server's **own** diagnostics at the current
revision, not from the `context.diagnostics` a client echoes back: those are
from whatever version the client last received, and an edit computed against
text that has since changed is applied to the wrong bytes.

## Consequences

- Every fix the editor offers is one `praxis check` also prints, and a test can
  apply it and re-analyze. That test exists for each family, and it is the one
  that catches a replacement that does not compile.
- A new diagnostic gets a code action by attaching a suggestion where it is
  built. No language-server change, no table to update.
- The fix and the message cannot name different things, because they are built
  from the same values at the same place.

## Gates

`crates/praxis-lsp/tests/m12.rs` — four families, each applying the offered edit
and requiring the result to analyze clean, plus
`actions_are_scoped_to_the_requested_range`.
`crates/praxis-source/src/suggest.rs`'s own tests pin the threshold, including
§15.3's `line` → `lines`.
`crates/praxis-cli/tests/lsp.rs::a_code_action_answers_with_an_applicable_edit`
drives it over the wire against the real binary.
