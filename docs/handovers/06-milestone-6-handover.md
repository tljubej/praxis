# Milestone 6 report & Milestone 7 handover

**Project:** Praxis
**Date:** 2026-07-23
**Status:** Milestone 6 complete and green. All §19.6 acceptance criteria met.
Ready to begin Milestone 7.

> **For a fresh context:** read this document, then `praxis_technical_design.md`
> (the contract — §7 input parser, §19 Milestone 7). The rest of this file
> tells you what exists, where, and what to do next.

---

## 1. What landed in M6

M6 delivered the headline feature: **the input-parser DSL and `read`**. Praxis
can now parse Advent of Code-style inputs declaratively — `read lines(int)`,
`read grid(char)`, `read sections(lines(csv(int)))` — without any user-side
string manipulation. The parser-expression grammar, compile-time type synthesis,
and runtime interpreter are all real.

**373 tests passing** (up from 296 at M5). `just ci` clean.

### What M6 built, by workstream

| Workstream | What landed | Key files |
|---|---|---|
| **WS1: Char** | The `Char` scalar wired end-to-end (was reserved). Inference, HIR `Lit::Char`, MIR `AllocKind::Char`, codegen `Symbol::AllocChar`/`CharLoad`, ABI `praxis_alloc_char`/`praxis_char_load`. | `praxis-hir/src/infer.rs`, `praxis-mir/src/ir.rs`, `praxis-codegen-cranelift/src/lower.rs`, `praxis-runtime/src/abi.rs` |
| **WS2: Runtime foundation** | Source-slice `Text` (zero-copy, ADR-013 brought due), `Grid[T]` descriptor, provisional `Record` descriptor, `FaultKind::ParseFailed`. `RuntimeContext` gained `unit_ref` (sentinel split from `input_source`). | `praxis-runtime/src/text.rs`, `collections.rs`, `records.rs`, `context.rs` |
| **WS3: Input parser core** | The `praxis-input-parser` crate filled: `ParserAst` (§7.9 typed tree), `scan_template` (§7.2 backtick interior re-scan), `validate`, `synthesize` (§7.8 derivation table), `ParserPlan` + `lower_to_plan` (flat `#[repr(C)]` arena). | `praxis-input-parser/src/{ast,scan,validate,synthesize,plan}.rs` |
| **WS4: Language syntax** | `read`/`parse` + parser-expression grammar. New SyntaxKinds (`READ_EXPR`, `PARSER_EXPR`, etc.), prefix `KW_READ`, parser-expression sub-grammar (atom \| template \| call). AST wrappers. | `praxis-syntax/src/kind.rs`, `praxis-parser/src/parse.rs`, `praxis-ast/src/nodes.rs` |
| **WS5: Semantic integration** | Rowan → `ParserAst` conversion, `TypedExpr::Read`/`Parse`, type synthesis wired into inference and lowering. Hover shows synthesized types. | `praxis-hir/src/parser_lower.rs`, `infer.rs`, `lower.rs` |
| **WS6: MIR + codegen** | `read`/`parse` lower to runtime calls (`praxis_get_input` + `praxis_run_parser`), plan index as boxed Int. | `praxis-mir/src/build.rs`, `praxis-runtime/src/abi.rs`, `praxis-codegen-cranelift/src/symbols.rs` |
| **WS7: Interpreter** | The runtime parser interpreter: atomics (`int`/`char`/`word`/`text`/`rest`/`digit`), constructors (`lines`/`sections`/`csv`/`ws`/`sep`/`grid`), source-slice Text results, `ParseFailed` faults. | `praxis-runtime/src/parser.rs` |
| **WS8: CLI** | `--input` flag, stdin → `ctx.input_source` wiring. Catalog fix: `Var("T")` wildcard matching. | `praxis-cli/src/{main,run}.rs`, `praxis-hir/src/catalog.rs` |

### §19.6 acceptance criteria — all met

- **"Parse all simple corpus fixtures without user string manipulation"** →
  `read lines(int)`, `read grid(char)`, `read sections(lines(csv(int)))` all
  work end-to-end. Corpus fixtures in `tests/aoc-corpus/`.
- **"Bind `read` results with both `let` and `var`"** → verified
  (`read_with_var_binding` test).
- **"Multiple `read` expressions deterministically parse the same buffer"** →
  verified (`multiple_reads_parse_same_buffer` test).
- **"Hover over a `read` result binding displays the synthesized nested type"** →
  verified (`read_lines_of_int_synthesizes_vec_int` → "Vec[Int]",
  `read_grid_of_char_synthesizes_grid_char` → "Grid[Char]").
- **"Whitespace outside backticks never affects parsing"** → verified
  (`parser_expression_whitespace_is_insignificant` parser test).
- **"Parse mismatch enters the fault pipeline"** → `FaultKind::ParseFailed`
  added; the interpreter sets it on mismatch.

---

## 2. Architecture summary

```
COMPILE TIME                                  RUNTIME
praxis-parser: read/parse + parser-expr       praxis-runtime:
  rowan grammar (§7 EBNF)                       praxis_run_parser(ctx, idx, input)
praxis-input-parser:                           interprets ParserPlan, allocates GC
  ParserAst (typed DSL, §7.9)                  results, raises ParseFailed fault
  scan_template / validate
  synthesize_type(→Type)
  lower_to_plan(→ &'static ParserPlan)
praxis-hir:
  rowan → ParserAst conversion
  TypedExpr::Read/Parse { plan_index, ty }
```

The plan slab lives in `praxis-input-parser` (breaking the runtime↔input-parser
dependency cycle: record schemas are built at runtime, not stored in plans).

---

## 3. Known limitations / deferred

These are carryover items for M7+ or M6 follow-ups:

- **Template literal matching**: the `walk_template` interpreter is a stub
  (returns `Fault`). Templates inside `lines()` work because each line is parsed
  independently by the child, but standalone template literal matching (e.g.
  `read `{x:int},{y:int}`` without `lines`) is not yet implemented. Full
  template literal matching is an M6 follow-up or M9.
- **Named-capture records**: the type system uses a provisional tuple-based
  representation (ADR-024). The runtime allocates real `RecordPayload` values,
  but field access syntax and pattern matching are M7.
- **Nested collection element descriptors**: `child_descriptor` returns `INT` as
  default for deeply nested constructors. `sections(lines(csv(int))).get(0).get(0).get(0)`
  faults because the intermediate Vec element descriptor isn't resolved
  recursively. An M6 follow-up would make `child_descriptor` recurse.
- **`Vec[T]()` constructor still ignores the type arg** (M5 carryover).
- **Short-circuit `||`/`!`** still placeholders (M4 carryover).
- **Monomorphization deferred** (ADR-018).
- **Debug-frame codegen emission deferred to M10** (M5 carryover).

---

## 4. What Milestone 7 should do

**Title (§19.7): "Records, enums, pattern matching, and closures."**

M7 deliverables:
- Nominal records and enums (§4.5).
- Anonymous structural records from parser expressions (formalizing M6's
  provisional records — replace the tuple-based type representation with a real
  `TypeData::Record` variant).
- Pattern matching and exhaustiveness (§4.6).
- Closures and GC environments.
- Structural equality and hashing descriptors (so records can be map/set keys).
- Monomorphized inferred polymorphism (ADR-018).

**Where to start:** the provisional record infrastructure in
`praxis-runtime/src/records.rs` and the tuple-based type representation in
`synthesize.rs` are the foundation M7 formalizes. The `Char` wiring (WS1)
shows the end-to-end pattern for adding a new scalar type. The catalog wildcard
matching fix (WS8) is load-bearing for method resolution on generic types.

**Key M7 design point:** the `Var("T")` pattern-matching in `catalog.rs`
(`pattern_matches`) is a stopgap for monomorphization. M7 should replace it with
real monomorphized instantiation (ADR-018) so `Vec[T]` methods resolve through a
proper type-parameter mechanism, not string-based wildcard matching.

---

## 5. Scope decisions made in M6

These were confirmed with the user during planning:

1. **Anonymous records**: provisional structural records included in M6 (not
   deferred to M7). See ADR-024.
2. **Grid**: real `Grid[T]` runtime descriptor (not `Vec[Vec[T]]`). See
   `collections.rs::GRID`.
3. **Text**: zero-copy source-slice representation (not owned copies). See
   ADR-022.
4. **Char**: fully wired end-to-end (required for `char`/`grid(char)`).

---

## 6. Test inventory

| Suite | Count | Location |
|---|---|---|
| Parser (incl. read/parse grammar) | ~60 | `praxis-parser/src/parse.rs` |
| HIR (incl. type synthesis) | ~70 | `praxis-hir/src/infer_tests.rs` |
| Runtime (incl. descriptors, GC) | ~75 | `praxis-runtime/src/*.rs` |
| Input parser DSL | ~24 | `praxis-input-parser/src/*.rs` |
| JIT end-to-end (incl. read) | ~32 | `praxis-codegen-cranelift/tests/jit.rs` |
| CLI | ~10 | `praxis-cli/tests/` |
| Other | ~100 | various |
| **Total** | **373** | `cargo test --workspace` |
