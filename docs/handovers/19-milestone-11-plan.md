# Milestone 11 plan — the language server MVP, the VS Code extension, and its highlighting

**Date:** 2026-08-01
**Tree:** `bcc5319` (every claim below about existing code was read there)
**Status:** **superseded** by `20-milestone-11-handover.md` — M11 is complete.
This document is kept as the record of what was planned and measured *before*
the code; the handover says what shipped, where the plan was right (§8's
completion measurement), and the three things it did not foresee.
**Predecessor:** `18-every-row-closed-handover.md` — the repair is closed, the
register is empty, so M11 starts from a green gate and not from a debt list.

> For a fresh context, read in this order: `praxis_technical_design.md` §15
> (language server, all five subsections), §19-M11 (the gate), §14.2 (the shared
> query API), §8.2 (diagnostic shape), §7.4/§7.5 (the atomics and constructors
> the highlighting has to know), then handover 18's "five things worth not
> rediscovering". ADR-051 (diagnostic-code allocation) and ADR-094 (a template
> ends at the line it opens on) are the load-bearing prior decisions.

## The one-paragraph answer

M11 is **smaller than it looks in the compiler and larger than it looks in the
editor.** The front end already answers most of what the LSP has to serve:
`praxis_hir::analyze_root` returns one `Analysis` carrying every expression's
type keyed by node, every name reference, every method call with its catalog
entry, the scope tree, and every diagnostic — and `Analysis::hover` already
exists, written for M2. There is no query cache, no protocol layer, and no
editor, and **the parser AST — the thing four of the ten deliverables need — is
built during inference and thrown away.** That last one is the only front-end
change M11 requires; everything else is new code in `praxis-lsp` and
`editors/vscode`, which today are a 22-line skeleton and an empty directory that
does not exist.

## 1. What M11 must deliver

§19.11's deliverables, verbatim: `praxis lsp` process · document synchronization
and incremental source revisions · diagnostics · hover · completion, including
receiver methods · signature help · go-to-definition · document symbols ·
semantic tokens · thin VS Code extension.

Its five acceptance criteria:

1. Editing a typical puzzle file updates diagnostics **without running JIT code**.
2. Typing `grid.` suggests valid grid methods **with signatures**.
3. Hovering a parser expression or `read` result displays its **inferred result type**.
4. Input parser constructors and capture types receive **distinct semantic highlighting**.
5. VS Code run/check commands **invoke the local Praxis binary**.

Plus one addition to scope, requested for this milestone and already named by
§15.4 as an extension responsibility: **a TextMate grammar, so a `.px` file is
highlighted before the server attaches and while it is down.** It is §6 of this
plan and its own workstream, so it cannot be quietly folded into "the extension".

## 2. What already exists — measured, not assumed

| Thing | Where | What it means for M11 |
|---|---|---|
| The whole front end behind one call | `praxis_hir::analyze_root(file, &tree)` | The LSP's analysis step is two lines: `praxis_parser::parse` then this |
| Every expression's type, keyed by node | `Analysis::expr_types: HashMap<NodeKey, Type>` | Hover on an expression is a map lookup. `NodeKey` is `(range, kind)` on purpose, so `PATH_EXPR` and its `Ident` do not collide |
| Every name reference and its resolved symbol | `Analysis::refs`, `ref_types`, `decls` | Go-to-definition is `refs[range].symbol` → `Symbol.decl`. Shadowed bindings already have distinct ids |
| Every method call with its catalog entry | `Analysis::method_refs: HashMap<TextRange, MethodRef>` | Hover over `.len` has the entry, the receiver type and the result type |
| A hover query | `praxis-hir/src/hover.rs` (98 lines, `hover`/`hover_decl`/`scheme_of`) | Written for M2's criterion 5. M11 extends it; it does not start from nothing |
| Completion data generated from the compiler's own catalog | `praxis_stdlib::completion::completion_data` → `{receiver, name, params, result, doc}` | M8 built this *for* M11 and gated it 1:1 against `builtin_catalog()`. Criterion 2's "with signatures" is `params`/`result` |
| Receiver-type → catalog lookup | `praxis_hir::catalog::type_to_pattern` + `lookup` | This is how `grid.` filters to grid methods without a second type table |
| The scope tree | `Analysis::scopes: ScopeTree` | Lexical-identifier completion reads it at the cursor's scope |
| Structured diagnostics | `praxis_source::Diagnostic` — severity, registered code, primary `FileSpan`, `notes: [DiagnosticNote]`, `suggestions: [Suggestion]` with an optional machine-applicable `replacement` | Maps onto `lsp_types::Diagnostic` field for field, including `relatedInformation`. The `replacement`s are M12's code actions, already carried |
| Parser diagnostics at check time | `infer_read` → `synthesize_parser_type` pushes into the same diagnostic vector | Template and constructor errors reach the LSP for free |
| Every parser AST node carries an absolute span | `ParserAst::span()`, `shift_spans` (§7.10, ADR-078) | Parser spans are directly usable as LSP ranges — no relative-to-absolute arithmetic at the boundary |
| Closed, swept keyword tables | `SyntaxKind::from_keyword`/`keyword_text`; `AtomicKind::ALL`/`keyword`; `Constructor::ALL`/`keyword`/`keyword_arg` | These are the single sources of truth the TextMate grammar is tested against (§6.4) |
| A line map | `praxis_source::LineMap` — 1-based line, **0-based byte column**, by explicit design | Not an LSP position. See ADR-096 below |

Two facts that shape the design and are easy to get wrong in the other direction:

**The LSP path never registers a parser plan.** `register_plan` is reached only
from `parser_lower::analyze_parser_expr`, which is called only from
`lower.rs` — typed-HIR lowering, which the LSP does not run. Inference reaches
`synthesize_parser_type` instead, which converts, validates, synthesizes and
returns a type. So the process-global, append-only `PLAN_ARENA` is *not* a leak
in a long-lived server, and M11 does not have to touch ADR-043's territory.

**`praxis lsp` today exits 2.** `main.rs:82` is `not_implemented("lsp", None, 11)`,
and `crates/praxis-cli/tests/check.rs:148` asserts exactly that. That test is
M11's first red gate: it flips from asserting the stub to asserting a handshake.

## 3. What does not exist, and which of it is load-bearing

1. **The parser AST does not survive inference.** `synthesize_parser_type` builds
   a `ParserAst`, reads the root type off it, and drops it; `convert_parser_expr`
   is private and its only escape hatch is `#[cfg(test)]`. So today there is **no
   data source** for: hover on an inner constructor (§15.3), completion inside
   `{...}` or after `read`, the four parser semantic-token classes (criterion 4),
   or "which mode is the cursor in" (§15.3's five-way question). This is the one
   front-end change M11 needs, and it is load-bearing for four deliverables.
2. **`synthesize` answers one type for the root.** §15.3 asks for the synthesized
   type of *a* parser expression, which includes inner ones. Per-node types have
   to be recorded where the AST is walked, not re-derived.
3. **No query cache, no document store, no revisions.** §14.2 names the queries;
   nothing implements them. The CLI re-reads and re-analyzes per invocation,
   which is correct for a process that exits.
4. **No position encoding.** There is no `utf16` anywhere in the workspace, and
   `LineMap`'s column is a byte offset by a documented design choice.
5. **No editor directory.** §14's layout has `editors/vscode/`; the repo has no
   `editors/` at all, and no Node/TypeScript toolchain anywhere.
6. **Nothing knows where the cursor is.** Completion fires on text that does not
   parse (`grid.` is not an expression), so the LSP needs a cursor-context query
   over the token stream. §8 measures what the tree actually looks like there —
   the answer is better than expected and needs no parser change.

## 4. Decisions to record before implementing

Per §20 rule 1, these are written as ADRs **before** the code, not after. Four,
numbered from the next free slot.

### ADR-095 — the language server is a synchronous, single-threaded stdio loop

`lsp-server` + `lsp-types` (the rust-analyzer transport crates), no async runtime.

*Why.* The workspace has zero async dependencies today and adding tokio for one
crate is a large dependency for no gain at one-file scope. `rowan::SyntaxNode` is
`!Send` — only `GreenNode` crosses threads — so a multi-threaded design pays an
immediate re-rooting cost to answer queries about a tree it cannot share. Every
M11 query is one file, and §15.5's targets are about one AoC file.

*Consequence.* A slow query blocks the loop; `$/cancelRequest` is honored by
dropping the queued request, not by killing a worker. If M13's measurements say
that is not enough, the split is a thread that owns the `GreenNode` — which is
why the query layer must never hand a `SyntaxNode` across a public boundary.

### ADR-096 — positions convert at the protocol boundary; `LineMap` stays byte-based

Negotiate `positionEncoding` at `initialize` (prefer UTF-8 when the client offers
it, fall back to UTF-16), and do every conversion in one `praxis-lsp` module
against the document text. Nothing below `praxis-lsp` learns what a UTF-16 code
unit is.

*Why.* An LSP position is `(line, character)` where `character` counts UTF-16
code units by default; `LineMap`'s column counts *bytes*, deliberately ("this
keeps the mapping lossless and O(1) to invert"). Pushing UTF-16 into
`praxis-source` would put a protocol concern under every crate that reports a
span, to serve one consumer.

*Gate.* A property test over arbitrary text — including astral-plane characters,
CRLF, and a multi-byte template interior — asserting byte → position → byte is
the identity in both encodings, and that the two agree exactly where the text is
ASCII. This is the class of bug that is invisible in every English fixture, so
the fixture corpus is explicitly not English-only.

### ADR-097 — the shared query layer lives in `praxis-lsp`, and `praxis check` routes through it

No new crate. §14.1 already assigns "LSP transport **and compiler queries**" to
`praxis-lsp`, and §14.2 requires the CLI and the LSP to share one front-end API.

*Consequence, and the point of it.* `crates/praxis-cli/src/check.rs` today has
its own parse → analyze → sort-diagnostics sequence. M11 deletes it and calls the
shared snapshot. One path means a divergence between what `praxis check` says and
what the editor underlines is unrepresentable rather than merely unlikely — which
is handover 17's "state a rule where it is enforced", applied to a pipeline.

*Alternative considered.* A separate `praxis-analysis` crate. Rejected: it
deviates from §14.1's table to solve a problem (the CLI carrying the transport
dependency) that costs nothing at this size. Keep transport in its own module so
a later split is a move, not a rewrite.

### ADR-098 — the parser AST is retained by inference, and per-node types with it

`Analysis` gains a spanned parser index: for each `read`/`parse` expression, the
`ParserAst` and a per-node synthesized type keyed by span. Built at the one place
the AST already exists (`synthesize_parser_type`), not by a second walk.

*Why here and not in the LSP.* Because the alternative is a second scanner over
template interiors living in the LSP crate, and handover 17's last section is
about exactly that failure: two hand-written scanners each stating the whitespace
rule, five review rounds, fixed by deleting both and putting the rule below the
crates that need it. §15.3's five-way cursor-mode question is then a lookup
against spans the compiler already computed, and cannot disagree with the
compiler about where a capture ends.

*Gate.* A test that hovers an inner constructor in
`sections(lines(\`{a:int},{b:int}\`))` and gets that node's type, not the root's.

## 5. The workstreams

Eleven, in dependency order. Each lands independently green (`just ci`), and each
names the gate that was **observed red with its own fix removed** — handover 17's
rule, which is the reason the repair's gates held.

### WS1 — transport, lifecycle, document store

`praxis lsp` runs a real loop: `initialize` (advertising exactly the M11
capabilities and no more), `initialized`, `shutdown`, `exit`; `didOpen` /
`didChange` (incremental) / `didClose` / `didSave`. A revisioned overlay store:
text + revision per URI, with `didChange` ranges applied through ADR-096's
conversion.

*Gate.* A scripted JSON-RPC session over a pipe, driving the real binary: the
handshake completes, an edit bumps the revision, and `shutdown`/`exit` returns 0.
`check.rs:148`'s "not implemented" assertion is replaced by it — the red
observation is that the new test fails against `bcc5319`'s binary.

### WS2 — the query layer (§14.2)

A `Snapshot`: URI + revision → parse → analyze, memoized by revision, in
`praxis-lsp::query`. The §14.2 query names are the public surface
(`source_text`, `parse`, `analyze`, `type_of`, `resolve_name`,
`input_parser_at`, `completion_context`). `praxis check` is rewired onto it and
its private pipeline deleted (ADR-097).

*Gate.* Two queries at one revision parse once (a counter proves it); an edit
invalidates. Plus: the whole existing `check.rs` suite still passes unchanged
after the rewire — a characterization gate, listed as such and not counted as a
red one.

### WS3 — diagnostics

`Diagnostic` → `lsp_types::Diagnostic`: severity, `code` from the registered
`DiagnosticCode` (never a hand-written string — ADR-051 owns the allocation),
primary span, `notes` → `relatedInformation`, advisory `suggestions` → the
message tail. Publish on open/change behind a short debounce (§15.2).

*Gates.* (a) An edit that introduces a `Y0xx` publishes it, and the fix retracts
it — assert the *code and the span*, not that something was published (handover
18: "if a diagnostic carries a span, a count or a name, assert it"). (b)
**Criterion 1 structurally**: `praxis-lsp`'s manifest does not depend on
`praxis-codegen-cranelift`, `praxis-mir` or `praxis-runtime`, asserted by a test
that reads `Cargo.toml`. "Without running JIT code" then holds by construction
rather than by observation, which is the only version of it that stays true.

### WS4 — hover

Extend `Analysis::hover` to cover expression nodes (via `expr_types`), decl
sites, method refs (already there), and **parser expressions** — the root `read`
result and, through ADR-098's index, an inner constructor. Markdown output,
types rendered by `db.render` so the editor and `praxis check` name a type the
same way.

*Gate.* Criterion 3, twice: hovering `read` shows the synthesized result type,
and hovering `lines(...)` *inside* it shows that node's — the second observed red
before ADR-098's index exists. Plus §15.2's own example shape: hovering a
segments-style binding renders the anonymous record with its fields.

### WS5 — completion

The §15.2 context list: lexical identifiers from the scope tree; fields after
`.`; receiver methods from the catalog filtered by `type_to_pattern`; enum cases
in patterns; record field names in construction; parser constructors outside
backticks; capture atom types inside `{...}`; and the named arguments — which are
`Constructor::keyword_arg` (`chars`'s `skip:`, `grid`'s `fill:`) and the `ragged`
flag, read from the constructor and never from a name list (the bug that table
exists to prevent).

*Gate.* Criterion 2 exactly: `grid.` offers the grid methods and only those, each
with its parameter and result types from `completion_data`. Observed red by
asserting a `Map` method is absent from the list.

### WS6 — signature help

Function calls and parser constructors, with the active parameter computed from
the cursor's position among the argument list. §15.2's three examples
(`sections(parser)`, the named-`sections` form, `lines(parser)`) are the fixture.

*Gate.* The active-parameter index advances across a comma — asserted per
position, since an implementation that always answers 0 passes any "signature
help was returned" test.

### WS7 — navigation

Go-to-definition (`refs` → `Symbol.decl` span) and document symbols (top-level
`fn`, `struct`, `enum`, and bindings). **Find references, rename and workspace
symbols are M12** and are not to be started here (§19.12).

*Gate.* Definition from the *second* of two shadowed bindings lands on the second
declaration — the one assertion that distinguishes a real symbol table from a
name match, and it is already supported (distinct `SymbolId` per declaration).

### WS8 — semantic tokens

Full-document only (delta is M12). The legend covers §15.2's nine classes:
keywords, types, functions/methods, locals, record fields, and the four parser
classes — constructors, template literal text, capture names, capture types. The
parser four read ADR-098's spanned index; the rest walk the rowan tree with
`Analysis` beside it.

*Gate.* Criterion 4: over one fixture containing
`` read lines(`{name:word} {n:int}`) ``, the four parser classes land on four
distinct ranges with four distinct token types — asserted as (range, type) pairs,
not as "tokens were produced".

### WS9 — the VS Code extension (thin)

`editors/vscode/` per §14's layout. `package.json` contributes the `praxis`
language and `.px` extension, a `language-configuration.json` (nested `/* */`,
`//`, brackets, autoclosing including the backtick, indentation), the four §15.4
commands (`Run File`, `Check File`, `Watch File`, `Restart Language Server`), and
configuration for the binary path and trace level. The client launches
`praxis lsp` via `vscode-languageclient`; run/check go to an integrated terminal
(§15.4's "display debugger output in an integrated terminal" — the crash REPL is
interactive, so it needs a terminal and not an output channel).

**No parsing or type logic in TypeScript** (§15.4, and §20 rule 3). The extension
is a launcher, a command set, and a grammar.

*Gate.* Criterion 5: the command invokes the configured binary with the expected
argv. Asserted without a VS Code host by keeping the argv construction in one
exported function with a unit test — and the manual check (`F5`, run a corpus
file) written down in the handover, since the last mile genuinely needs a host.

### WS10 — syntax highlighting

§6 below. Its own workstream because it is the deliverable that works when the
server is down, and because it is the one with a drift risk that only a test can
hold.

### WS11 — acceptance sweep, ADRs, handover, README

The five criteria re-run end to end, §15.5's targets measured (not asserted —
they are engineering targets), the four ADRs landed with the code that
implements them, this plan superseded by an M11 handover, and the README's status
paragraph and command-surface table updated to say `lsp` is real.

## 6. Syntax highlighting, in detail

Highlighting arrives in two layers that must **agree**, and the agreement is the
design problem — not either layer on its own.

### 6.1 The two layers

- **TextMate grammar** (`editors/vscode/syntaxes/praxis.tmLanguage.json`) —
  regex-based, instant, works on an unsaved buffer, on a file the server has not
  opened, and while the server is restarting or crashed. §15.4 calls it
  "fallback highlighting before semantic tokens arrive"; it is also what a
  GitHub-style renderer and any non-LSP consumer will use.
- **Semantic tokens** (WS8) — from the compiler, so they know a `Grid[T]` from a
  local named `grid`, and know that `int` inside `{n:int}` is a capture type and
  not a keyword.

Semantic tokens **refine**; they must not fight. If the grammar scopes a
constructor as a plain function and the semantic layer scopes it as something a
theme colors differently, the token flickers colour as the server attaches.

### 6.2 One scope vocabulary, used by both

The fix is that both layers emit the **same TextMate scope names**. The grammar
emits them directly; the extension maps each custom semantic token type onto one
via `contributes.semanticTokenScopes` in `package.json`. Themes then colour a
`.px` file identically before and after the server attaches.

| Construct | Scope | Grammar | Semantic type |
|---|---|---|---|
| Keywords (`let`, `fn`, `match`, `read`, …) | `keyword.control.praxis` | ✓ | `keyword` |
| Types (`Int`, `Grid`, user structs/enums) | `entity.name.type.praxis` | ✓ (capitalized ident) | `type` |
| Function declarations and calls | `entity.name.function.praxis` | ✓ | `function` / `method` |
| Record fields | `variable.other.property.praxis` | — | `property` |
| Comments (nested `/* */`) | `comment.block.praxis`, `comment.line.double-slash.praxis` | ✓ | — |
| Numbers (`42`, `3.14`, `.5`, `2.`, `1e10`) | `constant.numeric.praxis` | ✓ | `number` |
| Text literals + escapes | `string.quoted.double.praxis` | ✓ | `string` |
| **Parser constructor** (`lines`, `grid`, …) | `entity.name.function.parser.praxis` | ✓ | `parserConstructor` |
| **Template literal text** | `string.quoted.other.template.praxis` | ✓ | `parserTemplateText` |
| **Capture name** (`n` in `{n:int}`) | `variable.other.capture.praxis` | ✓ | `parserCaptureName` |
| **Capture type** (`int` in `{n:int}`) | `support.type.capture.praxis` | ✓ | `parserCaptureType` |
| Parser named args (`skip:`, `fill:`, `ragged`) | `variable.parameter.parser.praxis` | ✓ | `parameter` |

The four bolded rows are criterion 4. They are custom semantic token types — not
standard types with modifiers — because "distinct" is the requirement, and a
custom type that is mapped to a scope is distinct in every theme, whereas a
modifier is honoured by few.

### 6.3 What the grammar has to get right

The parts where a naive grammar is wrong, each of which has a decided answer in
the codebase already:

- **The backtick template is an embedded grammar, and it ends at its line.**
  ADR-094 decided a template ends at the line it opens on, and the lexer has two
  distinct kinds (`BacktickTemplate` / `UnterminatedBacktickTemplate`) so the
  state is unrepresentable rather than re-derived. The grammar's template rule
  must therefore be line-bounded — an unterminated backtick must not paint the
  rest of the file as a string.
- **Captures inside the template.** `{name:type}`, `{type}`, and `{name}` each
  scope their parts separately (name, `:`, type), which is what makes criterion
  4 visible without the server.
- **Numbers.** §4.12 allows `.5` and `2.`, and `..`/`..=` is a range and never
  part of a float. The lexer already draws this line; the grammar must draw the
  same one or a range expression paints as two floats.
- **Nested block comments.** `/* /* */ */` is one comment (`SyntaxKind::BlockComment`
  is documented as nestable). TextMate can do this with a recursive `begin`/`end`
  pattern; a flat one closes at the first `*/`.
- **Constructors are contextual.** `lines` is a parser constructor after `read`
  or inside a parser expression, and an ordinary identifier elsewhere. The
  grammar approximates by scoping constructor-shaped calls inside a `read`/`parse`
  region; the semantic layer corrects it. This is the one place the two layers are
  *allowed* to disagree, and the disagreement resolves toward the compiler.
- **`min=` / `max=`.** These are contextual operators built from an identifier
  plus `=` (`UPDATE_OP`), not keywords — so the grammar must not paint every `min`
  as one.

### 6.4 The drift gates — and why they are Rust tests

A grammar's keyword list is a copy of the lexer's, a copy that no compiler
checks. Every keyword added after M11 is a chance for the two to diverge silently
— the failure is invisible (a word stops being coloured) and nobody files it.

So the grammar is tested from Rust, reading
`editors/vscode/syntaxes/praxis.tmLanguage.json` at test time — the
`crates/praxis-cli/tests/design_doc.rs` precedent, which extracts the design
document's own programs rather than retyping them ("a test that quotes the doc
can drift from it; a test that reads it cannot"). Four gates:

1. Every keyword in `SyntaxKind`'s table appears in the grammar's keyword pattern.
2. Every `AtomicKind::keyword()` (§7.4's ten atomics) appears in the capture-type
   pattern.
3. Every `Constructor::keyword()` (§7.5's fourteen constructors) appears in the
   constructor pattern.
4. Every custom semantic token type in WS8's legend has a `semanticTokenScopes`
   entry in `package.json`, and that entry's scope is one the grammar also emits
   — which is §6.2's agreement, enforced instead of documented.

All three source lists are already `ALL`-swept closed tables, so each gate is a
loop over one of them. **This adds no Node toolchain to CI**, which matters: `just
ci` is the whole gate (ADR-002) and a second toolchain in it is a cost the
milestone does not need to pay. A `vscode-tmgrammar-test` snapshot suite is worth
revisiting at M14, when the extension is packaged and a Node step exists anyway.

## 7. Acceptance criteria → where each is closed

| §19.11 criterion | Closed by | The gate |
|---|---|---|
| Diagnostics update without running JIT code | WS3 | An edit publishes a code+span, **and** `praxis-lsp` provably cannot reach the JIT (manifest test) |
| `grid.` suggests grid methods with signatures | WS5 | Only grid methods, each with params and result; a `Map` method's absence asserted |
| Hover shows a parser/`read` result type | WS4 + ADR-098 | Root and inner constructor, the inner one red before the spanned index exists |
| Parser constructors and capture types highlight distinctly | WS8 + WS10 | Four distinct (range, type) pairs; and the two layers' scopes agree by test |
| VS Code run/check invoke the local binary | WS9 | argv unit test + a written-down manual host check |

## 8. Ordering, risk, and what is not M11

**Order.** WS1 → WS2 gate everything. ADR-098 gates WS4/WS5/WS8's parser halves,
so it lands early — its absence is what makes those three look expensive. WS9 and
WS10 are independent of the compiler work and can run in parallel from day one;
WS10 is the only workstream that delivers user-visible value with no server at all,
which makes it a good early one.

**Completion on text that does not parse — measured, and it is not a blocker.**
`grid.` is not an expression, so the question is whether the receiver's type is
in `Analysis` by the time completion is asked. It is. Probed at `bcc5319` against
`let grid = Vec()` / `grid.push(1)` / `grid.`:

- The tree is `EXPR_STMT [ PATH_EXPR "grid", DOT ]` — the `.` is bumped and the
  postfix loop breaks, so the checkpoint never becomes a node, but **the receiver
  is left as a complete `PATH_EXPR` immediately before the `DOT`**.
- `expr_types[(30..34, PATH_EXPR)]` is **`Vec[Int]`** — concrete, not a variable.
  This is `expr_types`' "filled at one insertion point" invariant paying off: an
  expression inference visited has a type whether or not its *parent* parsed.
- Exactly one parse diagnostic (`expected a name or a tuple index after \`.\``)
  and **zero** analysis diagnostics.

So approach (c) — walk left from the cursor's `DOT` to the preceding expression
node and read its recorded type — works today with **no parser change**, and
`type_to_pattern` filters the catalog from there. Approaches (a) parser recovery
producing a named-hole node and (b) speculative edit are not needed for M11.
**Do not open WS5 by rewriting parser recovery.**

**The residual risk is a different one, and it is diagnostics, not completion.**
A `.` typed on a line that has code below it swallows the next line's first
token: `grid.` followed by `out(grid)` parses as one `METHOD_CALL_EXPR` spanning
both lines, with **no** parse diagnostic and a spurious
`Y110: no method \`out\` on type \`Vec[Int]\`` on the line below. The receiver's
type is still recorded (`Vec[Int]`), so completion is unaffected — but the user
sees a red squiggle on a line they did not touch, for as long as the `.` is
unfinished.

This is **not a parser bug and must not be "fixed" as one**: ADR-049 decides that
a newline ends a statement but never an expression, and a trailing `.` is an
unfinished expression, so continuing to the next line is the rule working. What
M11 owes it is (i) WS3's debounce, so the diagnostic does not appear mid-keystroke,
and (ii) a decision, recorded if taken, about whether the server suppresses
diagnostics on the *statement containing the cursor* while it is syntactically
incomplete. Both are LSP-side; neither touches the language.

**Second risk: hover and completion inside a template read stale structure.**
Mitigated by ADR-098 putting the index where the AST is built, so it cannot lag
inference.

**Not M11, and not to be started:** find references, rename, workspace symbols,
inlay hints, the formatter, and code actions — all §19.12. Semantic token
*deltas*, multi-file/workspace indexing, and incremental reparse are also out:
§19.11 says "incremental source revisions", which is the document store, not the
parser. `praxis watch` and `praxis repl` remain unimplemented and are not part of
this gate, though WS9's `Praxis: Watch File` command will surface `watch`'s
current "not implemented" message — say so in the extension's command
description rather than hiding the command.

**Standing decisions from handover 18 that M11 does not touch:** REP-67 (the
`praxis_alloc_text` split) and D19 (whether there is a character literal). D19
would add one lexer arm and one TextMate pattern; if it is answered during M11,
§6.4's gate 1 catches the grammar half automatically.
