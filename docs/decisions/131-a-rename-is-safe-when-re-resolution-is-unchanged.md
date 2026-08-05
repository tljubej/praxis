# ADR-131: A rename is safe when re-resolution is unchanged

**Date:** 2026-08-05
**Status:** accepted
**Milestone:** 12

## Context

§19.12's first acceptance criterion: *"Rename updates all valid references and
**rejects unsafe collisions**."*

The first half is a lookup — a symbol's declaration plus every reference that
resolved to it, which `Analysis` already holds. The second half is the one that
needs a decision, because "unsafe" is not one thing:

- the new name is already declared in the same scope;
- a reference to the renamed binding would start resolving to an **outer**
  binding of the new name (`var n` renamed to `out`);
- a reference to some **other** binding of the new name would start resolving to
  the renamed one, because the rename put a shadowing declaration in its way;
- the new name is a keyword, or not an identifier at all.

An implementation that enumerates those cases is a list somebody has to be sure
they finished. The tempting home for such a list — the scope tree — cannot even
host it: `ScopeTree` is keyed by `ScopeId`, not by span, so "which scope is this
offset in" is not a question it answers (M11 handover §5.2). Any check built on
it would be an approximation whose failures are silent: a rename that quietly
changes what a program means.

## Decision

**Apply the edit to a copy, analyze it, and require name resolution to come out
the same.**

A rename is accepted when both hold of the edited text:

1. **The resolution sequence is identical.** A rename adds, removes and reorders
   no tokens, so the file's references and declarations come out in the same
   source order with the same `SymbolId`s — *unless* something was captured. An
   entry whose symbol changed is a name that now means something else; one that
   appears or disappears is a name that started or stopped resolving.
2. **No diagnostic code's count went up.** A collision resolution alone cannot
   see — `fn g` renamed to `f` beside an existing `fn f` — arrives as a new
   `N004`. Counting per code rather than comparing messages keeps a message that
   merely mentions the new spelling from reading as a new problem.

Spelling is checked first, against the lexer's own keyword table
(`SyntaxKind::from_keyword`) and its own identifier rule
(`praxis_syntax::ident::is_ident`), so a keyword added later is refused here
without anybody remembering to update the language server.

A refusal is returned as a **request error** carrying a sentence — which name
would have been safe, and what the collision was — because a client shows an
error and silently ignores an empty edit.

## Consequences

- The four cases above, and any case nobody thought of, are covered by
  construction: the question asked is the one that matters, and it is asked of
  the resolver rather than re-derived.
- A rename costs **one extra full analysis** — about 4 ms on a puzzle-sized file
  in a debug build. Rename is an operation a person performs by hand and waits
  for; this is the right thing to spend.
- The check is conservative in one direction on purpose: a rename that would
  *fix* an unresolved name (renaming a binding to the typo somebody wrote
  elsewhere) is refused, because that is a capture — the other name starts
  resolving to this binding.
- It is exact only for **one file**, which is the scope of the current query
  layer. Multi-file rename needs a workspace-wide analysis; when that arrives,
  the same rule applies to the set of files rather than to one.
- `prepareRename` refuses a position whose symbol has no declaration site — a
  prelude name is declared in the compiler, and renaming it in one file would
  rename nothing.

## Gates

`crates/praxis-lsp/tests/m12.rs`:
`rename_edits_every_reference_to_the_symbol_and_no_other` (a shadowed binding's
uses are not the shadowing one's), `rename_rejects_the_three_shapes_of_collision`
— which also asserts that the *same* rename to a free name is accepted, so the
refusal is about the collision and not about the declaration kind — and
`rename_refuses_a_spelling_that_is_not_a_name`.
