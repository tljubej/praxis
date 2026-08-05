# ADR-136: A text becomes a number through an `Option`, and a name with no value says so

**Date:** 2026-08-05
**Status:** accepted
**Milestone:** 12

## Context

Two defects from handover 30, filed apart and settled together because both are
about a name that does not denote what its surroundings claim.

**`Text` had no `int`.** The `Y001` help for the single most common mistake in a
puzzle program has read

```text
help: this is `Text`; call `.int()` on it (or use `read lines(int)`)
```

since it was written. `Text`'s catalog rows are `len`, `is_empty` and `get`, plus
the subscript and the pipeline combinators — so `raw.int()` reported `Y110`, and
half of the help sent the reader somewhere that does not exist. ADR-086's own
aside on `Char.to_text()` quotes §4.13's "building a `Text` out of a number is
not yet possible in any spelling", and the traffic in the other direction had no
spelling either.

**A builtin in value position was `Unit`.** `out(pi)` printed `Unit`. `pi` is a
nullary function (`() -> Float`), not a constant, so the missing parentheses are
the whole mistake — and "printed `Unit`" is the least useful way to say so.
Following it further, the defect is not `out`'s: `var h = abs` then `out(h(-3))`
**printed nothing and exited 0**, and `out([pi])` faulted with "value does not
have the declared type". A prelude builtin and a payload-carrying enum
constructor both lowered to the unit value in value position.
[ADR-061](./061-a-fn-name-in-value-position-is-a-closure.md) gave a user `fn` a
real function value — a closure over its adapter — and neither of these has an
adapter to close over.

## Decision

**`Text.int()` and `Text.float()` exist and answer `Option[Int]` and
`Option[Float]`.**

`Option` and not the scalar, for §4.7's reason and `Map.get`'s: a text that is
not a number is *absence*, not a fault. Input arrives as text and is routinely
not what the program hoped, so a panicking conversion would make `"abc".int()` a
crash the program has no way to prevent — where `read lines(int)`, the other half
of that help, reports at the parser and never produces the value at all.

**The accepted set is §7.4's own atomic**, over the whole trimmed text. `int` is
an optional `-` then digits; `float` is an optional sign, digits, an optional `.`
*with* a fraction, and an optional complete exponent. Both run the input
parser's own scanner — `parser::take_int_run` and `parser::take_float_run`, made
`pub(crate)` for exactly this — so `t.int()` and `parse(t, int)` cannot disagree
about what a number is.

The difference between a method and an atomic is *how much* must match, not
what: an atomic stops where its run stops and hands the rest of the line to its
template, and a method has no rest to hand anywhere. So a run that covers less
than the whole trimmed text is `None`, which is what makes `"1 2"`, `"12abc"`
and `"1."` rejections rather than partial answers. Trimming is the one liberty,
and it is what makes a line read off input usable without a second call.

Two consequences are worth stating because the obvious implementation gets them
wrong, and the first cut of `Text.int()` did:

- **`"+5".int()` is `None`.** `i64::from_str` takes a leading `+` and §7.4's
  `int` does not. `"+5.0".float()` *is* a value, because §7.4's `float` does —
  an asymmetry between the two atomics that is carried over rather than papered
  over here, since changing an atomic's accepted set is a change to the input
  language and wants its own decision.
- **`"inf".float()` and `"nan".float()` are `None`.** `f64::from_str` accepts
  both (and `"infinity"`, case-insensitively); §7.4's `float` accepts neither.
  `Float` still *has* those values — `1.0 / 0.0` is one, and `Float.to_text()`
  prints them — what has no spelling is reading one back out of arbitrary text.

A value outside `Int`'s range is `None` too, for `Y013`'s reason: a saturated
answer is a number nobody wrote.

The help is rewritten to say what the call actually gives you:

```text
help: this is `Text`; `.int()` answers `Option[Int]`, so take it apart with
      `match` (or use `read lines(int)`)
```

**A name that has no function value is `Y022`.** A prelude builtin or an enum
constructor written without being called is reported where it is written, next to
`Y018`, which says the same thing about a *generic* `fn`. The wording names the
remedy and which remedy depends on the arity: a nullary name wants its
parentheses (`` call it: `pi()` ``), one that takes arguments wants the closure
(`` write `|x| Some(x)` to call it ``).

**The test is the type, not a list of names.** The check asks whether the symbol
is a builtin or a constructor *and* instantiated to a function. That is what
distinguishes `Some` from `None` with no exception for either — a payload-less
variant instantiates to the enum type and is an ordinary value — and it is what
keeps a prelude entry added later from silently rejoining the broken set.

`pi` and `e`'s prelude doc strings say they are nullary functions. The doc string
is what hover shows, so "The constant π as a Float" was the wrong thing in front
of the one reader who had asked.

## Consequences

**What is bought.** Both halves of `Y001`'s help name something that exists, and
"read a number out of text" has one answer per type rather than one for `Int` and
none for `Float`. `out(pi)` names the missing parentheses. `var h = abs` is a
diagnostic rather than a program that prints nothing and exits 0, which is the
worst answer a compiler can give.

**What it costs.** `Y022` is a new refusal for programs that were accepted, and
every one of them was already broken — a builtin in value position produced
`Unit` and calling it produced nothing. No corpus program, benchmark or book
example wrote one. Making builtins genuinely first-class function values is the
alternative and it is a feature: it wants an adapter per prelude entry and a
decision about what a generic constructor's value even is.

**The book's "Text and numbers" wart passage is deleted** (`types/errors.md`),
and the method-catalog table gains a `Text` row.

**The gates.** `builtins::text_int_and_float_answer_options` is the catalog half
— pure data, red on the entry alone.
`jit::text_int_parses_a_number_or_answers_none` and its `_float_` twin are the
runtime half, over the accepted spellings and the rejected ones.
`parser::a_method_and_an_atomic_read_the_same_number` is the gate on the shared
scanner, and it is written as the *difference from the obvious implementation*:
every row is a text where `from_str` disagrees with §7.4, so a regression to
`from_str` turns it red.
`infer_tests::a_builtin_or_constructor_used_as_a_value_is_reported` covers `Y022`
at six shapes, with `None`, a payload-less user variant, and every call form as
the controls.
