# ADR-079: A grid cell is what its cell parser reads, a capture is non-greedy, and a collection's type is its child's

**Date:** 2026-07-31
**Status:** Accepted — implemented
**Milestone:** Repair (stage S20 — D11, D-S20-A, IPR-06 … IPR-08, IPR-10 … IPR-12)

## Context

ADR-078 records how the parser interpreter *represents* positions. This one
records what the parser **means**, because four of S20's findings could not be
closed without deciding it. Each is a user-visible grammar question: it changes
which inputs parse, or what type a program gets.

They were tracked as D11 (three parts) and D-S20-A, and answered in the plan
before the stage started. This records them as implemented, with the reasoning
that survived contact with the code.

## Decision 1: a `grid` cell is whatever its cell parser reads

§7.5's `grid(cell_parser)` entry shows two examples, `grid(char)` and
`grid(digit)`, and `digit` exists *for* the one-digit-per-cell case. If
`grid(int)` also meant one digit, `digit` would name nothing.

So the rule is the general one rather than a granularity: **a cell parser inside
`grid` parses a cell exactly as it would parse anywhere else.** `char` is one
Unicode scalar, `digit` is one digit, `int` is an integer token. A row is parsed
by applying the cell parser from the row's start until the row is consumed, and a
row's width is the number of cells that produced.

What it did before was **neither** candidate. Over `"12\n34\n"` it answered four
cells `[12, 2, 34, 4]`: width and iteration counted **bytes**, and the child was
walked against the whole remaining suffix, so it read the token at cell 0 and
then read the token's tail again at cell 1. Nobody had to choose between the two
semantics, because the code implemented a third that was not one.

Counting cells is also what closes IPR-06's Unicode half without a special case.
Width used to be a byte count, so a row holding one `é` was two columns wide and
was parsed twice — once at the scalar and once at its continuation byte, which is
not a character. A cell count means the same thing for every cell parser, and
`grid(char)` on `"é"` is one cell because `char` reads one scalar.

`scan` steps by scalar for the related reason: it advanced one byte on a miss, so
on a multi-byte run it attempted matches at continuation bytes.

**Alternative rejected: `grid(int)` is one digit per cell.** It makes `grid` and
`digit` redundant, and it makes "every row must have the same cell count" mean
two different things in one document. `matrix(element_parser)` is §7.5's
whitespace-tokenized constructor and remains a different one — it splits a row
into tokens itself, where `grid` lets the cell parser decide.

**Consequence acted on:** `a_grid_subscript_takes_both_coordinates_in_the_order_
the_design_names` read `grid(int)` over `"12\n34\n"` and expected `g[0, 0] == 12`
— a whole token per cell, *and* one such cell per row. The first half is this
decision; the second was an artefact of the byte-counted width. Its **input** is
amended to `"1 2\n3 4\n"`, which spells two cells per row; its subject, that a
subscript is x-then-y and an off-diagonal store catches a swap, is untouched.

## Decision 2: `text` is non-greedy, and *every* capture is bounded

§7.4 already decides this, verbatim: "`text`: minimally consumes text until the
following template literal can match". The implementation consumed to the end of
the whole buffer, so `pre{body:text}post` swallowed its own suffix and no
template with a trailing literal could match anything.

The bound is computed in `walk_template`, not in `walk_atomic`, and it is applied
to **every** capture rather than only the `text` ones. A capture is handed
`region.subregion(cursor, bound)` and must fill it, where `bound` is the earliest
position at which the next non-empty literal can match after its whitespace
policy.

Uniformity is the decision, not an implementation convenience:

- "Non-greedy" stops being a property `text` has to remember and becomes a
  property of the region every child is given.
- It is what bounds `{a:int}` to the comma in `{a:int},{b:int}`.
- It closes IPR-11 without touching `word`'s delimiter set (Decision 3).

The bound is taken **before** the following literal's whitespace policy runs, so
for `{a:int},{b:int}` on `"12 ,34"` the comma's `SpaceRun` absorbs the space and
`int` still consumes its region exactly.

## Decision 3: `word`'s delimiter set stays minimal

`word` stops on space, tab, `,`, `\n` and `\r`. The audit read
`{w:word}-to-{x:word}` swallowing its `-to-` as evidence that the set is too
small.

It is not. Growing it to "every template delimiter" breaks `sep(" -> ", word)`
and `{source:word}-to-{destination:word}` (tests/aoc-corpus/m9_almanac.px) the
moment `-` or `:` joins it, and any `word` that legitimately contains a `-`. A
longer list is the same bug with more entries.

What was missing is the bound. After Decision 2 a `word` inside a template stops
at the following literal whatever its own rule says, so the set stays minimal and
`take_word_run` documents itself as the *bare* `word`'s rule. Both halves are
pinned: the bounded `word` stops at the literal, and a bare `ws(word)` still
reads `a-b` as one word.

## Decision 4: `SpaceRun` requires one or more, and `WsPolicy` gains `None`

`WsPolicy::SpaceRun` is defined as "one or more spaces or tabs" and was
implemented as zero-or-more, with a comment in `consume_ws` admitting it.

The apology was accurate about the constraint. The scanner tagged **every**
literal `SpaceRun`, including literals with no whitespace anywhere near them, so
requiring one space would have made `{a:int},{b:int}` unmatchable — and
`WsPolicy` had no variant meaning "no run here", so the information the
interpreter needed did not exist in the plan.

`WsPolicy::None` is added; `flush` emits it for a literal that had no run
stripped from its front and `SpaceRun` for one that did; `consume_ws` requires
the one-or-more. This also fixes the *other* direction, which nobody had
complained about: a comma the template wrote with nothing in front of it no
longer matches an input that has a space there.

The pre-capture whitespace skip stops being a `WsPolicy` at all. It was
`SpaceRun`, and worked only because `SpaceRun` was zero-or-more; §7.4 puts
surrounding horizontal space on the caller, so it is `skip_capture_ws`, which is
what it always was.

**Consequence acted on:** REP-20's gate asserted that a template written ` -> `
also matches `1->2` — the contradiction itself, written into a test. That input
now faults, which the test asserts explicitly; the flexible half it exists to
protect (one space, many, tabs) is untouched.

## Decision 5: `chars(P, skip:)` is `Vec[result(P)]`

`synthesize` hardcoded `Vec[Char]` regardless of `P` while `walk_characters`
stored whatever `P` produced and tagged the payload `CHAR`. So
`chars(int, skip: none)` advertised `Vec[Char]` statically, tagged its elements
`Char`, and stored `Int` objects — a descriptor that disagrees with the values
behind it, which is the class of defect P0-11 closed for collections generally.

The type is derived from the child on both sides. `chars(one_of("LR"))` is still
`Vec[Char]`, because `one_of` synthesizes `Char`; it is derived now rather than
assumed.

**Alternative rejected: fix only the runtime descriptor.** That leaves the static
type and the runtime tag disagreeing, which *is* the bug. **Alternative rejected:
reject a non-`Char` child at validation.** It answers a descriptor disagreement
by removing a construct that works.

**Consequence acted on:** the exit test declared `-> Vec[Char]` for
`chars(int, …)`, which is the disagreement it exists to catch. Its annotation is
amended with a comment recording the inversion.

## Decision 6: `chars` reads its whole region or fails

`walk_characters` had `Err(_) => break`, so a child failure ended the loop and
the function returned `Ok`: `chars(digit, skip: none)` over `"12x34"` answered
`[1, 2]` and reported nothing.

The failure propagates. §7.5's rule then falls out of the loop's own shape — the
skip policy runs once more after the last match, so `skip: whitespace` and
`skip: newlines` absorb a trailing run, and under `skip: none` a trailing byte
the child cannot read is a mismatch. That is the whole policy set, and it needed
no extra rule.

## Consequences

- **This stage changes what parses.** A `grid(int)` program written against the
  old byte-counted width reads different numbers and a different width; a template that relied on
  `SpaceRun` accepting nothing now faults; a `chars` that silently truncated now
  reports. All three are cases where the previous behaviour was not a semantics
  anyone had chosen.
- **`WsPolicy::None` is a new variant in `praxis-input-parser`.** It is not
  `#[repr(C)]` and generated code never reads it, so no ABI bump.
- **`matrix` and `grid` are visibly different constructors** — `matrix` splits a
  row into whitespace-delimited tokens itself, `grid` lets the cell parser
  decide how far a cell reaches. That is what §7.5 always said and what
  Decision 1 depends on.
