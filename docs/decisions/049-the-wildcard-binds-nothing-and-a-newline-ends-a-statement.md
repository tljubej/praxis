# ADR-049: `_` is a wildcard that binds nothing, and a newline ends a statement but never an expression

**Date:** 2026-07-29
**Status:** Accepted — D7 and D8 both implemented; amended 2026-07-31 for REP-27
(a line-leading `(` no longer continues the expression before it)
**Milestone:** Repair (stage S12 — FE-02 and FE-04 landed)
**Answers:** the plan's D7 and D8

## Context

`is_ident_start` accepts `_`, and it must: `_x` and `snake_case` are ordinary
identifiers. But the lexer applied that rule to a **lone** `_` as well, so a
wildcard arrived downstream as an `Ident` and every `_` was a *binding named
`_`* — two `_` arms of one match were a duplicate declaration, `_` was readable
as a value in an arm body, and `Point { x: 1, _: 2 }` named a field. Splitting
the token out raises the question the grammar never answered: where is `_`
legal, and what does it mean there (**D7**).

Separately, statements are newline-separated in every `.px` fixture and every
doc example, but the parser discards the newline fact entirely — `eat_trivia`
throws it away — so two statements on one line with no separator are accepted
and `break`/`return` decide whether a value follows by looking at the *next
token's kind*, blind to the line break (**D8**).

## Decision D7: `_` is legal in every binding position and introduces nothing

`let _ = f()`, `fn g(_)`, `|_| 0` and a `_` pattern all parse. None of them
declares a symbol. The alternative — legal only in patterns — was rejected
because it deletes the discard idiom `let _ = sideEffect()`, which two JIT tests
already use.

The mechanism is that a wildcard binder is an **absent name**, not a symbol
called `_`: the AST's name accessors look for an `Ident`, so `LetStmt::name()`
answers `None` and the resolver has nothing to declare. One consequence had to be
written rather than inherited — `lower_let` returned `None` when there was no
name, which dropped the whole statement and with it the initializer's *effects*.
A nameless binding lowers to a statement expression instead: it runs, and keeps
nothing. That is what makes the idiom a discard rather than a deletion.

`_` has **no expression form**. Reading it is a parse error (`P001: expected an
expression`) at the token itself, which is a better report than the
"unresolved name" the old lexing produced and is what
`wildcard_pattern_does_not_bind_a_value_named_underscore` now asserts.

## Decision D8: a newline terminates a statement, never a subexpression

A newline is consulted in exactly two places:

1. **Between statements in a block, and at the top level.** The statement loop
   ends by demanding a separator: `;`, a newline, or the end of the block. Two
   statements adjacent on one line with neither is a diagnostic.
2. **At `break`/`return`'s optional-value decision.** A newline after the
   keyword means "no value", whatever the next token is.

It is consulted **nowhere else** — in particular never inside the Pratt loop. So
`1 +\n2` parses, a trailing `.method()` chain continues across lines, and an
open paren or bracket continues. F8's `StmtSeparator { Semicolon, Newline,
EndOfBlock }` is the shape that makes this checkable: the loop cannot advance
without producing a separator value or emitting a diagnostic, so "two adjacent
statements with no separator" has no accepted representation.

Two alternatives were rejected. Also terminating before a line-leading `(` or
`[` would fix the known FE-04 trap where `let x = 1\n(a, b)` parses as a *call*
of `(a, b)` — but it makes a line-leading parenthesized continuation
unwritable, and the trap has a workaround (bind the tuple to a name). Requiring
explicit `;` everywhere rewrites the entire corpus.

**Amended by REP-27 (2026-07-31):** half of the first alternative is taken after
all. A `(` asked to continue an expression does not cross a line break; a `[`
still does. See the amended consequence below for why the workaround stopped
existing.

## Consequences

- **`_` inside a name is untouched.** The split is on the whole run, not the
  first byte: `_x`, `__`, `x_`, `_1` and `snake_case` are all still identifiers.
  `an_underscore_inside_a_name_is_still_an_identifier` pins it.
- **HIR-05 (S16) needs no HIR change.** The plan says FE-02 is the entirety of
  its fix, and a `_` field name in a record literal is now a parse-level
  impossibility rather than a silently-accepted duplicate.
- **DBG-03 is unaffected by this.** It is S12's fourth finding and independent;
  it landed separately, by rejecting an unusable debugger local name rather than
  rewriting it.
- **The newline fact rides on the token, not on the trivia.** `Token` gained
  `preceded_by_newline`, set for the whole trivia run in front of it, so the
  answer survives the trivia having already been folded into the green tree.
- **Match arms are separated for real now.** The arm loop's "comma-OR-newline
  separated" comment sat above a check that only asked whether a pattern could
  start here; it demands a comma or a line break, and `P002` is the one new
  diagnostic code the stage spends (the parse block, not D13's).
- ~~**The FE-04 trap is still open.** `let x = 1` followed by a line starting with
  `(` still parses as a call — the progress doc records it as the reason
  `tuple_schema_uses_the_unit_descriptor_for_unit_elements` needed rewriting.
  D8's rule as chosen does not close it; the workaround stands (bind the tuple
  to a name) until someone decides whether to revisit.~~ **Closed by REP-27**, and
  this is the revisit that sentence invited. The workaround the rejection rested
  on — bind the tuple to a name — does not exist in the position that made the
  trap reachable: REP-10 gave `match` arms tuple patterns and REP-25 gave `for`
  bindings the same grammar, and a **match arm** cannot be renamed. So a
  newline-separated arm list whose next arm began `(a, b) =>` had that arm read as
  the previous arm's argument list, the arm loop found no pattern start, and every
  arm after the first silently left the tree.

  The rule is narrow and is D8's own: a `(` asked to **continue** an expression
  does not do so across a line break. It is not consulted in the Pratt operator
  loop, so `1 +\n2` and a `.method()` chain across lines are unchanged, and a `(`
  that *opens* an expression is untouched. The three doors that ask are the postfix
  loop, `parse_name_or_call`'s primary call form, and the field-vs-method decision
  after a `.` — `p.x\n(a, b)` was a method call by the same trivia-skipping
  lookahead.

  `[` is deliberately exempt, which is the asymmetry: no statement and no pattern
  begins with `[`, so a line-leading one can only continue the expression before
  it (REP-16). The cost is the one the original rejection named, and it is now
  written down as a test: `f\n(1)` is two expressions.
- **The predicted churn did not arrive.** F8 is annotated "HIGHEST TEST CHURN of
  any foundation: ~40 insta snapshots in `parse.rs` plus every `.px` fixture";
  the whole suite passed unchanged. Every fixture and every snapshot already
  separated its statements with newlines, which is what the rule now requires.
  This is the third F-block to predict wide churn and deliver none.
