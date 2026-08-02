# ADR-035: Full static `Type` id + source span threaded into the debug frame

**Date:** 2026-07-27
**Status:** Accepted
**Milestone:** M10b-WS1 (Thread spans + type ids, §9.3)
**Builds on:** ADR-021 (debug-frame metadata), ADR-025 (TypeData def-id
indirection)
**Amended by:** ADR-104 (decision 3's last hop; decision 1's ABI note)

## Context

M10a's `DebugLocal` carried a runtime `TypeDescriptor` (a GC-tracing vtable:
`"Vec"`, `"Map"`, …). That descriptor is sufficient for *formatting* a local
(§11.4) but **loses the static type's shape**: the element type of a `Vec[Int]`,
the field types of a record, the key/value types of a `Map`. §9.5's
"type-check using captured local types" needs the *full static* type — without
it, `p vec[0]`, `p vec.len()`, `p rec.field`, and `p map[k]` cannot type-check
against the snapshot locals.

Separately, §9.3's `source_span` field was reserved and zeroed in M10a (the MIR
did not carry per-function spans), so the `source` REPL command had nothing to
render.

## Decision

1. **Append `type_id: u32` to `DebugLocal` / `DebugLocalMeta`.** This is the
   `praxis_types::Type(u32)` handle (the positional slot index into `TypeDb`).
   Appended *after* `value` so `value`'s `offset_of!` — which the spill writes
   at `DEBUG_VALUE_OFFSET` — stays unchanged (§11.6 ABI stability). The codegen
   emits `local.ty.0` at push time.

   > **ADR-104 removes the constraint this care was taken for.** The debug
   > values are a separate 8-byte-stride array now, so `DebugLocal`'s layout no
   > longer constrains generated code at all — nothing emits an offset into it.
   > It is still the crash snapshot's element type and still `#[repr(C)]`, and
   > the field order is left as it is; there is simply nothing left to break by
   > reordering it.

2. **Deep-resolve the local's type before capturing its id.** `TypeDb::follow`
   resolves only the top-level representative; a `Collection`'s element/param
   vars are left untouched, so a `Vec[Linked→Int]` rendered as `Vec[?T]`. The
   new `TypeDb::deep_resolve` recursively follows links through `Collection` /
   `Tuple` / `Func` args, interning a resolved copy. `build_debug_local_metas`
   calls it before capturing the id. (This also surfaced and fixed a
   pre-existing inference bug where collection `TYPE_REF` annotations resolved
   to a type variable — the ctor name is a *nested* `TYPE_REF` child, not a
   direct token.)

3. **Thread the function's source span AST → HIR → MIR → backend.** `TypedFn`
   gains `span` (from the `FnItem`'s full syntax range); MIR `Function` gains
   `span`; the backend calls a new `praxis_set_frame_source_span(ctx, start,
   end)` in the prologue after the debug-frame push. Closures and synthetic
   functions (`__p_expr`) are span-less (`(0, 0)`); the `source` command
   degrades gracefully.

   > **Amended by ADR-104: only the last hop moves.** The AST → HIR `TypedFn` →
   > MIR `Function.span` threading is unchanged, and so is what the `source`
   > command reads. What is gone is `praxis_set_frame_source_span` — a runtime
   > call, in every prologue, to record a compile-time constant. The span is now
   > a field of the function's static `FunctionDebugMeta`, and the crash
   > snapshot reads it there when it builds the `SnapshotFrame`.
   > `m10b_ws1_snapshot_frame_carries_source_span` is the gate, unchanged.

4. **Type recovery from the runtime descriptor** (`descriptor_to_type`) is the
   evaluator's primary source for a local's type, with the static `type_id` as
   a fallback. This is necessary because Praxis's inference leaves collection
   element types as unbound vars when a `Vec()` is filled by later `push` calls
   (`let xs = Vec(); xs.push(11)` types `xs` as `Vec[?T]`, not `Vec[Int]`);
   the runtime descriptor carries the real shape. Collection element types
   default to `Int` for typing (sound for evaluation: `xs.len()` / `xs.get(0)`
   type-check, and the runtime formats the real value through its own
   descriptor).

## Consequences

- The debug frame grows by 4 bytes per local (`type_id`); ABI-stable because
  it's appended.
- `TypeDb::deep_resolve` interns new slots (idempotent on already-resolved
  types); `Jit::compile` now takes `&mut TypeDb`.
- Fixing the collection-annotation inference bug (point 2) unblocks typed
  `Vec`/`Map`/... params generally, not just for the debugger.
- The inference gap (point 4) is documented as a follow-up; the descriptor
  fallback makes `p EXPR` work despite it.
