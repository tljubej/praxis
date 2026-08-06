# ADR-143: The `to_text` family is `Int`, `Float` and `Char`

**Date:** 2026-08-06
**Status:** accepted
**Milestone:** 12
**Amended by:** [ADR-147](./147-a-hole-renders-anything-because-the-program-wrote-the-hole.md)
(2026-08-06) — decisions 4 and 5 below recorded §8.1's interpolation as open and
named settling ADR-085 decision 2 "by accident" as the thing to avoid. It is
settled now, on purpose, and both decisions carry dated notes. Decisions 1, 2
and 3 are unaffected: the family is still `Int`, `Float` and `Char`, and there
is still no universal `T.to_text()` **method**.

## Context

Since §4.12's `Float` methods landed, the catalog has had a `to_text` column
with exactly one row in it. `Int.to_text()` and `Char.to_text()` were both
`Y110`, and the reason is recorded in two places that agree with each other.

`crates/praxis-stdlib/src/builtins.rs`, in the block above the `Char`/`Int`
conversion pair:

> **`Char.to_text()`.** §4.13 records a standing gap in the design doc's own
> words: `Int` has no `to_text()` either, and §8.1's interpolation is specified
> and unimplemented. The `to_text` family is one decision and wants taking
> whole. Adding it here would also give a second spelling for "is this character
> a `#`" (`t[i].to_text() == "#"` beside `t[i] == "#"[0]`), and two spellings
> for one question is what ADR-077 refused.

[ADR-086](./086-a-text-subscript-answers-a-char.md) says the same and then
concedes the first half of it has decayed: §4.13's sentence that "building a
`Text` out of a number is not yet possible in any spelling" stopped being true
when [ADR-136](./136-a-text-becomes-a-number-through-an-option.md) went the
other way and [ADR-085](./085-text-concatenation-is-plus-and-nothing-else-is.md)
gave `Text` a `+`. So the family was two thirds open rather than three, and the
recorded reason for keeping it that way was **not** "interpolation must land
first" — it was "decide the family as a unit". This is that decision.

What moved it up the list is traffic. [Handover
31](../handovers/31-what-an-aoc-solve-found.md) records this as the item that
showed up most often per hour of writing three AoC solutions. Every labelled
debug line was two calls:

```praxis
out("splits:")
out(splits)
```

and the documented workaround for the `Int` half, `n.to_float().to_text()`,
renders `1660` as `1660.0`, so it is not one.

## Decision

### 1. The family is `Int`, `Float` and `Char`, and it is closed at three

Two catalog rows, two manifest rows, two runtime wrappers. `Int.to_text()` and
`Char.to_text()` are `Purity::Pure`, allocate a fresh `Text`, and cannot fault —
an `i64` always renders and a `CharPayload` is a validated Unicode scalar value
by construction (ADR-086), so there is nothing left to check. Declaring either
faulting would put a `CheckFault` after every call site that can never fire.

### 2. A `to_text` answer *is* what `out` writes, by construction

Each wrapper calls the same `scalars::write_*` function the type descriptor's
`format` callback calls: `write_int`, `write_char`, `write_float`. One renderer
per scalar, two callers.

This is the load-bearing half of the decision, and it is why the refactor came
before the rows. A program that prints a value and a program that builds a
`Text` from it producing different characters is a defect in itself — and §4.12's
shortest-round-trip rule for `Float` (ADR-083) is exactly the kind of rule that
drifts when it is written twice. `praxis_float_to_text` already had the shape;
`Int` and `Char` did not, because nothing had needed a second caller yet.

So the guarantee is not "tested and found to agree". A second renderer is not
representable: there is one function, and writing a `write!` inline in a wrapper
instead of calling it is the mistake a reviewer looks for.

### 3. `to_text` is not a second spelling of a character comparison

ADR-086's second objection was that `Char.to_text()` gives a second way to ask
"is this character a `#`" — `t[i].to_text() == "#"` beside `t[i] == "#"[0]` — and
that [ADR-077](./077-a-zero-argument-accessor-is-a-call-and-a-bare-dot-name-is-a-field.md) refuses two
spellings for one question.

It does, and this is not one. ADR-077 refused two *accessors*: two rows that
answer the same question with the same cost, where a reader has to know which is
idiomatic. The comparison through `to_text` allocates a `Text` to answer what a
four-byte scalar compare already answers, so nobody writes it, and after
[ADR-107](./107-a-small-char-is-one-object-and-there-is-no-char-literal.md)'s
character literal lands it is a third form nobody writes.

The question this row is here for is the other direction — producing output text
— and that had no spelling at all.

### 4. No `Bool.to_text()` and no universal `T.to_text()`

Neither is an oversight, and both are recorded beside the three rows so a later
reader meets the omission where the family is defined.

`Bool.to_text()` has no design-doc surface, which is what REP-46 refused to
invent rows past. `if b { "true" } else { "false" }` says it, and says which
spelling the program wanted.

A universal `T.to_text()` is §8.1 interpolation's question, not this one, and it
is entangled with ADR-085 decision 2: a rendering conversion defined on every
type is the implicit conversion to `Text` that decision refused for `+`. Adding
it as a rider on this change would settle that by accident.

> **2026-08-06 ([ADR-147](./147-a-hole-renders-anything-because-the-program-wrote-the-hole.md)).**
> Settled, on purpose. §8.1's interpolation renders **any** value, through the
> same `format` callback `out` dispatches to — so the universal rendering this
> row deferred now exists, and this row's instinct about where it belonged was
> right: it is a property of the *hole*, not of a method.
>
> **`Bool.to_text()` and a universal `T.to_text()` are still absent**, and this
> decision's closure at three still holds. Nothing was added to the catalog; the
> renderer is reached through `praxis_value_to_text`, a runtime wrapper with no
> spelling in the language. ADR-085 decision 2 also still holds — `"n = " + n` is
> still `Y001` — and ADR-147 decision 3 is the reconciliation this row said would
> otherwise happen by accident.

### 5. §8.1's interpolation stays open, and was never a prerequisite

`"Part 2: {part2}"` remains specified and unimplemented. Recording the cost so
the next reader does not re-derive the question: it needs a mode-stacked lexer
(`crates/praxis-parser/src/lex.rs` has one flat `TextLit` token today), new
`SyntaxKind`s, a parser node and typed AST wrapper, formatter reprinting, an HIR
formatting node or a desugar, a typing rule per hole, LSP semantic tokens and
rename inside holes, and `crates/praxis-hir/src/capture.rs` — because a name
inside a hole is a closure capture. It also forces decision 4's unanswered
question about what a hole may hold.

These rows make it **cheaper** rather than redundant. With `Int`, `Float`, `Char`
and `Text` covered, `"a{n}b"` can desugar to `"a" + n.to_text() + "b"` and needs
no new runtime path at all.

> **2026-08-06 ([ADR-147](./147-a-hole-renders-anything-because-the-program-wrote-the-hole.md)).**
> Implemented, and the cost estimate above is worth reading against what it took:
> the mode-stacked lexer, the new `SyntaxKind`s, the parser node, the typed AST
> wrapper, the HIR node and the `capture.rs` consequence were all real, and the
> capture consequence turned out to be the one that *decided the design* rather
> than merely appearing in it.
>
> **The desugar in the last paragraph was not taken**, and could not be. It
> bounds a hole's legal types to the four with a `to_text()` row, where ADR-147
> decision 2 renders anything — so a hole calls `GcRef::format` directly, which
> is one runtime path and no per-type dispatch. The estimate was right that the
> rows made it cheaper; it was wrong about which part they made cheaper, which
> was the reader's confidence that a rendered value and a printed one agree.

## Consequences

**What is bought.** A labelled line is one call: `out("splits: " +
splits.to_text())`. `Y110` on `to_text` now means the family is closed at three,
not that the column has one row. The catalog's `to_text` column and the
language's rendering are the same thing, which they were only by coincidence
before.

**What it costs.** Two rows the language has to keep forever, and a family that
is closed by a test rather than by anything structural — `Bool` has a payload
and a `format` callback like everything else, so the only thing keeping
`Bool.to_text()` out is `the_to_text_family_is_int_float_and_char`.

**No ABI version bump.** No `#[repr(C)]` type changed, so
`COMPILER_EXPECTED_ABI_VERSION` stays where it is. ADR-085 set that precedent
explicitly for `praxis_text_concat`, and appending a manifest symbol has never
been a bump.

**ADR-085 and ADR-086's prose about the gap is now historical**, and both carry
an amendment note rather than a deletion: a reader must meet the reversal where
they meet the reason. ADR-085 decision 2 is unaffected and still stands — the
conversion is explicit, which is exactly what that decision asked for.

**The gates.**
`builtins::the_to_text_family_is_int_float_and_char` is the catalog half, and it
asserts the *closure* as well as the members — no `Bool`, no `Byte`, no `Text` —
because the closure is the half a later edit would undo.
`abi::the_to_text_family_allocates_and_cannot_fault` guards the effect row,
whose wrong answer is otherwise silent.
`runtime::abi::int_to_text_renders_exactly_what_out_renders` and its `char_`
twin are the divergence gates, and they are written against `GcRef::format` —
`out`'s own path — rather than against a literal, so they cannot pass while the
two renderers disagree.
`jit::int_to_text_is_the_digits_out_prints`,
`jit::char_to_text_is_the_character_out_prints` and
`jit::a_labelled_line_is_one_call` are the end-to-end half; the last of those is
the handover's own complaint written as an assertion, and it is the one that
proves the row *composes* with `+` rather than merely existing.
