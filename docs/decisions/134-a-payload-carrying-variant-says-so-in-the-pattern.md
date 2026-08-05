# ADR-134: A payload-carrying variant says so in the pattern

**Date:** 2026-08-05
**Status:** accepted
**Milestone:** 12

## Context

HIR-06's padding rule made three spellings one test:

```praxis
enum Move { Step(Int, Int), Stay }

out(match m { Step(dx, dy) => dx * 10 + dy, Stay => 0 })
out(match m { Step(_)      => 1,            Stay => 0 })
out(match m { Step         => 1,            Stay => 0 })
```

The rule has a real reason. The usefulness matrix pairs each pattern column with
a type, so a row narrower than the payload pairs them off by one; padding to the
variant's arity is what makes the matrix well-formed. That reason applies to the
*shape the checker sees*, not to the spelling the author is allowed to write.

The bare spelling was reported by a user against this program:

```praxis
enum Bla { A(Int), B, C }
var bla = A(3)
match bla { A => {} B => {} C => {} }
```

Three arms, three variants, exhaustive, clean. The `A` arm says nothing about
the `Int` that `A` holds, and it reads exactly like `B` and `C`, which hold
nothing — the reader has to go and find the declaration to learn that these
three lines are not the same kind of line.

## Decision

**A bare variant name is a pattern only for a variant that carries no payload.**
A bare name for a variant that carries one is `Y124`, with a machine-applicable
fix that writes one `_` per slot.

Naming *fewer sub-patterns inside parentheses* stays legal and stays padded:
`Step(a)` on a two-slot variant is accepted, because the parentheses are where
the author said what they were doing. `Y124`'s two halves are therefore

- more sub-patterns than the variant holds — `Wrap(a, b)` on one slot, and
- **zero** sub-patterns, with no parentheses, for a variant that holds some.

and `Y124`'s name in the registry changed from `TooManySubPatterns` to
`PayloadArityMismatch` to stop claiming it is only the first.

**The padding is unchanged.** The pattern is still built at the variant's arity
after the report, so coverage still sees a well-formed row and the program still
gets its `Y120` about what the arm leaves out. One mistake, one new diagnostic.

A bare name that is **not** a variant of the scrutinee's enum is untouched: it is
a binding, which is HIR-07's rule and a different question.

## Consequences

**What is bought.** An arm's shape now tells the reader what the variant holds.
`A(_)` is one character longer than `A` and says the thing the reader otherwise
has to look up.

**What it costs.** It is a source-compatibility break for the bare spelling, and
the book documented it: `docs/book/examples/records-enums/enum-payloads.px`
carried `match m { Step => 1, Stay => 0 }` with a comment naming the rule, and
`language/records-enums.md` said it in prose. Both are rewritten. One JIT test
(`a_padded_payload_wildcard_selects_its_arm_at_runtime`) used the bare spelling
to reach a padded payload slot and now uses `Val(_)`, which is the same pattern
and the same padding, said out loud.

**The gate.**
`infer_tests::a_bare_name_for_a_variant_that_carries_a_payload_is_reported` — at
one slot, at two, nested inside another pattern, and in a `for` header — plus the
assertion that the fix writes a wildcard per slot, that a payload-less variant is
untouched, and that a non-variant bare name is still a binding.
