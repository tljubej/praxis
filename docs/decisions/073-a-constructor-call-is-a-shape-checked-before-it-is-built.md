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
argument list has exactly the shape §7.5 gives that constructor and there is
nothing left for it to drop. The `_ => {}` arms in `build_block`, `build_choice`
and `build_sections_named` are gone, not converted to `unreachable!()`: their
inputs are already shaped.

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
