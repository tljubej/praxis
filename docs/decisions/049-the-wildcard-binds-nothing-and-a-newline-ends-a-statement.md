# ADR-049: `_` is a wildcard that binds nothing, and a newline ends a statement but never an expression

**Date:** 2026-07-29
**Status:** Accepted — D7 implemented, D8 decided and **not yet implemented**
**Milestone:** Repair (stage S12 — FE-02 landed; FE-04/FE-06 outstanding)
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

**Not yet implemented** — this is F8/FE-04, and it is recorded here so the
stage does not have to re-ask.

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

## Consequences

- **`_` inside a name is untouched.** The split is on the whole run, not the
  first byte: `_x`, `__`, `x_`, `_1` and `snake_case` are all still identifiers.
  `an_underscore_inside_a_name_is_still_an_identifier` pins it.
- **HIR-05 (S16) needs no HIR change.** The plan says FE-02 is the entirety of
  its fix, and a `_` field name in a record literal is now a parse-level
  impossibility rather than a silently-accepted duplicate.
- **DBG-03 is unaffected by this.** `sanitize_name` still rewrites invalid
  debugger names non-injectively; it is S12's fourth finding and independent.
- **The FE-04 trap is still open.** `let x = 1` followed by a line starting with
  `(` still parses as a call — the progress doc records it as the reason
  `tuple_schema_uses_the_unit_descriptor_for_unit_elements` needed rewriting.
  D8's rule as chosen does not close it; the workaround stands until S12
  finishes and someone decides whether to revisit.
