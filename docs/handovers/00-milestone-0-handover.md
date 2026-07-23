# Milestone 0 report & Milestone 1 handover

**Project:** Praxis (formerly Tinsel — renamed this session)
**Commit:** `bb89881` — "Milestone 0: workspace and contracts" (on `main`)
**Date:** 2026-07-23
**Status:** Milestone 0 complete and green. Ready to begin Milestone 1.

> **For a fresh context:** read this document, then `praxis_technical_design.md`
> (the contract — §14 architecture, §19 milestones, §13 IRs, §4 language surface).
> The rest of this file assumes you have NOT seen the code yet and tells you
> exactly what exists, where, and what to do next.

---

## 1. What Praxis is

Praxis is a small, statically typed, garbage-collected language for Advent of
Code-style puzzle solving. Procedural + expression-oriented. Every runtime value
is a GC object referenced through a uniform `GcRef`. First-class input parsing
(`read lines(int)`). JIT-compiled via Cranelift. Ships with an LSP + VS Code
extension. No ownership, no traits, no operator overloading, no exceptions.

The pipeline is:

```
source -> parse -> resolve -> infer -> typed IR -> lower -> Cranelift -> execute
```

The full spec is **`praxis_technical_design.md`** at the repo root. Treat it as
the contract; deliberate deviations go in `docs/decisions/` (rule 20.1).

---

## 2. How to work in this repo

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

CI (`.github/workflows/ci.yml`) does no logic of its own — it runs `just ci`. So
never let `just ci` go red. See ADR-002.

**Two workspace rules from `AGENTS.md` that override everything:**
1. *"Make illegal states unrepresentable"* — prefer types that structurally
   forbid bad data (e.g. a `Span` stored as `start + len` so an inverted span
   cannot exist) over runtime checks or conventions.
2. *"Test every language feature extensively"* — every new behavior ships with
   unit + snapshot tests.

---

## 3. What Milestone 0 delivered

All §19 Milestone-0 deliverables except CI-from-CI-host (CI is configured and
runs `just ci`; we just haven't pushed to GitHub yet). Acceptance criteria met:

| Criterion | Result |
|---|---|
| `cargo test --workspace` passes | ✅ **75 tests** |
| A dummy `.px` file can be loaded and diagnosed through the CLI | ✅ `praxis check` |
| CI rejects fmt + clippy regressions | ✅ `just ci` clean |
| No crate dependency cycles | ✅ clean DAG, `praxis-source` is the root |

### 3.1 The 15-crate workspace

Topological root is `praxis-source`; everything depends on it. Dependency edges
are wired per the §14 DAG — a clean build proves there are no cycles.

**Substantive crates (real content + tests):**

| Crate | LOC | Tests | Owns |
|---|---|---|---|
| `praxis-source` | 1320 | 31 | `Span`, `FileSpan`, `BytePos`, `SourceMap`/`FileId`, `LineMap`, `Diagnostic` + `Renderer` |
| `praxis-runtime` | 464 | 7 | `GcRef`, `GcHeader`, `TypeDescriptor`, `RuntimeContext`, `RUNTIME_ABI_VERSION` |
| `praxis-stdlib` | 589 | 10 | `MethodCatalog` (+ builder that rejects duplicate `(receiver,name,arity)`), `TypePattern`, `PRELUDE` |
| `praxis-parser` | 368 | 7 | **Lexer stub** (M1 will replace) — `lex(file, text) -> LexOutput { tokens, diagnostics }` |
| `praxis-syntax` | 86 | 2 | `TokenKind`, `Token` (the vocabulary the lexer emits) |
| `praxis-test-support` | 123 | 3 | `insta` wrappers, `single_file()` fixture helper |
| `praxis-cli` | 225 src + 114 test | 6 | `praxis check` (works); `run`/`watch`/`repl`/`lsp` are honest stubs |

**Skeleton crates** (build + one trivial test; real content arrives at the noted milestone):

| Crate | Fills at | Dependency edges already wired |
|---|---|---|
| `praxis-ast` | M1 | source, syntax |
| `praxis-types` | M2 | source |
| `praxis-hir` | M2 | source, syntax, ast, types |
| `praxis-mir` | M4 | source, types, runtime |
| `praxis-codegen-cranelift` | M4 | mir, runtime (Cranelift dep NOT added yet) |
| `praxis-input-parser` | M6 | source, syntax |
| `praxis-debugger` | M10 | source, runtime |
| `praxis-lsp` | M11 | source, syntax |

### 3.2 Key types a fresh context should know

In `praxis-source`:
- `BytePos(u32)` — opaque byte offset (not `usize`).
- `Span { start: BytePos, len: u32 }` — **private fields**; the only constructor
  is `Span::new(start, end)` which debug-asserts `start <= end`. Inverted spans
  are unrepresentable. Methods: `start/end/len/is_empty/contains/cover`.
- `FileSpan { file: FileId, span: Span }` — a span always carries its file.
  `FileSpan::union` returns `Option` (across-file union is `None`).
- `SourceMap` — interns files (`intern(path, text) -> FileId`), append-only,
  thread-safe (`RwLock`). `get(id) -> Option<FileView>`.
- `LineMap` — byte-accurate, CRLF-aware, stores source bytes for terminator
  trimming. `offset_to_linecol` and `linecol_to_offset` round-trip.
- `Diagnostic` — non-optional `severity`, `code`, `message`, `primary: FileSpan`;
  optional `notes`/`suggestions`. Build via `Diagnostic::build(...).note(...).finish()`.
- `DiagnosticCode { category, number }` — `DiagnosticCategory` is a closed enum
  (`Lex`/`Parse`/`Name`/`Type`/`Input`/`Runtime`); renders as `T003`, `P012`, etc.
- `Renderer::new(&SourceMap).render(&diag, &mut String)` — produces the §8.2
  layout. Also `praxis_source::render_one(map, diag)`.

In `praxis-syntax`:
- `TokenKind` — **M0 subset only**: `Whitespace, LineComment, BlockComment, Ident,
  IntLit, Punct, BacktickTemplate, Unknown, Eof`. M1 must split keywords out of
  `Ident` and split multi-char operators out of `Punct`.
- `Token { kind, span }`.

In `praxis-parser`:
- `lex(file: FileId, text: &str) -> LexOutput` — the **stub lexer**. It recognizes
  trivia, idents, ints, collapsed punct runs, and backtick templates, and emits
  a real `Diagnostic` (`T001` unterminated block comment, `T002` unterminated
  template, `T003` unexpected byte). **M1 replaces this with the real lexer.**

In `praxis-cli`:
- `praxis check <file>` → reads → interns → runs the lexer stub → renders
  diagnostics → exit `0` clean / `1` errors / `2` usage (e.g. missing file).
- The `check` module is the integration point: as M1 adds parsing, thread it in
  after lexing and before rendering.

### 3.3 Decisions already made (in `docs/decisions/`)

- **ADR-001:** snapshot testing uses `insta` (via `praxis-test-support`). Golden
  `.snap` files; bless with `INSTA_UPDATE=always cargo test` or `cargo insta review`.
- **ADR-002:** CI runs `just ci`; the GitHub Actions workflow is a thin wrapper.
- **Renamed Tinsel → Praxis** this session; source extension `.tin` → `.px`.

### 3.4 Testing patterns in use

- **Inline insta snapshots** for diagnostics: `insta::assert_snapshot!(rendered,
  @r#"...expected..."#)`. To update, edit the literal by hand (inline snapshots
  are NOT auto-rewritten by `INSTA_UPDATE`).
- **File snapshots** (`*.snap` in `src/snapshots/`) for the harness smoke test.
  These ARE auto-updated by `INSTA_UPDATE=always`.
- **Integration test** `praxis-cli/tests/check.rs` invokes the compiled binary
  via `CARGO_BIN_EXE_praxis` and checks exit codes + stderr fragments (it does
  NOT snapshot stderr, because the path prefix is absolute/machine-specific —
  assert on path-stable fragments instead).
- Fixtures live in `praxis-cli/tests/fixtures/*.px`.

---

## 4. Status of the §19 Milestone 0 acceptance criteria — all green

Already covered in §3's table. To re-verify from scratch:

```sh
just ci                       # must print "all Praxis checks passed"
cargo run -q -p praxis-cli -- check crates/praxis-cli/tests/fixtures/clean.px      # exit 0
cargo run -q -p praxis-cli -- check crates/praxis-cli/tests/fixtures/bad_byte.px   # exit 1, renders T003
```

---

## 5. Milestone 1 — what to build

**From §19:** *Lossless syntax and basic parser.*

### 5.1 Deliverables (verbatim from the design)

1. **Lexer** (real one — replace the stub).
2. **Lossless syntax tree** (rowan-style: immutable green nodes + red wrappers,
   retaining trivia — §13.1).
3. **Parser** for: literals, bindings (`let`/`var`), blocks, calls, functions,
   arithmetic, `if`, `while`, and `out` calls.
4. **Error recovery** sufficient for LSP use (produce multiple diagnostics from
   one malformed file; never panic).
5. **Basic formatter skeleton.**

### 5.2 Acceptance criteria (verbatim)

- Golden syntax tests cover valid and invalid files.
- Parser produces multiple diagnostics from one malformed file.
- Formatter is idempotent on milestone syntax.
- No panic on fuzzed token streams.

### 5.3 Design constraints that shape the work

- **§4.1 lexical rules:** UTF-8; Unicode identifiers allowed; `//` line comments;
  **nestable** `/* */` block comments; statements separated by newlines or
  semicolons; semicolons optional unless two statements share a line; braces for
  blocks; **backticks delimit parser templates** (the contents are re-scanned by
  the input-parser lexer in M6 — for M1 a backtick template is one token).
- **§13.1 lossless tree:** must retain trivia (whitespace + comments) because the
  formatter, LSP incremental edits, and code actions all need it. "Use immutable
  green nodes plus lightweight red wrappers, or an equivalent rowan-style design."
- **§13.2 AST:** typed wrappers over syntax nodes; avoid copying source strings.

### 5.4 Where things go (recommended)

| Work | Crate | Notes |
|---|---|---|
| Token kinds (split keywords, operators) | `praxis-syntax` | The current `TokenKind` is M0-minimal. Expand it. Multi-char operators (`->`, `=>`, `==`, `!=`, `..=`, etc. from §4) need their own kinds. Keywords (`let`/`var`/`fn`/`if`/`else`/`while`/`for`/`match`/`return`/`read`/`struct`/`enum`/...) split out of `Ident`. |
| Real lexer | `praxis-parser` (`src/lex.rs`) | Replace the stub. Keep the `lex(file, text) -> LexOutput { tokens, diagnostics }` shape — the CLI depends on it. The stub currently collapses punct runs and doesn't split keywords; both must change. |
| Lossless tree (green/red nodes) | `praxis-syntax` (green tree) + `praxis-ast` (red wrappers) per §13.1/§13.2 | `praxis-ast` is currently a skeleton (M1) — this is its first real content. |
| Parser (recursive descent with recovery) | `praxis-parser` | New module, e.g. `src/parse.rs`. Produce the lossless tree + diagnostics. |
| Formatter skeleton | `praxis-parser` or a new `praxis-fmt`? | §14 does NOT list a separate fmt crate. Put it in `praxis-parser` (or ask) until a need to split arises. |
| Golden-tree tests | `tests/parser/` (already scaffolded with a README) + crate-level | Use insta to snapshot the lossless tree. |
| Fuzz target | `praxis-parser` with `cargo-fuzz` or a property test | "No panic on fuzzed token streams." A `proptest` over random byte strings is an acceptable M1 stand-in. |

### 5.5 Integration with existing code

- **Don't break `praxis check`.** The CLI calls `praxis_parser::lex(...)`. If you
  change the lexer's public shape, update `praxis-cli/src/check.rs` too. Ideally
  thread the new parser in after lexing so `check` starts reporting parse errors.
- **Reuse the diagnostic types.** Parse errors are `Diagnostic` with category
  `DiagnosticCategory::Parse` (prefix `P`). Keep using `Diagnostic::build(...)`.
- **Reuse `praxis-test-support`** (`single_file`, insta wrappers) for tests.
- **The current lexer stub's diagnostics codes** (`T001`/`T002`/`T003`) are a
  fine starting vocabulary; the real lexer will add more `T0xx` codes and the
  parser introduces `P0xx` codes.

### 5.6 Suggested sequencing (vertical slices, per §20.2)

The design explicitly prefers vertical slices over horizontal layering. A good
order:

1. **Expand `TokenKind`** (keywords + real operators) in `praxis-syntax`. Tests:
   classify each keyword/operator.
2. **Real lexer** producing the expanded token stream, lossless (trivia as
   tokens). Keep `LexOutput { tokens, diagnostics }`. Snapshot golden token
   streams. **Verify `praxis check` still works** after this.
3. **Lossless tree primitives** (green node + red node) in `praxis-syntax`. The
   tree must round-trip source exactly (formatter requirement). Round-trip test.
4. **Parser for the M1 subset**, one construct at a time, each landing with a
   golden-tree snapshot: literals → bindings → blocks → calls → functions →
   arithmetic → `if` → `while` → `out`. Add error recovery that emits multiple
   diagnostics and keeps parsing.
5. **Formatter skeleton** driven by the lossless tree. Acceptance: idempotent
   (`fmt(fmt(src)) == fmt(src)`) on the M1 subset.
6. **Fuzz/property test** asserting no panic on arbitrary input.
7. **Thread the parser into `praxis check`** so the CLI reports parse diagnostics
   end to end.

### 5.7 Things explicitly NOT in M1 (avoid scope creep)

- Name resolution, type inference → **M2**.
- GC heap, real runtime → **M3**.
- Cranelift JIT, MIR → **M4**.
- Input-parser DSL (`read`/`parse`) → **M6**. For M1 a backtick template is just
  a single token; do not parse its internals.
- LSP → **M11**. But design the parser's error recovery with LSP in mind (§15.2).

---

## 6. Open questions to resolve early in M1

These are small decisions a fresh context should make consciously (and record in
`docs/decisions/`) rather than silently:

1. **Lossless-tree library: hand-rolled rowan-style, or a crate?** §13.1 says
   "immutable green nodes plus lightweight red wrappers, or an equivalent
   rowan-style design." Options: (a) hand-roll green/red nodes (full control,
   more code), (b) use the `rowan` crate (battle-tested, adds a dep). rust-analyzer
   uses `rowan`. **Recommend evaluating `rowan` first** — it's purpose-built for
   exactly this and the design explicitly names the pattern.
2. **Parser technique:** recursive-descent with Pratt for arithmetic is the
   obvious fit (hand-written, great diagnostics, easy recovery). Confirm and
   record.
3. **Where does the formatter live?** No `praxis-fmt` crate in §14. Default:
   inside `praxis-parser` unless it grows large.
4. **Fuzzing approach for M1:** `cargo-fuzz` (needs nightly for libfuzzer) vs a
   `proptest`/quickcheck property test (stable, simpler). The acceptance criterion
   is just "no panic on fuzzed token streams" — a `proptest` feeding random bytes
   to the lexer satisfies it for M1.

---

## 7. Repo map for a fresh context

```
praxis_technical_design.md      # THE CONTRACT — read §14, §19, §13, §4
AGENTS.md                       # the two overriding workspace rules
justfile                        # quality gate (just ci)
.github/workflows/ci.yml        # runs `just ci`
Cargo.toml                      # workspace manifest, [workspace.dependencies]
rustfmt.toml, clippy.toml       # shared fmt/lint policy (stable options only)
docs/
  decisions/                    # ADRs — record M1 deviations here
  handovers/                    # this file
crates/
  praxis-source/                # START HERE — spans, diagnostics (leaf crate)
  praxis-syntax/                # TokenKind (expand for M1)
  praxis-parser/                # lexer stub (replace) + where M1 parser lives
  praxis-ast/                   # SKELETON (M1) — lossless-tree wrappers go here
  praxis-cli/                   # `praxis check`; thread parser in here
  praxis-runtime/ praxis-stdlib/ praxis-test-support/   # done for M0
  praxis-{types,hir,mir,codegen-cranelift,input-parser,debugger,lsp}/  # skeletons
tests/
  parser/                       # EMPTY — golden parser tests go here (§17.1)
  ui/, typecheck/, run-pass/, run-fault/, input-parsers/, aoc-corpus/  # later milestones
```

---

## 8. Quick start for the fresh context

```sh
# 1. Confirm everything is green before touching anything:
just ci

# 2. Read the contract sections that govern M1:
#    praxis_technical_design.md §4.1, §13.1, §13.2, §19 Milestone 1

# 3. Decide + record the open questions in §6 above (rowan? pratt? fmt home? fuzz?).

# 4. Start the vertical slices in §5.6. First: expand TokenKind, keep `just ci` green.

# 5. Make a feature branch for M1 work (don't commit M1 directly to main):
git switch -c milestone-1
```

Good luck. The foundation is deliberately small and well-tested; the types were
built to make the next layer hard to get wrong.
