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

### A `char` cell is positional, and a row's trailing spaces are padding

Two things this decision did not say, and had to be added after it shipped a
worse defect than the one it replaced.

**A space is a character.** `walk_atomic` opened every atomic by skipping
horizontal whitespace, so `char` inside a `grid` never produced a space cell:
`grid(char)` over `"a b"` was two columns, and `grid(char)` over `"ab\na b"`
counted two cells in *both* rows and reported a genuinely ragged input as a
clean 2x2 grid with `b` shifted into the space's slot. That is a wrong answer
where the byte-counted predecessor gave a wrong shape, and a wrong answer is
worse. §7.4's "surrounding horizontal space handled by caller" is a rule for the
numeric atomics; `char` and `one_of` are character classes and read the scalar
at the cursor. A caller that wants leading space skipped has `chars`' `skip:`,
`walk_exact`'s token bounds, and a template's pre-capture skip. With that,
"every row has the same cell count" rejects a ragged char grid again.

**A row's trailing whitespace is padding, not a cell — when the cell parser
cannot read it.** `grid(int)` faulted on a row ending in a space while
`matrix(int)` over the identical file succeeded — `whitespace_tokens` never
emits an empty token, so `matrix` had always dropped it. §7.5 asks only for
equal cell counts, so `walk_grid_row` stops when the cell parser fails and what
is left of the row is whitespace.

This is not a `grid` rule. It is ADR-078's bound rule — *a child that leaves
only whitespace has filled its bound* — reached through the same predicate,
`ByteRegion::is_all_whitespace`; `grid` is simply one of the two loops that are
not `walk_exact`-shaped. Stating it once is what stopped `lines` and `grid`
disagreeing about the identical run, which they did for a whole round.

A cell parser that *can* read the run never reaches that branch, which is why
this does not undo the paragraph above: `grid(char)` reads a trailing space as a
trailing cell, so `grid(char)` over `"ab\ncd \n"` is a **ragged grid** and says
so. Same rule, different child.

## Decision 2: `text` is non-greedy, and *every* capture is bounded

§7.4 already decides this, verbatim: "`text`: minimally consumes text until the
following template literal can match". The implementation consumed to the end of
the whole buffer, so `pre{body:text}post` swallowed its own suffix and no
template with a trailing literal could match anything.

The bound is computed in `walk_template`, not in `walk_atomic`, and it is applied
to **every** capture rather than only the `text` ones. A capture is handed
`region.subregion(cursor, bound)` and must fill it.

Uniformity is the decision, not an implementation convenience:

- "Non-greedy" stops being a property `text` has to remember and becomes a
  property of the region every child is given.
- It is what bounds `{a:int}` to the comma in `{a:int},{b:int}`.
- It closes IPR-11 without touching `word`'s delimiter set (Decision 3).

The bound is taken **before** the following run's whitespace policy runs, which
is what keeps that whitespace out of the capture: for `` `{name:text} {v:int}` ``
on `"foo 3"` the run is a literal with empty text and `WsPolicy::SpaceRun`, so
the earliest position it matches is byte 3 — `name` stops *at* the space rather
than inside it, and the policy eats it.

**AMENDED.** This paragraph used to illustrate that with `{a:int},{b:int}` on
`"12 ,34"`, crediting "the comma's `SpaceRun`" with absorbing the space and
claiming `int` "still consumes its region exactly". Both halves are wrong, by
two later decisions of this same ADR. The comma carries `WsPolicy::None`, not
`SpaceRun` — a template that writes nothing in front of a literal gets no run in
front of it (Decision 4), which is why the template still matches `"12,34"` with
no space at all. And `int` does *not* fill its region: the bound is the position
where the run can start, so on `"12 ,34"` it is the comma at byte 3, the capture
is handed `"12 "`, and the trailing space is forgiven by ADR-078's rule —
whitespace the parser offered it does not read is nobody's
(`ByteRegion::is_all_whitespace` in `walk_exact`). Remove that forgiveness and
the same program faults at `2..3`, which is how the mechanism was confirmed. A
template capture is not an exception to ADR-078; it is one of the constructs
that inherits it.

### What the bound is

**AMENDED.** This section said the bound was "the earliest position at which the
next **non-empty literal** can match after its whitespace policy". That was the
rule as first shipped, and it was the reported defect: a capture followed by a
whitespace-only part was not bounded at all, because §7.9 lowers a run of
whitespace to a `Literal` whose *text is empty* and whose policy carries the
requirement. So `` lines(`{name:text} {v:int}`) `` over `"foo 3"` reported
"expected whitespace" at the end of the line — for the most ordinary template
shape there is — while `{a:text} -> {b:int}` worked, purely because `->` has
bytes. Decisions 1 and 6 were amended in place when their code changed; this one
was missed, and an ADR that describes code which no longer exists is worse than
no ADR.

The rule is:

> The bound is the earliest position at which the next **constraining** part can
> match.

A part constrains when it demands at least one byte. A literal with text does; so
does a whitespace-only part whose policy is `SpaceRun`, `OneOrMore`,
`ExactSpace`, `Newline` or `Tab` — a plain space run, `\s+`, `\x20`, `\t`, `\n`.
§7.4's "until the following template literal can match" does not exempt a part
for having no text: a space run *is* that literal.

`WsPolicy::None` and `WsPolicy::ZeroOrMore` match the empty string, so they
constrain nothing and are skipped; the scan continues past them to whatever does.
That is why `` `{a:text}\s*{v:int}` `` over `"foo 3"` leaves the capture unbounded
and reports "expected int" — asking for zero-or-more is asking for no bound, and
that is the answer.

### It is the whole run that has to match, not its first constraining member

**AMENDED again**, for the reason two spellings of one policy behaved
differently. `` lines(`{a:text} bar`) `` read `"x y bar"` as `a = "x y"`, and
`` lines(`{a:text}\s+bar`) `` over the identical bytes faulted with
`expected literal "bar"` at the interior space — because `\s+` is lowered to its
own empty-text part, so "the first constraining part" is the space run alone, and
a space run matches at the *first* space, where `bar` is not.

Two spellings of one policy disagreeing is the same class of defect as `lines`
and `grid` disagreeing about a trailing space (ADR-078), and it has the same
kind of answer — state the rule so that neither spelling is a special case:

> The bound is the earliest position at which the **whole run of parts up to the
> next capture** can match.

A run constrains when *any* of its members does; a run that is empty, or whose
members all match the empty string, is no bound and the capture takes the rest of
its region as before. `match_literal_run` is the lookahead: exactly what
`walk_template`'s own `Literal` arm does, without committing.

This subsumes the previous wording rather than contradicting it — a run of one
constraining part is the same scan — and it is strictly more precise where the
run is longer. `` `{a:text}\s*bar` `` over `"x  bar"` now stops the capture at
`x` and lets the `\s*` take the spaces, where matching `bar` alone would have
put them inside the capture.

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

The failure propagates. §7.5's rule then falls out of the loop's own shape: the
skip policy runs once more after the last match, so a trailing run the policy
covers is absorbed, and a trailing byte the policy does not cover must be read
by the child or it is a mismatch.

**Amended.** This decision first said that "`skip: whitespace` and
`skip: newlines` absorb a trailing run", and that was false for
`skip: whitespace`, which is *horizontal* whitespace — spaces and tabs — and
therefore cannot absorb a `\n`. The claim was load-bearing for exactly the wrong
case: the input file's own trailing newline. So §7.5's documented example,
`read chars(one_of("^v<>"), skip: whitespace)`, faulted on every ordinary file.

Nothing here changes to fix that, because nothing here was the problem: the
file's terminator is not part of the data and is now not part of the root region
(ADR-078 Decision 3). What this decision governs is a trailing run *inside* the
data, and there the sentence above holds for each policy's own byte set.

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
