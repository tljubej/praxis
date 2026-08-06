# ADR-085: `Text + Text` is concatenation, and no other operator is defined for `Text`

**Date:** 2026-07-31
**Status:** accepted
**Supersedes:** nothing. Amends §4.12, which described `+` as numeric only.
**Amended by:** [ADR-143](./143-the-to-text-family-is-int-float-and-char.md)
(2026-08-06) — `Int.to_text()` and `Char.to_text()` now exist. Decision 2 below
is unaffected and still stands; the three passages that describe the missing
conversion carry dated notes.
**Amended by:** [ADR-147](./147-a-hole-renders-anything-because-the-program-wrote-the-hole.md)
(2026-08-06) — §8.1's interpolation is implemented, and a hole renders **any**
value. Decision 2 below is still unaffected and still stands: `"n = " + n` is
still `Y001`, and ADR-147 decision 3 is where the two are reconciled rather than
traded off.

## Context

There was no way to build a `Text` out of two `Text`s.

`+` was Int/Float only (§4.12), so `let c = a + b` over two `Text` bindings was a
type error. `Text`'s whole catalog was four rows — `len()`, `is_empty()`,
`get(i)` and the `t[i]` subscript — with no `concat`, no `join` and no `repeat`,
and only `Float` had a `to_text()` (both of the first two have since landed; see
the amendment note above). §8.1's interpolation (`out("Part 2: {part2}")`)
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

> **2026-08-06 (ADR-143).** `Int.to_text()` and `Char.to_text()` exist now, so
> the explicit conversion this decision asked for is writable: `"count: " +
> (3).to_text()`. **Decision 2 itself is unchanged** and is what the new rows
> are shaped by — a program says that it renders, at the point it renders. What
> the two rows deliberately do *not* add is a universal `T.to_text()`, because
> a rendering conversion defined on every type is this decision's refusal
> arriving through a method name instead of an operator.
>
> **2026-08-06 (ADR-147).** §8.1's interpolation is implemented, and `"{v}"`
> renders a value of *any* type. **Decision 2 is still unchanged**, and this is
> the amendment most likely to be misread as reversing it, so the reconciliation
> is stated here as well as there: a hole is a rendering site **the program
> wrote**, and the objection above is about `+` specifically. "A language where
> `+` stringifies its other operand has no error left to report" is true, and
> `"n = {n}"` leaves that error exactly where it was — `"n = " + n` is `Y001`,
> pinned by
> `infer_tests::text_plus_an_int_is_still_y001_beside_a_hole_that_renders_it`.
> "`1 + 2` inside a longer expression starts depending on what its neighbours
> are" is the second half, and a hole has no neighbours to depend on: `{v}`
> exists for no purpose but to render `v`. The residual asymmetry — `"{n}"`
> renders an `Int` and `"" + n` does not — is accepted deliberately, and is the
> distinction between a site that names a rendering and an operator that does
> not.

**3. `-`, `*`, `/` and `%` are not defined for `Text`.** They report `Y016`
(`operator_not_defined`), which is the diagnostic TY-27 added for `%` on `Float`
— the same shape and the same code: both operands agree, and the operation still
has no meaning. `"ab" * 3` is a plausible spelling for repetition in other
languages; it is not one here, and refusing it now keeps the spelling free.

**4. The operator is the whole feature. No `concat` method.** Two spellings for
one operation is what ADR-077 refused for accessors, and the catalog row would
have to answer why `a.concat(b)` and `a + b` both exist. A `Text.repeat(n)`, a
`join`, and `Int.to_text()` are separate rows with separate cases to make.

> **2026-08-06.** Two of those three cases have since been made:
> [ADR-143](./143-the-to-text-family-is-int-float-and-char.md) for
> `Int.to_text()` and
> [ADR-144](./144-a-sequence-of-text-joins-and-a-sequence-of-char-becomes-one.md)
> for `join`. `Text.repeat(n)` is still absent. Note that `join` is the answer
> to the quadratic-accumulation cost this decision's consequences record: it
> walks the sequence once and allocates once, where `+=` in a loop does neither.

## Consequences

- One new runtime wrapper, `praxis_text_concat`, declared `Allocates` — it
  allocates a `Text` and cannot fault, which is `praxis_float_to_text`'s row
  exactly. Concatenating two valid UTF-8 payloads yields valid UTF-8, so there is
  nothing for an `InvalidText` fault to check. (The contrast drawn here was with
  `praxis_alloc_text`, which validated because it was handed raw bytes.
  ADR-111 removed it: that wrapper's bytes are its caller's promise too, and the
  one caller holding raw *host* bytes — `praxis_get_input` — validates them
  itself. Both rows are `Allocates` now, for one reason instead of two.) No
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
