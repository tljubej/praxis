# ADR-098: The parser AST is retained by inference, and per-node types with it

**Date:** 2026-08-01
**Status:** accepted
**Milestone:** 11 (language server MVP)

## Context

`praxis_hir::parser_lower::synthesize_parser_type` is the whole of what the LSP
path knows about a `read`/`parse` body. It converts the rowan `PARSER_EXPR` into
a `praxis_input_parser::ParserAst`, validates it, synthesizes **one** `Type` for
the root, and **drops the AST on the floor**. `convert_parser_expr` is private;
its only escape hatch is `convert_parser_expr_for_test`, behind `#[cfg(test)]`.

So at `bcc5319` there is no data source for four of M11's deliverables:

- hover on an *inner* constructor (§15.3: "hovering a parser expression should
  show its synthesized result type" — a parser expression, not only the root);
- completion inside `{…}` or after `read`;
- the four parser semantic-token classes (§19.11 acceptance criterion 4);
- §15.3's five-way question — is the cursor outside a parser expression, in
  parser-expression mode, in a template, inside a capture, or inside an atomic
  name.

`synthesize` recurses to answer the root's type and discards every intermediate
result on the way back up, so per-node types are not recoverable from the root
either.

## Decision

**`Analysis` gains a spanned parser index, built where the AST already exists.**

For each `read`/`parse` expression in the file, inference records:

- the converted `ParserAst` (whose every node already carries an **absolute**
  span — §7.10, ADR-078 — so no rebasing happens at the boundary), and
- the synthesized `Type` of **each** node, keyed by that node's span.

Both are filled inside `synthesize_parser_type`, at the one place the AST is
built and the one place `synthesize` runs. There is no second walk and no second
conversion.

## Why here, and not in the LSP

Because the alternative is a second scanner over template interiors living in
the `praxis-lsp` crate — and handover 17's last section is about exactly that
failure mode: two hand-written scanners, each stating the whitespace rule in its
own words, five review rounds chasing the disagreement, closed by deleting both
and putting the rule below the crates that needed it.

The same shape would recur here. To answer "is the cursor inside a capture, and
which one", the LSP would have to find capture boundaries in a backtick token.
The compiler already knows where every capture ends — `scan_template` computed
it and `shift_part_spans` rebased it onto the file. A second implementation in
TypeScript-adjacent territory would be free to disagree with the compiler about
where a capture ends, and the symptom would be a hover that highlights the wrong
half of `{n:int}`.

With the index, §15.3's five-way question is a lookup against spans the compiler
computed, and **cannot** disagree with the compiler.

## Shape

```text
Analysis.parser_exprs: Vec<ParserIndex>

ParserIndex {
    span: TextRange,             // the read/parse body's own span
    ast: ParserAst,              // absolute spans throughout (ADR-078)
    node_types: Vec<(Span, Type)>,  // one entry per AST node, innermost-last
}
```

`node_types` is a vector rather than a map because the lookup is "the *smallest*
span containing the cursor", which is a scan with a min, not a hash hit — and
because two AST nodes can legitimately share a span (a single-capture template's
`Template` and its capture's parser do not, but a `Constructor` and its only
argument can when the source is `` `{int}` `` nested one deep). A map would lose
one of them; a vector keeps both and the innermost wins.

## Cost

Retaining the AST is retaining what conversion already built and then freed:
one `ParserAst` per `read`/`parse` in the file, of which a typical AoC program
has one. The per-node type vector is one `(Span, Type)` pair per AST node —
`Type` is an interned handle, so no type is minted that inference did not
already mint.

**This is not `PLAN_ARENA`.** The process-global, append-only plan arena is
reached only through `register_plan`, which is called only from
`parser_lower::analyze_parser_expr`, which is called only from `lower.rs` —
typed-HIR lowering, which the LSP never runs. The index lives on `Analysis` and
dies with it, so a long-lived server does not accumulate one per keystroke.

## Gate

Hover an inner constructor in

```praxis
let v = read sections(lines(`{a:int},{b:int}`))
```

and get **that node's** type — `Vec[{ a: Int, b: Int }]` for `lines(…)` — not
the root's `Vec[Vec[{ a: Int, b: Int }]]`. Observed red before the index exists:
without it there is nothing to look up and hover on the inner span answers the
root, or nothing at all.

## Consequences

- `Analysis` is no longer `Copy`-cheap to clone; it never was.
- A `read` whose body fails to convert records no index entry, and hover over it
  answers nothing rather than a stale one. Diagnostics are unchanged: the same
  conversion errors are pushed by the same code.
- WS4, WS5 and WS8's parser halves each become a lookup. Their gates
  (inner-constructor hover, capture-type completion, four distinct semantic
  token ranges) are red without this ADR and green with it, which is why it
  lands before them.
