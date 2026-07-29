# ADR-050: A record literal is legal wherever the brace cannot be a block

**Date:** 2026-07-29
**Status:** Accepted
**Milestone:** Repair (stage S12 — FE-06)

## Context

`if p { … }` is genuinely ambiguous. `p { … }` is a well-formed record literal,
and `p` followed by the then-block is a well-formed `if`. Four keyword heads
have the problem — `if` and `while`'s conditions, `for`'s iterator, `match`'s
scrutinee — and all four resolved it the same way: set a parser-wide
`no_struct_literal` flag, parse the head expression, restore it.

A parser-wide flag cannot express "until the ambiguity ends", only "until this
call returns", so the suppression leaked into every subexpression of the head:

```praxis
if (Point { x: 1 } == p) { 0 }     // P001 — the parenthesized literal was refused
match x { A => Point { x: 1 } }    // P001 — every arm body was suppressed
```

Neither has anything left to be ambiguous about. In the first, the `{` sits
inside parentheses the parser has already committed to, so it cannot be the
`if`'s block. In the second, the brace the `match` was worried about is the one
opening the arm list, and it was consumed before the first arm was parsed.

## Decision

Suppression is a parameter, `StructLit { Allowed, Suppressed }`, threaded down
the expression grammar (`parse_expr_bp` → `parse_prefix` → `parse_atom` →
`parse_name_or_call`, which is the one place that reads it).

**It follows operands, and it stops at brackets.** A suppressed head suppresses
its own operands — `if a == p { 0 }` and `if !p { 0 }` have the same ambiguity
`if p { 0 }` has — but every bracketed context re-enters at `Allowed`: `(…)`, an
argument list, a block, a record body, a match arm. Inside a bracket the
grammar knows what closes it, so no `{` there can be the block a keyword is
waiting for.

**A closure body inherits rather than resets.** `|` is not a bracket the
grammar closes over: after `|x|` the body runs to the end of the expression, so
a closure written directly as an `if` condition is ambiguous exactly as a name
is. In practice every closure that matters is already inside an argument list,
which resets.

## Consequences

- **A match arm may return a record literal at any depth** — directly, in a
  block, in a nested `if`, in a closure. The flag leaked into all of those.
- **The four heads still claim their brace.** `if p { 0 }`, `while p { 0 }`,
  `for q in ps { 0 }` and `match p { A => 1 }` are unchanged, and
  `a_keyword_head_still_claims_its_brace_as_a_block` asserts that the head is
  *not* read as a record literal — the property the flag existed to provide.
- **Ordering mattered.** FE-06 had to follow FE-04 (plan hazard H13): with
  record literals allowed in arm bodies but arm separation still deciding by
  "can this token start a pattern", `match x { A => Point { x: 1 } B => … }`
  had no separator to be missing. It is now a `P002`, which is the same answer
  the rule gives for any two arms run together on one line.
- **`Parser` has no mode state left.** `no_struct_literal` was the only field of
  its kind; the parser's behaviour is now a function of the token stream and the
  grammar position, which is what makes "does this leak?" answerable by reading
  one call chain.
