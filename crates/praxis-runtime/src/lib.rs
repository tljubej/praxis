//! Runtime heap, type descriptors, and runtime context for Praxis.
//!
//! This crate is the contract between JIT-generated code and the Rust runtime,
//! and (as of Milestone 3) it holds the real GC implementation.
//!
//! - Every language value is a uniform [`GcRef`] (§4.3, §11.1).
//! - Every generated function follows `fn(RuntimeContext*, GcRef...) -> GcRef`
//!   (§10.3).
//! - Runtime wrappers never let a Rust panic unwind across the ABI (§9.2); they
//!   set `pending_fault` and return a defined sentinel instead.
//! - A [`TypeDescriptor`] centralizes every operation on a value's payload
//!   (§11.4), so the compiler never has to scatter type switches.
//! - A precise, non-moving mark-and-sweep collector reclaims unreachable
//!   objects (§12.1, ADR-011).
//!
//! See `praxis_technical_design.md` §11, §12, and Appendix B.

pub mod abi;
pub mod collections;
pub mod context;
pub mod descriptor;
pub mod gc;
pub mod heap;
pub mod immortal;
pub mod roots;
pub mod scalars;
pub mod text;

pub use abi::{assert_abi_version, RUNTIME_ABI_VERSION};
pub use context::{DebugFrame, Fault, FaultKind, Runtime, RuntimeContext};
pub use descriptor::{
    DropFn, DynamicHasher, EqualsFn, FormatFn, HashFn, StructHasher, TraceFn, Tracer,
    TypeDescriptor, TypeId,
};
pub use gc::{GcHeader, GcRef};
pub use heap::{Heap, HeapStats};
pub use immortal::Immortals;
pub use roots::{RootScope, RootSet};
