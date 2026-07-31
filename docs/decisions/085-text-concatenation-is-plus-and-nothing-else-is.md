# ADR-085: `Text + Text` is concatenation, and no other operator is defined for `Text`

**Date:** 2026-07-31
**Status:** accepted
**Supersedes:** nothing. Amends §4.12, which described `+` as numeric only.

## Context

There was no way to build a `Text` out of two `Text`s.

`+` was Int/Float only (§4.12), so `let c = a + b` over two `Text` bindings was a
type error. `Text`'s whole catalog was four rows — `len()`, `is_empty()`,
`get(i)` and the `t[i]` subscript — with no `concat`, no `join` and no `repeat`,
and only `Float` had a `to_text()`. §8.1's interpolation (`out("Part 2: {part2}")`)
is specified and unimplemented: the braces print literally, and §13.3's "string
interpolation becomes formatting nodes" has not been built. So the language could
*take apart* a `Text` and could not put one together, which for a puzzle language
whose input is text is a gap rather than a deferral.

The prompting report was a diagnostic, not the gap: `"asdasd" + "sddddd"` said
`expected Text, found Int` — the operand named as the requirement (REP-61) and
the caret under the space before it (REP-63). Those are their own rows. This one
is the question they exposed: what *should* `+` do there?

## Decision

**1. `Text + Text` is `Text`, and it is a new immutable value.**

```praxis
let a = "asd"
let b = "qwe"
let c = a + b        // "asdqwe"
```

`Text` is immutable (§4.3), so concatenation allocates; neither operand is
touched. The result is an owned `Text`, not a slice of anything, because it has
no single owner to point into.

**2. A `Text` operand makes the operation `Text`, exactly as a `Float` operand
makes it `Float`.** That is §4.12's existing rule with a third type in it, not a
new rule:

> a Float operand makes the operation Float, an Int operand makes it Int

So `"a" + 1` is a type error and not a coercion — the same answer §4.12 already
gives `1 + 2.5`. **There is no implicit conversion to `Text`**, and this is the
half most likely to be asked for later: a language where `+` stringifies its
other operand has no error left to report, and `1 + 2` inside a longer
expression starts depending on what its neighbours are. The conversion is
explicit, and `Int.to_text()` does not exist yet — the honest statement of that
is REP-64, not a coercion that hides it.

**3. `-`, `*`, `/` and `%` are not defined for `Text`.** They report `Y016`
(`operator_not_defined`), which is the diagnostic TY-27 added for `%` on `Float`
— the same shape and the same code: both operands agree, and the operation still
has no meaning. `"ab" * 3` is a plausible spelling for repetition in other
languages; it is not one here, and refusing it now keeps the spelling free.

**4. The operator is the whole feature. No `concat` method.** Two spellings for
one operation is what ADR-077 refused for accessors, and the catalog row would
have to answer why `a.concat(b)` and `a + b` both exist. A `Text.repeat(n)`, a
`join`, and `Int.to_text()` are separate rows with separate cases to make.

## Consequences

- One new runtime wrapper, `praxis_text_concat`, declared `Allocates` — it
  allocates a `Text` and cannot fault, which is `praxis_float_to_text`'s row
  exactly. Concatenating two valid UTF-8 payloads yields valid UTF-8, so there is
  nothing for the `InvalidText` fault `praxis_alloc_text` carries to check; that
  wrapper validates because it is handed raw bytes and this one is not. No
  `#[repr(C)]` type changed, so no ABI version bump (H17).
- `+=` on a `var` of type `Text` follows without a second decision: the compound
  assignment path types its right-hand side against the binding, so
  `var s = ""; s += "x"` is the same operator.
- **Inference still defaults an unconstrained `+` to `Int`.** `fn f(a, b) { a + b }`
  with no other use infers `Int`, because the target type is chosen from the
  operands' *known* types and two type variables are not `Text`. This is exactly
  what `Float` already does and it is not new here — but it does mean a generic
  "concatenate these" helper needs an annotation. Recorded rather than fixed: the
  principled version is a `Numeric`/`Concat` capability (§5.4) resolved through
  the constraint channel, which is a wider change than this row.
- The five barrier combinators and the parser DSL are untouched: this adds an
  operator, not a `Text` API.
