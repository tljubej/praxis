# Milestone 1 report & Milestone 2 handover

**Project:** Praxis
**Commit:** `f3a0525` — "M1: formatter skeleton, proptest fuzz gate, parser in praxis check" (on `milestone-1`)
**Date:** 2026-07-23
**Status:** Milestone 1 complete and green. All four acceptance criteria met. Ready to begin Milestone 2.

> **For a fresh context:** read this document, then `praxis_technical_design.md`
> (the contract — §14 architecture, §19 milestones, §13 IRs, §4 language
> surface, §5 type system for M2). The rest of this file assumes you have NOT
> seen the M1 code yet and tells you exactly what exists, where, and what to do
> next.

---

## 1. What Praxis is (unchanged from M0)

Praxis is a small, statically typed, garbage-collected language for Advent of
Code-style puzzle solving. Procedural + expression-oriented. Every runtime value
is a GC object referenced through a uniform `GcRef`. First-class input parsing
(`read lines(int)`). JIT-compiled via Cranelift. Ships with an LSP + VS Code
extension. No ownership, no traits, no operator overloading, no exceptions.

The pipeline is:

```
source -> parse -> resolve -> infer -> typed IR -> lower -> Cranelift -> execute
```

`parse` is now real (this milestone). `resolve`/`infer` is Milestone 2.

The full spec is **`praxis_technical_design.md`** at the repo root. Treat it as
the contract; deliberate deviations go in `docs/decisions/` (rule 20.1).

---

## 2. How to work in this repo (unchanged)

```sh
cargo install just          # one-time
just ci                     # the full quality gate (fmt-check + clippy + test)
                            # — this is EXACTLY what GitHub Actions runs
just fmt        # reformat (modifies files; NOT part of ci)
just fmt-check  # verify formatting
just clippy     # lint, -D warnings
just test       # cargo test --workspace
just build      # cargo build --workspace
```

**⚠️ Resource note for agents:** the parser can infinite-loop on malformed input
if a grammar change forgets to advance the cursor — this happened during M1 and
OOM-killed the dev machine. Every loop in the parser has an `ensure_progress()`
guard; **never remove it**, and never run two `cargo` commands concurrently
(each spawns the compiler + linker + test harness; two at once exhausted RAM).
Test scoped (`-p <crate> --lib <module>`) before running `just test`.

**Two workspace rules from `AGENTS.md` that override everything:**
1. *"Make illegal states unrepresentable"* — prefer types that structurally
   forbid bad data over runtime checks or conventions.
2. *"Test every language feature extensively"* — every new behavior ships with
   unit + snapshot tests.

---

## 3. What Milestone 1 delivered

All §19 Milestone-1 deliverables. Acceptance criteria met:

| Criterion | Result |
|---|---|
| Golden syntax tests cover valid and invalid files | ✅ 15 parser golden/structural tests + tree printer |
| Parser produces multiple diagnostics from one malformed file | ✅ `parse_error.px` → 3 `P0xx` (CLI integration test) |
| Formatter is idempotent on milestone syntax | ✅ 6 idempotency tests (`fmt(fmt(src)) == fmt(src)`) |
| No panic on fuzzed token streams | ✅ proptest: 512 random inputs through lex + parse |
| `cargo test --workspace` passes | ✅ **118 tests** (was 75 at M0) |
| `just ci` clean | ✅ |

### 3.1 What changed at the crate level

| Crate | What landed in M1 |
|---|---|
| `praxis-syntax` | **`SyntaxKind`** enum (all tokens, trivia, node kinds) replaces M0 `TokenKind`. **`PraxisLanguage`** impls `rowan::Language`; `SyntaxNode`/`SyntaxToken`/`SyntaxElement` aliases. **Span↔TextRange** bridge (`span_to_range`/`range_to_span`). |
| `praxis-parser` | **Real lexer** (`lex.rs`) — longest-match operators, keyword splitting, text literals, nestable comments, backtick templates. **Recursive-descent + Pratt parser** (`parse.rs`) over the M1 grammar. **Formatter skeleton** (`fmt.rs`). **Fuzz gate** (`tests/fuzz.rs`). |
| `praxis-ast` | Still a skeleton — the typed AST wrappers (§13.2) were deferred; the rowan tree is consumed directly so far. See §6 open question 1. |
| `praxis-test-support` | **`format_syntax_tree()`** — the stable golden-tree dump format shared by all parser tests. |
| `praxis-cli` | `praxis check` now runs **lex + parse** and renders both `T0xx`/`P0xx` diagnostics end to end. |

New dependencies: `rowan = "0.16"` (on syntax, ast, parser, test-support),
`proptest = "1"` (parser dev-dep). `praxis-source` stayed dependency-free; the
DAG is still clean.

### 3.2 Decisions recorded in M1 (in `docs/decisions/`)

- **ADR-003:** lossless tree uses the **`rowan`** crate (§13.1 names the pattern).
- **ADR-004:** **hand-written recursive-descent + Pratt** parser.
- **ADR-005:** formatter skeleton lives in **`praxis-parser`**.
- **ADR-006:** M1 fuzz gate is a **`proptest`** property test.

### 3.3 The M1 grammar (what parses cleanly)

The parser handles the full §19 Milestone-1 subset:

- **Literals:** `IntLit`, `TextLit` (`"…"`), `BacktickTemplate` (one token),
  `true`/`false`.
- **Bindings:** `let x = …`, `var x = …` (optional `: Type` annotation parsed
  leniently — a single ident; full types are M2).
- **Reassignment:** `x = …`, `x += …`, etc. (§4.2; `ASSIGN_STMT` node).
- **Blocks:** `{ stmt; stmt; expr }` — last expression is the value (§4.11).
  Semicolons optional unless two statements share a line (§4.1).
- **Functions:** `fn name(a: Int, b: Int) -> Int { … }`.
- **Calls:** `f(args)` including `out(…)`. `out`/`panic` are builtins, not
  keywords.
- **Arithmetic:** `+ - * / %`, comparisons `== != < > <= >=`, unary `-`/`!`.
  Correct precedence + left-associativity via Pratt.
- **Control flow:** `if … { } else { }` (and `else if`), `while … { }`.

Not yet parsed (recover with a `P0xx` diagnostic): `match`, `for`, `loop`,
`break`/`continue`/`return`, records/struct/enum, closures, `read`/`parse`,
indexing `[…]`, field access `.` (parsed as a token but not as a structured
expression). These are fine for M1; they land with their own milestones.

---

## 4. Key types a fresh context should know

In `praxis-syntax`:
- **`SyntaxKind`** — the single `#[repr(u16)]` enum of *everything*: trivia
  (`Whitespace`, `LineComment`, `BlockComment`), tokens (`KW_LET`…, `PLUS`…,
  `IntLit`…), and tree nodes (`SOURCE_FILE`, `LET_STMT`, `BIN_EXPR`…). Helpers:
  `is_trivia()`, `is_keyword()`, `is_token()`, `is_node()`,
  `from_keyword(&str) -> Option<SyntaxKind>`, `keyword_text()`.
- **`PraxisLanguage`** + type aliases `SyntaxNode`/`SyntaxToken`/`SyntaxElement`
  (rowan generic types tagged with Praxis's kind).
- **`span_to_range`/`range_to_span`** — the *only* place Praxis `Span` and
  rowan `TextRange` meet. Praxis `Span` stays the diagnostic source of truth.
- **`Token { kind: SyntaxKind, span: Span }`** — the lexer's intermediate output.

In `praxis-parser`:
- **`lex(file, text) -> LexOutput { tokens, diagnostics }`** — the real lexer.
  Diagnostic codes: `T001` unterminated block comment, `T002` unterminated
  template, `T003` unexpected byte, `T004` unterminated text literal,
  `T005` invalid escape. Shape unchanged from M0 (CLI depended on it).
- **`parse(file, text) -> ParseOutput { tree, tokens, diagnostics }`** — lex +
  parse. Always returns a tree (root is `SOURCE_FILE`), even for total garbage.
  Diagnostics are lex + parse merged, sorted by span.
- **`format_source(file, text) -> String`** / **`format_node(&SyntaxNode)`** —
  the formatter skeleton.
- Parser internals (`Parser` struct, Pratt table, `ensure_progress`) are all
  private to `parse.rs`.

In `praxis-test-support`:
- **`format_syntax_tree(&SyntaxNode) -> String`** — the golden-tree dump format
  (`KIND@start..end`, tokens show `"text"`, trivia included). Use this for every
  new parser snapshot test.

---

## 5. Status of the §19 Milestone 1 acceptance criteria — all green

```sh
just ci                       # must print "all Praxis checks passed"
# Multiple parse diagnostics from one malformed file:
cargo run -q -p praxis-cli -- check crates/praxis-cli/tests/fixtures/parse_error.px
# Fuzz gate (no panic on random input):
cargo test -p praxis-parser --test fuzz
```

---

## 6. Open questions / known limitations to resolve in M2

1. **Typed AST wrappers (`praxis-ast`, §13.2) are still a skeleton.** M1 consumed
   the rowan `SyntaxNode` tree directly. M2 (name resolution, type inference)
   will want typed wrappers over nodes (`LetStmt`, `NameRef`, …) implementing
   rowan's `AstNode`. Decide: build them incrementally as M2 needs each, or lay
   the `AstNode` trait foundation up front. Either is fine; don't over-build.
2. **Trivia attachment is "leading-trivia into the enclosing context."** The
   parser eats trivia *before* opening a node (see `bump_meaningful`), so it
   attaches to the parent. This is workable but not the only valid policy;
   revisit if the formatter or LSP wants trailing-trivia attachment instead.
3. **Unicode identifiers** are accepted permissively (`b >= 0x80`); a real XID
   table is a TODO in `lex.rs`. Not blocking for M2 but worth noting.
4. **`ensure_progress()` is load-bearing.** It guarantees parser termination on
   any input. Any new loop in `parse.rs` MUST call it (pattern: capture
   `meaningful_index()` before the body, call `ensure_progress(before)` after).
   Removing it reintroduces the OOM infinite-loop bug.
5. **Snapshot files:** parser golden trees are *inline* insta snapshots (hand-
   edited, not auto-rewritten by `INSTA_UPDATE`). The harness smoke test uses
   *file* snapshots (`*.snap`, auto-updated by `INSTA_UPDATE=always`).

---

## 7. Milestone 2 — what to build

**From §19:** *Name resolution and core type inference.*

### 7.1 Deliverables (verbatim from the design)

- Scopes and symbol IDs.
- Built-in static types.
- Function inference.
- Tuples.
- `let`, `var`, assignment, and Rust-style shadowing.
- Basic method catalog lookup.
- User-facing type diagnostics.

### 7.2 Acceptance criteria (verbatim)

- Infer non-recursive function parameters and return values from use.
- Accept `let a = 4; let a = "Foo"` and resolve each occurrence to the correct
  symbol.
- Resolve a shadowing initializer against the previous binding.
- Reject cross-type `var` reassignment.
- Hover query returns the inferred type and symbol identity for each shadowed
  occurrence.

### 7.3 Where things go (recommended)

The design (§14) lists `praxis-types` and `praxis-hir` as the homes for this
work; both are currently skeletons.

| Work | Crate | Notes |
|---|---|---|
| Scopes, symbol IDs, name resolution | `praxis-hir` (M2 skeleton) | Walk the `SyntaxNode` tree from `parse`, build scopes, mint `SymbolId`s. §13.3: "HIR resolves names." |
| Built-in static types, inference | `praxis-types` (M2 skeleton) | `Int`/`Text`/`Bool`/etc., type IDs, unification. §5.1–5.3. |
| Type diagnostics | reuse `Diagnostic` (`DiagnosticCategory::Type`, `Y0xx`) | Wire into `praxis check` after parsing. |
| Hover (last acceptance criterion) | defer the *query* to M11 (LSP); for M2 surface via a test | The LSP doesn't exist yet; satisfy "hover" with a library-level `hover(symbol) -> Type` test. |

### 7.4 Integration with M1 code

- The parser's `ParseOutput.tree` is the input to name resolution — do NOT re-
  parse. Walk `SyntaxNode`/`SyntaxToken` (or typed `praxis-ast` wrappers once
  built).
- `praxis-runtime` already has `TypeDescriptor`; `praxis-stdlib` has
  `MethodCatalog` + `TypePattern` from M0 — M2's type system should align with
  these, not duplicate them (rule 20.3: never duplicate type/method knowledge).
- Thread M2 diagnostics into `praxis check` the same way M1 did: after `parse`,
  run resolution/inference, merge `Y0xx` diagnostics into the rendered set.

### 7.5 Things explicitly NOT in M2

- GC heap, real runtime → **M3**.
- Cranelift JIT, MIR → **M4**.
- Records, enums, pattern matching, closures → **M7** (tuples ARE in M2).
- Input-parser DSL → **M6**. LSP → **M11**.

---

## 8. Repo map for a fresh context

```
praxis_technical_design.md      # THE CONTRACT — read §14, §19, §13, §4, §5
AGENTS.md                       # the two overriding workspace rules
justfile                        # quality gate (just ci)
docs/
  decisions/                    # ADR-001..006 — read 003/004/005/006 for M1 choices
  handovers/                    # this file + 00-milestone-0-handover.md
crates/
  praxis-syntax/                # SyntaxKind, PraxisLanguage, span bridge, Token
  praxis-parser/                # lex.rs (real lexer), parse.rs (parser), fmt.rs (formatter)
                                # tests/fuzz.rs (proptest gate)
  praxis-test-support/          # format_syntax_tree(), single_file(), insta wrappers
  praxis-cli/                   # praxis check (lex + parse + render)
  praxis-source/                # spans, diagnostics (unchanged since M0)
  praxis-runtime/ praxis-stdlib/  # done in M0 (TypeDescriptor, MethodCatalog)
  praxis-ast/                   # SKELETON (M2) — typed wrappers go here
  praxis-types/ praxis-hir/     # SKELETONS (M2) — name resolution + types go here
  praxis-{mir,codegen-cranelift,input-parser,debugger,lsp}/  # skeletons (later milestones)
tests/
  parser/                       # EMPTY — corpus golden trees can go here (§17.1)
  ui/, typecheck/, run-pass/, run-fault/, input-parsers/, aoc-corpus/  # later
```

---

## 9. Quick start for the fresh context

```sh
# 1. Confirm everything is green before touching anything:
just ci

# 2. Read the contract sections that govern M2:
#    praxis_technical_design.md §5 (type system), §13.3/13.4 (HIR/typed HIR),
#    §19 Milestone 2.

# 3. Read ADR-003/004 to understand the tree the resolver will walk.

# 4. Start in praxis-types / praxis-hir (both skeletons). First vertical slice:
#    scopes + symbol IDs + resolve `let`/`var`/name references over the M1 tree.

# 5. Branch off milestone-1 (or main once M1 is merged):
git switch -c milestone-2
```

Good luck. The front end now produces a real, lossless, recoverable tree; the
type system is the next layer on top of it.
