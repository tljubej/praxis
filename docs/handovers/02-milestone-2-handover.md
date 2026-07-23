# Milestone 2 report & Milestone 3 handover

**Project:** Praxis
**Commit:** `abfb338` — "M2: hover query, method-catalog bridge, CLI integration" (on `milestone-2`)
**Date:** 2026-07-23
**Status:** Milestone 2 complete and green. All five acceptance criteria met. Ready to begin Milestone 3.

> **For a fresh context:** read this document, then `praxis_technical_design.md`
> (the contract — §14 architecture, §19 milestones, §13 IRs, §5 type system,
> §12 GC for M3, §11 runtime ABI). The rest of this file assumes you have NOT
> seen the M2 code yet and tells you what exists, where, and what to do next.

---

## 1. What Praxis is (unchanged from M1)

Praxis is a small, statically typed, garbage-collected language for Advent of
Code-style puzzle solving. Procedural + expression-oriented. Every runtime value
is a GC object referenced through a uniform `GcRef`. First-class input parsing
(`read lines(int)`). JIT-compiled via Cranelift. Ships with an LSP + VS Code
extension. No ownership, no traits, no operator overloading, no exceptions.

The pipeline is:

```
source -> parse -> resolve -> infer -> typed IR -> lower -> Cranelift -> execute
```

`parse` (M1), `resolve`, and `infer` are now real (this milestone). The runtime
heap + Cranelift are Milestones 3 and 4.

The full spec is **`praxis_technical_design.md`** at the repo root. Treat it as
the contract; deliberate deviations go in `docs/decisions/` (rule 20.1).

---

## 2. How to work in this repo (unchanged)

```sh
cargo install just          # one-time
just ci                     # the full quality gate (fmt-check + clippy + test)
                            # — EXACTLY what GitHub Actions runs
just fmt        # reformat (modifies files; NOT part of ci)
just fmt-check  # verify formatting
just clippy     # lint, -D warnings
just test       # cargo test --workspace
just build      # cargo build --workspace
```

**⚠️ Resource note for agents (still load-bearing):** never remove a parser
`ensure_progress()` guard (it guarantees termination on malformed input). Never
run two `cargo` commands concurrently (each spawns the compiler + linker; two at
once can exhaust RAM). Test scoped (`-p <crate> --lib <module>`) before `just test`.

**Two workspace rules from `AGENTS.md` override everything:**
1. *"Make illegal states unrepresentable"* — prefer types that structurally
   forbid bad data over runtime checks or conventions.
2. *"Test every language feature extensively"* — every new behavior ships with
   unit + snapshot tests.

---

## 3. What Milestone 2 delivered

All §19 Milestone-2 deliverables. Acceptance criteria met:

| Criterion | Result |
|---|---|
| Infer non-recursive function parameters and return values from use | ✅ `fn add(a, b){ a + b }` → `(Int, Int) -> Int` (inferred, no annotations) |
| Accept `let a = 4; let a = "Foo"` and resolve each to the correct symbol | ✅ each `let a` gets a distinct `SymbolId` and scheme (Int, then Text) |
| Resolve a shadowing initializer against the previous binding | ✅ `let a = a + 1`'s RHS `a` resolves to the *first* binding |
| Reject cross-type `var` reassignment | ✅ `var x = 0; x = "hi"` → `Y001 expected Int, found Text` (CLI end-to-end) |
| Hover returns inferred type + symbol identity for each shadowed occurrence | ✅ `Analysis::hover(range)` returns distinct `(SymbolId, type)` per occurrence |
| `cargo test --workspace` passes | ✅ **201 tests** (was 118 at M1) |
| `just ci` clean | ✅ |

### 3.1 What changed at the crate level

| Crate | What landed in M2 |
|---|---|
| `praxis-types` | **The inference engine.** `TypeDb` (interned arena, ADR-007), `Type`/`VarId` handles, `TypeData` (Scalar/Unit/Tuple/Func/Var), `VarState` (Unbound/Linked/Generalized), `unify` (with occurs check + level lowering), `generalize`/`instantiate` (Pottier levels, ADR-008), `pretty` rendering. Reuses `ScalarType`/`CollectionCtor` from `praxis-stdlib` (rule 20.3). |
| `praxis-ast` | **Typed AST wrappers** (ADR-009). `AstNode` trait (`cast`/`syntax`/`span`) + minimal node wrappers (SourceFile, LetStmt, VarStmt, AssignStmt, FnItem, Param/List, ExprStmt, TypeRef, all expression nodes, TupleExpr). Names stay bare `Ident` tokens. |
| `praxis-hir` | **Name resolution + type inference.** `Symbol`/`SymbolId`/`SymbolKind`, `ScopeTree` (lexical, shadowing), `NameTable` (symbol table), `resolve` (walks the typed tree, `N001`/`N002`), `infer` (HM with let-generalization, `Y001`/`Y002`), `hover`, `catalog` (Type→TypePattern bridge). `analyze(file, &SourceFile) -> Analysis`. |
| `praxis-parser` | **Real type annotations + tuples.** `parse_type()` (scalar/tuple/fn types, right-assoc `->`), `TUPLE_EXPR`/`TUPLE_TYPE`/`FN_TYPE`/`TYPE_REF` kinds, optional param type annotations (for inference-from-use, §4.9). M1 snapshots unchanged. |
| `praxis-cli` | `praxis check` now runs `analyze()` after parse and renders `N0xx`/`Y0xx` end to end. |
| `praxis-syntax` | `TUPLE_EXPR`/`TYPE_REF`/`TUPLE_TYPE`/`FN_TYPE` `SyntaxKind` variants. |

New dependency edges: `praxis-types` → `praxis-stdlib`; `praxis-hir` →
`praxis-stdlib`, `rowan`; `praxis-cli` → `praxis-hir`. The DAG stays clean (no
cycles).

### 3.2 Decisions recorded in M2 (in `docs/decisions/`)

- **ADR-007:** type representation is an **interned arena** (`TypeDb`); type
  variables live in the arena, lifecycle states are an explicit enum.
- **ADR-008:** let-generalization uses **Pottier-style binding levels** (sound
  with `var`); `var` is never generalized.
- **ADR-009:** **minimal typed AST wrappers** (incremental; only M2's nodes).
- **ADR-010:** **method catalog bridge** in M2; `.method()` dispatch deferred to
  M5 (needs collections).

### 3.3 The M2 type universe & inference behavior

- **Types:** `Int`, `Text`, `Bool`, `Unit`, `Never` (constructible); tuples;
  functions; type variables. `UInt`/`Float`/`Byte`/`Char` reserved (§4.3) → using
  one in an annotation is `N002 unknown type`.
- **Arithmetic** (`+ - * / %`) forces `Int` operands, yields `Int` (§4.12).
  **Comparisons** (`== != < > <= >=`) require matching operands, yield `Bool`.
  `||` yields `Bool`; unary `-`→`Int`, `!`→`Bool`.
- **`let`** generalizes; **`var`** never does. **`fn`** is bound to a
  monomorphic placeholder (for recursion), inferred, then generalized. Recursive
  functions require annotations (criterion 1 is non-recursive only).
- **Builtins:** `out` = `forall T. (T) -> Unit`; `panic` = `forall T. (T) -> Never`.
- **Diagnostics:** `N001` unresolved name, `N002` unknown type, `Y001` type
  mismatch, `Y002` occurs/infinite type, `Y003` annotation conflict (reserved).

---

## 4. Key types a fresh context should know

In `praxis-types`:
- **`TypeDb`** — the interned type arena. `int()/text()/bool()/unit()/never()`,
  `func(params, result)`, `tuple(els)`, `fresh_var()`, `enter_level`/`exit_level`/
  `scoped_return`, `prune`/`follow`, `unify(a,b) -> Result<(), UnifyError>`,
  `generalize(t) -> Scheme`, `instantiate(&scheme) -> Type`, `render(t)`/`render_scheme`.
- **`Type`/`VarId`** — copyable `u32` handles. **Distinct from the runtime's
  `TypeId`** (descriptor id) — do not confuse them.
- **`Scheme { quantified, body }`** — `forall <quantified>. body`;
  `monotype(t)` for the empty case.

In `praxis-ast`:
- **`AstNode`** trait — `cast(SyntaxNode) -> Option<Self>`, `syntax()`, `span()`.
- **`Expr`** enum — `Literal/Path/Bin/Unary/Paren/Tuple/Block/If/While/Call/Error`.
  `cast_from_child(SyntaxNode)` dispatches over expression kinds once.

In `praxis-hir`:
- **`analyze(file: FileId, root: &SourceFile) -> Analysis`** / **`analyze_root
  (file, &SyntaxNode) -> Analysis`** — the entry point the CLI calls after parse.
- **`Analysis { db, names, scopes, refs, ref_types, diagnostics }`** — the full
  semantic result. `is_clean()` if no diagnostics.
- **`Symbol { id, name, kind, decl, scheme }`** / **`SymbolId`** — distinct ids
  per declaration (shadowing mints new ids).
- **`Analysis::hover(range) -> Option<HoverInfo>`** — criterion 5.

---

## 5. Open questions / known limitations to resolve later

1. **`.method()` dispatch is deferred to M5** (ADR-010). The `Type → TypePattern`
   bridge exists; expression-level dispatch needs collection types in `TypeDb`.
2. **The internal capability system** (`Numeric(T)`, `Iterable(T, Item)`, …, §5.4)
   is deferred to M7, where records/structural operations first need it.
3. **`fn` expressions / closures** (`let id = fn(x){x}`) are **not parsed** as
   expressions in M2 — `fn` is only a top-level `FN_ITEM`. So let-generalization
   of a *polymorphic* `let` is not yet exercisable from source (the only
   polymorphic schemes in M2 are the builtins `out`/`panic`). Closures land in M7.
4. **Anonymous records, enums, pattern matching, exhaustive match** → M7. Tuples
   ARE in M2.
5. **Names are bare `Ident` tokens** (ADR-009) — no `NAME`/`NAME_REF` wrapper
   nodes. Refinement point if the LSP wants richer name structure.
6. **Recursive functions require annotations** (criterion 1 is non-recursive).
   The monomorphic-placeholder approach is sound but won't infer a recursive
   signature from use alone.
7. **Hover is a library query**, not yet an LSP feature (M11). The CLI does not
   expose hover; it's tested at the `praxis-hir` level.

---

## 6. Milestone 3 — what to build

**From §19:** *Uniform object heap and collector.*

### 6.1 Deliverables (verbatim from the design)

- `GcRef` and object header representation.
- Type descriptors for `Unit`, `Bool`, `Int`, `Byte`, `Char`, and `Text`.
- Precise non-moving mark-and-sweep collector.
- Root-frame API and runtime context.
- Immortal singleton support for `Unit` and booleans.
- Allocation and payload access helpers.

### 6.2 Acceptance criteria (verbatim)

- Every runtime value in interpreter/runtime tests is a `GcRef`.
- Scalar allocation, tracing, formatting, equality, and collection work correctly.
- GC stress tests preserve nested references.
- No source-language storage slot or public runtime wrapper uses an unboxed
  scalar ABI.

### 6.3 Where things go (recommended)

| Work | Crate | Notes |
|---|---|---|
| GC heap, header, collector | `praxis-runtime` (already has `GcRef`, `GcHeader`, `TypeDescriptor` from M0) | M0 shipped the *shapes*; M3 fills them in. §12.1–12.6. |
| Descriptors for scalars | `praxis-runtime` (`descriptor.rs`) | The M0 `TypeDescriptor` is the vtable; add real `trace`/`drop`/`format`/`equals`/`hash` for Unit/Bool/Int/Byte/Char/Text. |
| Root frames, `RuntimeContext` | `praxis-runtime` (`context.rs`) | M0 has a `RuntimeContext` skeleton. §12.3, §11. |
| Immortal singletons | `praxis-runtime` | `Unit` and `true`/`false` as pre-allocated immortal objects (§4.3). |

### 6.4 Integration with M2 code

- The **static** type system (`praxis-types`) and the **runtime** descriptors
  (`praxis-runtime::TypeDescriptor`) are *deliberately separate* in M2. They will
  meet when M4 lowers typed HIR to MIR and emits descriptor references. Do not
  merge them yet.
- M3 does **not** touch the front end. The `praxis check` pipeline is unchanged;
  M3 adds runtime tests under `praxis-runtime`.

### 6.5 Things explicitly NOT in M3

- Cranelift JIT, MIR → **M4**.
- `Text`/`Vec` as full collections with methods → **M5** (M3 ships their
  descriptors and allocation, not their method surface).
- Input-parser DSL → **M6**. Records/enums/closures → **M7**.

---

## 7. Repo map for a fresh context

```
praxis_technical_design.md      # THE CONTRACT — read §12, §19-M3, §11, §5
AGENTS.md                       # the two overriding workspace rules
justfile                        # quality gate (just ci)
docs/
  decisions/                    # ADR-001..010 — read 007/008/009/010 for M2 choices
  handovers/                    # this file + 00/01 milestone handovers
crates/
  praxis-types/                 # TypeDb, unify, generalize, pretty (M2)
  praxis-ast/                   # AstNode + typed wrappers (M2)
  praxis-hir/                   # resolve, infer, hover, catalog (M2)
  praxis-source/                # spans, diagnostics (unchanged since M0)
  praxis-syntax/                # SyntaxKind (added tuple/type nodes in M2)
  praxis-parser/                # lex, parse (+ types/tuples in M2), fmt
  praxis-cli/                   # praxis check (lex + parse + analyze + render)
  praxis-runtime/ praxis-stdlib/  # START HERE for M3 — GC + descriptors
  praxis-{mir,codegen-cranelift,input-parser,debugger,lsp}/  # skeletons (later)
tests/
  parser/  ui/, typecheck/, run-pass/, run-fault/  # later
```

---

## 8. Quick start for the fresh context

```sh
# 1. Confirm everything is green before touching anything:
just ci

# 2. Read the contract sections that govern M3:
#    praxis_technical_design.md §12 (GC), §11 (runtime ABI), §19 Milestone 3.

# 3. Read ADR-007 (type representation) to understand what M3 must NOT merge with.

# 4. Start in praxis-runtime (descriptors + collector). First vertical slice:
#    allocate an Int GcRef, trace it, format it, collect it.

# 5. Branch off milestone-2:
git switch -c milestone-3
```

Good luck. The front end now parses, resolves names, and infers types end to end;
the runtime heap is the foundation the JIT will allocate into.
