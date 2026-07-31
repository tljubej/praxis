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
  second opinion read from the context; `rt_owner` is deleted.
- **`ByteRegion`** is a pair of cursors whose only derivation is `subregion`,
  which cannot widen — debug builds assert, release builds clamp. A child parser
  gets a *narrower window on the same buffer*, never a fresh buffer starting at
  zero, so its offsets are already the input's offsets and nothing is rebased.

`walk` returns `Walked { value, next }`. `next` is a `Cursor`, so "bytes
consumed" is not a reading that exists.

The conversion of `walk`, its seventeen helpers and every recursive call site was
**one commit**, deliberately. A half-converted interpreter has both cursor kinds
in play with no type telling them apart, which is the original bug in a form that
is harder to find.

`Input` also holds a validated `&str`, which retires three
`str::from_utf8(..).unwrap_or("")` calls. Those turned a mis-computed region into
a silently *empty* one — a zero-row `Grid` where there should have been a
mismatch.

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
to fill it" is no longer expressible. Combined with `subregion`, a child can
neither read past its bound nor stop short of it.

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
does `walk_characters`, which hardcoded `CHAR`. There are no hardcoded element
descriptors left in the interpreter.

This decision depends on ADR-072 (S19): before a capture named its own parser
body, `capture.child` was the wrong node and deriving from it would have shipped
a green test asserting the wrong descriptor.

## Decision 6: the parser paces, and the unpaced back door has one caller

ADR-040 Decision 3 named `parser.rs` as one of two legitimate callers of
`Heap::alloc_unpaced`, and said the exemption ends when the interpreter's
intermediates are rooted. It ends here.

Seventeen `Vec<GcRef>` intermediates now live inside `NativeScope`s. Nothing is
threaded through the interpreter: a scope links itself into `ctx.native_roots`
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
