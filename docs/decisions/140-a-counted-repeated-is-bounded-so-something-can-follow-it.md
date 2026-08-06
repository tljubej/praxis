# ADR-140: A counted `repeated` is bounded, so something can follow it

**Date:** 2026-08-06
**Status:** accepted
**Milestone:** 12

## Context

Three AoC solves in handover 31 met the same input shape and none of them could
write it down: a **known** number of identically-shaped sections, then a section
of something else. Day 12's file is six shape blocks and then a list of regions.
§7.5 has one spelling for "a repeating group of sections", `repeated(P)`, and it
is greedy — it takes every section that is left — so `regions: lines(...)` after
it is `I028`, correctly. The workaround was to name the six blocks individually:

```praxis
var data = read sections(
    s0: block(`{i:int}:`, rows: grid(char)),
    s1: block(`{i:int}:`, rows: grid(char)),
    // … four more, identical …
    regions: lines(`{w:int}x{h:int}: {counts:ws(int)}`),
)
```

followed by six lines of code re-collecting `data.s0 … data.s5` into the `Vec`
the program wanted in the first place. The parser expression is the one part of
a Praxis program that is supposed to be a *description of the file*; six copies
of one description, plus a manual re-assembly, is the parser failing at the one
thing it is for.

The greedy rule itself is not the problem, and weakening it would be a
different, worse language. `repeated(P)` really cannot be followed. What was
missing was a way to say **how many**.

Three things had to be true before the feature could be written, and only the
first was obvious:

- **The grammar had no shape for a positional number.** `repeated(P, 6)` did not
  parse at all: `parse_parser_call` accepted a text literal, a `name:` argument
  or a parser expression, and an `IntLit` in positional position was none of
  them, so the count reported `P001 expected a parser expression` at the `6`.
  (A *keyword* value may already be a number — `grid(P, ragged, fill: 0)` — but
  that path is only reachable after a `name:`.)
- **The AST had thrown away the information the feature needs.**
  `SectionsNamed { fields, repeated_tail }` split the tail out of the field
  list, and the runtime hard-coded "the fields take `sections[0..fields.len()]`
  and the tail takes the rest". A group that wants six sections in a
  non-final position is not expressible in either.
- **`repeated`'s own arity was the one call site that never went through
  `check_call`.** `build_repeated_tail` hand-rolled `args.len() != 1`, which is
  precisely the exemption [ADR-073](./073-a-constructor-call-is-a-shape-checked-before-it-is-built.md)
  Decision 1 was written against.

## Decision 1: the unbounded `repeated(P)` rule is unchanged

`repeated(P)` still consumes every section that is left, there is still at most
one of it, and it is still `I028` anywhere but last. Nothing about the counted
form is an argument for relaxing it: the reason a greedy tail must be last is
that a field after it could never match, and that reason is as good as it was.

The two `I028` messages are reworded to say **unbounded**, and the position one
now names the counted form as the spelling that does not have the restriction —
a diagnostic whose fix is a different construct should say which one.

## Decision 2: `repeated(P, N)` consumes exactly N sections, and N is a literal

The count is a whole number written in the program. Not a variable, not a
constant, not an expression: `lower_to_plan`/`register_plan` run at compile time
in `analyze_parser_expr`, and the plan is a `&'static` arena keyed by a `PlanId`
the MIR passes as an immediate. There is no runtime value in scope when the plan
is built, so a non-literal count is not a feature that was declined — it is
unrepresentable. The diagnostic says so rather than saying "unknown atomic
parser `n`", which was the old answer and carries none of the fix.

`RepeatCount` is the newtype, beside `Separator` (ADR-073 Decision 4) and
`CaptureName`. Its one constructor refuses `0`, negatives, and anything past
`u32::MAX`. `repeated(P, 0)` would consume nothing and produce an empty `Vec`,
which is not a parser anybody writes on purpose and reads as a typo for the
unbounded form; a `validate` arm would catch it only where somebody remembered
to call one, and a type catches it at every construction site there will ever
be.

**Too few sections is a fault**, exactly as too few sections for a fixed field
already is. A group of six that finds four is input that did not match the
parser — truncating it to a `Vec` of four is the one outcome no program can
notice, since writing the count is the program saying it knows the number. The
fault is the existing `input parse mismatch`; the `expected` text stays
`section header` when every field wants one section (so the message the book
documents is untouched) and otherwise names the group that came up short:
``expected 6 sections for `shapes` ``.

## Decision 3: the AST keeps `repeated_tail` as its own `Option`

`SectionsNamed`'s `fields` becomes a `Vec<SectionItem>` — `One` or `Counted` —
and the unbounded tail stays where it was, in a separate `Option` beside the
list rather than as a third variant inside it. That is what keeps "at most one
unbounded tail, and it is last" **structural**: it is not a rule the AST can
express a violation of, so the `I028` check is purely about *source order* and
nothing downstream has to re-establish the invariant.

The tempting alternative — keep `fields: Vec<(String, ParserAst)>` and add a
parallel map of which positions are counted — is the exact drift ADR-073
Decision 3 was written about: a position recorded twice is a position that can
disagree with itself, and a tail whose recorded position was not its source
position is how a misordered call used to compile into a working parser that
was not the one written.

## Decision 4: the grammar accepts a positional literal; the shape table decides who may have one

`parse_parser_call` now accepts an integer literal (and a `-` with digits behind
it, as one `LITERAL` node) as a positional argument, for *every* constructor.
Which constructors accept which literal is `Constructor::arg_shape`'s question,
answered by `check_call` — the same division of labour `PARSER_KEYWORD_VALUE`
already uses for `skip:` and `fill:` (ADR-073's third amendment):
`Constructor::keyword_arg` decides who has a keyword, not the parser.

`Constructor::Repeated` moves from `ArgShape::Positional(1)` to
`ArgShape::ParserWithOptionalCount`, and `build_repeated_tail` routes through
`check_call` like every other builder. That is ADR-073 Decision 1's own thesis
finally applied to the call it exempted.

## What this does not do

**No count on `lines`, `block`, `grid` or `chars`.** `lines(P)` is greedy at the
only position it can occupy, and a `lines` inside a `sections` field is already
bounded by its section; `block`'s items are line-anchored and individually
windowed by [ADR-090](./090-a-block-item-is-offered-its-own-lines.md),
so a count there would be a second opinion about an extent the item already
states. A second opinion about an extent is how two answers to one question get
into the tree.

## Consequences

- **No new diagnostic code.** `I014`, `I022` and `I028` are all pre-allocated by
  [ADR-051](./051-the-diagnostic-code-allocation.md); the counted form adds
  three `I014` messages (not a literal, at least 1, fits in 32 bits) and
  rewords two `I028` ones.
- **`repeated`'s `I022` text changes** from "`repeated` expects 1 argument, got
  N" to "1 or 2 arguments", because the shape table now owns its arity.
- **A positional number is now accepted by the grammar everywhere**, so a
  pre-existing mistake like `lines(int, 5)` reports `I022`/`I014` naming `lines`
  instead of `P001 expected a parser expression` plus `I000`. That is a strict
  improvement and it is the same trade `fill: 0` made, but it is a user-visible
  diagnostic change beyond this decision's own subject.
- **The LSP's exhaustive `match ctor.arg_shape()`** forces an arm for the new
  shape, which is the mechanism that stops a constructor from shipping without
  a signature. `repeated` now offers both forms in signature help.
- `sections(shapes: repeated(P, 6))` with no other field is **legal**, while
  `sections(boards: repeated(P))` alone is still `I025`: the latter is
  `sections(P)` written the long way round, and the former reads a known prefix
  and leaves the rest.
