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
pub mod bitset;
pub mod closures;
pub mod collections;
pub mod context;
pub mod crash_snapshot;
pub mod debug;
pub mod descriptor;
pub mod dynamic_key;
pub mod enums;
pub mod gc;
pub mod graph;
pub mod heap;
pub mod heaps;
pub mod immortal;
pub mod input;
pub mod maps;
pub mod parse_detail;
pub mod parser;
pub mod range;
pub mod records;
pub mod repr;
pub mod roots;
pub mod scalars;
pub mod shadow_frame;
pub mod teardown;
pub mod text;
pub mod tuples;
pub mod var_cell;

pub use abi::{assert_abi_version, RUNTIME_ABI_VERSION};
pub use closures::ClosurePayload;
pub use collections::{DequePayload, GridPayload, VecPayload};
pub use context::{
    current_fault_kind, DebugFrame, DebugLocal, Fault, FaultKind, FaultMessage, Runtime,
    RuntimeContext, MAX_RECURSION_DEPTH,
};
pub use crash_snapshot::{CrashSnapshot, SnapshotFrame, SnapshotSlot};
pub use debug::{DebugLocalMeta, LOCAL_KIND_TEMP, LOCAL_KIND_USER};
pub use descriptor::{
    BuiltinTypeId, CompareFn, DropFn, DynamicHasher, EqualsFn, FormatFn, HashFn, StructHasher,
    TraceFn, Tracer, TypeDescriptor, TypeId, BUILTINS,
};
pub use gc::{GcHeader, GcRef, HeapId};
pub use heap::{Heap, HeapStats};
pub use immortal::Immortals;
pub use input::{clear_input_reader, install_input_reader, InputReader};
pub use parse_detail::{ParseDetail, ParseFail};
pub use records::{RecordField, RecordPayload, RecordSchema, SchemaIdentity};
pub use repr::{instance_repr, InstanceArg, InstanceRepr};
pub use roots::{NativeRootFrame, NativeScope, RootScope, RootSet, Rooted, RuntimeRoots};
pub use shadow_frame::{ShadowFrame, MAX_SHADOW_SLOTS};
pub use teardown::{retire_parser_plans, HeapDrained};
pub use text::TextPayload;
pub use tuples::{TuplePayload, TupleSchema};
