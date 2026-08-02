# Milestone 11 handover — the language server, the extension, and the highlighting

**Date:** 2026-08-01
**Status:** M11 complete; `just ci` green
**Supersedes:** `19-milestone-11-plan.md`
**Predecessor:** `18-every-row-closed-handover.md` (the repair register is empty
and stays empty — M11 opened no rows)

> For a fresh context, read in this order: ADR-095 through ADR-098 (they are the
> shape of the milestone), then `crates/praxis-lsp/src/lib.rs`'s module map, then
> `crates/praxis-lsp/tests/queries.rs` — which is §19.11's five acceptance
> criteria written as assertions.

## 1. What shipped

All ten §19.11 deliverables, all five acceptance criteria, and §6 of the plan's
addition (a TextMate grammar, so a `.px` file is highlighted before the server
attaches).

| Deliverable | Where | Gate |
|---|---|---|
| `praxis lsp` process | `praxis-lsp::server`, `main.rs` | `crates/praxis-cli/tests/lsp.rs` — a scripted JSON-RPC session over a pipe, driving the real binary |
| Document sync, incremental revisions | `praxis-lsp::document` | an edit changes what hover answers, end to end |
| Diagnostics | `praxis-lsp::diagnostics` | code **and** span asserted on introduce and retract; plus the manifest gate below |
| Hover | `praxis-lsp::hover`, `Analysis::hover_parser` | the inner constructor's own type, not the root's |
| Completion incl. receiver methods | `praxis-lsp::completion` | only grid methods, with signatures, and a `Map` method's **absence** |
| Signature help | `praxis-lsp::signature` | the active parameter per position, across a comma |
| Go-to-definition | `praxis-lsp::navigation` | the **second** of two shadowed declarations |
| Document symbols | `praxis-lsp::navigation` | `fn`/`struct`/`enum`/bindings, with fields and variants nested |
| Semantic tokens | `praxis-lsp::semantic` | four distinct (range, type) pairs |
| Thin VS Code extension | `editors/vscode/` | argv unit test + a Rust drift gate + a written-down manual check |
| TextMate grammar | `editors/vscode/syntaxes/` | four drift gates reading the grammar at test time |

Four ADRs landed with the code that implements them: **095** (a synchronous
stdio loop), **096** (positions convert at the protocol boundary), **097** (the
shared query layer lives in `praxis-lsp` and `praxis check` routes through it),
**098** (the parser AST is retained by inference).

## 2. The five things worth not rediscovering

### 2.1 The plan's §8 measurement was right, and it saved WS5

`grid.` is not an expression, and the plan measured — rather than estimated —
that the receiver's type is nonetheless in `Analysis` by the time completion is
asked. It is: the parser leaves a complete `PATH_EXPR` immediately before the
`DOT`, and `expr_types` has its concrete type. `receiver_before_dot` walks left
from the cursor and reads it. **No parser recovery was written and no
speculative edit was needed**, exactly as the plan said. If a later milestone is
tempted to rewrite recovery for completion, the reason not to is in
`crates/praxis-lsp/src/completion.rs`'s own doc comment.

### 2.2 The capture *name* needed a span, and giving it one found a real bug

ADR-098 says the parser AST is retained. It does not say — and the plan did not
foresee — that `TemplatePart` had **no spans at all**. The capture type is
`parser.span()` and was already there, but the capture *name* and the literal
runs were not, so §19.11 criterion 4's four distinct ranges were three.

`TemplatePart::Literal` and `::Capture` now carry spans, filled where the
scanner already knows them. Writing the gate found the bug the ADR predicted in
the abstract: the first implementation computed the name's span from
`capture_extent`'s returned *body* offset, which is the position **after the
colon** — so `{name:word}` reported its name as `word`, one token to the right.
`capture_extent` returns the name's own offset now. The lesson is the ADR's: an
offset derived from a different offset is a second implementation of the same
question.

### 2.3 A per-document revision counter is a stale-cache bug waiting

The query cache keys on `(uri, revision)`. A revision counter that starts at
zero **per document** makes a re-open — `didClose` then `didOpen`, or an editor
reloading from disk — produce revision 0 twice with different text, and the
analyzer hands back the previous analysis. `Revision::next()` is process-wide
for that reason, and `reopening_a_uri_is_a_new_revision` is the gate. This was
found by a test failing, not by review.

### 2.4 The catalog has rows that are not names

`grid.` completion filtered the catalog by receiver and offered everything that
matched — including `[]`, `[]=`, `[]min=` and `[]max=`, which are the subscript
and updating-store **operators** (§6.2, REP-16/REP-21). They are catalog entries
because dispatch goes through the catalog for them too. `grid.[]` is not syntax.
`completion::is_operator` filters them, reading the four names from
`praxis_stdlib::catalog`'s constants rather than spelling them again.

### 2.5 The CRLF interior is the one byte offset an LSP position cannot name

ADR-096's property test asserts `byte → position → byte` is the identity. It is
not, at exactly one place: the byte between a `\r` and its `\n`. Both bytes
belong to the terminator, which *ends* a line rather than living in one, so
`(line, character)` addresses the byte before the `\r` and the byte after the
`\n` and nothing between. The rule lives on `PositionMap::is_addressable` and
the property is stated against it, rather than being restated in each test —
which is the house rule about stating a rule where it is enforced.

### 2.6 A synchronous loop can still honour `$/cancelRequest` — but only if it batches

ADR-095 says cancellation is "honored by dropping the queued request". A loop
that blocks on `recv()` and serves each message as it arrives **cannot do that**:
a cancel always arrives *after* the request it names, so by the time the loop
sees it the request has already been answered, and cancellation is a no-op
dressed up as a feature.

The loop therefore blocks for the first message and then `try_recv`s everything
already queued behind it. A cancel that arrived while an earlier request was
being served is in that batch, ahead of the request it cancels being
*processed* — so the drop is real. This is what makes fast typing cheap: several
superseded hover and semantic-token requests can pile up during one analysis and
all be dropped together.

The same loop also refuses requests that arrive after `shutdown`, which is the
protocol's own rule and one line in the same place.

## 3. What §15.5's targets measured

Engineering targets, not language semantics, so they are **measured and
reported** rather than asserted. Numbers from a **debug** build (`cargo build`,
not `--release`) on the author's machine, 20 runs each, against
`tests/aoc-corpus/day02_grid_of_char.px` (13 lines) and
`rep15_iterating_every_collection.px` (104 lines, the corpus's largest):

| | day02 (13 lines) | rep15 (104 lines) |
|---|---|---|
| `praxis check`, whole process | 3.9 ms median | 3.9 ms median |
| `initialize` handshake | 2.5 ms | 1.6 ms |
| `didOpen` → `publishDiagnostics` | 161.7 ms | 163.3 ms |
| `textDocument/hover` round trip | 0.1 ms median | 0.1 ms median |
| `semanticTokens/full` | 0.2 ms (12 tokens) | 0.8 ms (174 tokens) |

Reading them against §15.5's four targets:

- **Syntax diagnostics for a typical AoC file should feel immediate.** They do:
  the whole front end is ~4 ms in a debug build, and the 150 ms in the
  `didOpen` → diagnostics figure is the **debounce**, deliberately. The analysis
  itself is the ~12 ms remainder.
- **Local type diagnostics should update without checking unrelated
  functions** — *partially met, and stated rather than claimed*. `analyze_root`
  is whole-file; there is no per-function inference cache. At one-file AoC scope
  the whole file is the unit and it costs ~4 ms, so the target's *intent* is
  met while its literal mechanism is not. A per-function cache is M13's if
  measurement ever asks for one.
- **Completion should use cached receiver types.** It reads `expr_types` off the
  memoized `Analysis` and never re-infers — which is why a hover after the first
  request costs 0.1 ms rather than 4 ms.
  `two_queries_at_one_revision_parse_once` is the proof the memoization is real
  rather than the timing being lucky.
- **The server must remain responsive when the JIT runtime is not available.**
  By construction: `praxis-lsp` does not depend on `praxis-mir`,
  `praxis-codegen-cranelift` or `praxis-runtime`, and
  `crates/praxis-lsp/tests/no_jit.rs` reads the manifest and says so.

## 4. What is deliberately not here

**M12 (§19.12), not started:** find references, rename, workspace symbols,
inlay hints, the formatter, code actions, and hover documentation for methods
and parsers. The server does **not advertise** any of their capabilities —
`the_handshake_completes_and_advertises_the_m11_capabilities` asserts their
absence, because advertising a capability the server does not implement makes
the editor stop offering its own fallback.

**Also out, and named in the plan:** semantic token *deltas*, multi-file and
workspace indexing, and incremental **reparse**. §19.11 says "incremental source
revisions", which is the document store, not the parser.

`praxis watch` and `praxis repl` remain unimplemented. The extension's
`Praxis: Watch File` command exists and surfaces `watch`'s own "not implemented"
message; its command title says so, because hiding the command would be a
quieter lie.

## 5. The three approximations, stated where they are made

Each is a place the implementation is knowingly less than exact. None is a
defect; each is written down so a later reader does not "fix" it by accident.

1. **The grammar's parser region.** `lines` is a parser constructor inside a
   parser expression and an ordinary identifier outside one, and a regex grammar
   cannot tell. The grammar scopes constructor-shaped calls inside a `read`
   region; **`parse(text, P)`'s second argument is not covered**, because
   finding it means counting commas. The semantic layer supplies the parser
   classes there. This is §6.3's sanctioned disagreement and it resolves toward
   the compiler.

2. **Lexical completion's visibility test.** `ScopeTree` is keyed by `ScopeId`,
   not by span, so "which scope is this offset in" is not a question it can
   answer. Lexical completion offers every symbol and lets the prefix filter,
   which is generous rather than wrong. A span-keyed scope tree would make it
   exact; nothing in M11 needed it.

3. **The `.` that swallows the next line.** The plan's §8 residual risk is real
   and unchanged: `grid.` followed by `out(grid)` parses as one
   `METHOD_CALL_EXPR` across both lines and reports `Y110` on the line below.
   **This is ADR-049 working**, not a parser bug — a newline ends a statement
   but never an expression. M11 pays it the debounce (§15.2), which is what the
   plan owed it. The second half — whether the server suppresses diagnostics on
   the statement containing the cursor while it is syntactically incomplete —
   was **not** taken: it is a decision about what an editor shows rather than
   about the language, it needs a cursor the `didChange` notification does not
   carry, and doing it wrong hides real errors. It is registered here rather
   than decided.

## 6. The manual check the milestone owes

Criterion 5's last mile needs a VS Code host, so it is written down in
[`editors/vscode/README.md`](../../editors/vscode/README.md) §"The manual check"
— seven steps, from `F5` to `Praxis: Restart Language Server`. The automated
half is two tests: `argv.test.ts` (the extension's own toolchain) and
`crates/praxis-cli/tests/grammar.rs`'s
`the_extensions_argv_names_only_subcommands_the_cli_has`, which reads `argv.ts`
as text in `just ci` and checks every subcommand and flag it names against
`praxis --help`'s own surface. That is the drift that matters — the CLI renaming
something the extension invokes — and it is caught with no Node toolchain in CI.

## 7. Where the grammar is not tested

The four drift gates check the grammar's **word lists** against the compiler's
closed tables. They do not *execute* the grammar: matching a TextMate pattern
needs an Oniguruma-compatible engine, and the number rules use lookaround that
Rust's `regex` crate does not support.

The number rules were verified by hand against the cases §6.3 names — `1..2` and
`1..=9` produce two integers and no floats, `.5` and `2.` and `1e10` and `3.14`
each produce one float — and the verification is a manual one. A
`vscode-tmgrammar-test` snapshot suite is the right home for it, at M14 when the
extension is packaged and a Node step exists anyway.

## 8. Register

No open rows. M11 closed the one front-end change it needed (ADR-098), took the
three approximations in §5 knowingly, and registered one decision it did not
take: **§5.3's second half** — whether the server suppresses diagnostics on the
statement containing the cursor while it is syntactically incomplete.

The two standing decisions from handover 18 are untouched: **REP-67** (the
`praxis_alloc_text` split) and **D19** (whether there is a character literal).
If D19 is answered, §6.4's gate 1 catches the grammar half automatically — a new
keyword joins `SyntaxKind::all_keyword_texts()` by construction and the gate then
requires the grammar to learn it.
