# ADR-094: A backtick template ends at the line it opens on

**Date:** 2026-08-01
**Status:** accepted
**Milestone:** Repair (answers open decision **D18**, S19 round 3)

## Context

§7.2 says `\n` *matches* a line ending. It never says whether a **raw** newline
may appear in a template's source form. Nothing needed the answer until a
template was left unclosed — and then it decided everything about the report.

`` read `{int` `` today is `T002 unterminated backtick template` spanning the
rest of the file, plus the `P001`/`Y001` cascade that follows from a block whose
closing brace was eaten inside the token. **The diagnostic is true**: under D10 a
backtick inside a capture opens a nested template, so the source really does
leave one open and the token really does run to EOF. It is truthful and *wide*.

Two rules would bound it. The register recorded both and took neither, because
the first is a language decision that a bug fix must not make by accident, and
the second — error recovery, ending an unclosed run at the first inner backtick —
was tried far enough to measure and made the output worse: a *well-formed*
two-level template reported `T002` twice.

## Decision: a template ends at the line it opens on

A raw newline may not appear inside a backtick template. `\n` is how a template
matches a line ending, and it is now the only way.

1. **This is not a new rule. It is the rule the language already has, applied to
   the one delimiter that escaped it.** The lexer already refuses a raw newline
   inside a `"…"` literal, in those words, and `T004` therefore names one line
   and reports once with no cascade. The backtick template was the only delimited
   literal in Praxis that could silently span a line. ADR-049 made a newline end
   a statement; a token that hides newlines from that rule is an anomaly.

2. **§7.2 never gave a raw newline a meaning, and the accidental one is worse
   than the spelled one.** §7.2 lists literal text, a space run, `\s*`, `\s+`,
   `\n`, `\t`, `\x20` and ordinary escapes. A raw newline is whitespace but is
   not a space, so it matches none of them and falls through to *literal text*.
   Measured: `` `{a:int}X⏎Y{b:int}` `` matches LF input and **fails on CRLF**,
   while the `\n` escape (`WsPolicy::Newline` — `\n` optionally preceded by
   `\r`) matches both. Today's multi-line template is a strictly weaker,
   silently CRLF-hostile shadow of the construct §7.2 specifies. Making it
   illegal removes a trap; the replacement is one character longer and correct.

3. **It costs nothing that exists.** Swept every `.px` in the tree, every
   ```praxis fence in the design document (extracted programmatically), and
   every non-comment line of every `.rs` under `crates/`: **no template anywhere
   spans a raw newline.** The two Rust tests that look like they do write `\\n`,
   the escape. §7.1's multi-line example breaks the *parser expression* across
   lines, not the template; §7.7's `block` puts each backtick on its own line.

4. **It bounds the diagnostic, which is what D18 was registered for.**
   `` read `{int` `` becomes one `T002` spanning `` `{int` `` and nothing else.
   The `}` closing the enclosing block is no longer inside a token, so the
   `P001` and the `Y001` both disappear. Three errors become one.

5. **It makes the rejected recovery unnecessary, as the register predicted.**
   With a newline terminating a template, an unclosed run has a bound and
   recovery has nothing to guess, so the two-`T002` regression that killed that
   candidate never arises.

6. **Leaving it alone was not neutral.** It kept a diagnostic covering the rest
   of the file, plus the cascade, plus a second symptom nobody had registered: an
   unterminated template in value position reports `T002` **and** `Y023` "write
   `read` before it" — advice that does not close the template.

### One code, not two

`T002` keeps the job; no `T006 "a template may not span a line break"`. The lexer
cannot distinguish a deliberate multi-line template from a typo — both are "the
run did not close before the line ended" — so a code claiming to know which would
be wrong for `` read `{int` ``. The rule and the fix go in the `help:` line,
which is what ADR-084 argued a report should do:

```
error[T002]: unterminated backtick template
  f.px:11:14
  11 | let v = read `{int`
     |              ^^^^^^ unterminated backtick template
help: a template ends at the line it opens on; write `\n` inside it to match a line ending
```

### The extent question is asked once, or this reintroduces a closed defect

`parser_lower` decides "is this token terminated" with
`strip_prefix('`').and_then(strip_suffix('`'))` — a third hand-rolled
implementation of the extent question, correct today **only** because an
EOF-swallowing token cannot end in a backtick. Under this rule the common
unterminated token *is* `` `{int` ``, the strip succeeds, and `I030` comes
back — the fabricated-interior class the existing gate
`an_unterminated_template_does_not_also_report_a_fabricated_interior`
explicitly forbids. **That gate goes red against a naive fix, which is the good
news: the protection is already in the tree.**

So the state is made unrepresentable rather than re-derived: the lexer emits
`SyntaxKind::UnterminatedBacktickTemplate` for an open run and `BacktickTemplate`
only for a closed one. A `BacktickTemplate`'s text is then a complete template by
construction, `convert_template` has no question left to ask, and the
unterminated kind can be given a fresh variable and no `Y023` — which fixes the
second symptom for free.

Having `convert_template` call `template_end` a third time is rejected: it fixes
one of the two symptoms, leaves the extent question living in a third consumer,
and the whole reason `praxis-syntax::template` exists is that extent questions
must be asked once.

## Consequences

- §7.2 gains one sentence stating the rule; it is enforced in the lexer and
  referred to from nowhere else.
- A multi-line template becomes a lex error. Nothing in the tree is one.
- `Y023` ("write `read` before it") stops firing on an unterminated template,
  where it was advice that could not help.
- The `T002` span shrinks from "rest of file" to one line, and the `P001`/`Y001`
  cascade it caused disappears with it.
