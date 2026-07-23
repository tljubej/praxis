//! Runtime ABI for the Praxis compiler, encoded as Rust types.
//!
//! This crate is the contract between JIT-generated code and the Rust runtime.
//! It deliberately contains **types and constants only** for Milestone 0 — the
//! real GC heap, descriptor tables, and fault machinery land in Milestones
//! 3–10. What is here is normative:
//!
//! - Every language value is a uniform [`GcRef`] (§4.3, §11.1).
//! - Every generated function follows `fn(RuntimeContext*, GcRef...) -> GcRef`
//!   (§10.3).
//! - Runtime wrappers never let a Rust panic unwind across the ABI (§9.2); they
//!   set `pending_fault` and return a defined sentinel instead.
//! - A [`TypeDescriptor`] centralizes every operation on a value's payload
//!   (§11.4), so the compiler never has to scatter type switches.
//!
//! See `praxis_technical_design.md` §11 and Appendix B.

pub mod abi;
pub mod context;
pub mod descriptor;
pub mod gc;

pub use abi::{assert_abi_version, RUNTIME_ABI_VERSION};
pub use context::{DebugFrame, Fault, Heap, RuntimeContext};
pub use descriptor::{
    DropFn, DynamicHasher, EqualsFn, FormatFn, HashFn, TraceFn, Tracer, TypeDescriptor, TypeId,
};
pub use gc::{GcHeader, GcRef};
