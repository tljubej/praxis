# ADR-078: A parser position is absolute, a region only narrows, and exhaustion is the parent's decision

**Date:** 2026-07-31
**Status:** Accepted — implemented
**Milestone:** Repair (stage S20 — IPR-01 … IPR-05, IPR-09, IPR-10, IPR-13, IPR-14)

## Context

Fourteen findings were filed against `crates/praxis-runtime/src/parser.rs`. They
read as fourteen bugs and they are three, wearing different clothes.

The first is a type. `walk` returned

```rust
type WalkResult = Result<(GcRef, usize), ParseFail>;
```

documented as "a value + the number of bytes consumed". Twelve helpers produced
that `usize` as `bytes.len() - offset` — a length. Four callers assigned it
straight to a cursor — a position. One of the two readings was always wrong, and
which one depended on which function you were looking at, because nothing in the
type distinguished them. Nesting at a non-zero offset moved the cursor backwards.

The second is ownership. Five sites re-sliced a sub-buffer and walked the child
at offset 0, while `rt_owner` handed every source-slice `Text` the whole input as
its owner. A `word` in the second section therefore named bytes at the start of
the file. `rt_owner` read `ctx.input_source`, so `parse(text, P)` — which is
handed a *different* `Text` — produced slices that were views of the stdin buffer
at offsets chosen by a different string. A ragged grid's `fill` was worse: a plan
literal, walked as its own buffer, with its cells sliced out of the input.

The third is a missing requirement. `lines`, `sections`, `csv`, `ws`, `sep` and
`matrix` each computed the bound of the thing they were about to parse and then
walked the child against everything from that bound's start to the end of the
buffer, discarding the child's cursor. §7.5 says "each application must consume
the entire line". `csv`'s discard was written out: `let _ = token_end;`.

## Decision 1: one representation, and it makes the wrong answers unwritable

`parser/cursor.rs` holds three types and the interpreter is written in terms of
them.

- **`Cursor`** is an absolute byte offset with no `usize` constructor. The only
  mints are `Input::whole` and `Cursor::advance`, both of which start from a
  position that is already absolute, so `bytes.len() - offset` cannot become one.
- **`Input`** carries the `GcRef` its bytes belong to. Ownership stops being a
  second opinion read from the context; `rt_owner` is deleted. The reference it
  carries is the **root owned** `Text`, resolved once in `Input::new`, plus the
  base offset at which the parsed bytes begin inside it — see below.
- **`ByteRegion`** is a pair of cursors whose only derivation is `subregion`,
  which cannot widen — debug builds assert, release builds clamp. A child parser
  gets a *narrower window on the same buffer*, never a fresh buffer starting at
  zero, so its offsets are already the input's offsets and nothing is rebased.

`walk` returns `Walked { value, next }`. `next` is a `Cursor`, so "bytes
consumed" is not a reading that exists.

The conversion of `walk`, every helper it dispatches to and every recursive call
site was **one commit**, deliberately. A half-converted interpreter has both
cursor kinds in play with no type telling them apart, which is the original bug
in a form that is harder to find.

`Input` also holds a validated `&str`, which retires three
`str::from_utf8(..).unwrap_or("")` calls. Those turned a mis-computed region into
a silently *empty* one — a zero-row `Grid` where there should have been a
mismatch.

### The owner is the root, and it is resolved once

Taking the owner from the `input` argument is what makes `parse(t, P)` correct,
and it introduced a second problem the first draft did not close: `t` may itself
be a source slice, so each parse produced a slice **of a slice**. `text_bytes`
follows the chain on every read, so `t = parse(t, rest)` in a loop was O(depth)
per read, O(n²) over the loop, and at 100 000 links it overflowed the stack and
aborted the process — inside `extern "C"`, the one outcome §10.4 rules out.

`Input::new` resolves to the root owned `Text` once (`text_root`) and carries the
base offset; `Input::owner_offset` rebases every allocation. So a slice the
interpreter allocates is always exactly one level deep, whatever it was handed,
and the loop above is linear (100 000 links: 64s and an abort, now 0.3s). A chain
is still legal — the host can build one — so `text_bytes` is iterative as well,
because a depth no validation bounds must not cost stack.

**Alternative rejected: fix the twelve returns and keep the tuple.** It is the
smaller diff and it leaves the defect. The two readings would still both be
available, and the next helper written would pick one at random. F14's
instruction was a type that cannot hold a relative offset, not a convention that
one should not.

## Decision 2: `walk_exact` returns no cursor

A bounded parent calls `walk_exact`, which errors unless the child stopped at
`region.end()` and hands back a bare `GcRef`.

The missing return value is the decision. There is no cursor left over for a
caller to forget to check, so "I computed the bound and did not require the child
to fill it" is no longer expressible. Combined with `subregion`, a child cannot
read past its bound, and it cannot stop short of it except where what it leaves
is whitespace — round three's rule, stated under Decision 3: whitespace the
parser offered it does not read is nobody's.

## Decision 3: exhaustion is decided by the parent, in one place

`choice` does not require its region to be exhausted, and neither does the root
parse.

The audit read `choice`'s tolerance of a prefix match as an inconsistency to
resolve, and it is — but not by giving `choice` a policy. `scan(choice(...))`
matches fragments by design; `lines(choice(...))` must fill the line. The same
combinator, two answers, so the answer does not belong to the combinator. It
belongs to whoever bounded the region, and after Decision 2 that party is the one
holding `walk_exact`.

The root follows the same rule and therefore requires nothing: every real input
ends with a newline (the CLI reads the file verbatim), and a root that demanded
its region be consumed would fault every fixture in the corpus.

A capture with no literal after it is unbounded for the same reason — there is
nothing to stop before, and requiring exhaustion there would fault every
root-level template on its input's trailing newline.

### Whitespace is data when the parser offered it reads it

**AMENDED, three times.** This section stated a rule about the root *buffer* on
the first two attempts, and both were wrong in the same way; the third split the
question between two halves that then answered it differently. They are recorded
here because the shape of the mistake is more useful than any of the answers.

*Round one* applied the exhaustion rule at a root region that ran to the end of
the file. So `read ws(int)` over `"1 2 3\n"` cut its last token as `3\n` and
required `int` to fill it; `read sep(" -> ", word)` ran its final token to
`region.end()`; `read chars(P, skip: whitespace)` reached the `\n`, could not
skip it (`whitespace` is horizontal whitespace), and handed it to a child that
could not read it. Each faulted on every real input file, §7.5's own
`chars(one_of("^v<>"), skip: whitespace)` example among them.

*Round two* trimmed exactly one line terminator off the buffer in
`Input::root_region`, and defended stopping at one on the grounds that a file
ending `"\n\n"` has a blank final line that `sections` splits on. That defence
was not true — `sections` gives the same answer with the second terminator and
without it, because `split_sections` never emits an empty section — and the cost
was that a file ending in a blank line reproduced round one verbatim, one byte
later, with character-for-character the same three messages. It also fixed
nothing about `lines(int)` over `"1 \n2 \n"`, which faulted on an ordinary
trailing space, while `grid(int)` over the same bytes called that space padding:
two constructs in one stage disagreeing about one byte.

Both answers were a **count of bytes**. The rule is not a count.

*Round three* stopped counting and split the rule into a parser-independent
*extent* half (`split_lines` drops a trailing run of lines holding nothing but
whitespace) and a parser-dependent *bound* half (`walk_exact` forgives a
leftover run the child declined). That is the right shape, and the two halves
then gave **opposite answers to the one question the rule exists to settle**:
is a trailing space a cell `char` could read? `grid(char)` over `"ab\ncd \n"`
was a ragged grid, because the bound half asked `char` and `char` said yes;
`grid(char)` over `"ab\ncd\n  \n"` silently answered a 2x2 grid, because the
extent half deleted the line without asking, and `"  \n  \n"` was an *empty*
grid — four cells deleted. `lines(rest)` lost a line the same way, which is
`rest`'s identity property failing one level up. A parser-independent half
cannot answer a parser-dependent question. That is the mistake this amendment
fixes, and it is the same mistake as the first two in a new place: something
other than the parser was deciding what the parser could read.

> **A run of whitespace the parser offered it does not read is not data and not
> a mismatch.**

There is **one question** — *does the parser offered these bytes read them?* —
so there is one answer, and the half that can ask it is the half that decides:

* **Bound — the deciding half.** *Whitespace the parser offered it does not read
  is nobody's.* `walk_exact` asks it once, so every bounded construct there is —
  a line, a section, a CSV field, a `ws`/`sep` token, a matrix cell, a template
  capture — gets the same answer. `walk_characters` and `walk_grid_row` are the
  two loops that are not `walk_exact`-shaped and they call the same predicate,
  `ByteRegion::is_all_whitespace`. The *same* question applied to a whole line
  is `cursor::trailing_blank_run`: a **trailing** line of nothing but whitespace
  is offered like any other, and `trailing_blank_run`'s four callers —
  `walk_lines`, `walk_grid`, `walk_grid_ragged` and `walk_matrix` — drop it only
  when their parser makes nothing of it — no element, no cell, no token. `int`
  makes nothing of `"  "`; `char` makes two cells of it.
* **Extent — the half that decides nothing.** *A region does not end in **empty**
  lines.* `split_lines` drops the trailing run of lines holding no bytes at all,
  however long it is: the file's `\n`, an editor's `"\n\n"`, `"\r\n\r\n"`. It
  runs before any parser, so it is restricted to what has nothing in it to
  decide about. It used to drop lines of *whitespace* too, which is exactly the
  parser's answer being given by something that could not ask.

Three consequences worth stating, because they are the parts that look like
exceptions and are not:

1. **The root region is the whole buffer.** There is no trim and no count. The
   two halves above leave the file's terminator to nobody without one: a `ws`
   token contains no whitespace, a line does not extend into a blank one, and
   whatever is left at the end of a `sep` token or a `chars` region is
   whitespace the child declined. This is also the *only* correct answer for the
   other caller: `run_plan` is the single body behind both `read <parser>` and
   the host `parse(text, P)`, so a trim here deleted a byte from a `Text` the
   *program* wrote, and `parse(t, rest)` stopped being the identity on `t`. It
   is the identity again.
2. **`grid(char)` over `"ab\ncd \n"` is a ragged grid, and that is the rule
   working.** `char` reads a space as a cell (ADR-079), so it reads the trailing
   run — the rule asks the child, and the child said yes. The fault names the
   real complaint, "a grid row of the same cell count as the first", which is a
   statement about the data and not about a file convention. Put the space on
   every row and the grid is one column wider. Compare `grid(int)`, where `int`
   reads no cell there and it is padding: same rule, different child.

   The test of whether that is a rule or an exception is what it says about the
   *neighbouring* shapes, and it now says the same thing about all of them:
   `grid(char)` over `"ab\ncd\n  \n"` is **three rows** and over `"  \n  \n"` is
   a **2x2 grid of spaces**, because `char` reads those bytes too. Round three
   answered 2x2 and 0x0 for those, i.e. the exact opposite of the answer it gave
   six lines away in the same test. Under one rule they are one answer.
3. **An interior blank line is structure, and no constructor skips one.** The
   forgiveness is for a *trailing* run and nothing else. `matrix` used to skip
   any line that trimmed to nothing, so `matrix(int)` over `"1 2\n  \n3 4\n"`
   silently deleted the middle line while `lines(int)` and `grid(digit)` faulted
   on the identical shape — three constructs, three answers, for the rule they
   are all supposed to inherit. It is a zero-token row now and the width check
   rejects it, like everything else. `sections` is the one construct for which a
   blank line is *its own* separator wherever it appears, because `sections` is
   defined on blank lines the way `csv` is defined on commas; that is its
   contract, stated at `split_sections`, and not an exception to this rule.

Two smaller rules fall out of the same sentence and are stated with it, because
they are the places where a token or a line was being asked to hold whitespace
it could not:

* **A whitespace-delimited token contains no whitespace.** §7.5's "split on one
  or more spaces or tabs" names `ws`'s *separator*; a line terminator is not
  that separator but it is still a token terminator. `walk_ws` shares
  `whitespace_tokens` with `matrix` now, so the two whitespace-token
  constructors cannot disagree, and `read ws(int)` over `"1 2\n3 4\n"` is four
  tokens instead of three with the middle one faulting.
* **A blank line is a line of whitespace, not only an empty one.**
  `split_sections` and `trailing_blank_run` call a line blank by the same
  predicate, `ByteRegion::is_all_whitespace`. `split_lines` is the one place
  where the narrower word *empty* is meant, because it is the one place that
  decides without asking a parser, and `ByteRegion::is_empty` is a separate
  method so the two cannot be confused for each other.

An *interior* run is untouched by all of this and must stay so. `lines(int)`
over `"12junk"` still faults; `chars(digit, skip: none)` over `"1\n2"` still
faults; `sep(",", int)` over `"1,2\n3,4\n"` still faults, because the second
field really is `"2\n3"` and `sep` splits on the separator it was given — the
multi-line spelling is `lines(sep(...))`. "Trailing" is load-bearing.

The corollary for anyone adding a constructor: **do not write a trailing-newline
or blank-line special case.** If a construct tokenizes to `region.end()`, bound
its children with `walk_exact` and it has already inherited the rule; if it
splits lines, take `trailing_blank_run` and drop a trailing blank line only when
your parser made nothing of it. Anything that forgives whitespace per
constructor is fixing this in the wrong place, N times, and will disagree with
itself — which is how `csv` came to be the one constructor that survived round
one, purely because `csv_tokens` called `trim()`, and how `matrix` came to be the
one constructor that deleted an interior blank line, purely because
`walk_matrix` called `trim()` too.

**Amended: the two constructs that were still answering for their children.**
This decision's list of what `walk_exact` covers has always named "a CSV field"
and "a template capture", and neither reached its child with the bytes intact:

* A **template capture** advanced its cursor past leading horizontal whitespace
  before the child was offered anything (`skip_capture_ws`), so the same child
  on the same file answered one way as `lines(char)` and another as
  ``lines(`{a:char}`)`` — silently for `char`, as a hard fault at an interior
  blank line, and as lost bytes for `{a:text}`/`{a:rest}`. The skip is now a
  **bound-scan offset only** (ADR-079 Decision 4's amended §): a capture may not
  be bounded by its own leading whitespace, but what the child is *offered*
  starts at the cursor.
* **`csv`** trimmed every field with `str::trim()`, so `csv(char)` faulted on
  `"a, ,c"` where `sep(",", char)`, `ws(char)` and `grid(char)` all read the
  space as a cell, and — because `trim()` eats vertical whitespace too, which
  §7.5's csv entry never authorised — `csv(rest)` lost the terminator
  `sep(",", rest)` keeps. `csv_tokens` splits on commas and nothing else now.
  §7.5's "ignore horizontal whitespace around each comma" is *kept*, and kept by
  the rule: `int` skips it (§7.4 puts surrounding horizontal space on the caller
  for the numeric atomics, and `walk_atomic` is where that lives), the bound
  half forgives what is left, and `csv(int)` over `" 1, 2, 3"` still reads three
  ints. `csv` is not given `sections`' "this is its definition, not an
  exception" paragraph, because after this it does not need one.

Both trims were §7.4's caller rule re-imposed one level up, for exactly the
children `walk_atomic` refuses to apply it to — `char`, `text` and `rest`. The
gate is the capture/bare and csv/sep pairs at the end of
`every_root_parser_reads_every_file_ending`, each spelling asserted beside the
other so they cannot drift apart again.

The gate is `every_root_parser_reads_every_file_ending` in
`adversarial_audit.rs`: every root constructor §7.5 names, crossed with every
ending real input arrives with (none, `\n`, `\r\n`, `\n\n`, a trailing space, a
final line of spaces). It is a matrix and not an example on purpose. This class
of defect shipped twice as a byte count and once as two halves that disagreed,
and each time the fix was checked against one example. Its `grid(char)` block
asserts the shapes that used to disagree next to each other, and
`an_interior_blank_line_is_a_row_and_a_trailing_one_is_nobodys` does the same
for `matrix` against `lines` and `grid`.

## Decision 4: a failed `choice` reports the case that got furthest

Every case's failure used to be discarded and replaced with `"any choice case"`
at the position where the choice *started*, so §7.11's detail named the outermost
construct and pointed at a byte where nothing had gone wrong.

The deepest failure is kept. This is not a new rule: `ParseDetail::consider`
already resolves competing failures by taking the one furthest into the input, on
the reasoning that it is the most specific point at which parsing broke. `choice`
was the one place that had a pile of competing failures and threw them all away.
The generic message survives only for a choice with no cases, which is the one
shape it ever described honestly.

## Decision 5: a collection's element descriptor is derived, never defaulted

`template_result_descriptor` returned `&scalars::INT` for a single anonymous
capture, defended by a comment calling it "a sound default (the value's real
descriptor governs tracing)". Tracing is not what the descriptor is for: it is
the tag a collection carries for its *elements*, and `vec_format`, `vec_equals`
and `vec_hash` dispatch through exactly it. ``lines(`{word}`)`` produced a `Vec`
of `Text` objects tagged `Int`, and rendering it read a `Text` payload through
the `Int` callback.

It takes the plan now and returns `child_descriptor(plan, capture.child)`. So
does `walk_characters`, which hardcoded `CHAR`.

**No hardcoded element descriptor is left.** The last one — a template with two
or more anonymous captures answering `&scalars::UNIT`, so ``lines(`{int},{int}`)``
produced a `Vec` of `Tuple`s tagged `Unit` — was **REP-54**, and it is fixed
(ADR-092). Worth recording that this paragraph mispredicted the fix: it expected
"a tuple descriptor built from the child descriptors, which the static-descriptor
path has no constructor for". There was nothing to construct. `TUPLE` is a
uniform descriptor like `VEC` and `RECORD`, the per-shape `TupleSchema` lives in
the payload, and `VecPayload::element_descriptor` is a `*const TypeDescriptor`
that could not hold a schema in any case. "Derived, never defaulted" is now the
rule with no exception.

This decision depends on ADR-072 (S19): before a capture named its own parser
body, `capture.child` was the wrong node and deriving from it would have shipped
a green test asserting the wrong descriptor.

## Decision 6: the parser paces, and the unpaced back door has one caller

ADR-040 Decision 3 named `parser.rs` as one of two legitimate callers of
`Heap::alloc_unpaced`, and said the exemption ends when the interpreter's
intermediates are rooted. It ends here.

The interpreter's `Vec<GcRef>` intermediates now live inside `NativeScope`s.
Nothing is threaded through: a scope links itself into `ctx.native_roots`
and `RuntimeRoots` walks the parent chain, so a scope opened deeper already
covers everything its callers hold. `run_plan` roots the `input` itself, which no
other arm covers — `RuntimeRoots`'s `input` arm reads `ctx.input_source`, and for
`parse(text, P)` that is a different `Text` while this one owns every slice the
parse produces.

**The order was the finding.** Pacing was the only thing keeping the
intermediates alive, so a safepoint added before the roots would have converted
unbounded heap growth into a use-after-free. Roots and pacing landed in one
commit, and every other commit in the stage was checked for introducing a
safepoint.

`alloc_unpaced` now documents one legitimate caller: the host's own
`Runtime::alloc_*` helpers, whose results live in Rust locals that no root set can
see. A third caller would be making the argument the parser used to make, and the
answer to that argument is a `NativeScope`.

## Consequences

- **Previously-accepted inputs now fault, correctly.** A child that leaves bytes
  in its region is a parse failure. `csv(int)` over a region containing a newline
  is the shape that showed up in the suite: it "worked" because the leftover was
  nobody's.
- **`sections` no longer includes a section's trailing newline.** It did, which
  was invisible while a section's child was walked against the whole buffer, and
  faults every `sections(word)` the moment the child must consume exactly.
- **§7.11's detail names real positions.** Every `ParseFail` offset is a cursor
  into the buffer the failure happened in.
- **A deleted panic.** `region_offset_of` located a CSV token by searching the
  region for the token's text — so duplicate fields resolved to the first
  occurrence — and called `slice::windows(0)` for a token that trimmed to
  nothing, which panics. `"10,20,"` reached it, inside `extern "C"`. Bounds are
  computed while splitting now; there is nothing to search for and nothing to be
  empty. See ADR-080 for the policy that would have caught it anyway.
- **A ragged grid's `fill` gets its own owned `Text`.** It is a plan literal, not
  a region of the input, and this is what makes a sliced fill cell name the fill.
- **`Cursor` and `ByteRegion` are `pub(crate)`.** Nothing outside the runtime
  parser needs them, and a public position type would invite a second one.
