# ADR-021: Debug frame metadata and shadowed-symbol registration

**Date:** 2026-07-23 · **Status:** accepted

## Context

§4.2 specifies Rust-style shadowing: a later `let a` in the same scope creates a
*distinct* symbol with a new id, and "shadowed locals are distinguishable in
debugger frames by source name and symbol ID" (§19.5 acceptance criterion).

M4's `DebugFrame` was an opaque `{ _opaque: () }` placeholder so the
`RuntimeContext` shape was fixed; `debug_top` was always null. M5 gives it a
real layout so the metadata is correct and registered, even though the
crash-debugger REPL that *reads* it lands in M10.

## Decision

Give `DebugFrame` a real `#[repr(C)]` layout and provide extern helpers:

1. **`DebugLocal`** — `{ source_name: *const u8, name_len: u32, symbol_id: u32,
   value: GcRef }`. The `(source_name, symbol_id)` pair disambiguates shadowed
   bindings; `value` is the current `GcRef` (updated by the spill).

2. **`DebugFrame`** — `{ parent: *mut DebugFrame, func_name, func_name_len,
   locals: *mut DebugLocal, local_count }`. The parent pointer chains the call
   stack.

3. **`praxis_push_debug_frame` / `praxis_pop_debug_frame`** extern helpers
   (`praxis-runtime/src/debug.rs`): allocate/free a frame, chain it onto
   `ctx.debug_top`. The `DebugLocalMeta` FFI struct carries `(name_ptr, name_len,
   symbol_id)` triples for construction.

4. **Symbols registered** in the JIT module so M10 can wire them into
   prologues/epilogues. M5 does not emit the calls from the Cranelift backend
   yet (the metadata layout and helpers are the deliverable; the wiring is M10).

## Reason

- The §4.2 shadowing guarantee is testable *now* by constructing a frame with
  two `a` bindings and asserting distinct `symbol_id`s — no REPL needed.
- Keeping the layout `#[repr(C)]` with raw pointers matches the `RuntimeContext`
  pattern (FFI-safe, fixed offsets for generated code).
- Deferring the Cranelift prologue/epilogue wiring to M10 avoids emitting
  per-function debug overhead until the debugger exists to consume it.

## Consequences

- `RuntimeContext.debug_top` is still null at runtime in M5 (no prologue emits
  the push yet); M10 wires the codegen and builds the REPL.
- The `DebugLocal.value` fields will be updated by the same spill mechanism as
  the shadow frame (ADR-019) once the prologue is wired — the two frame chains
  are parallel (roots for the GC, debug frames for the debugger).
