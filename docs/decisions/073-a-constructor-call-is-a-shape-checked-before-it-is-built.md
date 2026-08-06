# ADR-073: A parser constructor call is a shape, checked before anything is built

**Date:** 2026-07-31
**Status:** Accepted — implemented
**Milestone:** Repair (stage S19 — IP-07, IP-08, IP-09, IP-10)

## Context

```praxis
read optional(int, word)
read frobnicate(int)
read sections(boards: repeated(matrix(int)), draws: csv(int))
read sep("", int)
```

All four compiled. The first ran as `optional(int)`; the second ran as nothing
at all, with no diagnostic; the third ran as a *different* parser than the one
written; the fourth reached a runtime loop that cannot advance.

They are one defect. `Constructor` knew six names — §7.5 has fourteen — so the
eight M9 constructors were dispatched **ahead** of the arity table by a chain of

```rust
if ctor_name == "optional" {
    if let Some(CallArg::Parser(child)) = args.into_iter().next() { … }
    return None;
}
```

`args.into_iter().next()` takes the first argument and drops the rest. A
constructor with no row in the table had no arity, so it had no arity *error*
either: the fall-through was `Constructor::from_keyword(&ctor_name)?` — a `?` on
an `Option`, which returns `None` from a function whose `None` means "already
reported".

And the check that did exist could not have caught the rest of it. It compared
one number, `positional_arity`, against another, `expected_arity`. A count
cannot say that `sep`'s first argument is a **string**, that `choice` takes no
positional argument at all, or that `grid`'s `ragged` and `fill:` come together.

## Decision 1: the table states the **shape**, not a count

`Constructor` is the whole of §7.5 — fourteen names — and `expected_arity() ->
usize` is replaced by `arg_shape() -> ArgShape`:

| Shape | Constructors |
|---|---|
| `Positional(1)` | `lines`, `csv`, `ws`, `matrix`, `optional`, `scan`, `repeated` |
| `StringThenParser` | `sep` |
| `OneString` | `one_of` |
| `ParserWithSkip` | `chars` |
| `GridMaybeRagged` | `grid` |
| `OnePositionalOrNamed` | `sections` |
| `Items` | `block` |
| `NamedOnly { at_least: 1 }` | `choice` |

`check_call(ctor, &[ArgKind], span)` returns **every** problem, and it runs
before a single node is built — so by the time a builder arm executes, the
argument list has exactly the shape §7.5 gives that constructor.

*Amended 2026-07-31.* This decision first claimed "there is nothing left for it
to drop", and the claim was false in two ways at once.

`CallArg::Keyword{name}` and `CallArg::Named{name}` projected onto the **same**
`ArgKind::Named(name)`, so `check_call` could not tell a `skip:`/`fill:` keyword
from a named parser; `block`, `choice` and named `sections` accepted the keyword
as a well-shaped named argument and their builders' `filter_map` then threw it
away. And both front ends minted a keyword from the argument's *name alone*
(`if name == "skip" || name == "fill"`), with no reference to the constructor
being called. So `read sections(rules: lines(int), fill: lines(int))` compiled
to a record with one field, and reported nothing.

A keyword belongs to a constructor, so the constructor answers the question:
`Constructor::keyword_arg()` gives `chars` a `skip:` and `grid` a `fill:`, and
nothing else has one. A `sections` field or a `block` item called `fill` is a
field. `ArgKind::Keyword(String)` is then its own kind, accepted only by
`ParserWithSkip` and `GridMaybeRagged` and refused everywhere else — including
by named `sections`, whose rejection list was a positive list of three kinds and
is now "anything that is not a named parser or the tail".

The `_ => {}` and `filter_map` arms are gone from every builder, and where an
argument still cannot be placed the builder **reports** it rather than dropping
it. `grid`'s `ragged` flag carries nothing and has its own arm, so discarding it
is a decision rather than a leak. A `_ => {}` in a builder is how an argument
disappears; the property worth stating is not that the arm is unreachable but
that if it ever is reached, it is visible.

Three codes that ADR-051 had allocated and nothing constructed now fire:
`UnknownConstructor` I013, `InvalidConstructorArgument` I014 (a wrong argument
*kind*, and an unrecognized `skip:` policy — which used to leave the default
`whitespace` silently in place), and `MisplacedRepeatedTail` I028.

`ragged` becomes a real argument (`CallArg::Flag`) rather than a token the
extractor skipped by text comparison. That is what lets the table require it:
`grid(P, fill: 0)` used to *become* the ragged parser on the strength of the
`fill:` alone, which is not what §7.5 spells.

## Decision 2: one table and one builder, shared across the two front ends

There are two places a `sep(",", int)` is constructed: the rowan bridge in
`praxis-hir`, walking `read sep(",", int)`, and the capture-body parser in
`praxis-input-parser`, reading the text of `{xs:sep(",", int)}` (ADR-072).

Both produce a `CallArg` list and both call `call::build_call`. The alternative
— each front end building its own AST — is two copies of §7.5, and the shape
rules would drift the first time one of them was extended. `build_call` lives in
`praxis-input-parser` because that is the crate both can reach; `praxis-hir`
keeps only rowan→`CallArg`.

*Amended 2026-07-31.* The claim held for every call **except** the one written
as a `sections` tail marker, which is the one place the bridge did not go
through `build_call` at all. It unwrapped `name: repeated(P)` with a `find_map`
that returned the first parser-expr child of the argument list and ignored the
rest, so `repeated(matrix(int), word, int)` lowered as `repeated(matrix(int))`
with two arguments silently gone and `repeated()` produced no diagnostic at all
— while the identical text inside a capture body was rejected with I022. §7.5's
rule for the marker (exactly one argument, and it must be a parser) is now
`call::build_repeated_tail`, beside `build_call` and called by both front ends;
the bridge reads the marker's whole argument list through the same
`extract_call_args` it uses for any other call. Pinned by
`both_front_ends_apply_one_repeated_tail_rule`, which asserts every case through
both spellings and on the same `DiagCode`.

*Amended again 2026-07-31.* Decision 2's claim held for the argument *list* and
not for a keyword argument's **value**, and there the two front ends disagreed
on identical source text.

`grid(P, ragged, fill: <literal>)` is §7.5's own spelling. The rowan grammar had
no shape for a literal after `name:` — it called `parse_parser_expr`, which
cannot parse `0` — so the call reported `P001 expected a parser expression`, and
the bridge, reading the value as `ParserExpr::text().unwrap_or_default()` (the
first `Ident` in the subtree, of which a `PARSE_ERROR` over an `IntLit` has
none), turned the absent value into `""`. A `GridRagged` padded with the empty
string was built and registered in silence. The capture-body front end reads a
keyword value as raw text and kept the `0`.

A keyword argument's value is now a grammar shape of its own —
`PARSER_KEYWORD_VALUE`, holding the raw literal token — and *whether the
constructor has a keyword of that name* is still `Constructor::keyword_arg`'s
question, asked where it already lives. `unwrap_or_default` is gone: a value the
AST cannot read is reported (`I014`), not laundered.

Two smaller disagreements fell out of measuring the pair, and both are fixed in
the one shared place:

- `fill:` was the only parser string literal nothing ever decoded, so `fill: "-"`
  reached the plan as `"\"-\""`. `build_call` decodes it, beside decision 5's
  one decoder.
- `body::take_keyword_value` searched for the argument delimiter without
  honouring quoting, so `fill: ","` ended at the comma **inside** the literal
  and the scanner reported `unterminated string literal` for text that is not
  malformed — while the rowan front end accepted the same call.

And the value is now part of the shape. `check_call` answers from `ArgKind`s,
which carry a keyword's name and not its value, so `grid(P, ragged, fill:)` was
accepted by the capture-body front end with **zero** diagnostics. `chars`'s
`skip:` has always checked its value; `fill:` checks its own now, in
`build_call`, under decision 4's rule one field over — an empty pad fills
nothing, exactly as an empty separator never advances.

## Decision 3: `repeated(...)` is the tail marker, and its position is checked

§7.5: "`repeated(parser)` may appear only as the final named argument." Three
things followed and none was checked.

- Its **name** was not a field name. `validate`'s duplicate set was built from
  `fields` and the tail's name was bound to `_`, so
  `sections(items: lines(int), items: repeated(int))` synthesized a record with
  two fields called `items`.
- Its **position** was not its position. The tail was appended wherever it was
  found and lowering always emits it last, so a misordered call compiled into a
  different parser — silently, since the reordered parser is well-formed. A tail
  cannot be followed by anything: it consumes every remaining section, so the
  field after it can never match.
- There could be **two**. `repeated_tail = Some(...)` in a loop, so the second
  overwrote the first.

All three are `I028`, and `build_call` refuses to build rather than building
something else. A bare `read repeated(int)` is the same code: it is a marker,
not a parser, and there is nothing outside `sections` for it to repeat over.

*Amended 2026-08-06 ([ADR-140](./140-a-counted-repeated-is-bounded-so-something-can-follow-it.md)).*
The position rule above is the **unbounded** `repeated(P)`'s, and the argument
for it is greed: a field after a parser that takes every remaining section can
never match. `repeated(P, N)` takes exactly N and leaves the rest, so it is a
named argument like any other — it may sit anywhere, including last, and only
the uncounted form is `I028` out of position. The name rule and the at-most-one
rule are unchanged for the unbounded form. `build_repeated_tail`'s hand-rolled
`args.len() != 1` is also gone: it now routes through `check_call` like every
other builder, which is Decision 1's own thesis applied to the one call it
exempted.

## Decision 4: a separator that cannot advance has no representation

`sep("", P)` is not a parser that matches nothing. It is a cursor that never
moves: `walk_sep` asks `region[pos..].starts_with(sep_bytes)`, which is
**unconditionally true** for an empty needle, so `pos += sep_bytes.len()` is
`pos += 0` and the loop pushes a freshly allocated value forever.

`praxis-hir` was manufacturing it. Its `sep` arm read the first argument and,
when it was not a string literal, wrote `String::new()` — so a call with no
separator at all *became* the separator that hangs.

The fix is the type. `Separator` has exactly one constructor and it refuses the
empty string, so `ParserAst::Sep` cannot hold one and no future construction
site can forget a rule a `validate` arm would have carried. `walk_sep` keeps a
`debug_assert` naming what it relies on. This is `AGENTS.md`'s maxim in its
plainest form: a check can be forgotten by the next caller, a type cannot.

## Decision 5: one text decoder for the workspace

A constructor's string literal was decoded by
`raw.trim_start_matches('"').trim_end_matches('"')` — a second decoder beside
`praxis-hir`'s `unquote_text`, which never unescaped and which stripped a *run*
of quotes at each end. `sep("\t", int)` split on the two characters `\` and `t`;
`one_of("\"")` was broken; `sep("\"\"", int)` lost both real quotes and became
the empty separator decision 4 has just made unrepresentable.

`unquote_text` moves to `praxis_syntax::literal`, which the HIR lowerer, the
input parser and the capture-body parser can all reach. Two decoders in one
crate was the finding; three across two crates would have been the fix's own
version of it.

## Consequences

- **No new diagnostic code and no ABI change.** Every case maps to a code
  ADR-051 had already allocated: I013, I014, I022, I023, I024, I028.
- **A wrong argument now stops the compile.** Programs that relied on an
  argument being dropped will report. Every `.px` under `tests/` is run with its
  real input by `every_corpus_program_runs_and_prints_the_answer_it_documents`.
- **`check_call` returns a list, not an `Option`.** A call with two mistakes
  reports both, which is what §17.1 asks of every other pass.
- **Adding a constructor now requires deciding its shape.** `Constructor::ALL`
  is swept by a test, and `arg_shape` has no default arm.
