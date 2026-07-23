# ADR-017: Runtime ABI wrappers and the no-panic fault protocol

**Date:** 2026-07-23 · **Status:** accepted

## Context

§10.2 names the `praxis_*` runtime symbols generated code calls (`praxis_alloc`,
`praxis_int_add`, …). §10.4 mandates: **"Runtime wrappers must not panic across
the ABI."** A wrapper either returns normally with `pending_fault` clear, or sets
`pending_fault` and returns a defined dummy value. §19 M4 acceptance requires
that overflow and division-by-zero "return to the host without Rust unwinding."

In M3, `Fault` was an opaque stub and no `praxis_*` wrappers existed (their names
appeared only as strings in the stdlib/catalog).

## Decision

- Implement the `praxis_*` wrappers in `praxis-runtime/src/abi.rs` as
  `#[no_mangle] pub unsafe extern "C" fn`, each taking `*mut RuntimeContext` +
  `GcRef`/scalar args and returning a `GcRef`/scalar. Covered: `praxis_alloc_`
  `{int,bool,unit,text}`, `praxis_{int,bool}_load`, `praxis_int_{add,sub,mul,
  div,rem,neg}`, `praxis_int_{eq,ne,lt,gt,le,ge}`, `praxis_check_fault`.
- Checked arithmetic uses `i64::checked_{add,sub,mul}`; division/rem test for a
  zero divisor explicitly. On fault, the wrapper writes `FaultKind` into the
  context's fault slot and returns the Unit sentinel — **never panics**.
- `Fault` becomes a real `#[repr(C)]` struct `{ pending: bool, kind: FaultKind }`
  with `FaultKind { None, IntOverflow, DivByZero }`. `Runtime` owns the slot;
  `context()` wires `pending_fault` to it. `has_pending_fault` reads the slot.
- The Cranelift `JITBuilder` registers every `praxis_*` address via
  `builder.symbol(name, ptr)` so imports resolve without a linker.

## Reason

- `extern "C"` + `#[no_mangle]` is the stable, FFI-safe calling convention
  Cranelift-generated code can call.
- Owning the `Fault` slot in `Runtime` (not the context) keeps its address stable
  for the runtime's lifetime; the context just points at it.
- Checking overflow/div-zero *before* allocating avoids producing a malformed
  object on a fault path.

## Consequences

- Every faultable operation is a runtime call (not inline Cranelift arithmetic);
  a later optimization can inline the checked op and branch directly.
- Bool is allocated through the immortal path, so `praxis_alloc_bool` is not the
  pre-allocated singleton — acceptable because Bool equality is structural (§5.5).
- `pending_fault` is always non-null once wired; the pending state lives in the
  `Fault` slot, not in pointer-nullness.
