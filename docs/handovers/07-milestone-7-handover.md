# Milestone 7 report & handover

**Project:** Praxis
**Date:** 2026-07-25
**Status:** **Milestone 7 complete.** All §19.7 deliverables landed and green;
all four acceptance criteria met. **485 tests passing**, `just ci` clean.

> **For a fresh context:** read this document, then
> `praxis_technical_design.md` (the contract — §4.5 records, §4.6 enums,
> §4.10 closures, §5.5 equality/hashing, §13.6 monomorphization, §19.7 Milestone
> 7). The two predecessor handovers (`07-milestone-7-part1-handover.md`,
> `07b-milestone-7-part2-progress.md`) cover WS1–WSP; this document closes M7
> with WS7 lowering, WS8, WS9, WS7b, and WS10.

---

## 1. What M7 delivered (WS1–WS10)

M7 added the data-modeling and abstraction layer: nominal records, enums, full
pattern matching with exhaustiveness, closures (immutable + mutable captures),
structural equality/hashing, monomorphized polymorphism, and closed the two M6
input-parser carryovers.

| Workstream | What landed | Part | Commit(s) |
|---|---|---|---|
| **WS1: Type-system foundation** | `TypeData::Record`/`Enum` via def-id indirection (ADR-025). Two-pass resolver. | 1 | `1644488` |
| **WS2: Short-circuit carryover** | `\|\|` short-circuits; `!` as eq-with-false. | 1 | `1caf57e` |
| **WS3: Nominal records** | `struct`/construction/field-access, full vertical slice. ABI→3. | 1 | `8b973b9` |
| **WS4: Enums** | `enum`/variants/payloads, `ENUM` descriptor (TypeId 9). | 1 | `cb77399` |
| **WS5: Pattern matching** | `match`, recursive patterns, MIR decision tree. | 1 | `ce8255b`, `9389581` |
| **WS6: Structural eq/hash** | Tuple runtime, eq/hash on RECORD/ENUM/TUPLE, internal capability check (`Y004`), `Inst::StructEq`. ABI→4. ADR-026. | 2 | `0e57872` |
| **WSP: Pattern completeness** | Recursive `TypedPattern`, exhaustiveness (`Y120`/`Y121`), nested/literal lowering. | 2 | `d5a8a36` |
| **WS7a: Closure lowering (immutable)** | Capture analysis, `TypedExpr::Closure`, synthetic MIR fn, `AllocKind::Closure`, `Inst::CallIndirect`, Approach B ABI. | 3 | `48d2d79` |
| **WS7b: VarCell mutable captures** | Escape analysis, `VarCell` (TypeId 12), `praxis_alloc_var_cell`/`get`/`set`, captured-`var` boxing. ABI→6. | 3 | `d9a4a7c` |
| **WS8: Monomorphization** | `mono.rs` pass between HIR and MIR; `Analysis.call_sites`; removed `Y100`. ADR-018 superseded. | 3 | `4196c77` |
| **WS9: Input-parser carryovers** | `child_descriptor` recursion; full `walk_template` interpreter (§7.2/§7.3); multi-anon → tuple via Template. | 3 | `cb1807a` |
| **WS10: Docs** | ADR-027 (closures); ADR-018 superseded; README→M7 complete; corpus fixtures; this handover. | 3 | (this commit) |

### §19.7 acceptance criteria — final status

| Criterion | Status | Notes |
|---|---|---|
| Store parser-generated records in vectors and maps | ✅ vectors; ❌ maps (M8) | |
| Use tuples and records as set/map keys | ✅ eq/hash | WS6 eq/hash machinery; M8 containers close end-to-end keying |
| Compile closure pipelines with captured values | ✅ | WS7a (immutable) + WS7b (mutable via VarCell) |
| Reject non-exhaustive matches | ✅ | WSP exhaustiveness checker (`Y120`/`Y121`) |

The "monomorphized inferred polymorphism" §19.7 *deliverable* is also landed (WS8) —
`fn id(x) { x }` now compiles and runs.

---

## 2. Key design decisions in Part 3

1. **Closure calling convention: Approach B.** The synthetic fn signature is
   `fn(ctx, closure_self, params...)`; captures are loaded at entry via
   `praxis_closure_capture`. The call site reads `fn_ptr` and `call_indirect`s
   with `[ctx, closure, args...]`, knowing nothing about captures. Chosen over
   "env as trailing params" (Approach A) because it keeps the highest-risk piece
   (Cranelift indirect calls) simplest, yields a uniform per-arity signature,
   and scales to closures-in-collections and currying. See ADR-027 for the full
   tradeoff analysis.
2. **`praxis_closure_fn_ptr` takes `ctx` (unused).** Every `praxis_*` wrapper is
   called as `fn(ctx, args...)` from generated code; adding the unused `ctx`
   keeps the calling convention uniform. (The `closures.rs` doc comment, which
   described the stale Approach-A sketch, was corrected.)
3. **Capture analysis scans NAME tokens, not PathExprs.** The lhs of `+=` is a
   bare `NAME` token (not a `PathExpr`), so a PathExpr-only walk misses
   assigned-to captures. The walker scans every resolved `NAME` token in the
   closure subtree and classifies by symbol kind + decl-range-outside-closure.
4. **Monomorphization canonicalizes by rendered type strings, not type ids.** The
   type arena doesn't structurally intern, so two `Int` args from different call
   sites have distinct arena slots. Canonicalizing via `db.render(db.follow(t))`
   makes them share one clone.
5. **Multi-anon-capture templates lower to `Template`, not `Tuple`.** The `Tuple`
   node dropped the literal separators between captures, so the runtime couldn't
   match them. Emitting a `Template` node preserves the literals; the interpreter
   assembles the tuple from the captured values.
6. **`VarCell` boxing is per-escaping-`var`, transparent.** Only `var`s captured
   by some closure are boxed (escape analysis); uncaptured `var`s stay plain
   locals. Reads/writes of an escaping `var` route through the cell automatically
   — the source program sees no difference.
7. **`child_descriptor` returns the result descriptor, not the leaf atomic.**
   Collection descriptors (`VEC`/`GRID`) are uniform — the per-instance element
   type lives in the payload — so a nested constructor's result descriptor is
   just `VEC`/`GRID`, and `vec_format`/`vec_equals`/`vec_hash` recurse correctly
   through the payload chain.

---

## 3. Where things live

**Closures:**
- `praxis-hir/src/capture.rs` — capture analysis (free-variable detection).
- `praxis-hir/src/lower.rs` — `lower_closure` (WS7a), escape analysis
  (`collect_escaping_vars`, WS7b), `TypedExpr::Closure`.
- `praxis-mir/src/build.rs` — `lower_closure_fn` (synthetic fn + prologue),
  `AllocKind::Closure` + `Inst::CallIndirect` lowering, `VarCell` boxing in
  `Var`/`Path`/`Assign`.
- `praxis-mir/src/ir.rs` — `AllocKind::Closure`, `Inst::CallIndirect`.
- `praxis-codegen-cranelift/src/lower.rs` — `AllocKind::Closure` (`func_addr` +
  alloc/set_capture), `Inst::CallIndirect` (`call_indirect`).
- `praxis-runtime/src/closures.rs` — `ClosurePayload`, `CLOSURE` (TypeId 11).
- `praxis-runtime/src/var_cell.rs` — `VarCellPayload`, `VAR_CELL` (TypeId 12).
- `praxis-runtime/src/abi.rs` — closure + var_cell wrappers (ABI v6).

**Monomorphization:**
- `praxis-hir/src/mono.rs` — the pass.
- `praxis-hir/src/lib.rs` — `Analysis.call_sites`, `CallSite`.
- `praxis-hir/src/infer.rs` — `infer_call` records call-site witnesses.
- `praxis-cli/src/run.rs` — mono inserted between `lower` and `lower_module`.

**Input-parser carryovers:**
- `praxis-runtime/src/parser.rs` — `child_descriptor` (recursive),
  `walk_template` (full interpreter), `consume_ws` (§7.2 policies),
  `alloc_record`/`alloc_tuple` + schema caches.
- `praxis-input-parser/src/plan.rs` — multi-anon templates now emit `Template`.

**Docs:** ADR-027 (new), ADR-018 (superseded), README (M7 complete), corpus
fixtures (`day07_closure_pipeline.px`, `day08_named_capture_template.px`).

---

## 4. Test inventory (485 total)

| Suite | Count | Location |
|---|---|---|
| JIT end-to-end | 92 | `praxis-codegen-cranelift/tests/jit.rs` |
| HIR (incl. capture, mono, eq capability, exhaustiveness, closure inference) | 85 | `praxis-hir/src/*.rs` |
| Runtime (tuple, record/enum eq/hash, closure/var-cell descriptors) | ~80 | `praxis-runtime/src/*.rs` |
| Types | ~44 | `praxis-types/src/types_tests.rs` |
| Parser | ~60 | `praxis-parser/src/parse.rs` |
| Other | ~124 | various |
| **Total** | **485** | `cargo test --workspace` |

Notable Part 3 additions: 6 immutable-closure JIT tests, 4 mutable-capture JIT
tests, 5 monomorphization JIT tests, 5 input-parser JIT tests (templates,
tuples, nested-collection equality), 7 capture-analysis unit tests, 4
`consume_ws` unit tests, 3 mono unit tests.

---

## 5. Definition of Done — verified

- [x] All four §19.7 acceptance criteria green and demonstrated by tests.
- [x] "Monomorphized inferred polymorphism" deliverable landed (WS8).
- [x] Both M6 input-parser stubs closed (WS9).
- [x] Closures: immutable + mutable captures end-to-end (WS7a + WS7b).
- [x] ADR-027 written; ADR-018 superseded; README bumped; corpus fixtures added.
- [x] `just ci` clean (fmt-check + clippy `-D warnings` + test) on `main`.

---

## 6. Known limitations / follow-ups for M8+

- **Maps/Sets close end-to-end keying.** The eq/hash machinery (WS6) is in place;
  M8's `Map`/`Set`/`Counter`/`Deque` containers close the "tuples and records as
  keys" criterion fully.
- **Tuple field access (`.0`/`.1`)** remains a follow-up (not required for the
  keys criterion; noted in ADR-026).
- **`Vec[T]()` honoring the type arg** is partially in place (the call-site
  element type is now captured via `Analysis.call_sites`); passing a real
  descriptor instead of the null default is a small follow-up.
- **Closures as `Map`/`Set` values** are correctly non-equatable/non-hashable
  (§5.5: functions never are); no work needed, noted for clarity.
- **`for` loops over iterables, `loop`/`break`/`continue`/`return`** (§4.11) are
  not yet implemented — M8+ work.
- **Recursive named closures** (`let rec f = |...|`) are not specially handled;
  a closure literal captures by value, so true mutual recursion needs a follow-up.
- **WS9 `walk_tuple`** is retained defensively (the `Tuple` node is reserved) but
  currently unreachable — multi-anon templates lower to `Template`.
