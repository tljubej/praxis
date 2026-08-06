# ADR-147: A hole renders anything, because the program wrote the hole

**Date:** 2026-08-06
**Status:** accepted
**Milestone:** 12
**Amends:** [ADR-085](./085-text-concatenation-is-plus-and-nothing-else-is.md)
decision 2 (which stands, and is now bounded rather than merely asserted) and
[ADR-143](./143-the-to-text-family-is-int-float-and-char.md) decisions 4 and 5
(which recorded this question as open and named settling it "by accident" as the
thing to avoid).

## Context

§8.1 has specified string interpolation since the design document was written:

```praxis
out("Part 2: {part2}")
```

It was never implemented. The braces printed literally, so that line wrote
`Part 2: {part2}` and the program looked like it worked. Three ADRs in a row
recorded the gap and declined to close it — ADR-085's context, ADR-086's, and
ADR-143 decisions 4 and 5 — and each declined for the same reason: interpolation
forces a question the language had not answered, which is **what a hole may
hold**.

ADR-143 decision 4 stated the entanglement precisely:

> A universal `T.to_text()` is §8.1 interpolation's question, not this one, and
> it is entangled with ADR-085 decision 2: a rendering conversion defined on
> every type is the implicit conversion to `Text` that decision refused for `+`.
> Adding it as a rider on this change would settle that by accident.

That is the sentence this ADR exists to retire. The question is settled here, on
purpose, with the reconciliation written down — which is the thing "by accident"
would not have produced.

ADR-143 decision 5 also proposed a cheap route: with `Int`, `Float`, `Char` and
`Text` covered, `"a{n}b"` could desugar to `"a" + n.to_text() + "b"` and need no
new runtime path. That route is **not** taken, and decision 2 below says why: it
would make the set of things a hole can hold exactly the set of types that have
a `to_text()` row — four — and every other type would earn a diagnostic for
appearing in a position the program explicitly wrote it into.

## Decision

### 1. A hole holds a full expression, and it is a real subtree of the tree

`{a + b}`, `{p.0}`, `{xs.len()}`, `{m["k"]}` and `{if x { 1 } else { 2 }}` are
all holes. A hole is not a name slot.

The load-bearing half is not the grammar, it is the *representation*. The hole's
expression is lexed as ordinary tokens and parsed into an ordinary subtree, with
real token ranges, inside an `INTERP_EXPR` node. It is emphatically **not** one
opaque `TextLit` token that some later pass re-lexes.

The reason is [`crates/praxis-hir/src/capture.rs`](../../crates/praxis-hir/src/capture.rs).
Closure capture analysis finds free variables by scanning
`descendants_with_tokens()` and looking each token's **range** up in the
resolver's `refs` map. A name that is not a token at a real range in the lossless
tree is not a capture — so

```praxis
var f = |_| "{outer}"
```

would have allocated a closure with an empty environment and then read a slot
nothing filled. That is a silent wrong answer, not a compile error, and it is the
same defect class as handover 31 item 1 (a nested closure whose environment was
filled from an empty one) and as ADR-141's `"ab"[0]`. Re-lexing holes later is
the cheap implementation and it is the one that reintroduces the bug, which is
why the representation is a decision and not an implementation detail.

Everything that walks the tree therefore gets holes for free: name resolution,
inference, rename, semantic tokens, `N001` for an unknown name, and capture
analysis. Nothing had to be taught about holes twice.

### 2. A hole renders **any** type, exactly as `out` renders it

There is no bound on a hole's type. `"{v}"` on a `Vec[Int]` is `[1, 2, 3]`,
because that is what `out(v)` writes.

That guarantee is structural, not tested-and-found-to-agree. `out` lowers to
`RuntimeSymbol::WriteStdout`, whose wrapper `praxis_write_stdout` does
`value.format(&mut out)`. A hole lowers to `RuntimeSymbol::ValueToText`, whose
wrapper `praxis_value_to_text` does `value.format(&mut out)` and allocates the
result as a `Text`. One renderer — `GcRef::format`, dispatching through the type
descriptor — and two callers, which is exactly the shape ADR-143 decision 2
bought for the three scalar rows. A second renderer is not representable here for
the same reason it is not representable there.

This is what rules out ADR-143 decision 5's desugar. Routing a hole through
`to_text()` would make the hole's legal types the four with a `to_text()` row,
and would need a per-type dispatch in the lowerer that the descriptor already
performs at run time. Worse, it would let `out(v)` and `"{v}"` drift, which is
the property the whole arrangement exists to prevent.

### 3. This is not the implicit conversion ADR-085 decision 2 refused, and `"n = " + n` is still `Y001`

ADR-085 decision 2 refused an implicit conversion to `Text` for `+`:

> a language where `+` stringifies its other operand has no error left to
> report, and `1 + 2` inside a longer expression starts depending on what its
> neighbours are.

Both halves of that objection are about `+` specifically, and neither survives
the move to a hole.

**A hole is a rendering site the program wrote.** `"{v}"` is three characters
that exist for no other purpose than to render `v`; there is no other reading of
them, and no expression means something different because of what a hole nearby
holds. `+` is not that: `a + b` is arithmetic in every other type in the
language, and making it render would mean the compiler renders values the program
never asked to have rendered — which is exactly ADR-085's "no error left to
report".

So the two rules are complements rather than a contradiction:

| The program writes | It gets |
| --- | --- |
| `"n = " + n` | `Y001 expected Text, found Int` (ADR-085 decision 2, unchanged) |
| `"n = " + n.to_text()` | `Text` (ADR-143's explicit conversion) |
| `"n = {n}"` | `Text` (this ADR — the site is the request) |

`+` gains nothing here. It gets no new operand type, no coercion and no new
diagnostic. The gate is
`infer_tests::text_plus_an_int_is_still_y001_beside_a_hole_that_renders_it`,
which asserts both rows of the table from one source file, so an edit that
"unifies" the two by relaxing `+` fails a test whose name says what it broke.

The residual asymmetry is real and is accepted: `"{n}"` renders an `Int` and
`"" + n` does not. It is not an inconsistency, it is the distinction between a
site that names a rendering and an operator that does not.

### 4. `\{` is the escape, and a bare `}` is literal text

A `{` in a text literal now opens a hole. `\{` is a literal brace, joining the
six escapes in [`praxis_syntax::literal::decode_escape`] rather than inventing a
doubling rule (`{{`) that only interpolation would use — the language has one
escape table and it is shared with `'…'` (ADR-141). `\}` is accepted too, so a
brace pair can be written symmetrically; it is not required, because outside a
hole a `}` closes nothing and is unambiguous.

**And `{{` is refused rather than left to mean something else.** Choosing `\{`
has a trap the choice itself creates: `{{` is the escape in Rust, C# and Python,
so it is the first thing a reader tries — and here it *parses*. `{` opens the
hole, `{}` is an empty block, `}` closes the hole, so `"a{{}}b"` was a
well-typed program printing `aUnitb`, and `"a{{x}}b"` reported `N001` about a
name the author believed they had escaped. Neither says what went wrong.

So a hole whose expression opens with a brace is a parse error naming `\{`
(`P001`, no new code — the parse family already carries several messages). It
costs a block as a hole's whole expression, which nothing wants: a hole is a
rendering site, and `"{ var t = f(); t }"` is a sentence nobody writes inside a
string. A record literal is unaffected, because it opens with its type's name
rather than with a brace. `parse::tests::a_doubled_brace_is_refused_rather_than_meaning_a_block`
holds both halves — the four refusals and the three spellings that must stay
clean.

**No existing program changes meaning.** Every `.px` in the tree was scanned: no
double-quoted text literal anywhere in `docs/book/examples/`, `tests/` or the
crate fixtures contains a `{`. The braces that do appear next to string literals
are in backtick templates (`` `{a:int}` ``), which are a different token scanned
whole by `praxis_syntax::template::template_end` and are untouched by this — a
template's captures are the input-parser DSL's and share nothing with a hole but
the character.

### 5. The lexer enters interpolation mode only for a literal it has proved closes

A text literal may not span a line (that rule predates this and is why ADR-094
gave backtick templates the same one). So `eat_text` **pre-scans** the whole
literal with `praxis_syntax::interp::text_end` before it emits anything, and only
splits it into fragment tokens when that scan proves the literal closes on its
line with every hole balanced. A literal that does not close is one `TextLit`
token plus `T004`, byte for byte what it was before this change.

That ordering is what makes the lexer's mode stack safe. The stack is a brace
depth per open hole, pushed by a fragment token and popped by the `}` that closes
it; because it is only ever pushed for a literal already known to close, there is
no path on which a newline or EOF arrives with the stack non-empty. **No new
diagnostic code** was needed, and an unterminated interpolated literal reports
the same `T004` as an unterminated plain one rather than a novel cascade.

The scanner lives in `praxis-syntax` beside `template.rs` for the reason that
module's doc gives: two scanners that must agree about the extent of a run drift
immediately when they are two implementations. Here the second reader is the
lexer's own resume path, and both call the same function.

## Consequences

**What is bought.** §8.1 is implemented, and the design document's §4.13
workaround note ("a labelled line is `"n = " + n.to_text()`") is now historical.
A labelled line is `out("n = {n}")`. Because the hole is universal, the
`out(label); out(value)` pair that handover 31 recorded as the most frequent
irritant per hour of AoC writing collapses to one call **for every type**, not
just the four with a `to_text()` row.

**What it costs.**

- Three new token kinds (`InterpOpen`, `InterpMiddle`, `InterpClose`), one node
  kind (`INTERP_EXPR`), one AST wrapper, one `Expr` variant, one `TypedExpr`
  variant, one MIR lowering and one runtime wrapper.
- A lexer mode stack, which the lexer did not have. Decision 5 is the bound that
  keeps it from being a source of new failure modes.
- One intermediate `Text` allocation per hole: the parts are folded left with
  `praxis_text_concat`, the same wrapper `+` calls. That is what
  `"a" + n.to_text() + "b"` already cost, so interpolation is not worse than the
  spelling it replaces — but it is not the single-allocation build a dedicated
  wrapper could do, and that is recorded rather than done. The part count is
  bounded by the source text, so this is a constant factor and not the quadratic
  accumulation ADR-085's consequences warn about for `+=` in a loop.

**No ABI version bump.** No `#[repr(C)]` type changed. Appending a manifest
symbol has never been a bump (ADR-085 set that precedent for
`praxis_text_concat`; ADR-143 followed it).

**`praxis_value_to_text` is `Allocates`, not `AllocatesAndFaults`.** Every
`GcRef` has a descriptor and every descriptor has a `format` callback, so there
is no value it can be handed that it cannot render; and a `String` built from
`format` is valid UTF-8 by construction, so there is nothing for an `InvalidText`
fault to check. This is `praxis_text_concat`'s row exactly.

**ADR-085 decision 2 and ADR-143 decisions 4 and 5 carry dated amendment notes
rather than deletions**, for the reason ADR-143 gave when it amended ADR-085: a
reader must meet the reversal where they meet the reason. ADR-085 decision 2 is
*unchanged* — `+` still refuses — and is now stronger for having a stated
boundary instead of a slippery slope.

**The gates.**

- `lex::a_name_in_a_hole_is_a_token_at_its_own_range` and
  `capture::a_hole_in_a_closure_body_captures_the_name_it_holds` are decision 1's
  gates. The second is written against `capture.rs` directly, so it fails against
  any implementation that re-lexes holes — which is the whole point of writing it.
- `jit::a_hole_renders_what_out_renders` walks the interesting types (a `Vec`, a
  tuple, a record, an enum variant, a `Float`) and asserts the hole's characters
  equal `out`'s, rather than equal a literal. It cannot pass while the two
  renderers disagree.
- `infer_tests::text_plus_an_int_is_still_y001_beside_a_hole_that_renders_it` is
  decision 3's gate, described above.
- `lex::an_unterminated_interpolated_literal_is_one_text_lit_and_t004` is
  decision 5's: it asserts the *fallback*, which is the half a later edit would
  quietly lose.
