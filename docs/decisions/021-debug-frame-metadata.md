# ADR-021: Debug frame metadata and shadowed-symbol registration

**Date:** 2026-07-23 · **Status:** accepted · **Amended by:** ADR-104

> **ADR-104 supersedes decisions 2 and 3 and the second Consequences bullet.** A
> frame is no longer a `Box` chained through `ctx.debug_top`: it is one
> `DebugFrameEntry` on a contiguous stack, pairing a static, arena-interned
> `FunctionDebugMeta` (the name, the source span, and the `DebugLocalMeta`
> array — everything that does not vary per call) with the base of that call's
> run of value slots on a second contiguous stack. `praxis_push_debug_frame`,
> `praxis_pop_debug_frame` and `praxis_set_frame_source_span` no longer exist;
> the prologue and epilogue are inline bumps. The `value` fields are written
> once per definition rather than by the safepoint spill.
>
> **Decision 1 and the §4.2 guarantee are unchanged.** `DebugLocal` keeps its
> layout and its `(source_name, symbol_id)` pair, and this ADR's Reason — that
> the shadowing guarantee is testable by constructing a frame directly, with no
> REPL — is why its unit test was rebuilt on the new API rather than deleted:
> see `a_functions_metadata_distinguishes_shadowed_bindings`.

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
