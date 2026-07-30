# ADR-072: A template capture body is a parser expression, and the scanner parses it

**Date:** 2026-07-31
**Status:** Accepted — implemented
**Milestone:** Repair (stage S19 — IP-01…IP-06, decision D10)

## Context

```praxis
read `{name:word},{port:int}`
```

Both captures were `Text`. Not because `int` was misread — because **no capture
body was read at all**.

`scan_template` split a capture into `(name, body)` and bound the body to `_`.
In its place it stored a placeholder:

```rust
let (name, _parser_body) = split_capture(body);
parser: Box::new(ParserAst::Atomic {
    kind: AtomicKind::Int,          // "the HIR overwrites it"
    span: Span::at(start as u32),
})
```

The HIR did try to overwrite it, with `extract_capture_kind(&template_text,
&name)` — a function that rescans the **whole template from the beginning**,
returns the first atomic name it recognizes, and never looks at `name` (the
parameter is spelled `_name`). Every capture in a template therefore got the
same parser, and a template it recognized nothing in got

```rust
AtomicKind::Int // default
```

so `read \`{value:intr}\`` compiled and typed `Int`.

The reason the body was thrown away is visible in the same function: the
`}`-scan was `while i < bytes.len() && bytes[i] != b'}'`. A body containing a
`}` — or a `(`, or a `,`, or a string — could not be found at all, so the
grammar a capture could hold was whatever survived a scan to the first `}`.
That is the shape of the question D10 asked.

## Decision 1: a capture body is a **full** parser expression

`{items:csv(int)}`, `{x:optional(int)}`, `{s:sep("-", int)}`,
`` {g:choice(Pt: `{x:int},{y:int}`, Name: word)} `` are all legal.

"Atomics only" was never the smaller language. **§7.7's own monkey example
writes `` `  Starting items: {items:csv(int)}` ``**, and §7.8's derivation table
gives a result type for every parser including the nesting ones — so restricting
the body would have produced a language that cannot run the design document's
text. `synthesize`'s `template_type` already recursed into each capture's own
parser; it had simply never been given one.

The middle option — atomics plus one call level — buys nothing. Tracking brace
depth to 1 and tracking it to *n* is one loop with one counter, and refusing
depth 2 would then be an arbitrary rule needing a diagnostic of its own.

## Decision 2: the body is parsed **in `praxis-input-parser`**, not handed back to the grammar

ADR-023 fixes the direction: `praxis-input-parser → praxis-types`,
`praxis-runtime → praxis-input-parser`. Re-lexing a capture body through
`praxis-parser` would have inverted it.

So `body.rs` is a hand-written recursive-descent parser over the same text. What
keeps it honest is that it does **not** own the argument grammar: it produces a
`CallArg` list and hands it to `call::build_call`, which is the *same* function
the rowan bridge in `praxis-hir` calls (ADR-073). There is one shape table and
one builder, so the capture-body grammar and the constructor-call grammar cannot
drift — which is the failure this factoring exists to prevent, because they are
the same grammar written twice otherwise.

## Decision 3: the extent of a capture is found by depth, and the depth is bounded

The `}`-scan becomes a cursor that tracks brace depth, paren depth,
double-quoted-string state and backtick state. Three consequences worth naming:

- `{c:one_of("}")}` works. A `}` inside a string does not end the capture.
- `{g:choice(A: word, B: int)}` is a *named* capture called `g`, not an
  anonymous one: the name split fires only on a `:` at depth zero, so the
  colons inside the call are not candidates.
- An unbalanced `(` or `"` is a reported error rather than a body silently cut
  short.

`scan_template` and `parse_capture_body` are **mutually recursive** now, which
is new: the old scan could not recurse at all. `"{a:" + "{".repeat(100_000)` is
therefore adversarial input, and a compiler may not answer adversarial input
with a stack overflow. `praxis_syntax::MAX_TEMPLATE_NESTING` (32) bounds it,
with its own `ScanError` variant.

## Decision 4: the lexer learns the same rule, and shares the same bound

D10's answer does not mention the lexer, and the lexer is where it costs
something. `Lexer::eat_template` ended the `BacktickTemplate` token at the first
unescaped backtick, so

```praxis
read `{g:choice(Pt: `{x:int},{y:int}`, Name: word)}`
```

lexed as three unrelated token runs and `scan_template` never saw the template
the source wrote. A backtick now closes the token only at **brace depth 0**;
inside a capture it opens a nested run.

The nesting bound lives in `praxis-syntax`, which both crates already depend on,
rather than being written twice. The lexer's run and the scanner's recursion are
driven by the same file text: if they refused at different depths, one of them
would accept input the other cannot read. `praxis-parser` gains **no** dependency
on `praxis-input-parser`.

## Decision 5: a capture name is the language's identifier, and there is no default kind

Two rules that were local copies become the shared ones.

`CaptureName::parse` delegates to `praxis_syntax::ident::is_ident` (F3). The
scanner's private ASCII rule meant `{λ:int}` was not recognized as a named
capture *at all* — the whole body `λ:int` was reinterpreted as the parser
expression, which then failed for an unrelated reason. A name a consumer cannot
accept is now **reported** (`I011`), never rewritten into a different name.

And there is no `AtomicKind::Int` fallback. A body naming nothing is
`UnknownCaptureKind` (`I012`); a body calling an unknown constructor is
`UnknownConstructor` (`I013`). `ScanError::code()` is an exhaustive match with no
wildcard, which is what stops the next variant from silently inheriting the
generic `TemplateScan` (`I030`) the way all of them used to.

## Consequences

- **`{name:word},{port:int}` is `{ name: Text, port: Int }`.** Six JIT tests
  exercise that template through record schema caching, `Set` hashing and GC
  survival; they assert lengths and equality and stayed green, but the field
  descriptors they run through are different now.
- **A template whose capture kind was unrecognized used to mean `int`; it now
  fails the compile.** Every `.px` under `tests/` is covered by
  `every_corpus_program_runs_and_prints_the_answer_it_documents`, which runs
  each one with its real input — `praxis check` alone would not have seen it.
- **Spans are rebased, not invented.** The scanner works in offsets relative to
  the template's interior; `Span::shifted` and `ParserAst::shift_spans` move the
  tree onto the file by the token's start + 1, so a capture's diagnostic carets
  the capture instead of the top of the file.
- **No new diagnostic code.** I011, I012 and I013 were allocated by ADR-051 and
  constructed nowhere in the tree; they are constructed now.
