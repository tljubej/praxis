# Milestone 7 Part 2 — progress report & handover (in-progress)

**Project:** Praxis
**Date:** 2026-07-24
**Status:** M7 Part 2 partially complete. WS6 (eq/hash + tuple runtime) and WSP
(pattern-matching completeness) are landed and green. WS7 (closures) frontend +
runtime infra are landed; the closure *lowering* (capture analysis, MIR, codegen)
remains. WS8–WS10 not started. **447 tests passing**, `just ci` clean.

> **For a fresh context:** read this document, then
> `07-milestone-7-part1-handover.md` (the Part 1 handover), then
> `praxis_technical_design.md` (the contract — §4.6, §4.10, §5.5, §13.6, §19.7).

---

## 1. What landed in M7 Part 2 so far

| Workstream | What landed | Commit |
|---|---|---|
| **WS6: Structural eq/hash + tuple runtime** | Tuple runtime rep (`TuplePayload`/`TUPLE` descriptor TypeId 10/`AllocKind::Tuple`/ABI `praxis_alloc_tuple`/`set`/`get`/codegen `tuple_schema_for`). eq/hash on RECORD/ENUM/TUPLE (mirroring `vec_equals`/`vec_hash`). Internal capability check (`capability.rs`: `supports_eq`/`supports_hash`). `==`/`!=` branching: composite → `Inst::StructEq` → `praxis_struct_eq`; scalar → native `IntCmp`. `Y004` for non-equatable. ADR-026. **Closes §19.7 criterion "tuples/records as keys" (eq/hash machinery; M8 containers close end-to-end).** | `0e57872` |
| **WSP: Pattern completeness** | Recursive `TypedPattern` (Wildcard/Lit/Bind/EnumVariant with nested subpatterns). Fixes two silent WS5 bugs: nested patterns were dropped, literal patterns always matched first arm. MIR `lower_match` rewritten as a decision tree (`emit_pattern_test` recurses through subpatterns). Exhaustiveness checker (`exhaustive.rs`): enum/Bool closed-set coverage; other types require `_`. `Y120` non-exhaustive / `Y121` unreachable. AST `Pattern::literal_token()`. **Closes §19.7 criterion "reject non-exhaustive matches."** | `d5a8a36` |
| **WS7 (partial): Closure frontend + runtime** | Parser: `|params| expr` via bare `PIPE` in `parse_prefix` (max-munch keeps `\|\|` as `PIPE2`). `CLOSURE_EXPR` kind, `ClosureExpr` AST wrapper, `Expr::Closure`. Resolver + inference: closures type-check as `Func`, capture outer vars through scope chain. Runtime: `ClosurePayload { fn_ptr, env }` + `CLOSURE` descriptor (TypeId 11) + ABI wrappers. ABI bumped to 5. **Lowering is a placeholder (Unit); runtime lowering is the remaining work.** | `40dbeb1`, `be52d57` |

### §19.7 acceptance criteria — current status

| Criterion | Status | Notes |
|---|---|---|
| Store parser-generated records in vectors and maps | ✅ vectors; ❌ maps (M8) | |
| Use tuples and records as set/map keys | ✅ eq/hash | WS6 closes the eq/hash machinery; M8 containers close end-to-end keying |
| Compile closure pipelines with captured values | ❌ partial | WS7 frontend + runtime infra done; lowering (capture, indirect call) remains |
| Reject non-exhaustive matches | ✅ | WSP exhaustiveness checker (Y120/Y121) |

---

## 2. Key design decisions made in Part 2

1. **Tuple schema cache keyed by descriptor sequence, not `Type` id.** The type
   arena doesn't structurally intern (`intern` just appends), so two `(Int, Int)`
   literals get different `Type` ids. `tuple_schema_for` keys on the resolved
   element-descriptor pointer sequence for true structural de-duplication, so
   same-shaped tuples share one schema and compare structurally equal.
2. **`==`/`!=` lowering branches on operand type** (MIR build.rs comparison arm):
   composite GC types → `Inst::StructEq` → `praxis_struct_eq(ctx, a, b)`; scalars
   and ordering ops stay on native `IntCmp`. `!=` lowers as `!(==)`.
3. **Recursive `TypedPattern`** replaces the flat `(variant_idx, bindings)`. The
   MIR emits a decision tree (`emit_pattern_test` / `emit_subpattern_tests`).
4. **Exhaustiveness runs during `lower()`**, not `analyze()` — it needs the
   lowered patterns. Diagnostics go to `TypedModule.diagnostics`; the test helper
   `has_type_error_with_lower` runs the full pipeline.
5. **Capability check is internal** (§5.4): `Y004` wording says "values of type
   `T` cannot be compared with `==`" — never mentions trait/capability.

---

## 3. What remains for M7 Part 2 completion

### WS7 — Closure lowering (the big remaining piece)

**What's done:** parser (`|params| expr`), AST, resolver, inference (closures
type-check as `Func` with captures), runtime descriptor + ABI wrappers.

**What remains (dependency-ordered):**

1. **Capture analysis** (`praxis-hir/src/capture.rs`): walk a closure body, find
   free variables (refs resolving to outer-scope bindings). Classify as by-value
   (`let` captures — copied into env) vs mutable (`var` captures — need `VarCell`).
2. **HIR `TypedExpr::Closure`**: carry `{ params, body, captures, fn_type }` plus
   a synthesized unique name for the closure's MIR function. Replace the current
   Unit placeholder in `lower_closure`.
3. **Synthetic MIR function**: emit one extra `Function` per closure (appended to
   the module's `Vec<Function>`). Its signature is `fn(ctx, params..., env...)`.
   The env values are loaded via `praxis_closure_capture` at function entry and
   bound to the captured symbols in `b.locals`.
4. **`AllocKind::Closure { fn_name, captures }`**: construct the closure object
   via `praxis_alloc_closure(ctx, fn_ptr, n)` + `praxis_closure_set_capture`.
5. **Indirect calls**: `Inst::CallIndirect { dst, callee_local, args, env }` —
   reads the fn_ptr via `praxis_closure_fn_ptr`, then a Cranelift `call_indirect`.
   The current `Inst::Call` only handles static `CallTarget::User(name)`.
   **This is the highest-risk sub-task** — Cranelift indirect calls need a
   signature + function pointer.
6. **`VarCell` (mutable captures)**: `var` bindings captured by a closure need a
   GC-managed heap cell so the closure shares the variable. Affects `Let`/`Var`/
   `Assign` lowering. Gate behind mutable-capture path. **Second-highest risk.**
7. **Tests**: `let o=10; let f=|x| x+o; f(5)`→15; mutable capture; closure in Vec.
8. **ADR-027**: closure representation.

**Simplest path to a working demo:** start with immutable captures only (defer
`VarCell`). A closure `|x| x + offset` where `offset` is a `let` copies the value
into env. This covers `values.map(|x| x + offset)`-style use.

### WS8 — Monomorphization (ADR-018)
- `praxis-hir/src/mono.rs`: instantiate polymorphic callees, cache by
  `FunctionId + canonical type args`. Remove Y100 gate (`lower.rs:440`).
- Review `catalog.rs` `Var("T")` wildcard — likely correct, keep it.
- `Vec[T]()` honor type arg (`build.rs:268`).

### WS9 — Input-parser carryovers
- `child_descriptor` recursion for nested constructors.
- `walk_template` real standalone template matching.

### WS10 — Docs
- Final M7 handover (`07-milestone-7-handover.md`).
- ADR-027 (closures); update ADR-018 (mono), 024 (superseded).
- README → "Milestone 7 complete."
- Corpus fixtures.

---

## 4. Test inventory (447 total)

| Suite | Count | Location |
|---|---|---|
| JIT end-to-end | ~72 | `praxis-codegen-cranelift/tests/jit.rs` |
| HIR (incl. eq capability, exhaustiveness, closure inference) | ~69 | `praxis-hir/src/*.rs` |
| Runtime (incl. tuple, record/enum eq/hash, closure descriptor) | ~79 | `praxis-runtime/src/*.rs` |
| Types | ~44 | `praxis-types/src/types_tests.rs` |
| Parser | ~60 | `praxis-parser/src/parse.rs` |
| Other | ~123 | various |
| **Total** | **447** | `cargo test --workspace` |

---

## 5. Where to start to resume

**WS7 closure lowering** is the natural next step. The runtime infrastructure
(`ClosurePayload`, ABI wrappers) and frontend (parse/resolve/infer) are ready.
The gap is the HIR→MIR→codegen bridge:

- Start with **capture analysis** (`capture.rs`) — identify free vars in a
  closure body. Immutable captures only first.
- Then **`TypedExpr::Closure`** in HIR + the synthetic MIR function emission.
- Then **`AllocKind::Closure`** + **`Inst::CallIndirect`** (the indirect-call
  MIR instruction + codegen).
- Test with `let o = 10; let f = |x| x + o; f(5)` → 15.

**Key files:**
- `praxis-hir/src/lower.rs:lower_closure` — currently a Unit placeholder.
- `praxis-mir/src/build.rs:lower_fn` — template for synthetic closure functions.
- `praxis-mir/src/build.rs:284` (`TypedExpr::Call`) — where indirect calls hook in.
- `praxis-codegen-cranelift/src/lower.rs:534` (`Inst::Call`) — codegen call site.
- `praxis-runtime/src/closures.rs` — runtime descriptor (done).
- `praxis-runtime/src/abi.rs` — ABI wrappers (done: `praxis_alloc_closure`,
  `praxis_closure_set_capture`, `praxis_closure_fn_ptr`, `praxis_closure_capture`).
