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
/// Size-class pages: the storage and the liveness bitmaps behind [`Heap`]
/// (ADR-103). Crate-private — a page is the collector's business and nothing
/// outside it has a reason to name one.
pub(crate) mod page;
pub mod parse_detail;
pub mod parser;
pub mod range;
pub mod records;
pub mod repr;
pub mod repr_c_vec;
pub mod roots;
pub mod scalars;
pub mod shadow_stack;
pub mod small_char;
pub mod small_int;
pub mod teardown;
pub mod text;
pub mod tuples;
pub mod var_cell;

pub use abi::{assert_abi_version, RUNTIME_ABI_VERSION};
pub use closures::ClosurePayload;
pub use collections::{DequePayload, GridPayload, VecPayload};
pub use context::{
    current_fault_kind, frame_cost, DebugLocal, Fault, FaultKind, FaultMessage, Runtime,
    RuntimeContext, StackBudget, FRAME_BYTES_BASE, FRAME_BYTES_PER_SLOT, MAX_RECURSION_DEPTH,
    REFERENCE_FRAME_SLOTS, STACK_BUDGET_BYTES,
};
pub use crash_snapshot::{CrashSnapshot, SnapshotFrame, SnapshotSlot};
pub use debug::{
    DebugFrameEntry, DebugFrameStack, DebugFrameStackHeader, DebugLocalMeta, DebugValueStack,
    DebugValueStackHeader, FunctionDebugMeta, LOCAL_KIND_TEMP, LOCAL_KIND_USER,
};
pub use descriptor::{
    BuiltinTypeId, CompareFn, DropFn, DynamicHasher, EqualsFn, FormatFn, HashFn, StructHasher,
    TraceFn, Tracer, TypeDescriptor, TypeId, BUILTINS,
};
pub use gc::{GcHeader, GcRef, HeapId};
pub use heap::{
    Heap, HeapStats, InlineInternSite, Pacer, INITIAL_COLLECT_THRESHOLD, LIVE_HEADROOM,
    MAX_COLLECT_THRESHOLD,
};
pub use immortal::Immortals;
pub use input::{clear_input_reader, install_input_reader, InputReader};
pub use parse_detail::{ParseDetail, ParseFail};
pub use records::{RecordField, RecordPayload, RecordSchema, SchemaIdentity};
pub use repr::{instance_repr, InstanceArg, InstanceRepr};
pub use repr_c_vec::{ReprCVec, VecMut};
pub use roots::{
    NativeRootStore, NativeScope, RootScope, RootSet, Rooted, RuntimeRoots, NATIVE_ROOT_RESERVATION,
};
pub use shadow_stack::{
    push_frame, ShadowFrameGuard, ShadowStack, ShadowStackHeader, SlotCount, SlotStack,
    SlotStackHeader, MAX_SHADOW_SLOTS, SHADOW_STACK_SLOTS,
};
pub use small_int::{
    index_of as small_int_index, SMALL_INT_COUNT, SMALL_INT_MAX, SMALL_INT_MIN, SMALL_INT_STRIDE,
};
pub use teardown::{retire_parser_plans, HeapDrained};
pub use text::TextPayload;
pub use tuples::{TuplePayload, TupleSchema};
