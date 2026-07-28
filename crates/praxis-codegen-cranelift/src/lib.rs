//! Cranelift JIT code generation and ABI lowering (§10, §14.1, ADR-016/017).
//!
//! Responsibility: lower MIR to Cranelift IR, register the `praxis_*` runtime
//! symbols, and finalize the JIT module. Every generated function follows the
//! uniform `fn(RuntimeContext*, GcRef...) -> GcRef` calling convention (§10.3).
//!
//! `GcRef` is `#[repr(transparent)]` over a pointer; Cranelift treats it as an
//! `i64` (pointer-sized integer) across the ABI — it is opaque to generated code.
//! MIR locals map to Cranelift `Variable`s; Cranelift turns the slot-based CFG
//! into SSA automatically (§13.5: "the Cranelift lowering layer creates SSA").
//!
//! Milestone 4 fills this crate. GC root tracking via a generated shadow-stack
//! frame (§12.3) is implemented; the per-safepoint root set comes from MIR
//! liveness (`live_roots`).

mod lower;
mod module;
pub mod symbols;

pub use module::{Jit, JitError, RunnableFunction};
