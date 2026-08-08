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
//! GC root tracking goes through a generated shadow-stack frame (§12.3); the
//! per-safepoint root set is the `roots` field MIR liveness (`annotate`) puts on
//! each safepoint instruction, read back through `praxis_mir::roots_of`.
//!
//! # Reading the emitted code
//!
//! Two environment variables dump what the backend emitted, on **stderr**, from
//! the real compile path — `PRAXIS_DUMP_CLIF` for the Cranelift IR as
//! `define_function` leaves it and `PRAXIS_DUMP_VCODE` for the machine-level
//! listing. Each takes `1`/`all` or a
//! comma-separated list of function names (`<entry>`, `main`, a user `fn`), and
//! each dump is headed by its instruction count per block:
//!
//! ```text
//! PRAXIS_DUMP_CLIF='<entry>' praxis run loop.px
//! ```
//!
//! They are permanent because an instruction count is the deterministic result
//! for a change that removes a few instructions from a loop, and the clock is
//! not. The module doc of `dump.rs` has the rest of the why.

mod dump;
pub mod generation;
mod lower;
mod module;
pub mod symbols;

pub use generation::{Generation, GenerationId, GenerationStats};
pub use module::{Jit, JitError, RunnableFunction};
