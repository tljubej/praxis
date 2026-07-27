# Milestone 10 Part 2 Handover — Crash debugger: the read-only evaluator, context commands, and restart/reload (§9, §19.10)

**Date:** 2026-07-27
**Status:** Part 2 complete. M10b closes Milestone 10: the read-only
`p EXPR`/`type EXPR` JIT evaluator, the `source`/`input`/`parser`/`heap`
context commands, `restart`/`reload`, and the formal *no command can mutate*
purity gate. All five §19.10 acceptance criteria now pass; 858 tests pass;
`just ci` clean. All fifteen §9.4 REPL commands are wired.

> For a fresh context: read this document, then
> `praxis_technical_design.md` §9 (Runtime failure and crash REPL),
> §9.4–§9.7 (interactive commands, expression evaluation, reload), §19.10
> (Milestone 10 acceptance), §12.3 ("active debugger-expression arguments").
> The M10a handover (`10a-milestone-10-part1-handover.md`) covers the
> crash-debugger substrate (snapshots, rendering, the REPL core); this
> document covers what was built on top. ADR-034/035/036 are the load-bearing
> M10b decisions; ADR-032 (main-heap allocation) and ADR-021 (debug-frame
> metadata) carry forward from M10a.

## 1. What M10b delivered (WS1–WS6)

M10b = **the read-only evaluator + context commands + restart/reload**. Six
workstreams, each committed independently green:

- **WS1 — Thread source spans + full static `Type` id into the debug frame
  (§9.3).** `DebugLocal` gains `type_id: u32` (the `Type(u32)` handle,
  appended for ABI stability); `DebugLocalMeta` carries it through; MIR
  `Function` gains `span`; `TypedFn` carries the function's source span; a
  new `praxis_set_frame_source_span` prologue call records it on the
  just-pushed frame; `SnapshotFrame` carries `source_span`. `TypeDb::deep_resolve`
  recursively follows links through `Collection`/`Tuple`/`Func` args so the
  captured id points at a fully-concrete type. (Also fixed a pre-existing
  inference bug: collection `TYPE_REF` annotations like `Vec[Int]` resolved to
  a type variable because the ctor name is a nested `TYPE_REF` child.)
- **WS2 — `Repl` owns the debugging session.** The M10a `Repl { snapshot,
  selected }` couldn't reach the Jit/Runtime/TypeDb/source/input the M10b
  commands need. Pulled the pipeline deps into `praxis-debugger`; new
  `session::DebugSession` owns jit/main_entry/func_ids/runtime/analysis/
  source_text/path/input_text/path; the `Repl` holds `Option<DebugSession>`;
  the CLI hands the live session off. `Runtime::clear_for_rerun` resets the
  fault/snapshot/parse-detail slots before a re-run.
- **WS3 — `source` / `input` / `parser` context commands (§9.4).** `source
  [N]` renders the selected frame's source extent (lines + caret); `input`
  renders the §7.11 ParseDetail input span + preview; `parser` renders the
  expected description + the failing parser expression's span.
- **WS4 — `p EXPR` / `type EXPR` read-only JIT evaluator (§9.5, closes §19.10
  gate 2).** Synthesizes `fn __p_expr(<typed params>) { EXPR }`, runs the
  standard pipeline (resolution + typing for free), purity-gates, JITs a fresh
  function, calls it with the snapshot locals as ABI args (arity-dispatched
  0–6, rooted for the call), formats the result. `type EXPR` stops before JIT
  and renders the inferred type. Type recovery uses the runtime *descriptor*
  (`descriptor_to_type`) as the primary source — the inference gap leaves
  `Vec()`-filled-by-`push` typed as `Vec[?T]`, but the descriptor carries the
  real shape. The formal mutation gate (`praxis_debugger::purity`) threads the
  catalog's `Purity` tag into `TypedExpr::MethodCall` and rejects impure calls,
  user calls, `Read`/`Parse`, closures, and diverging nodes.
- **WS5 — `heap EXPR` recursive inspection (§9.4).** Evaluates the expression
  (reusing WS4) and renders the result prefixed with its type —
  `Vec[Int]: [11, 22]`.
- **WS6 — `restart` / `reload` (§9.7, closes §19.10 gate 3).** `restart` reruns
  the same code+input (`DebugSession::restart`); `reload` re-reads the source,
  recompiles, and on success swaps in the new Jit/analysis then reruns
  (`DebugSession::reload`). Per §9.7, old JIT + snapshots are discarded only
  after the new compile succeeds; a failed recompile leaves the session intact.

## 2. §19.10 acceptance criteria — status

| Criterion | Status | Where |
|---|---|---|
| Inspect scalar-object, text, record, vector, map, and grid locals | **done** — `locals` formats through descriptors; M10a closed this | M10a WS5 |
| Evaluate expressions using selected-frame locals | **done (M10b)** — the `p EXPR` JIT evaluator | WS4 CLI `m10b_ws4_*` |
| Reload after editing and rerun with identical input | **done (M10b)** — `reload` | WS6 CLI `m10b_ws6_*` |
| GC retains all objects reachable from snapshots | **done** — M10a | M10a WS3 |
| No command can mutate or resume a faulted state in v1 | **done (M10b)** — the formal purity gate | WS4 `praxis_debugger::purity` |

**All five criteria pass.** The milestone gate (§20 rule 12) is satisfied;
Milestone 10 is complete.

## 3. Where things live

- **Purity gate:** `praxis-debugger/src/purity.rs` (`assert_read_only`,
  the accept/reject walk over `TypedExpr`); the `purity` field on
  `TypedExpr::MethodCall` in `praxis-hir/src/lower.rs`.
- **Evaluator:** `praxis-debugger/src/evaluate.rs` (`evaluate`, `type_of`,
  `heap`, `build_pipeline`, `exec`, `descriptor_to_type`, `call_with_arity`).
- **Session (restart/reload):** `praxis-debugger/src/session.rs`
  (`DebugSession`, `rerun_main`, `restart`, `reload`).
- **Type-id + span threading:** `DebugLocal.type_id` / `DebugLocalMeta.type_id`
  (`praxis-runtime/src/context.rs`, `debug.rs`); MIR `Function.span`
  (`praxis-mir/src/ir.rs`); `praxis_set_frame_source_span`
  (`praxis-runtime/src/debug.rs`); `TypeDb::deep_resolve`
  (`praxis-types/src/db.rs`); `SnapshotFrame.source_span`
  (`praxis-runtime/src/crash_snapshot.rs`).
- **Inference fix:** `resolve_type_node` collection arm
  (`praxis-hir/src/infer.rs`).
- **REPL + rendering:** `praxis-debugger/src/repl.rs` (dispatch, HELP_TEXT);
  `render.rs` (`render_source_span`, `render_input_context`,
  `render_parser_context`).
- **CLI hand-off:** `praxis-cli/src/run.rs` (builds `DebugSession`, hands to
  `Repl::new_session`).
- **ADRs:** `docs/decisions/034-read-only-purity-gate.md`,
  `035-static-type-id-and-source-span-threading.md`,
  `036-synthetic-p-expr-function.md`.

## 4. Key engineering insights

1. **Synthesize, don't re-resolve.** The cleanest way to type-check `EXPR`
   against snapshot locals is to emit `fn __p_expr(<typed params>) { EXPR }`
   and run the standard pipeline — name resolution + typing come for free. A
   bespoke resolver that maps identifier ranges to snapshot locals would have
   fought the text-range-keyed `Analysis` machinery.
2. **The runtime descriptor is the source of truth for a local's type.**
   Praxis's inference leaves `let xs = Vec(); xs.push(11)` typed as `Vec[?T]`
   (the element var is never linked to `Int` in the `TypeDb`). The runtime
   descriptor carries the real shape (`VEC` with an `INT` element descriptor),
   so `descriptor_to_type` recovers `Vec[Int]` for the synthesized annotation.
   The static `type_id` (WS1) is the fallback. Documented as a follow-up
   inference fix.
3. **Each `p EXPR` builds its own short-lived `Jit`.** Cranelift's `JITModule`
   doesn't support ad-hoc late compilation into an existing module; multiple
   `Jit`s coexist (the jit.rs suite creates one per test). The returned `GcRef`
   is on the main heap (ADR-032), so it outlives the `Jit`'s drop.
4. **Clear the stale fault before the call.** The original crash leaves
   `pending_fault` set; `__p_expr`'s `CheckFault` safepoints would otherwise
   bail immediately. `take_fault` clears it; the snapshot is rooted separately.
5. **`reload` swaps only after success (§9.7).** The new `Jit` is built first;
   on a compile error the old session is untouched and the old snapshot stays
   inspectable. On success, the assignment `self.jit = new_jit` drops the old
   one. `§10.5`'s "a generation is released only after no code references it"
   is satisfied by construction — the old JIT is unreferenced once swapped.
6. **Assignment is statement-only — a free win for the purity gate.** It
   cannot appear inside an expression in Praxis, so the expression-level walk
   never sees it. Mutation rejection reduces to rejecting impure method calls,
   user calls, `Read`/`Parse`, closures, and diverging nodes.

## 5. Definition of done (§20.1)

- Each WS has unit tests in its crate + CLI integration tests.
- `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace`
  all pass (858 tests).
- ADR-034/035/036 record the three M10b design decisions; the README index is
  updated.

## 6. Known limitations / follow-ups for M11+

**Debugger:**
- **The `Vec[?T]` inference gap.** `let xs = Vec(); xs.push(11)` types `xs` as
  `Vec[?T]`, not `Vec[Int]` — the element var is never linked. The evaluator
  works around it via `descriptor_to_type`, but the underlying inference
  (unifying the constructor's element var with the `push` argument's type)
  should be fixed so the static `type_id` is correct on its own.
- **Per-`p-EXPR` JIT latency.** Each call recompiles a one-function module. A
  persistent `Jit` that accepts late compilation would reduce latency for
  rapid `p` sessions.
- **Arity limit (6).** `call_with_arity` dispatches on fixed arities 0–6;
  frames with more named locals get a clear error. Extendable.
- **Shadow-disambiguation by real symbol id.** Still uses the local's position
  as a placeholder `symbol_id` (MIR locals don't carry the HIR `SymbolId`).
- **`parser_path` threading.** The `parser`/`input` commands render from the
  §7.11 `ParseDetail` (sufficient for M10b); the `DebugFrame.parser_path`
  field is still reserved/zeroed.

**Parser (carried from M9/M10a):**
- Template leading-space double-handling; `word` greedy at `-`/`:`;
  ragged `fill:` bare-value grammar; source-slice offsets inside sections.
- Anonymous-record field access through repeated-only sections / match-bound
  choice payloads (inference gap).

**Carry-forwards from M8 (still open):**
- `find`/`position` → `Option[Int]`; tuple `.0`/`.1` field access (ADR-026);
  `min=`/`max=` parser ops; `for` over Map/Set/Grid/Counter; `Grid.map`;
  recursive named closures; pipeline barriers.

## 7. Test count

858 tests workspace-wide (was 822 at M10a close). M10b added: 2 runtime
(`set_frame_source_span`) + 2 JIT (WS1 span/type_id) tests; 1 debugger unit
test (session-less REPL); 7 render unit + 2 CLI (WS3 source/input/parser)
tests; 8 purity + 3 evaluate unit + 6 CLI (WS4 p/type) tests; 2 CLI (WS5
heap) tests; 3 CLI (WS6 restart/reload) tests; plus the inference-fix
coverage.

## 8. Milestone 10 — complete

Milestone 10 (Crash debugger REPL) is fully delivered across M10a + M10b:
- Terminal crash REPL ✓ (M10a)
- Stack/frame navigation ✓ (M10a)
- Local display ✓ (M10a)
- Read-only expression evaluator through JIT ✓ (M10b)
- Input/parser context commands ✓ (M10b)
- Restart and reload ✓ (M10b)
- Noninteractive fallback behavior ✓ (M10a)

All §19.10 acceptance criteria pass. The next milestone (M11, per §19) is the
LSP / IDE integration.
