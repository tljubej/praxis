# Milestone 10 Part 1 Handover — Crash Debugger: snapshots, rendering, and the REPL core (§9, §19.10)

**Date:** 2026-07-27
**Status:** Part 1 complete. M10a delivers the crash-debugger substrate — §7.11
rich parse diagnostics, debug-frame codegen wiring, crash snapshots with GC
rooting, the §9.6 noninteractive fallback, and the interactive REPL core
(`bt`/`frame`/`up`/`down`/`locals`/`help`/`quit`). 822 tests pass; `just ci`
clean. Part 2 (M10b) adds the read-only `p EXPR`/`type EXPR` JIT evaluator,
the `source`/`input`/`parser`/`heap` context commands, and `restart`/`reload`.

> For a fresh context: read this document, then
> `praxis_technical_design.md` §9 (Runtime failure and crash REPL), §9.1–§9.7
> (fault model, snapshot preservation, interactive/noninteractive behavior,
> debug-expression evaluation, reload), §19.10 (Milestone 10), §7.11 (parse
> fault behavior), and §12.3 (root tracking). The M9 handover
> (`09-milestone-9-handover.md`) covers the input-parser foundation and names
> the §7.11 work as M10's on-ramp. ADR-021 (debug-frame metadata) and ADR-019
> (shadow-stack spill) are the load-bearing prior decisions; ADR-032 and
> ADR-033 are new.

## 1. What M10a delivered (WS1–WS6)

M10a = **the crash-debugger substrate** — the runtime + codegen + CLI machinery
that captures a fault's context and lets the user inspect it. Six workstreams,
each committed independently green:

- **WS1 — §7.11 rich parse diagnostics (the on-ramp).** `WalkResult` is now
  `Result<(GcRef, usize), ParseFail>`; all 17 mismatch sites in the parser
  interpreter carry context-appropriate `expected` strings (`"int"`,
  `"literal ':'"`, `"section header"`, …). A host-managed `ParseDetail` slot
  (appended to `RuntimeContext` for ABI stability) records the deepest
  (highest-offset) failure with input span, expected, and a bounded single-line
  actual preview. The M9-deferred §19.9 "useful failure diagnostic" item is now
  *useful*.
- **WS2 — DebugFrame §9.3 fields + codegen wiring (ADR-021).** `DebugLocal`
  gains `descriptor` (local type descriptors); `DebugFrame` gains `source_span`
  and `parser_path` (reserved/zeroed in M10a — MIR does not yet carry
  per-function spans; M10b fills them for `source`/`input`/`parser`). The
  Cranelift backend now emits `praxis_push_debug_frame` in the prologue,
  `praxis_pop_debug_frame` in all three epilogues (Return, Fault, stack-
  overflow), and the spill mirrors each live-root write into the matching
  `DebugLocal.value` (parallel to the shadow-stack slot). `ctx.debug_top` is
  now non-null mid-execution.
- **WS3 — Crash snapshot + GC rooting (§9.3, §19.10 acceptance).** The first
  fault epilogue deep-copies the debug-frame chain into a Runtime-owned
  `CrashSnapshot` *before* unwinding (`praxis_snapshot_debug_chain`, idempotent
  via a `SnapshotSlot::taken` guard). `CrashSnapshot: RootSet` yields every
  copied `DebugLocal.value`, so the collector retains all snapshot-reachable
  objects (ADR-033). The acceptance test builds a `Vec[Int]`, faults, collects
  3× with the snapshot as root, and asserts the Vec survives intact.
- **WS4 — §9.6 noninteractive fallback + `--debug=auto|always|never`.** A fault
  now renders the fault line + a numbered backtrace + the top frame's locals
  (with §7.11 detail appended for `ParseFailed`), instead of a bare one-liner.
  The `--debug` flag (default `auto`: REPL iff stdin & stdout are TTYs) controls
  REPL entry. Rendering lives in `praxis-debugger::render`, shared with the REPL.
- **WS5 — Interactive REPL core (§9.4).** `praxis-debugger::repl::Repl` owns the
  snapshot + a selected-frame cursor. Commands: `bt`, `frame N`, `up`, `down`,
  `locals`, `help`, `quit` (+ EOF). The M10b commands (`p`, `type`, `source`,
  `input`, `parser`, `heap`, `restart`, `reload`) are acknowledged but deferred.
  The CLI hands off to the REPL when `debug.wants_repl()`; program input comes
  from `--input` (freeing stdin for REPL commands when scripted).
- **WS6 — Acceptance sweep, ADRs, handover.** Fixed the stale ADR README index
  (was missing 030, 031); added ADR-032 (§21.8 → debugger expressions allocate
  on the main heap) and ADR-033 (crash-snapshot rooting design). This document;
  README bump.

## 2. §19.10 acceptance criteria — status

| Criterion | Status | Where |
|---|---|---|
| Inspect scalar-object, text, record, vector, map, and grid locals | **done (M10a)** — `locals` formats through descriptors; scalar/text/vec confirmed in CLI tests; record/map/grid reuse the same `Value::format` path | WS5 `render_frame_locals`; CLI `m10ws4_noninteractive_renders_backtrace_and_locals` |
| Evaluate expressions using selected-frame locals | **deferred (M10b)** — the `p EXPR` JIT evaluator | M10b WS6 |
| Reload after editing and rerun with identical input | **deferred (M10b)** — `reload` | M10b WS7 |
| GC retains all objects reachable from snapshots | **done (M10a)** | WS3 `m10ws3_snapshot_retains_reachable_objects_across_gc` |
| No command can mutate or resume a faulted state in v1 | **partial (M10a)** — no M10a command mutates (read-only by construction); the formal `p EXPR` mutation-rejection gate lands in M10b | WS5 (read-only commands); M10b WS6 (mutation gate) |

Three of five criteria are fully closed in M10a; two (`p EXPR`, `reload`) are
inherently M10b. The milestone gate (§20 rule 12) therefore does not fully pass
until M10b — consistent with the agreed M10a/M10b split (like M7).

## 3. Where things live

- **Rich parse diagnostics:** `praxis-runtime/src/parse_detail.rs`
  (`ParseFail`, `ParseDetail`, deepest-wins `consider`, bounded single-line
  preview); the `parse_detail: *mut ParseDetail` field on `RuntimeContext`
  (appended at the end — ABI-stable); the walker error sites in
  `praxis-runtime/src/parser.rs`.
- **Debug-frame structs + helpers:** `praxis-runtime/src/context.rs`
  (`DebugFrame`, `DebugLocal`, `current_fault_kind`); `debug.rs`
  (`DebugLocalMeta`, `praxis_push/pop_debug_frame`).
- **Crash snapshot:** `praxis-runtime/src/crash_snapshot.rs` (`CrashSnapshot`,
  `SnapshotFrame`, `SnapshotSlot`, `praxis_snapshot_debug_chain`,
  `CrashSnapshot: RootSet`); the `crash_snapshot: *mut SnapshotSlot` field on
  `RuntimeContext`.
- **Codegen wiring:** `praxis-codegen-cranelift/src/lower.rs` —
  `build_debug_local_metas`, the prologue debug-frame push, the extended
  `SpillCtx::emit_spill` (writes both shadow + debug slots),
  `emit_pop_debug_frame`, `emit_snapshot_debug_chain` in the fault epilogues.
  Symbols registered in `symbols.rs` + `module.rs`.
- **Rendering:** `praxis-debugger/src/render.rs` (`render_noninteractive`,
  `render_backtrace`, `render_frame_locals`).
- **REPL:** `praxis-debugger/src/repl.rs` (`Repl`, `handle`, `run`,
  `PROMPT`, `HELP_TEXT`).
- **CLI:** `praxis-cli/src/debug_mode.rs` (`DebugMode`); `run.rs` fault hook
  (REPL hand-off + noninteractive render); `main.rs` `--debug` flag.
- **ADRs:** `docs/decisions/032-debugger-expr-main-heap.md`,
  `033-crash-snapshot-rooting.md`; README index fixed.

## 4. Key engineering insights

1. **The §7.11 detail slot is appended to `RuntimeContext`, not interleaved.**
   Generated code reads existing fields at fixed `#[repr(C)]` offsets; appending
   `parse_detail` and `crash_snapshot` at the *end* preserves every JIT-read
   offset (§11.6 ABI stability). Generated code only passes these pointers to
   the snapshot helper — it never reads them.
2. **The snapshot must capture before the unwind pops.** The fault unwind is
   multi-frame: each epilogue pops its frames as it returns. The idempotency
   guard (`SnapshotSlot::is_set`) ensures the innermost frame's epilogue —
   which runs first, while the full chain is intact — captures the whole chain;
   outer frames skip. Without the guard, later (shorter) chains would overwrite
   the good one.
3. **The debug-frame spill mirrors the shadow-frame spill exactly.** WS2's
   `SpillCtx::emit_spill` writes each live root into both `shadow.slots[i]` and
   `debug_frame.locals[i].value` at every safepoint. This keeps snapshot values
   fresh with zero extra mechanism — the two frames are parallel (roots for the
   GC, debug-locals for the debugger), exactly as ADR-021 anticipated.
4. **Non-moving GC (ADR-011) is what makes shallow snapshot copies safe.** A
   copied `GcRef` keeps its address; the collector never relocates the object.
   `CrashSnapshot: RootSet` pins only the entry-point locals; transitive
   reachability is the collector's job.
5. **Stdin is shared between program input and REPL commands.** When `--input`
  is absent and stdin is piped, the program reads all of stdin eagerly (for
  `read` expressions). The REPL therefore works interactively (TTY) or when
  `--input` frees stdin for REPL commands (the test pattern). This is
  acceptable for M10a; M10b's `reload` (§9.7) retains the original input bytes
  separately, which resolves the tension for scripted sessions.

## 5. Definition of done (§20.1)

- Each WS has unit tests in its crate + CLI/JIT integration tests.
- `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace`
  all pass (822 tests).
- ADR-032 (§21.8) and ADR-033 (snapshot rooting) record the two M10a design
  decisions; the stale README index (missing 030/031) is fixed.

## 6. Known limitations / follow-ups for M10b+

**Debugger (M10b scope):**
- **`p EXPR` / `type EXPR` read-only JIT evaluator (§9.5).** Parse → resolve
  against snapshot locals → type-check with captured local types → JIT a
  synthetic read-only function → execute against snapshot slots → format.
  Reject mutating expressions (the formal "no command can mutate/resume" gate).
  Free-alloc on the main heap per ADR-032. The standalone expression parser
  entry exists (`parse_expr`, `praxis-parser/src/parse.rs:594`) but is not yet
  exposed as a public entry point.
- **Context commands.** `source`, `input`, `parser`, `heap`. These consume
  (a) the `DebugFrame.source_span` / `parser_path` fields (reserved/zeroed in
  M10a — thread per-function source spans from AST → HIR `TypedFn` → MIR
  `Function` → backend, and the active-parser-path from the plan), and (b) the
  WS1 `ParseDetail` for `input`/`parser`.
- **`restart` / `reload` (§9.7).** `restart` reruns the same code+input;
  `reload` recompiles source and reruns, discarding old JIT code + snapshots on
  success. Ties into §10.5 JIT generation arenas.
- **Shadow-disambiguation by real symbol id.** M10a uses the local's position
  as a placeholder `symbol_id` (MIR locals don't carry the HIR `SymbolId`).
  Thread the id for true §4.2 shadow disambiguation once two same-named locals
  appear in one frame.

**Parser (carried from M9, not M10a scope):**
- Template leading-space double-handling; `word` greedy at `-`/`:`;
  ragged `fill:` bare-value grammar; source-slice offsets inside sections.
- Anonymous-record field access through repeated-only sections / match-bound
  choice payloads (inference gap).

**Carry-forwards from M8 (still open):**
- `find`/`position` → `Option[Int]`; tuple `.0`/`.1` field access (ADR-026);
  `min=`/`max=` parser ops; `for` over Map/Set/Grid/Counter; `Grid.map`;
  recursive named closures; pipeline barriers.

## 7. Test count

822 tests workspace-wide (was 779 at M9 close). M10a added: 8 `parse_detail`
unit tests; 2 `crash_snapshot` unit tests; the `DebugLocal` descriptor unit
test; 4 WS1 JIT tests; 3 WS2 JIT tests; 3 WS3 JIT tests (incl. the GC-retention
acceptance test); 5 `render` unit tests; 8 `repl` unit tests; 3 `debug_mode`
unit tests; 3 WS4 + 4 WS5 CLI integration tests.

## 8. The transition into Milestone 10 Part 2

**M10b = the read-only expression evaluator + context commands + restart/reload**
(§9.5, §9.7). Deliverables:
- `p EXPR` / `type EXPR`: the §9.5 pipeline (parse → resolve against snapshot →
  type-check → JIT synthetic fn → execute → format), with mutation rejection.
- `source` / `input` / `parser` / `heap`: render the §9.3 span/parser-path
  fields (thread them first) and the §7.11 parse detail.
- `restart` / `reload`: §9.7; ties into §10.5 JIT generations.
- Acceptance fixtures + ADRs (e.g. the `p EXPR` read-only-gating decision) +
  the full M10 handover + README bump.

**M10b's natural first step** is exposing a standalone expression-parse entry
(`parse_expr` over a `&str`) and the snapshot-local resolution path, since
`p EXPR` is the largest remaining §19.10 acceptance item. The §7.11 detail and
the snapshot rooting M10a landed are directly reused by `input`/`parser` and by
rooting `p`-expression arguments (§12.3 "Active debugger-expression
arguments").
