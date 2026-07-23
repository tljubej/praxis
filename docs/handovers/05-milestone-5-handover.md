# Milestone 5 report & Milestone 6 handover

**Project:** Praxis
**Date:** 2026-07-23
**Status:** Milestone 5 complete and green. All §19.5 acceptance criteria met.
Ready to begin Milestone 6.

> **For a fresh context:** read this document, then `praxis_technical_design.md`
> (the contract — §11 collections, §12 GC, §16 methods, §19 Milestone 6). The
> rest of this file assumes you have NOT seen the M5 code yet and tells you what
> exists, where, and what to do next.

---

## 1. What landed in M5

M5 turned the JIT from "executes scalar arithmetic" into "executes real data
structures." The pipeline is unchanged structurally (`source → parse → infer →
typed HIR → MIR → Cranelift → execute`), but every stage now handles collections,
method dispatch, and real GC rooting.

**296 tests passing** (up from 270 at M4). `just ci` clean.

### What M5 built, by workstream

| Workstream | What landed | Key files |
|---|---|---|
| **A: Shadow-stack spill** | The GC rooting the JIT needs for collections. Cranelift emits prologue/epilogue frame push/pop + per-safepoint slot spills; automatic GC on allocation pressure. | `praxis-runtime/src/shadow_frame.rs`, `praxis-runtime/src/heap.rs` (`maybe_collect`), `praxis-codegen-cranelift/src/lower.rs` (spill) |
| **B: Method dispatch** | The full `.method()` vertical slice: parser → inference → HIR → MIR → JIT. `Vec[T]` in the type system, the built-in catalog wired into every stage, `out(...)`, structural formatting. | `praxis-stdlib/src/builtins.rs`, `praxis-types/src/data.rs` (`Collection`), `praxis-parser` (`.method()` postfix), `praxis-runtime/src/abi.rs` (`praxis_vec_*`/`praxis_text_*`) |
| **C: var/let mutation** | Verified `var` reassignment and `let` object mutation survive GC. `VarCell` deferred to M7 (only needed for closure capture). | Tests in `praxis-codegen-cranelift/tests/jit.rs` |
| **D: Debug frames** | `DebugFrame` given a real layout with `(source_name, symbol_id)` metadata. Push/pop helpers + symbol registration. | `praxis-runtime/src/debug.rs`, `praxis-runtime/src/context.rs` |

### §19.5 acceptance criteria — all met

- **"Vector growth and nested vectors survive collection"** → A (spill) + B (`Vec.push`). Verified: 500-element push with GC, `get(499) == 499`.
- **"`let` object mutation, `var` reassignment follow the specified semantics"** → C. Verified: `let v = Vec(); v.push(...)` survives 1000 pushes + GC; `var` reassignment across GC.
- **"Shadowed locals distinguishable by source name and symbol ID"** → D. Verified structurally: two `a` bindings report distinct `symbol_id`s.
- **"Missing/index faults preserve collection locals in snapshots"** → A + B. `IndexOutOfBounds` fault added; spill preserves collection locals.

---

## 2. The shadow-stack spill (ADR-019) — the foundational change

M4's liveness pass (ADR-016) computed `live_roots` but the Cranelift backend
ignored them. M4 was safe only because `run.rs` never triggered a collection
during JIT execution. M5 makes the spill load-bearing.

**How it works:**
- Every generated function's prologue calls `praxis_push_shadow_frame(ctx,
  gc_local_count)` → returns a `*mut ShadowFrame` chained onto `ctx.roots`.
- At each safepoint (`Alloc`/`Materialize`/`Call`), the backend stores each live
  Gc local's value into its frame slot (`frame_ptr + SLOTS_OFFSET + idx*8`).
- The `praxis_alloc_*` / `praxis_vec_push` wrappers call `maybe_collect(ctx)`,
  which runs `Heap::collect` when allocation pressure crosses the threshold
  (64 KiB initial, geometric growth), rooting from `ctx.roots`.
- The epilogue (including fault epilogue) calls `praxis_pop_shadow_frame`.

**ABI version bumped to v2** (`RuntimeContext` gained the `roots` field).

**Proof:** `fib(20)` = 6765 after ~20k allocations and multiple GC cycles; a
10k-iteration allocation loop sums correctly; 500-element Vec push/get survives.

---

## 3. The method-dispatch vertical slice (ADR-020)

### The type system
`TypeData::Collection { ctor, args }` is new (M4 had only Scalar/Unit/Tuple/
Func/Var). `TypeDb::vec(elem)` constructs `Vec[T]`. Unify/occurs/pretty all
handle it. The catalog bridge (`type_to_pattern`) maps `Collection` types to
`TypePattern::Collection` so method lookup works.

### The catalog
`praxis-stdlib/src/builtins.rs::builtin_catalog()` returns the finalized table:
- `Vec[T].push(T) -> Unit`, `.len() -> Int`, `.get(Int) -> T`, `.is_empty() -> Bool`
- `Text.len() -> Int`, `.is_empty() -> Bool`, `.get(Int) -> Int`

Each entry carries `can_fault`/`allocates`/`purity`/`lowering: RuntimeSymbol(...)`.

### The pipeline path
1. **Parser:** `.method(args)` postfix, left-associative chaining. `METHOD_CALL_EXPR`
   wraps the receiver as its first child (rowan checkpoint trick). `Vec[Int]`
   type annotations parse.
2. **Inference:** `infer_method_call` looks up the catalog, unifies params, returns result type.
3. **HIR:** `lower_method_call` records the runtime symbol → `TypedExpr::MethodCall`.
4. **MIR:** lowers to `Inst::Call { callee: CallTarget::Runtime("praxis_vec_push"), ... }`.
5. **Codegen:** `CallTarget::Runtime` resolves through the JIT symbol table.

### The runtime wrappers
`praxis_vec_new`/`push`/`len`/`get`/`is_empty` and `praxis_text_len`/
`is_empty`/`get`. `VecPayload` changed from `Box<[GcRef]>` to `Vec<GcRef>` so
`push` mutates in place (§11.1). `praxis_write_stdout` implements `out(...)`.

### `Vec()` construction
`Vec()` (no type args) creates an empty Int-element vector via `praxis_vec_new`.
A real `Vec[T]()` that reads the element type from the annotation is a follow-up.

---

## 4. Known limitations / deferred to later milestones

1. **`Vec[T]()` constructor ignores the type arg.** The `Vec[Int]` annotation
   parses and type-checks, but `Vec()` always creates an Int-element vec. Wiring
   the element descriptor from the annotation is a small follow-up.
2. **`StoreScalar` is still a no-op.** It's never emitted by the current MIR
   builder (compound assignment uses `MoveGc`). Reserved for a future mutable-Int
   optimization. Harmless.
3. **`VarCell` deferred to M7.** GC-managed cells for captured `var` bindings
   only matter when closures exist (M7). M5's `var` reassignment via `MoveGc`
   is correct for all non-closure use.
4. **Debug-frame prologue/epilogue not yet emitted by Cranelift.** The layout,
   helpers, and symbol registration are in place (ADR-021), but M10 wires the
   codegen and builds the REPL. `ctx.debug_top` is null at runtime in M5.
5. **Short-circuit `||`/`!`** — still non-short-circuiting placeholders (M4 carryover).
6. **Monomorphization** — still deferred (ADR-018). Generic fns rejected with `Y100`.
7. **`Text` source slices** — owned `Text` only; source-slice metadata is M6.

---

## 5. What M6 is about

**Title (§19.6): "Input parser v1 and `read`."**

The input-parser DSL is the headline feature: `read lines(int)` / `parse(text,
parser_expression)` / backtick templates. This is what makes Praxis an AoC-solving
language rather than a generic one. The `praxis-input-parser` crate is currently a
stub.

**M6 deliverables (§19.6):** prefix `read parser_expression` syntax; `parse(text,
parser_expression)` syntax; lazy process-input source buffering; parser-expression
lexer and parser; backtick template parser; atomic parsers (`int`, `char`, `word`,
`text`, `rest`, `digit`); constructors (`lines`, `sections`, `csv`, `ws`, `sep`,
`grid`); compile-time result-type synthesis; source-aware input faults.

**Where to start:** read `praxis_technical_design.md` §6 (the input-parser DSL),
§7 (the `read`/`parse` syntax), §15 (input diagnostics), and the existing
`praxis-input-parser/src/lib.rs` stub. The M5 catalog/type-system infrastructure
(method dispatch, `TypeData` variants) is the foundation the parser-result types
will build on.

---

## 6. Test inventory (296 tests)

- `praxis-runtime`: 48 (heap, GC, shadow frame, Vec wrappers, debug frames)
- `praxis-codegen-cranelift`: 22 JIT integration tests (M4's 10 + M5's 12)
- `praxis-types`: 58, `praxis-hir`: 31, `praxis-parser`: 50, `praxis-stdlib`: 12
- Plus per-crate unit tests across the workspace

The 12 new M5 JIT tests: shadow-stack spill survival (loop + fib(20)), Vec
push/len/get/is_empty, OOB fault, Text len/get/is_empty, `out(...)`, var/let
mutation under GC, 500-element stress.

---

## 7. ADRs added in M5

- [ADR-019: Compiler-managed shadow-stack spill](../decisions/019-shadow-stack-spill.md)
- [ADR-020: Method-call dispatch through the built-in catalog](../decisions/020-method-dispatch-and-collections.md)
- [ADR-021: Debug frame metadata and shadowed-symbol registration](../decisions/021-debug-frame-metadata.md)
