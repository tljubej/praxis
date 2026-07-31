# ADR-084: A backtick template is a parser expression, so in value position it is a diagnostic

**Date:** 2026-07-31
**Status:** Accepted — implemented
**Milestone:** Repair (register REP-47)

## Context

```praxis
let t = `n = {int}`
out(t)
```

This passed `praxis check` and printed `n = {int}`.

`parse_atom` accepted `BacktickTemplate` in its literal set beside `IntLit`,
`FloatLit` and `TextLit`. Inference typed it `Text` unconditionally — "backtick
templates are M6; treat as Text for now (a fresh var would be sounder but Text
matches the eventual type)" — and lowering built a `Lit::Text` of the raw
interior with the braces still in it. So a capture became characters, and the
program that asked to parse an integer printed the word `{int}`.

§7.1 says the parser-expression sublanguage is entered at `read` or at
`parse(text, …)`, and §7 defines a template only as a parser expression. REP-34
established that same boundary from the other side: a labelled argument
(`skip: whitespace`) is parser-expression grammar, four design-doc fences that
wrote it bare did not parse, and the answer was that the doc was wrong rather
than the grammar. This is the same boundary, and the same answer — except that
here the language did not refuse, it *reinterpreted*.

## Decision: a template outside `read`/`parse` is `Y023`, reported in inference

The token still parses. `parse_atom` builds the same `LITERAL` node it always
did, so the tree round-trips the source and there is one node holding one
mistake. Inference reports `Y023` and gives the expression a **fresh type
variable**.

### Why not remove `BacktickTemplate` from `parse_atom`

That was the other candidate — make the state unrepresentable, which is the
house move. It is rejected for two reasons that are about the *report*, not the
principle. A refusal at `parse_atom` produces `P001: expected an expression` at
the backtick and then a cascade, because the token stays unconsumed and the
statement recovery starts over on it; and a `P0xx` code cannot say the useful
sentence, which is not "this token cannot appear here" but "this token means
something, and you left out the word that gives it somewhere to read from". The
message names the fix: write `read`.

The parser also is not where the language's other kind-mistakes are reported.
REP-26 put a record literal on a non-`struct` head in inference (`N008`), and
ADR-050's reasoning applies unchanged: `praxis check` must see it (REP-12's
asymmetry), and `check` does not run lowering.

### Why a fresh variable and not `Text`

The old comment argued `Text` was "sounder than a fresh var" because it matches
the eventual type. It was the opposite: `Text` is a *plausible* type, so `out(t)`
type-checked, `t + "x"` type-checked, and the program ran. A reported expression
should have no type anyone can compute with, and a fresh variable unifies with
whatever the context wants without ever producing a second diagnostic about a
value that does not exist. One mistake, one error.

Lowering answers `Unit` in the same spirit, and only defensively: a reported
program is not lowered, so reaching that arm is a compiler bug, and `Unit` is the
value hardest to mistake for a program's own.

## Consequences

- `` `n = {int}` `` alone is `error[Y023]`. `read \`n = {int}\`` and
  `parse(text, \`n = {int}\`)` are untouched — the sublanguage is entered by the
  word, so no template's *meaning* changed.
- `Y023` is spent (ADR-051 amended). `Y022` is deliberately skipped: this
  session's plan reserved `Y023` upward, and a gap in a registry costs nothing.
- The type-checker's `Text` claim is gone, so when templates in value position
  eventually mean something — they do not today, and no design section asks for
  it — the decision will be made by a section rather than by a `for now`.
