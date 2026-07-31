//! Runtime ABI versioning (§11.6) and the `praxis_*` extern wrappers (§10.2,
//! §10.4, §11.1) that JIT-generated code calls.
//!
//! The runtime ABI is private to one Praxis executable build: there is no
//! cross-version compatibility promise, and no externally linkable surface for
//! user programs. Even so, the compiler and runtime are built from the same
//! workspace, and a single constant — checked at startup — catches accidental
//! internal drift between the code that *generates* calls and the code that
//! *implements* them.
//!
//! The `praxis_*` wrappers are the **only** functions generated code may call.
//! Every argument and return value that represents a language value is a
//! [`GcRef`]; scalars (`i64` payloads) cross the ABI only as transient values
//! tied to a single non-safepointed computation (§10.3). Per §10.4, **no wrapper
//! ever lets a Rust panic unwind across the ABI**: on overflow or division by
//! zero the wrapper writes the fault into the context's fault slot and returns a
//! defined sentinel.

use crate::context::{RaisedFault, RuntimeContext};
use crate::dynamic_key::DynamicKey;
use crate::gc::GcRef;
use crate::graph::GraphOracle;
use crate::heap::{Heap, Safepoint};
use crate::roots::{NativeScope, Rooted};
use crate::scalars;
use crate::{
    collections::VecPayload,
    descriptor::{Payload, TypeDescriptor},
};
pub use praxis_stdlib::abi::{AbiKind, AbiRet, AbiSig, Effect, RuntimeSymbol};

/// The runtime ABI version for this build. Bump this whenever the layout of
/// [`RuntimeContext`](crate::RuntimeContext), the calling convention, or the
/// signature set of `praxis_*` runtime wrappers changes in an incompatible way.
///
/// v2 (M5): `RuntimeContext` gained the `roots: *mut ShadowFrame` field for the
/// compiler-managed shadow-stack spill (ADR-019), and the `praxis_push_shadow_frame`
/// / `praxis_pop_shadow_frame` extern helpers were added.
/// v3 (M7 Part 1): record/enum object model — `praxis_alloc_record`,
/// `praxis_record_set_field`, `praxis_record_field`, `praxis_alloc_enum`,
/// `praxis_enum_set_payload`, `praxis_enum_tag`, `praxis_enum_payload`.
/// v4 (M7 Part 2, WS6): tuple object model and structural equality —
/// `praxis_alloc_tuple`, `praxis_tuple_set`, `praxis_tuple_get`, `praxis_struct_eq`.
/// v5 (M7 Part 2, WS7): closure object model — `praxis_alloc_closure`,
/// `praxis_closure_set_capture`, `praxis_closure_fn_ptr`, `praxis_closure_capture`.
/// v6 (M7 Part 3, WS7b): mutable-capture cells — `praxis_alloc_var_cell`,
/// `praxis_var_cell_get`, `praxis_var_cell_set`. (`praxis_closure_fn_ptr` also
/// gained a `ctx` param for ABI uniformity in WS7a, kept within v5's window.)
/// v7 (M8 WS1): collection construction now carries the real element descriptor
/// (closing the M7 `Vec[T]()` null-descriptor carryover). The construction path
/// is generalized across all M8 collection kinds as they land (Deque/Map/Set/
/// Counter/Heap/BitSet/complete Grid); each adds its own `praxis_<kind>_*`
/// wrappers within this version window.
/// v8 (repair S4): `GcHeader` repacked from 16 to 24 bytes — it gained
/// `payload_offset` (the single object-layout authority, replacing three
/// independent copies of the calculation) and `heap_id` (allocation
/// provenance). Generated code reads the header to reach an enum payload, so
/// this is an incompatible layout change.
/// v10 (repair S6): `RuntimeContext` gained `true_ref` and `false_ref`, the
/// cached `Bool` immortals, so `praxis_alloc_bool` hands back a singleton
/// instead of minting a fresh unreclaimable immortal per call. Appended after
/// `native_roots`.
/// v9 (repair S5): `RuntimeContext` gained `native_roots`, the head of the
/// native root-frame chain, so the runtime's own Rust helpers can root what
/// they hold across an allocation. Appended after `crash_snapshot`, so every
/// generated-code-read offset is unchanged — but the struct's size changed and
/// the automatic collector's root set is now the whole `RuntimeRoots`, which is
/// a behavioural contract generated code depends on.
/// v11 (repair S7): `Fault` lost its `pending: bool`, so the struct behind
/// `RuntimeContext.pending_fault` is one `FaultKind` wide. Generated code never
/// read the field — it calls `praxis_check_fault` — but the type is `#[repr(C)]`
/// and reachable from the context, so the shape change is declared rather than
/// assumed. `FaultKind` also gained `InvalidChar` and `InvalidText`, the two
/// kinds that previously had to be raised as `None` (RT-17).
/// v12 (repair S9): `DebugLocal.value` is an `Option<GcRef>`, and generated
/// code's meaning for the word changed with it. The struct's *layout* is
/// unchanged — the `NonNull` niche keeps it one machine word — but "no value
/// yet" moved from `NonNull::dangling()` (a sentinel the runtime wrote and the
/// hosts compared against by pointer identity) to the all-zero `None`, and
/// generated code now writes zero into a shadow slot whose local has died
/// (MIR-01). A runtime of the previous version would read either of those zeros
/// as a real reference (F18, MIR-16).
/// v13 (repair S17): `FaultKind` gained `Panic` and `AssertFailed`, the two
/// kinds §9.1 lists that had no encoding, and `RuntimeContext` gained
/// `fault_message` — the slot `praxis_panic`/`praxis_assert` write the
/// program's message into. Appended after `false_ref`, so every
/// generated-code-read offset is unchanged; the struct's size is not.
/// v14 (repair S18): `EnumPayload` gained a leading `schema: *const EnumSchema`
/// — an enum value now records which enum *type* it is (RT-13) — so the `tag`
/// moved from offset 0 to offset 8 and generated code reads it through
/// `offset_of!` rather than a literal. `praxis_alloc_enum` changed shape with
/// it: `(ctx, schema, tag)`, with the arity read from the schema instead of
/// passed beside it. `FaultKind` also gained `EmptyRange` and `NoAnswer`, the
/// two kinds three S17 ADRs recorded as owed (ADR-058, ADR-059, ADR-060), and
/// four raises that had been borrowing `InvalidSize` moved onto them. A runtime
/// of the previous version would read an enum's schema pointer as its tag.
pub const RUNTIME_ABI_VERSION: u32 = 14;

/// Assert that the compiler's expected ABI version matches this build's.
///
/// Called once at CLI / LSP startup. Today the compiler and runtime are the
/// same binary, so the assertion is trivially satisfied; the point is to have
/// the check in place before the runtime is split across build artifacts.
///
/// # Panics
/// Panics if the versions disagree. A disagreement is always a build bug, never
/// a user-facing condition.
pub fn assert_abi_version() {
    assert_eq!(
        COMPILER_EXPECTED_ABI_VERSION, RUNTIME_ABI_VERSION,
        "compiler/runtime ABI version mismatch: compiler expected \
         {COMPILER_EXPECTED_ABI_VERSION}, runtime reports {RUNTIME_ABI_VERSION}. \
         This is a build inconsistency; rebuild the workspace."
    );
}

/// The ABI version the compiler front end assumes when generating code. Kept in
/// lockstep with [`RUNTIME_ABI_VERSION`] within a single build.
const COMPILER_EXPECTED_ABI_VERSION: u32 = 14;

// ---------------------------------------------------------------------------
// The runtime symbol table (F4).
// ---------------------------------------------------------------------------

/// The address of a runtime wrapper, for the JIT to resolve an import to.
///
/// This match is the **only** symbol→address table in the workspace, and it is
/// exhaustive over [`RuntimeSymbol`]: adding a row to the manifest without
/// giving it an address here is a compile error, which is the property the old
/// name-keyed resolver could not have. There is no fallback — the JIT never
/// reaches `dlsym`, so a symbol the compiler failed to register cannot
/// accidentally "work" because it happens to be linked in.
#[must_use]
pub fn address(symbol: RuntimeSymbol) -> *const u8 {
    let ptr: *const () = match symbol {
        RuntimeSymbol::AllocBool => praxis_alloc_bool as *const (),
        RuntimeSymbol::AllocChar => praxis_alloc_char as *const (),
        RuntimeSymbol::AllocClosure => praxis_alloc_closure as *const (),
        RuntimeSymbol::AllocEnum => praxis_alloc_enum as *const (),
        RuntimeSymbol::AllocFloat => praxis_alloc_float as *const (),
        RuntimeSymbol::AllocInt => praxis_alloc_int as *const (),
        RuntimeSymbol::AllocRecord => praxis_alloc_record as *const (),
        RuntimeSymbol::AllocText => praxis_alloc_text as *const (),
        RuntimeSymbol::AllocTuple => praxis_alloc_tuple as *const (),
        RuntimeSymbol::AllocUnit => praxis_alloc_unit as *const (),
        RuntimeSymbol::AllocVarCell => praxis_alloc_var_cell as *const (),
        RuntimeSymbol::Assert => praxis_assert as *const (),
        RuntimeSymbol::AStar => praxis_a_star as *const (),
        RuntimeSymbol::Bfs => praxis_bfs as *const (),
        RuntimeSymbol::BfsDistance => praxis_bfs_distance as *const (),
        RuntimeSymbol::BitsetContains => praxis_bitset_contains as *const (),
        RuntimeSymbol::BitsetInsert => praxis_bitset_insert as *const (),
        RuntimeSymbol::BitsetIsEmpty => praxis_bitset_is_empty as *const (),
        RuntimeSymbol::BitsetItems => praxis_bitset_items as *const (),
        RuntimeSymbol::BitsetLen => praxis_bitset_len as *const (),
        RuntimeSymbol::BitsetNew => praxis_bitset_new as *const (),
        RuntimeSymbol::BitsetRemove => praxis_bitset_remove as *const (),
        RuntimeSymbol::BoolLoad => praxis_bool_load as *const (),
        RuntimeSymbol::CharLoad => praxis_char_load as *const (),
        RuntimeSymbol::CheckFault => praxis_check_fault as *const (),
        RuntimeSymbol::ClosureCapture => praxis_closure_capture as *const (),
        RuntimeSymbol::ClosureFnPtr => praxis_closure_fn_ptr as *const (),
        RuntimeSymbol::ClosureSetCapture => praxis_closure_set_capture as *const (),
        RuntimeSymbol::CounterGet => praxis_counter_get as *const (),
        RuntimeSymbol::CounterInc => praxis_counter_inc as *const (),
        RuntimeSymbol::CounterIsEmpty => praxis_counter_is_empty as *const (),
        RuntimeSymbol::CounterLen => praxis_counter_len as *const (),
        RuntimeSymbol::CounterKeys => praxis_counter_keys as *const (),
        RuntimeSymbol::CounterNew => praxis_counter_new as *const (),
        RuntimeSymbol::CounterSet => praxis_counter_set as *const (),
        RuntimeSymbol::CounterValues => praxis_counter_values as *const (),
        RuntimeSymbol::DequeGet => praxis_deque_get as *const (),
        RuntimeSymbol::DequeIsEmpty => praxis_deque_is_empty as *const (),
        RuntimeSymbol::DequeLen => praxis_deque_len as *const (),
        RuntimeSymbol::DequeNew => praxis_deque_new as *const (),
        RuntimeSymbol::DequePopBack => praxis_deque_pop_back as *const (),
        RuntimeSymbol::DequePopFront => praxis_deque_pop_front as *const (),
        RuntimeSymbol::DequePushBack => praxis_deque_push_back as *const (),
        RuntimeSymbol::DequePushFront => praxis_deque_push_front as *const (),
        RuntimeSymbol::Dbg => praxis_dbg as *const (),
        RuntimeSymbol::Dfs => praxis_dfs as *const (),
        RuntimeSymbol::Dijkstra => praxis_dijkstra as *const (),
        RuntimeSymbol::EnumPayload => praxis_enum_payload as *const (),
        RuntimeSymbol::EnumSetPayload => praxis_enum_set_payload as *const (),
        RuntimeSymbol::EnumTag => praxis_enum_tag as *const (),
        RuntimeSymbol::FloatAbs => praxis_float_abs as *const (),
        RuntimeSymbol::FloatCeil => praxis_float_ceil as *const (),
        RuntimeSymbol::FloatE => praxis_float_e as *const (),
        RuntimeSymbol::FloatFloor => praxis_float_floor as *const (),
        RuntimeSymbol::FloatIsInfinite => praxis_float_is_infinite as *const (),
        RuntimeSymbol::FloatIsNan => praxis_float_is_nan as *const (),
        RuntimeSymbol::FloatLoad => praxis_float_load as *const (),
        RuntimeSymbol::FloatMax => praxis_float_max as *const (),
        RuntimeSymbol::FloatMin => praxis_float_min as *const (),
        RuntimeSymbol::FloatPi => praxis_float_pi as *const (),
        RuntimeSymbol::FloatRound => praxis_float_round as *const (),
        RuntimeSymbol::FloatSign => praxis_float_sign as *const (),
        RuntimeSymbol::FloatSqrt => praxis_float_sqrt as *const (),
        RuntimeSymbol::FloatToInt => praxis_float_to_int as *const (),
        RuntimeSymbol::FloatToText => praxis_float_to_text as *const (),
        RuntimeSymbol::FloodFill => praxis_flood_fill as *const (),
        RuntimeSymbol::GetInput => praxis_get_input as *const (),
        RuntimeSymbol::GridCells => praxis_grid_cells as *const (),
        RuntimeSymbol::GridColumn => praxis_grid_column as *const (),
        RuntimeSymbol::GridContains => praxis_grid_contains as *const (),
        RuntimeSymbol::GridFind => praxis_grid_find as *const (),
        RuntimeSymbol::GridFindAll => praxis_grid_find_all as *const (),
        RuntimeSymbol::GridGet => praxis_grid_get as *const (),
        RuntimeSymbol::GridHeight => praxis_grid_height as *const (),
        RuntimeSymbol::GridNeighbors4 => praxis_grid_neighbors4 as *const (),
        RuntimeSymbol::GridNeighbors8 => praxis_grid_neighbors8 as *const (),
        RuntimeSymbol::GridNew => praxis_grid_new as *const (),
        RuntimeSymbol::GridPositions => praxis_grid_positions as *const (),
        RuntimeSymbol::GridRotateLeft => praxis_grid_rotate_left as *const (),
        RuntimeSymbol::GridRotateRight => praxis_grid_rotate_right as *const (),
        RuntimeSymbol::GridRow => praxis_grid_row as *const (),
        RuntimeSymbol::GridSet => praxis_grid_set as *const (),
        RuntimeSymbol::GridTranspose => praxis_grid_transpose as *const (),
        RuntimeSymbol::GridWidth => praxis_grid_width as *const (),
        RuntimeSymbol::IntAbs => praxis_int_abs as *const (),
        RuntimeSymbol::IntAdd => praxis_int_add as *const (),
        RuntimeSymbol::IntClamp => praxis_int_clamp as *const (),
        RuntimeSymbol::IntDiv => praxis_int_div as *const (),
        RuntimeSymbol::IntEq => praxis_int_eq as *const (),
        RuntimeSymbol::IntGcd => praxis_int_gcd as *const (),
        RuntimeSymbol::IntGe => praxis_int_ge as *const (),
        RuntimeSymbol::IntGt => praxis_int_gt as *const (),
        RuntimeSymbol::IntLcm => praxis_int_lcm as *const (),
        RuntimeSymbol::IntLe => praxis_int_le as *const (),
        RuntimeSymbol::IntLoad => praxis_int_load as *const (),
        RuntimeSymbol::IntLt => praxis_int_lt as *const (),
        RuntimeSymbol::IntMax => praxis_int_max as *const (),
        RuntimeSymbol::IntMin => praxis_int_min as *const (),
        RuntimeSymbol::IntMul => praxis_int_mul as *const (),
        RuntimeSymbol::IntNe => praxis_int_ne as *const (),
        RuntimeSymbol::IntNeg => praxis_int_neg as *const (),
        RuntimeSymbol::IntRem => praxis_int_rem as *const (),
        RuntimeSymbol::IntSign => praxis_int_sign as *const (),
        RuntimeSymbol::IntSub => praxis_int_sub as *const (),
        RuntimeSymbol::IntToFloat => praxis_int_to_float as *const (),
        RuntimeSymbol::MapContains => praxis_map_contains as *const (),
        RuntimeSymbol::RangeGet => praxis_range_get as *const (),
        RuntimeSymbol::RangeLen => praxis_range_len as *const (),
        RuntimeSymbol::RangeNew => praxis_range_new as *const (),
        RuntimeSymbol::RangeNewInclusive => praxis_range_new_inclusive as *const (),
        RuntimeSymbol::MapGet => praxis_map_get as *const (),
        RuntimeSymbol::MapIndex => praxis_map_index as *const (),
        RuntimeSymbol::MapInsert => praxis_map_insert as *const (),
        RuntimeSymbol::MapIsEmpty => praxis_map_is_empty as *const (),
        RuntimeSymbol::MapKeys => praxis_map_keys as *const (),
        RuntimeSymbol::MapLen => praxis_map_len as *const (),
        RuntimeSymbol::MapNew => praxis_map_new as *const (),
        RuntimeSymbol::MapRemove => praxis_map_remove as *const (),
        RuntimeSymbol::MapUpdateMax => praxis_map_update_max as *const (),
        RuntimeSymbol::MapUpdateMin => praxis_map_update_min as *const (),
        RuntimeSymbol::MapValues => praxis_map_values as *const (),
        RuntimeSymbol::MaxHeapIsEmpty => praxis_max_heap_is_empty as *const (),
        RuntimeSymbol::MaxHeapItems => praxis_max_heap_items as *const (),
        RuntimeSymbol::MaxHeapLen => praxis_max_heap_len as *const (),
        RuntimeSymbol::MaxHeapNew => praxis_max_heap_new as *const (),
        RuntimeSymbol::MaxHeapPeek => praxis_max_heap_peek as *const (),
        RuntimeSymbol::MaxHeapPop => praxis_max_heap_pop as *const (),
        RuntimeSymbol::MaxHeapPush => praxis_max_heap_push as *const (),
        RuntimeSymbol::MinHeapIsEmpty => praxis_min_heap_is_empty as *const (),
        RuntimeSymbol::MinHeapItems => praxis_min_heap_items as *const (),
        RuntimeSymbol::MinHeapLen => praxis_min_heap_len as *const (),
        RuntimeSymbol::MinHeapNew => praxis_min_heap_new as *const (),
        RuntimeSymbol::MinHeapPeek => praxis_min_heap_peek as *const (),
        RuntimeSymbol::MinHeapPop => praxis_min_heap_pop as *const (),
        RuntimeSymbol::MinHeapPush => praxis_min_heap_push as *const (),
        RuntimeSymbol::Panic => praxis_panic as *const (),
        RuntimeSymbol::PopDebugFrame => crate::debug::praxis_pop_debug_frame as *const (),
        RuntimeSymbol::PopShadowFrame => crate::shadow_frame::praxis_pop_shadow_frame as *const (),
        RuntimeSymbol::PushDebugFrame => crate::debug::praxis_push_debug_frame as *const (),
        RuntimeSymbol::PushShadowFrame => {
            crate::shadow_frame::praxis_push_shadow_frame as *const ()
        }
        RuntimeSymbol::RaiseDivByZeroIf => praxis_raise_div_by_zero_if as *const (),
        RuntimeSymbol::RaiseEmptyCollection => praxis_raise_empty_collection as *const (),
        RuntimeSymbol::RaiseIntOverflowIf => praxis_raise_int_overflow_if as *const (),
        RuntimeSymbol::RaiseStackOverflow => praxis_raise_stack_overflow as *const (),
        RuntimeSymbol::RecordField => praxis_record_field as *const (),
        RuntimeSymbol::RecordSetField => praxis_record_set_field as *const (),
        RuntimeSymbol::RunParser => praxis_run_parser as *const (),
        RuntimeSymbol::SetContains => praxis_set_contains as *const (),
        RuntimeSymbol::SetFrameSourceSpan => {
            crate::debug::praxis_set_frame_source_span as *const ()
        }
        RuntimeSymbol::SetInsert => praxis_set_insert as *const (),
        RuntimeSymbol::SetIsEmpty => praxis_set_is_empty as *const (),
        RuntimeSymbol::SetItems => praxis_set_items as *const (),
        RuntimeSymbol::SetLen => praxis_set_len as *const (),
        RuntimeSymbol::SetNew => praxis_set_new as *const (),
        RuntimeSymbol::SetRemove => praxis_set_remove as *const (),
        RuntimeSymbol::SnapshotDebugChain => {
            crate::crash_snapshot::praxis_snapshot_debug_chain as *const ()
        }
        RuntimeSymbol::StructEq => praxis_struct_eq as *const (),
        RuntimeSymbol::TextGet => praxis_text_get as *const (),
        RuntimeSymbol::TextIsEmpty => praxis_text_is_empty as *const (),
        RuntimeSymbol::TextLen => praxis_text_len as *const (),
        RuntimeSymbol::TupleGet => praxis_tuple_get as *const (),
        RuntimeSymbol::TupleSet => praxis_tuple_set as *const (),
        RuntimeSymbol::ValueCmp => praxis_value_cmp as *const (),
        RuntimeSymbol::VarCellGet => praxis_var_cell_get as *const (),
        RuntimeSymbol::VarCellSet => praxis_var_cell_set as *const (),
        RuntimeSymbol::VecGet => praxis_vec_get as *const (),
        RuntimeSymbol::VecIsEmpty => praxis_vec_is_empty as *const (),
        RuntimeSymbol::VecLen => praxis_vec_len as *const (),
        RuntimeSymbol::VecNew => praxis_vec_new as *const (),
        RuntimeSymbol::VecPush => praxis_vec_push as *const (),
        RuntimeSymbol::WriteStdout => praxis_write_stdout as *const (),
    };
    ptr as *const u8
}

// ---------------------------------------------------------------------------
// Internals the wrappers share.
// ---------------------------------------------------------------------------

/// Raise `fault` on `ctx`'s fault slot (§10.4). Does nothing if the context's
/// fault pointer is null (a misuse, but never panics across the ABI).
///
/// Takes a [`RaisedFault`], not a `FaultKind`: two wrappers used to pass
/// `FaultKind::None` for want of a kind that described them, leaving generated
/// code branching to its fault path while the host reported "no fault" (RT-17).
unsafe fn set_fault(ctx: *mut RuntimeContext, fault: RaisedFault) {
    if let Some(slot) = unsafe { (*ctx).pending_fault.as_mut() } {
        slot.set(fault);
    }
}

/// Record `text` as the message the fault about to be raised carries (§9.1).
/// Does nothing if the context's message slot is null (a host that wired no
/// runtime), so a `panic` still faults even where nothing can render its words.
unsafe fn set_fault_message(ctx: *mut RuntimeContext, text: String) {
    if let Some(slot) = unsafe { (*ctx).fault_message.as_mut() } {
        slot.set(text);
    }
}

/// The heap pointer out of a context, or null.
#[inline]
unsafe fn heap<'a>(ctx: *mut RuntimeContext) -> &'a Heap {
    // SAFETY: the caller guarantees `ctx` points at a live, wired context whose
    // `heap` field references a valid `Heap` for the duration of the call.
    unsafe { &*(*ctx).heap }
}

/// Trigger a collection on allocation pressure, rooting from the context's
/// shadow-stack frame chain (§12.4, ADR-019). Called by every allocating
/// `praxis_*` wrapper. Safe to call with a null/unwired context (no-op).
///
/// The roots are every arm of [`RuntimeRoots`](crate::roots::RuntimeRoots) —
/// the shadow-stack chain, the ambient input buffer, a parse failure's partial
/// value, a runtime-owned crash snapshot, and the native root frames. This used
/// to read `ctx.roots` alone **and return early when it was null**, which meant
/// nothing was collected at all during host-driven allocation or anywhere in
/// the parser interpreter, and that the other four owners were invisible to
/// automatic GC even when a frame was pushed (P0-06).
#[inline]
unsafe fn maybe_collect(ctx: *mut RuntimeContext) {
    if ctx.is_null() {
        return;
    }
    // SAFETY: ctx is live and wired.
    let roots = unsafe { crate::roots::RuntimeRoots::from_context(ctx) };
    unsafe { heap(ctx).maybe_collect(&roots) };
}

/// Pace the collector and mint the token one allocation needs (P0-08b).
///
/// Every `praxis_*` wrapper reaches the heap through [`gc_alloc`] or
/// [`gc_alloc_with`], which call this — and even a wrapper that reached
/// `Heap::alloc` directly would have to come through here, because the token
/// has no other producer. That is the whole point: fourteen wrappers used to
/// gc-allocate without pacing, so a program whose pressure came from `Text`,
/// `.len()` or checked arithmetic could run arbitrarily long without the
/// collector ever being offered a turn.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`. Every allocating
/// wrapper already requires this to reach the heap at all.
#[inline]
unsafe fn safepoint<'a>(ctx: *mut RuntimeContext) -> (&'a Heap, Safepoint<'a>) {
    // SAFETY: caller upholds ctx validity.
    let h = unsafe { heap(ctx) };
    // SAFETY: as above; the roots are read out of the same live context.
    let roots = unsafe { crate::roots::RuntimeRoots::from_context(ctx) };
    let sp = h.pace(&roots);
    (h, sp)
}

/// Pace the collector, then allocate a `Copy` payload (§12.4).
///
/// The descriptor arrives as a [`Payload<T>`] — `scalars::INT_PAYLOAD`, not
/// `&scalars::INT` — so `value`'s Rust type is checked against it here, at the
/// call. This signature *is* REP-02's fix.
///
/// While the descriptor was a bare reference and `T` was free,
/// `gc_alloc(ctx, &scalars::INT, 0)` compiled — a Rust integer literal defaults
/// to `i32` — and aborted the process from inside `extern "C"` with "payload
/// size mismatch for descriptor Int", a non-unwinding panic across the ABI that
/// §10.4 forbids. Two things changed: a value of the wrong type is now an
/// `E0308` at the call, and an *untyped* literal infers as the payload type
/// instead of defaulting, so REP-02's own reproduction is correct code rather
/// than a rejected one.
///
/// # Safety
/// `ctx` must be live and wired.
#[inline]
unsafe fn gc_alloc<T: Copy>(ctx: *mut RuntimeContext, payload: Payload<T>, value: T) -> GcRef {
    // SAFETY: caller upholds ctx validity.
    let (h, sp) = unsafe { safepoint(ctx) };
    h.alloc(sp, payload, value)
}

/// Pace the collector, then allocate a payload that owns Rust resources.
///
/// # Safety
/// `ctx` must be live and wired; `init` must fully initialize `size` bytes as
/// `descriptor`'s payload type ([`Heap::alloc_with`]'s contract).
#[inline]
unsafe fn gc_alloc_with(
    ctx: *mut RuntimeContext,
    descriptor: &'static TypeDescriptor,
    size: usize,
    align: usize,
    init: impl FnOnce(*mut u8),
) -> GcRef {
    // SAFETY: caller upholds ctx validity.
    let (h, sp) = unsafe { safepoint(ctx) };
    // SAFETY: forwarded from this function's contract.
    unsafe { h.alloc_with(sp, descriptor, size, align, init) }
}

/// The immortal `Bool` for `value`, off the context's cached singletons.
///
/// Never an allocation: there are exactly two `Bool` values and the runtime
/// minted both at startup. Every wrapper that answers a predicate used to call
/// `alloc_immortal` instead, consuming unregistered arena storage — permanently
/// — once per comparison, `contains`, `is_empty` or emptiness check a program
/// evaluated (RT-03). It is also what makes those rows honestly `Effect::Pure`:
/// nothing here can collect, so the call site is not a safepoint.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[inline]
unsafe fn bool_ref(ctx: *mut RuntimeContext, value: bool) -> GcRef {
    // SAFETY: caller upholds ctx validity.
    let c = unsafe { &*ctx };
    if value {
        c.true_ref
    } else {
        c.false_ref
    }
}

/// Read `r`'s payload through a [`Payload`] handle, first checking that `r`
/// really is that handle's type.
///
/// This is the reader to reach for whenever a wrapper receives a `GcRef` it did
/// not itself allocate — a value handed back by a program's closure, most of
/// all. Two mistakes are impossible through it and were both possible without
/// it: reading a value of the wrong *type* (the identity check answers `None`,
/// and the caller decides whether that is a `TypeMismatch` fault), and reading
/// the right type at the wrong *width* (the width is `size_of::<T>()`, which
/// [`Payload::new`] proved is the descriptor's width when the handle was
/// declared).
///
/// REP-37 is why it exists: `ClosureOracle::is_goal` read a `Bool` — a
/// **one**-byte payload — with `int_payload`, an eight-byte read, so every
/// graph goal predicate consumed seven bytes of arena padding past the object
/// and answered whatever the allocator had left there.
///
/// # Safety
/// `r` must be a valid `GcRef` into a live heap.
#[inline]
unsafe fn read_scalar<T: Copy>(r: GcRef, handle: crate::descriptor::Payload<T>) -> Option<T> {
    if !std::ptr::eq(r.descriptor(), handle.descriptor()) {
        return None;
    }
    // SAFETY: the identity check proves `r`'s payload is this handle's type, and
    // the handle's own construction proved `T` is that type's layout.
    Some(unsafe { handle.read(r.payload::<u8>()) })
}

/// Read the `i64` payload of an `Int` `GcRef`. Used by every arithmetic wrapper.
///
/// Prefer [`read_scalar`] for any value whose type is not already established:
/// this reads eight bytes unconditionally, and a `debug_assert` is all that
/// stands between it and a narrower payload (REP-37).
#[inline]
unsafe fn int_payload(r: GcRef) -> i64 {
    debug_assert_eq!(
        r.descriptor().size(),
        std::mem::size_of::<i64>(),
        "int_payload reads eight bytes; `{}` is {} wide — use `read_scalar` with \
         that type's `Payload` handle instead (REP-37)",
        r.descriptor().name,
        r.descriptor().size(),
    );
    // SAFETY: the compiler only emits these calls with Int-typed operands; the
    // payload follows the header and is an `i64`. Faults that would feed a
    // non-`Int` (e.g. the Unit sentinel) into an arithmetic wrapper are diverted
    // before reaching here by `Inst::CheckFault` branching to the fault block
    // (§10.4), so operands on the normal path are always valid `Int`s.
    unsafe { *r.payload::<i64>() }
}

/// The Unit sentinel GcRef from the context's input source slot. (Unit is an
/// immortal; in M4 we reuse the input_source field which the runtime sets to
/// The Unit GcRef returned on fault paths as the "defined dummy" (§10.4). Reads
/// the cached immortal `unit_ref` from the context — stable for the program
/// lifetime regardless of whether `input_source` holds the read-in buffer.
#[inline]
unsafe fn unit_sentinel(ctx: *mut RuntimeContext) -> GcRef {
    unsafe { (*ctx).unit_ref }
}

// ---------------------------------------------------------------------------
// Allocation wrappers.
// ---------------------------------------------------------------------------

/// Allocate a boxed `Int` initialized to `value` (§4.3, §11.1).
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext` whose `heap` is valid.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_int(ctx: *mut RuntimeContext, value: i64) -> GcRef {
    // `gc_alloc` paces first, rooted at the whole `RuntimeRoots`. The new object
    // is not yet a root, but it is returned by value to the caller, which spills
    // it — so it is safe across this collection (the *previous* allocation's
    // result was already spilled by the backend before this wrapper was called).
    // SAFETY: caller upholds the ctx/heap validity.
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, value) }
}

/// Allocate a boxed `Bool` from a 0/1 value (§4.3). Returns the immortal
/// singleton, never a fresh allocation.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_bool(ctx: *mut RuntimeContext, value: i64) -> GcRef {
    // There are two `Bool` values, and the runtime allocated both at startup.
    // This used to mint a *fresh* immortal per call — unregistered arena storage
    // no collection can ever reclaim, one per comparison a program evaluates
    // (RT-03). `value != 0` is true; `0` is false.
    // SAFETY: caller upholds ctx validity.
    let c = unsafe { &*ctx };
    if value != 0 {
        c.true_ref
    } else {
        c.false_ref
    }
}

/// Allocate the `Unit` singleton (§4.3).
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_unit(ctx: *mut RuntimeContext) -> GcRef {
    // The one `Unit` value, cached on the context since M6 for the fault path.
    // As for `praxis_alloc_bool`, allocating a fresh immortal per call leaked
    // arena storage permanently (RT-03).
    // SAFETY: caller upholds ctx validity.
    unsafe { (*ctx).unit_ref }
}

/// Allocate a boxed `Char` from a Unicode scalar value (§4.3, M6). The `value`
/// is the `u32` code point carried as `i64` (the uniform scalar ABI width). If
/// the code point is not a valid scalar, the fault is set and the Unit sentinel
/// is returned (no panic crosses the ABI).
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_char(ctx: *mut RuntimeContext, value: i64) -> GcRef {
    // Range-check the `i64` *before* narrowing it. `value as u32` truncates, so
    // `0x1_0000_0041` became `0x41` and a program that computed a nonsense code
    // point silently got `'A'` (RT-18). The scalar ABI is 64 bits wide; a code
    // point is not, and the conversion has to say so rather than wrap.
    let Ok(code) = u32::try_from(value) else {
        unsafe { set_fault(ctx, RaisedFault::INVALID_CHAR) };
        return unsafe { unit_sentinel(ctx) };
    };
    if !crate::scalars::is_valid_char(code) {
        // Defensive: the parser validates scalars, but a malformed code point must
        // not panic across the ABI.
        unsafe { set_fault(ctx, RaisedFault::INVALID_CHAR) };
        return unsafe { unit_sentinel(ctx) };
    }
    // SAFETY: caller upholds ctx/heap validity; code is a validated scalar.
    unsafe { gc_alloc(ctx, scalars::CHAR_PAYLOAD, code) }
}

/// Allocate an owned `Text` from a UTF-8 byte buffer (§4.3, ADR-013).
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`; `bytes` must point at
/// `len` valid UTF-8 bytes that remain valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_text(
    ctx: *mut RuntimeContext,
    bytes: *const u8,
    len: usize,
) -> GcRef {
    let slice = if bytes.is_null() || len == 0 {
        &[]
    } else {
        // SAFETY: caller guarantees `bytes..bytes+len` is a valid, UTF-8 buffer.
        unsafe { std::slice::from_raw_parts(bytes, len) }
    };
    // The buffer must be valid UTF-8 (the compiler emits Text from string
    // literals). Fall back to a replacement on malformed input rather than
    // panicking across the ABI.
    let owned: Box<str> = match std::str::from_utf8(slice) {
        Ok(s) => s.into(),
        Err(_) => {
            unsafe { set_fault(ctx, RaisedFault::INVALID_TEXT) };
            std::string::String::from_utf8_lossy(slice)
                .into_owned()
                .into_boxed_str()
        }
    };
    // SAFETY: TextPayload matches TEXT's size/align and is fully initialized.
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::text::TEXT,
            std::mem::size_of::<crate::text::TextPayload>(),
            std::mem::align_of::<crate::text::TextPayload>(),
            |payload| {
                (payload as *mut crate::text::TextPayload)
                    .write(crate::text::TextPayload::Owned(owned));
            },
        )
    }
}

// ---------------------------------------------------------------------------
// Scalar extraction / materialization.
// ---------------------------------------------------------------------------

/// Read the `i64` payload of an `Int` `GcRef` (§10.3 transient scalar).
///
/// # Safety
/// `r` must be a valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_load(_ctx: *mut RuntimeContext, r: GcRef) -> i64 {
    // SAFETY: caller guarantees `r` is an Int.
    unsafe { int_payload(r) }
}

/// Read a `Bool` payload as 0/1 (§10.3 transient scalar).
///
/// # Safety
/// `r` must be a valid `Bool` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_bool_load(_ctx: *mut RuntimeContext, r: GcRef) -> i64 {
    // Bool payload is stored as the immortal true/false; compare pointer identity
    // against the descriptor to recover the bit is not possible without the
    // immortals. Instead read the stored byte payload (immortal Bools carry a
    // bool payload).
    // SAFETY: caller guarantees `r` is a Bool; payload is a bool-sized value.
    let p: *mut bool = r.payload::<bool>();
    if unsafe { *p } {
        1
    } else {
        0
    }
}

/// Read a `Char` payload as its `u32` code point widened to `i64` (§4.3, M6).
///
/// # Safety
/// `r` must be a valid `Char` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_char_load(_ctx: *mut RuntimeContext, r: GcRef) -> i64 {
    // SAFETY: caller guarantees `r` is a Char; payload is a u32 scalar value.
    let p: *mut u32 = r.payload::<u32>();
    unsafe { *p as i64 }
}

/// Allocate a boxed `Float` from an `i64` carrying the IEEE-754 binary64 bit
/// pattern (§4.3, §4.12). The uniform scalar ABI carries every payload as
/// `i64`; a float is transported as `f64::to_bits()` and reassembled here.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_float(ctx: *mut RuntimeContext, value: i64) -> GcRef {
    let f = f64::from_bits(value as u64);
    // SAFETY: caller upholds ctx/heap validity; all f64 values are valid Floats.
    unsafe { gc_alloc(ctx, scalars::FLOAT_PAYLOAD, f) }
}

/// Read a `Float` payload as its IEEE-754 bit pattern widened to `i64`
/// (§10.3 transient scalar). Generated code keeps floats in the uniform `i64`
/// scalar channel; the bit pattern is reassembled into an `f64` only at the
/// point of an arithmetic/comparison instruction.
///
/// # Safety
/// `r` must be a valid `Float` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_load(_ctx: *mut RuntimeContext, r: GcRef) -> i64 {
    // SAFETY: caller guarantees `r` is a Float; payload is an f64.
    let p: *mut f64 = r.payload::<f64>();
    unsafe { (*p).to_bits() as i64 }
}

// ---------------------------------------------------------------------------
// Float conversion & methods (§4.12). Float arithmetic never faults (IEEE-754
// produces inf/nan); only the narrowing `to_int` conversion does.
// ---------------------------------------------------------------------------

/// Read a `Float` payload as an `f64` (private helper).
///
/// # Safety
/// `r` must be a valid `Float` `GcRef`.
unsafe fn float_payload(r: GcRef) -> f64 {
    // SAFETY: caller guarantees `r` is a Float; payload is an f64.
    unsafe { *r.payload::<f64>() }
}

/// Widen an `Int` to a `Float` (§4.12). Never faults — every `i64` is exactly
/// representable as an `f64`? No: integers above 2^53 lose precision, but the
/// conversion is still total and well-defined (rounds to nearest). This is the
/// explicit widening method `Int.to_float()`.
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_to_float(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    let i = unsafe { int_payload(r) };
    // SAFETY: ctx/heap valid; every widened int is a valid Float payload.
    unsafe { gc_alloc(ctx, scalars::FLOAT_PAYLOAD, i as f64) }
}

/// Narrow a `Float` to an `Int` by truncating toward zero (§4.12). Faults
/// (`FloatToInt`) on NaN, ±infinity, or a finite value outside the signed
/// 64-bit range — these have no exact `Int` representation. On fault, sets
/// `pending_fault` and returns the Unit sentinel (no panic crosses the ABI).
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Float` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_to_int(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    let f = unsafe { float_payload(r) };
    // NaN, infinities, and out-of-range finite values are not exactly
    // representable as i64. Rust's `as i64` saturates (inf→i64::MAX,
    // -inf→i64::MIN, nan→0), which would silently produce a plausible-but-wrong
    // value; per §4.12 these cases fault instead.
    if f.is_nan() || f.is_infinite() || f < i64::MIN as f64 || f >= i64::MAX as f64 {
        unsafe { set_fault(ctx, RaisedFault::FLOAT_TO_INT) };
        return unsafe { unit_sentinel(ctx) };
    }
    // The range check above bounds f to (-2^63, 2^63); truncation toward zero is
    // then exact for every representable integer and inexact-but-safe for the
    // fractional part (which is discarded).
    // SAFETY: ctx/heap valid; the value is in i64 range.
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, f as i64) }
}

/// Re-box a `Float` after a pure transform (no fault possible). Used by
/// `abs`/`sqrt`/`floor`/`ceil`/`round`/`sign`.
unsafe fn rebox_float(ctx: *mut RuntimeContext, out: f64) -> GcRef {
    // SAFETY: ctx/heap valid; every f64 is a valid Float payload.
    unsafe { gc_alloc(ctx, scalars::FLOAT_PAYLOAD, out) }
}

/// `Float.abs()` — absolute value (§4.12).
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Float` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_abs(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    let f = unsafe { float_payload(r) };
    unsafe { rebox_float(ctx, f.abs()) }
}

/// `Float.sqrt()` — square root (§4.12). Negative inputs yield NaN (IEEE-754);
/// this never faults.
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Float` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_sqrt(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    let f = unsafe { float_payload(r) };
    unsafe { rebox_float(ctx, f.sqrt()) }
}

/// `Float.floor()` — round toward negative infinity (§4.12).
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Float` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_floor(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    let f = unsafe { float_payload(r) };
    unsafe { rebox_float(ctx, f.floor()) }
}

/// `Float.ceil()` — round toward positive infinity (§4.12).
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Float` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_ceil(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    let f = unsafe { float_payload(r) };
    unsafe { rebox_float(ctx, f.ceil()) }
}

/// `Float.round()` — round half away from zero (§4.12, matches Rust's `f64::round`).
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Float` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_round(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    let f = unsafe { float_payload(r) };
    unsafe { rebox_float(ctx, f.round()) }
}

/// `Float.sign()` — sign as -1.0 / 0.0 / 1.0 (§4.12). NaN yields NaN.
///
/// Not `f64::signum`: that returns `1.0` for `+0.0` and `-1.0` for `-0.0`,
/// because it reports the IEEE *sign bit*, not the sign of the value. Zero has
/// no sign in the sense `sign()` documents, so both zeros yield `0.0`.
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Float` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_sign(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    let f = unsafe { float_payload(r) };
    let sign = if f.is_nan() || f == 0.0 {
        // `f == 0.0` is true for both `+0.0` and `-0.0`; NaN falls through as
        // itself, which is what §4.12 specifies.
        f
    } else if f > 0.0 {
        1.0
    } else {
        -1.0
    };
    unsafe { rebox_float(ctx, sign) }
}

/// `Float.is_nan()` — true iff NaN (§4.12).
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Float` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_is_nan(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    let result = unsafe { float_payload(r) }.is_nan();
    // SAFETY: ctx valid; Bool immortal path.
    unsafe { bool_ref(ctx, result) }
}

/// `Float.is_infinite()` — true iff ±infinity (§4.12).
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Float` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_is_infinite(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    let result = unsafe { float_payload(r) }.is_infinite();
    // SAFETY: ctx valid; Bool immortal path.
    unsafe { bool_ref(ctx, result) }
}

/// `Float.min(other)` — the smaller of two floats (§4.12). Per IEEE-754 /
/// Rust's `f64::min`: if either operand is NaN, returns the other (NaN only
/// propagates when both are NaN). `-0.0` is less than `+0.0`.
///
/// # Safety
/// `ctx` must be live and wired; both operands must be valid `Float` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_min(
    ctx: *mut RuntimeContext,
    lhs: GcRef,
    rhs: GcRef,
) -> GcRef {
    let a = unsafe { float_payload(lhs) };
    let b = unsafe { float_payload(rhs) };
    unsafe { rebox_float(ctx, a.min(b)) }
}

/// `Float.max(other)` — the larger of two floats (§4.12). See `praxis_float_min`
/// for NaN handling.
///
/// # Safety
/// `ctx` must be live and wired; both operands must be valid `Float` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_max(
    ctx: *mut RuntimeContext,
    lhs: GcRef,
    rhs: GcRef,
) -> GcRef {
    let a = unsafe { float_payload(lhs) };
    let b = unsafe { float_payload(rhs) };
    unsafe { rebox_float(ctx, a.max(b)) }
}

/// `Float.to_text()` — the same text `out()` writes, which is the shortest form
/// that reads back as the same Praxis `Float` (§4.12, ADR-083).
///
/// It goes through `scalars::write_float` rather than restating the rule,
/// because `to_text()` and `out()` disagreeing is a defect in itself: a program
/// that prints a value and a program that builds a string from it must produce
/// the same characters.
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Float` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_to_text(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    let f = unsafe { float_payload(r) };
    let mut s = String::new();
    scalars::write_float(&mut s, f);
    // SAFETY: `s` is valid UTF-8 for the duration of the call; ctx/heap valid.
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::text::TEXT,
            std::mem::size_of::<crate::text::TextPayload>(),
            std::mem::align_of::<crate::text::TextPayload>(),
            |payload| {
                let owned: Box<str> = s.clone().into_boxed_str();
                (payload as *mut crate::text::TextPayload)
                    .write(crate::text::TextPayload::Owned(owned));
            },
        )
    }
}

/// `pi()` — the constant π as a `Float` (§4.12 prelude free function).
///
/// # Safety
/// `ctx` must be live and wired.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_pi(ctx: *mut RuntimeContext) -> GcRef {
    // SAFETY: ctx/heap valid.
    unsafe { gc_alloc(ctx, scalars::FLOAT_PAYLOAD, core::f64::consts::PI) }
}

/// `e()` — Euler's number as a `Float` (§4.12 prelude free function).
///
/// # Safety
/// `ctx` must be live and wired.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_e(ctx: *mut RuntimeContext) -> GcRef {
    // SAFETY: ctx/heap valid.
    unsafe { gc_alloc(ctx, scalars::FLOAT_PAYLOAD, core::f64::consts::E) }
}

// ---------------------------------------------------------------------------
// Checked arithmetic (§4.12). All fault rather than panic (§10.4).
// ---------------------------------------------------------------------------

macro_rules! checked_int_binop {
    ($name:ident, $op:tt, $fault:expr) => {
        #[doc = concat!("Checked `Int ", stringify!($op), "` (§4.12). On fault sets `pending_fault` and returns Unit.")]
        ///
        /// # Safety
        /// `ctx` must be live and wired; both operands must be valid `Int` `GcRef`s.
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            ctx: *mut RuntimeContext,
            lhs: GcRef,
            rhs: GcRef,
        ) -> GcRef {
            let a = unsafe { int_payload(lhs) };
            let b = unsafe { int_payload(rhs) };
            match a.$op(b) {
                Some(result) => unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, result) },
                None => {
                    unsafe { set_fault(ctx, $fault) };
                    unsafe { unit_sentinel(ctx) }
                }
            }
        }
    };
}

checked_int_binop!(praxis_int_add, checked_add, RaisedFault::INT_OVERFLOW);
checked_int_binop!(praxis_int_sub, checked_sub, RaisedFault::INT_OVERFLOW);
checked_int_binop!(praxis_int_mul, checked_mul, RaisedFault::INT_OVERFLOW);

/// Checked `Int` division (§4.12). Faults on division by zero, and on overflow
/// (`Int::MIN / -1`, the one signed-division case that overflows §4.12).
///
/// # Safety
/// `ctx` must be live and wired; both operands must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_div(ctx: *mut RuntimeContext, lhs: GcRef, rhs: GcRef) -> GcRef {
    let a = unsafe { int_payload(lhs) };
    let b = unsafe { int_payload(rhs) };
    if b == 0 {
        unsafe { set_fault(ctx, RaisedFault::DIV_BY_ZERO) };
        return unsafe { unit_sentinel(ctx) };
    }
    // `i64::MIN / -1` is the sole overflowing signed division: the mathematical
    // result (+2^63) is not representable, and the raw `/` panics on overflow in
    // debug builds (violating the no-panic-across-the-ABI rule, §10.4). Treat it
    // as checked-arithmetic overflow per §4.12.
    if a == i64::MIN && b == -1 {
        unsafe { set_fault(ctx, RaisedFault::INT_OVERFLOW) };
        return unsafe { unit_sentinel(ctx) };
    }
    // Division truncates toward zero (Rust's `i64::div_euclid` rounds differently;
    // Praxis follows C/Rust integer division semantics toward zero).
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, a / b) }
}

/// Checked `Int` remainder (§4.12). Faults on division by zero, and on overflow
/// (`Int::MIN % -1`, whose result is not representable under the §4.12 rule).
///
/// # Safety
/// `ctx` must be live and wired; both operands must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_rem(ctx: *mut RuntimeContext, lhs: GcRef, rhs: GcRef) -> GcRef {
    let a = unsafe { int_payload(lhs) };
    let b = unsafe { int_payload(rhs) };
    if b == 0 {
        unsafe { set_fault(ctx, RaisedFault::DIV_BY_ZERO) };
        return unsafe { unit_sentinel(ctx) };
    }
    // `i64::MIN % -1`: the remainder is 0 mathematically, but the raw `%` traps
    // on this exact case in debug builds because the corresponding quotient
    // overflows. Guard it for the same no-panic reason as `praxis_int_div`.
    if a == i64::MIN && b == -1 {
        unsafe { set_fault(ctx, RaisedFault::INT_OVERFLOW) };
        return unsafe { unit_sentinel(ctx) };
    }
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, a % b) }
}

/// Negate an `Int` (§4.12). Faults on overflow (`Int::MIN`).
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_neg(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    let a = unsafe { int_payload(r) };
    match a.checked_neg() {
        Some(result) => unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, result) },
        None => {
            unsafe { set_fault(ctx, RaisedFault::INT_OVERFLOW) };
            unsafe { unit_sentinel(ctx) }
        }
    }
}

// ---------------------------------------------------------------------------
// The §16.1 numeric prelude helpers: `abs`, `sign`, `min`, `max`, `clamp`,
// `gcd`, `lcm`.
//
// All seven are monomorphic on `Int` (ADR-058), so every payload read here is
// an `Int` payload and no descriptor check is needed. `min`/`max`/`clamp` hand
// back one of the references they were given rather than allocating a copy:
// an `Int` object is immutable, so sharing it is what "the smaller of the two"
// means. The four that compute a *new* number allocate one, and the three that
// can leave the `Int` range fault rather than wrapping — `abs(Int::MIN)` has no
// positive counterpart, and `gcd`/`lcm` reach the same edge through it.
// ---------------------------------------------------------------------------

/// `abs(n)` (§16.1). Faults on overflow: `Int::MIN` has no positive
/// counterpart, exactly as `praxis_int_neg` faults on it.
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_abs(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    let a = unsafe { int_payload(r) };
    match a.checked_abs() {
        Some(result) => unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, result) },
        None => {
            unsafe { set_fault(ctx, RaisedFault::INT_OVERFLOW) };
            unsafe { unit_sentinel(ctx) }
        }
    }
}

/// `sign(n)` (§16.1): `-1`, `0` or `1`. Total — every `Int`, `Int::MIN`
/// included, has a sign in range.
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_sign(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    let a = unsafe { int_payload(r) };
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, a.signum()) }
}

/// `min(a, b)` (§16.1): the smaller operand, returned as **the reference that
/// was passed in**. Allocates nothing.
///
/// # Safety
/// `ctx` must be live and wired; both operands must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_min(
    _ctx: *mut RuntimeContext,
    lhs: GcRef,
    rhs: GcRef,
) -> GcRef {
    let a = unsafe { int_payload(lhs) };
    let b = unsafe { int_payload(rhs) };
    if b < a {
        rhs
    } else {
        lhs
    }
}

/// `max(a, b)` (§16.1): the larger operand, returned as **the reference that
/// was passed in**. Allocates nothing.
///
/// # Safety
/// `ctx` must be live and wired; both operands must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_max(
    _ctx: *mut RuntimeContext,
    lhs: GcRef,
    rhs: GcRef,
) -> GcRef {
    let a = unsafe { int_payload(lhs) };
    let b = unsafe { int_payload(rhs) };
    if b > a {
        rhs
    } else {
        lhs
    }
}

/// `clamp(value, low, high)` (§16.1): `value` confined to the inclusive range
/// `low..=high`, returned as one of the three references passed in.
///
/// **Faults when `low > high`.** The range is empty, so there is no value to
/// return and no answer that is not a guess — clamping to an empty range is a
/// mistake in the program, not in the data, and TY-28 already settled that the
/// repair reports those rather than inventing a number. (Rust's `Ord::clamp`
/// panics on the same input; a panic across `extern "C"` is what §10.4 forbids,
/// so it is a fault.) The kind is `EmptyRange` — the kind ADR-058 recorded as
/// owed to whichever stage next spent an ABI bump, which S18 does.
///
/// # Safety
/// `ctx` must be live and wired; all three operands must be valid `Int`
/// `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_clamp(
    ctx: *mut RuntimeContext,
    value: GcRef,
    low: GcRef,
    high: GcRef,
) -> GcRef {
    let v = unsafe { int_payload(value) };
    let lo = unsafe { int_payload(low) };
    let hi = unsafe { int_payload(high) };
    if lo > hi {
        unsafe { set_fault(ctx, RaisedFault::EMPTY_RANGE) };
        return unsafe { unit_sentinel(ctx) };
    }
    if v < lo {
        low
    } else if v > hi {
        high
    } else {
        value
    }
}

/// The non-negative greatest common divisor of two `i64`s, computed by
/// Euclid's algorithm **in `i128`** so that `Int::MIN`'s absolute value needs no
/// special case. Returns `None` only when the mathematical result is outside the
/// `Int` range, which happens for exactly one input pair:
/// `gcd(Int::MIN, Int::MIN)` is `2^63`.
///
/// `gcd(0, 0)` is `0` — the conventional answer, and the identity `gcd(n, 0) ==
/// abs(n)` extended to `n == 0`.
fn checked_gcd(a: i64, b: i64) -> Option<i64> {
    let mut x = (a as i128).abs();
    let mut y = (b as i128).abs();
    while y != 0 {
        let t = x % y;
        x = y;
        y = t;
    }
    i64::try_from(x).ok()
}

/// `gcd(a, b)` (§16.1): the non-negative greatest common divisor. Faults on the
/// one pair whose result is out of range (`gcd(Int::MIN, Int::MIN)`).
///
/// # Safety
/// `ctx` must be live and wired; both operands must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_gcd(ctx: *mut RuntimeContext, lhs: GcRef, rhs: GcRef) -> GcRef {
    let a = unsafe { int_payload(lhs) };
    let b = unsafe { int_payload(rhs) };
    match checked_gcd(a, b) {
        Some(result) => unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, result) },
        None => {
            unsafe { set_fault(ctx, RaisedFault::INT_OVERFLOW) };
            unsafe { unit_sentinel(ctx) }
        }
    }
}

/// `lcm(a, b)` (§16.1): the non-negative least common multiple, `0` when either
/// operand is `0`. Faults when the result does not fit an `Int` — which it
/// often does not, since the product of two large operands overflows long
/// before their multiple does.
///
/// # Safety
/// `ctx` must be live and wired; both operands must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_lcm(ctx: *mut RuntimeContext, lhs: GcRef, rhs: GcRef) -> GcRef {
    let a = unsafe { int_payload(lhs) };
    let b = unsafe { int_payload(rhs) };
    // `lcm(n, 0)` is 0 for every n: 0 is a multiple of everything, and dividing
    // by the gcd below would divide by zero when both are 0.
    if a == 0 || b == 0 {
        return unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, 0i64) };
    }
    // |a / gcd * b| in i128, which cannot overflow: both operands fit i64, so
    // the product fits i128 with room to spare. The range check is the only
    // thing that can refuse.
    let result = checked_gcd(a, b)
        .map(|g| ((a as i128) / (g as i128) * (b as i128)).abs())
        .and_then(|m| i64::try_from(m).ok());
    match result {
        Some(result) => unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, result) },
        None => {
            unsafe { set_fault(ctx, RaisedFault::INT_OVERFLOW) };
            unsafe { unit_sentinel(ctx) }
        }
    }
}

// ---------------------------------------------------------------------------
// Comparisons (yield a Bool GcRef).
// ---------------------------------------------------------------------------

macro_rules! int_cmp {
    ($name:ident, $op:tt) => {
        #[doc = concat!(" `Int ", stringify!($op), "` comparison; returns a Bool GcRef (§4.12).")]
        ///
        /// # Safety
        /// `ctx` must be live and wired; both operands must be valid `Int` `GcRef`s.
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            ctx: *mut RuntimeContext,
            lhs: GcRef,
            rhs: GcRef,
        ) -> GcRef {
            let a = unsafe { int_payload(lhs) };
            let b = unsafe { int_payload(rhs) };
            let result = a $op b;
            // SAFETY: ctx/heap valid; Bool immortal path.
            unsafe { bool_ref(ctx, result) }
        }
    };
}

int_cmp!(praxis_int_eq, ==);
int_cmp!(praxis_int_ne, !=);
int_cmp!(praxis_int_lt, <);
int_cmp!(praxis_int_gt, >);
int_cmp!(praxis_int_le, <=);
int_cmp!(praxis_int_ge, >=);

// ---------------------------------------------------------------------------
// Fault check.
// ---------------------------------------------------------------------------

/// Return 1 if a fault is pending on `ctx`, else 0 (§10.4). Generated code
/// emits this after any faultable operation.
///
/// # Safety
/// `ctx` must point at a live `RuntimeContext` (a null/unwired context reports
/// no fault rather than panicking).
#[no_mangle]
pub unsafe extern "C" fn praxis_check_fault(ctx: *mut RuntimeContext) -> i64 {
    if ctx.is_null() {
        return 0;
    }
    if let Some(fault) = unsafe { (*ctx).pending_fault.as_ref() } {
        return fault.is_pending().into();
    }
    0
}

/// Raise a [`FaultKind::StackOverflow`] fault on `ctx` (§9.2, §17.4). Called by
/// the generated prologue guard when `recursion_depth` exceeds
/// [`MAX_RECURSION_DEPTH`], so the host survives deep recursion instead of
/// overflowing the native stack. The prologue then unwinds to its fault
/// epilogue (pop frame + return Unit) — same path as any other fault.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[no_mangle]
pub unsafe extern "C" fn praxis_raise_stack_overflow(ctx: *mut RuntimeContext) {
    unsafe { set_fault(ctx, RaisedFault::STACK_OVERFLOW) };
}

/// Raise a [`FaultKind::EmptyCollection`] fault on `ctx` (§9.2).
///
/// `reduce`, `min_by` and `max_by` have no answer for an empty sequence: they
/// seed their accumulator from the first element, and there is no first
/// element. The lowering used to hand back the accumulator anyway — a `Gc` slot
/// no instruction had ever written, so the caller received whatever the
/// register held, materialized as a `GcRef` that is `NonNull` by type and
/// arbitrary in fact (MIR-09). This is the defined failure instead; a fault
/// is what the other empty-collection accessors (`Deque.pop_front`, heap
/// `pop`/`peek`) already raise for the same reason.
///
/// Unconditional, unlike the two arithmetic raise wrappers: the emptiness test
/// is a branch generated code has to make anyway (the seen-flag gates the whole
/// sink), so there is no predicate worth passing. It returns the Unit sentinel
/// rather than nothing, so the MIR `Call` that emits it has an ordinary `Gc`
/// destination — a `Void` row would put the context pointer in a rootable slot.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[no_mangle]
pub unsafe extern "C" fn praxis_raise_empty_collection(ctx: *mut RuntimeContext) -> GcRef {
    unsafe { set_fault(ctx, RaisedFault::EMPTY_COLLECTION) };
    unsafe { unit_sentinel(ctx) }
}

/// Raise a [`FaultKind::IntOverflow`] fault on `ctx` iff `condition` is
/// non-zero (§4.12).
///
/// Generated code lowers `Int` arithmetic natively — `iadd`/`isub`/`imul` on
/// the raw scalar channel — and computes the overflow predicate inline. This is
/// how it reports one: the caller passes the predicate, and the wrapper decides
/// nothing else. It allocates nothing, so an arithmetic site is not a
/// safepoint, and taking the condition rather than branching around the call
/// keeps arithmetic to a single basic block.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[no_mangle]
pub unsafe extern "C" fn praxis_raise_int_overflow_if(ctx: *mut RuntimeContext, condition: i64) {
    if condition != 0 {
        unsafe { set_fault(ctx, RaisedFault::INT_OVERFLOW) };
    }
}

/// Raise a [`FaultKind::DivByZero`] fault on `ctx` iff `condition` is non-zero
/// (§4.12). The division counterpart of [`praxis_raise_int_overflow_if`].
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[no_mangle]
pub unsafe extern "C" fn praxis_raise_div_by_zero_if(ctx: *mut RuntimeContext, condition: i64) {
    if condition != 0 {
        unsafe { set_fault(ctx, RaisedFault::DIV_BY_ZERO) };
    }
}

// ---------------------------------------------------------------------------
// Vec[T] collection methods (§11.1, §11.2, §11.5, M5).
//
// `VecPayload` stores a `Box<[GcRef]>` (immutable length). `push` therefore
// *reallocates*: it copies the existing elements into a grown `Vec`, appends
// the new value, and returns a fresh `GcRef` to a new `VecPayload`. The old
// vec is left to the collector. Per §11.5 reallocation safety, we never retain
// an interior pointer into the Rust vector across a capacity-mutating op; the
// new payload is built in full before the box is sealed.
// ---------------------------------------------------------------------------

/// Read the `VecPayload` out of a `GcRef` as a shared ref, asserting it is a Vec.
///
/// # Safety
/// `r` must be a valid `Vec` `GcRef`.
unsafe fn vec_payload(r: GcRef) -> &'static VecPayload {
    // SAFETY: caller guarantees `r` is a Vec; the non-moving GC (ADR-011) keeps
    // the payload address stable for the object's lifetime. The `'static` is
    // unbounded because the raw FFI boundary has no lifetime to carry; the
    // caller (a wrapper that holds `ctx`) ensures the object outlives the use.
    unsafe { &*r.payload::<VecPayload>() }
}

/// Read the `VecPayload` out of a `GcRef` as a mutable ref, asserting it is a
/// Vec. Used by `push` to mutate the vector in place (§11.1).
///
/// # Safety
/// `r` must be a valid `Vec` `GcRef`, rooted for `'s`.
unsafe fn vec_payload_mut<'s>(r: Rooted<'s>) -> &'s mut VecPayload {
    // SAFETY: caller guarantees `r` is a Vec; the non-moving GC (ADR-011) keeps
    // the payload address stable for the object's lifetime, and `Rooted` proves
    // the object is in the collector's root set for `'s`, so a collection
    // triggered while this reference is held cannot reclaim what it points at.
    unsafe { &mut *r.get().payload::<VecPayload>() }
}

/// Build a `Vec[T]` holding `items`, with `element_descriptor` as its element
/// type — the shape every wrapper that answers with a collection needs.
///
/// The `Vec` is rooted across the pushes, which is the part worth having in one
/// place: `praxis_vec_new` allocates, and so may the caller's own iteration, so a
/// collection between the allocation and the last push would reclaim it.
///
/// `element_descriptor` may be **null**: the source collection's label is what
/// its own construction site knew, and that may have been nothing (REP-41). A
/// `Vec`'s null means "empty" — `vec_format` reads it that way — so a null label
/// with members present would answer `[]`. The first member's own descriptor is
/// what the `Vec` adopts instead, which is exactly what `praxis_vec_push` does.
///
/// # Safety
/// `ctx` must be live and wired; `element_descriptor` must be a valid
/// `'static TypeDescriptor` or null; every item must be a valid `GcRef` whose
/// payload matches its own header.
unsafe fn vec_of(
    ctx: *mut RuntimeContext,
    element_descriptor: *const TypeDescriptor,
    items: impl Iterator<Item = GcRef>,
) -> GcRef {
    let items: Vec<GcRef> = items.collect();
    let element_descriptor = if element_descriptor.is_null() {
        items
            .first()
            .map_or(std::ptr::null(), |first| first.descriptor() as *const _)
    } else {
        element_descriptor
    };
    let result = unsafe { praxis_vec_new(ctx, element_descriptor) };
    let scope = unsafe { NativeScope::new(ctx) };
    let rp = unsafe { vec_payload_mut(scope.root(result)) };
    rp.items.extend(items);
    result
}

/// Allocate a new empty `Vec[T]` with the given element descriptor (§11.2).
/// Returns a `GcRef` to a zero-length vector.
///
/// # Safety
/// `ctx` must be live and wired. `element_descriptor` must be a valid pointer to
/// a `'static TypeDescriptor`.
#[no_mangle]
pub unsafe extern "C" fn praxis_vec_new(
    ctx: *mut RuntimeContext,
    element_descriptor: *const TypeDescriptor,
) -> GcRef {
    // A null descriptor is kept null: it means "the caller has no static
    // element type", which is a thing this payload can hold (P0-11). Spelling
    // it `INT` is what made an empty `Vec[Float]` claim to hold `Int`s.
    // SAFETY: VecPayload matches VEC's size/align and is fully initialized.
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::collections::VEC,
            std::mem::size_of::<VecPayload>(),
            std::mem::align_of::<VecPayload>(),
            |payload| {
                (payload as *mut VecPayload).write(VecPayload {
                    element_descriptor,
                    items: Vec::new(),
                });
            },
        )
    }
}

/// Allocate a nominal record (M7, §4.5) with all fields initialized to Unit.
/// The `schema_ptr` points at a `'static RecordSchema` (built and leaked by the
/// codegen from the record def). Fields are filled in declaration order via
/// [`praxis_record_set_field`] after allocation. Returns the record `GcRef`.
///
/// # Safety
/// `ctx` must be live and wired; `schema_ptr` must be a valid `'static` pointer.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_record(
    ctx: *mut RuntimeContext,
    schema_ptr: *const crate::records::RecordSchema,
) -> GcRef {
    if schema_ptr.is_null() {
        return unit_sentinel(ctx);
    }
    // SAFETY: caller guarantees schema_ptr is a valid 'static pointer.
    let schema = unsafe { &*schema_ptr };
    let arity = schema.fields.len();
    let unit = unit_sentinel(ctx);
    // SAFETY: RecordPayload matches RECORD's size/align and is fully initialized.
    // Every field slot starts as Unit (a valid GcRef), keeping the GC sound
    // before the caller fills them in via praxis_record_set_field.
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::records::RECORD,
            std::mem::size_of::<crate::records::RecordPayload>(),
            std::mem::align_of::<crate::records::RecordPayload>(),
            |payload| {
                (payload as *mut crate::records::RecordPayload).write(
                    crate::records::RecordPayload {
                        schema: schema_ptr,
                        items: vec![unit; arity],
                    },
                );
            },
        )
    }
}

/// Set field `idx` of `record` to `value` (M7, §4.5). Used by the codegen to
/// fill in fields after [`praxis_alloc_record`]. Returns the record (the
/// receiver is mutated in place).
///
/// # Safety
/// `ctx` must be live; `record` must be a valid record `GcRef`; `idx` must be
/// in bounds.
#[no_mangle]
pub unsafe extern "C" fn praxis_record_set_field(
    ctx: *mut RuntimeContext,
    record: GcRef,
    idx: u32,
    value: GcRef,
) -> GcRef {
    let _ = ctx;
    // SAFETY: caller guarantees record is a valid record GcRef.
    let payload = record.payload::<u8>() as *mut crate::records::RecordPayload;
    // SAFETY: the payload is a RecordPayload for any RECORD-descriptor object.
    let rp = unsafe { &mut *payload };
    if let Some(slot) = rp.items.get_mut(idx as usize) {
        *slot = value;
    }
    record
}

/// Read field `idx` out of a record `GcRef` (M7, §4.5). Returns the field's
/// `GcRef` value. Returns Unit if the record is malformed or the index is out
/// of bounds (defensive; the type checker prevents this in well-typed code).
///
/// # Safety
/// `ctx` must be live; `record` must be a valid record `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_record_field(
    ctx: *mut RuntimeContext,
    record: GcRef,
    idx: u32,
) -> GcRef {
    // SAFETY: caller guarantees record is a valid record GcRef; the payload is
    // a RecordPayload for any RECORD-descriptor object.
    let payload = record.payload::<u8>() as *const crate::records::RecordPayload;
    let rp = &*payload;
    rp.items
        .get(idx as usize)
        .copied()
        .unwrap_or_else(|| unsafe { unit_sentinel(ctx) })
}

/// Allocate an enum value (M7, §4.6) of the type `schema_ptr` describes, with
/// variant `tag` and every payload slot initialized to Unit. Payload values are
/// filled via [`praxis_enum_set_payload`] after allocation. Returns the enum
/// `GcRef`.
///
/// The arity is **read from the schema** rather than passed alongside it, as
/// [`praxis_alloc_tuple`] already does: a schema and an arity that disagree is
/// a state no caller can now reach. A null schema, or a tag the schema has no
/// variant for, allocates nothing and answers the Unit sentinel — the same
/// answer `praxis_alloc_tuple` gives a null schema.
///
/// # Safety
/// `ctx` must be live and wired; `schema_ptr` must be null or a valid
/// `'static` pointer.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_enum(
    ctx: *mut RuntimeContext,
    schema_ptr: *const crate::enums::EnumSchema,
    tag: i64,
) -> GcRef {
    if schema_ptr.is_null() || tag < 0 {
        return unsafe { unit_sentinel(ctx) };
    }
    // SAFETY: caller guarantees schema_ptr is a valid 'static pointer.
    let schema = unsafe { &*schema_ptr };
    if schema.variant_at(tag as usize).is_none() {
        return unsafe { unit_sentinel(ctx) };
    }
    let arity = schema.arity_of(tag as usize);
    let unit = unsafe { unit_sentinel(ctx) };
    let items = vec![unit; arity];
    // SAFETY: EnumPayload matches ENUM's size/align and is fully initialized.
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::enums::ENUM,
            std::mem::size_of::<crate::enums::EnumPayload>(),
            std::mem::align_of::<crate::enums::EnumPayload>(),
            |payload| {
                (payload as *mut crate::enums::EnumPayload).write(crate::enums::EnumPayload {
                    schema: schema_ptr,
                    tag: tag as u32,
                    items,
                });
            },
        )
    }
}

/// Allocate `Some(value)` under the runtime's own [`option_schema`].
///
/// `value` is rooted across the enum allocation: the allocation is a safepoint,
/// and a bare `GcRef` argument is not in anyone's root set.
///
/// [`option_schema`]: crate::enums::option_schema
///
/// # Safety
/// `ctx` must be live and wired; `value` must be a valid `GcRef`.
pub(crate) unsafe fn option_some(ctx: *mut RuntimeContext, value: GcRef) -> GcRef {
    // SAFETY: the caller upholds ctx/value validity.
    unsafe {
        let scope = NativeScope::new(ctx);
        let rooted = scope.root(value);
        let some = praxis_alloc_enum(
            ctx,
            crate::enums::option_schema(),
            crate::enums::OPTION_SOME_TAG,
        );
        praxis_enum_set_payload(ctx, some, 0, rooted.get());
        some
    }
}

/// Allocate `None` under the runtime's own `option_schema`.
///
/// # Safety
/// `ctx` must be live and wired.
pub(crate) unsafe fn option_none(ctx: *mut RuntimeContext) -> GcRef {
    // SAFETY: the caller upholds ctx validity.
    unsafe {
        praxis_alloc_enum(
            ctx,
            crate::enums::option_schema(),
            crate::enums::OPTION_NONE_TAG,
        )
    }
}

/// Set payload slot `idx` of `enum_value` to `value` (M7, §4.6). Returns the
/// enum value (mutated in place).
///
/// # Safety
/// `ctx` must be live; `enum_value` must be a valid enum `GcRef`; `idx` in bounds.
#[no_mangle]
pub unsafe extern "C" fn praxis_enum_set_payload(
    ctx: *mut RuntimeContext,
    enum_value: GcRef,
    idx: i64,
    value: GcRef,
) -> GcRef {
    let _ = ctx;
    // SAFETY: caller guarantees enum_value is a valid enum GcRef.
    let payload = enum_value.payload::<u8>() as *mut crate::enums::EnumPayload;
    let ep = unsafe { &mut *payload };
    if let Some(slot) = ep.items.get_mut(idx as usize) {
        *slot = value;
    }
    enum_value
}

/// Read the variant tag (discriminant) of an enum value (M7, §4.6). Returns the
/// tag as a boxed `Int` `GcRef` (the uniform ABI convention), so the `match`
/// lowering can extract the scalar and compare. Used by `match` to branch.
///
/// # Safety
/// `ctx` must be live; `enum_value` must be a valid enum `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_enum_tag(ctx: *mut RuntimeContext, enum_value: GcRef) -> GcRef {
    // SAFETY: caller guarantees enum_value is a valid enum GcRef.
    // Read the tag BEFORE allocating — the alloc below may trigger GC, and
    // enum_value is not explicitly rooted (it's only in a Cranelift local).
    let payload = enum_value.payload::<u8>() as *const crate::enums::EnumPayload;
    let tag = unsafe { (*payload).tag as i64 };
    // SAFETY: alloc boxes the i64 into a fresh Int object. The tag value is
    // already in a register, so GC collecting enum_value here is safe.
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, tag) }
}

/// Read payload slot `idx` of an enum value (M7, §4.6). Returns the slot's
/// `GcRef`. Used by `match` to bind variant payload variables.
///
/// # Safety
/// `ctx` must be live; `enum_value` must be a valid enum `GcRef`; `idx` in bounds.
#[no_mangle]
pub unsafe extern "C" fn praxis_enum_payload(
    ctx: *mut RuntimeContext,
    enum_value: GcRef,
    idx: i64,
) -> GcRef {
    // SAFETY: caller guarantees enum_value is a valid enum GcRef.
    let payload = enum_value.payload::<u8>() as *const crate::enums::EnumPayload;
    let ep = unsafe { &*payload };
    ep.items
        .get(idx as usize)
        .copied()
        .unwrap_or_else(|| unsafe { unit_sentinel(ctx) })
}

/// Allocate a tuple (M7, §4.5 structural tuples) with all element slots
/// initialized to Unit. The `schema_ptr` points at a `'static TupleSchema`
/// (built and leaked by the codegen from the tuple's element-type sequence).
/// Elements are filled in positional order via [`praxis_tuple_set`] after
/// allocation. Returns the tuple `GcRef`.
///
/// # Safety
/// `ctx` must be live and wired; `schema_ptr` must be a valid `'static` pointer.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_tuple(
    ctx: *mut RuntimeContext,
    schema_ptr: *const crate::tuples::TupleSchema,
) -> GcRef {
    if schema_ptr.is_null() {
        return unit_sentinel(ctx);
    }
    // SAFETY: caller guarantees schema_ptr is a valid 'static pointer.
    let schema = unsafe { &*schema_ptr };
    let arity = schema.descriptors.len();
    let unit = unit_sentinel(ctx);
    // SAFETY: TuplePayload matches TUPLE's size/align and is fully initialized.
    // Every element slot starts as Unit (a valid GcRef), keeping the GC sound
    // before the caller fills them in via praxis_tuple_set.
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::tuples::TUPLE,
            std::mem::size_of::<crate::tuples::TuplePayload>(),
            std::mem::align_of::<crate::tuples::TuplePayload>(),
            |payload| {
                (payload as *mut crate::tuples::TuplePayload).write(crate::tuples::TuplePayload {
                    schema: schema_ptr,
                    items: vec![unit; arity],
                });
            },
        )
    }
}

/// Set element `idx` of `tuple` to `value` (M7, §4.5). Used by the codegen to
/// fill in elements after [`praxis_alloc_tuple`]. Returns the tuple (the
/// receiver is mutated in place).
///
/// # Safety
/// `ctx` must be live; `tuple` must be a valid tuple `GcRef`; `idx` in bounds.
#[no_mangle]
pub unsafe extern "C" fn praxis_tuple_set(
    ctx: *mut RuntimeContext,
    tuple: GcRef,
    idx: i64,
    value: GcRef,
) -> GcRef {
    let _ = ctx;
    // SAFETY: caller guarantees tuple is a valid tuple GcRef.
    let payload = tuple.payload::<u8>() as *mut crate::tuples::TuplePayload;
    // SAFETY: the payload is a TuplePayload for any TUPLE-descriptor object.
    let tp = unsafe { &mut *payload };
    if let Some(slot) = tp.items.get_mut(idx as usize) {
        *slot = value;
    }
    tuple
}

/// Read element `idx` out of a tuple `GcRef` (M7, §4.5). Returns the element's
/// `GcRef` value. Returns Unit if the tuple is malformed or the index is out of
/// bounds (defensive; the type checker prevents this in well-typed code).
///
/// # Safety
/// `ctx` must be live; `tuple` must be a valid tuple `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_tuple_get(
    ctx: *mut RuntimeContext,
    tuple: GcRef,
    idx: i64,
) -> GcRef {
    // SAFETY: caller guarantees tuple is a valid tuple GcRef; the payload is a
    // TuplePayload for any TUPLE-descriptor object.
    let payload = tuple.payload::<u8>() as *const crate::tuples::TuplePayload;
    let tp = unsafe { &*payload };
    tp.items
        .get(idx as usize)
        .copied()
        .unwrap_or_else(|| unsafe { unit_sentinel(ctx) })
}

/// Structural equality between two GC values (§5.5, M7). Reads the descriptor
/// from `a` and dispatches to its `equals` callback, which recurses element/field
/// wise for composite types (records, tuples, enums, collections). Returns 1 for
/// equal, 0 for not equal. Returns 0 if `a`'s type is not equatable (functions
/// are never equatable, §5.5) — the type checker rejects this in well-typed code,
/// so this is defensive.
///
/// # Safety
/// `ctx` must be live and wired; `a` and `b` must be valid `GcRef`s of the same
/// type (the caller has already unified their types at compile time).
#[no_mangle]
pub unsafe extern "C" fn praxis_struct_eq(ctx: *mut RuntimeContext, a: GcRef, b: GcRef) -> i64 {
    let _ = ctx;
    // SAFETY: caller guarantees a is a valid GcRef; the descriptor header is
    // always present and its `equals` (if Some) is safe to call with a/b.
    let desc = a.descriptor();
    // Both operands must be the same runtime type before any callback runs
    // (ADR-045 decision 3). Well-typed code has unified them, so this is the
    // miscompile case — and a callback dispatched on a foreign layout is how a
    // type confusion becomes a wild read rather than a wrong answer.
    if !std::ptr::eq(desc, b.descriptor()) {
        return 0;
    }
    match desc.equals {
        // SAFETY: both a and b are values of desc's type (caller has type-checked
        // them equal); the equals callback is safe under that invariant.
        Some(eq) => {
            let pa = a.payload::<u8>() as *const u8;
            let pb = b.payload::<u8>() as *const u8;
            if unsafe { eq(pa, pb) } {
                1
            } else {
                0
            }
        }
        // Not equatable: treat as not-equal. The type checker rejects this in
        // well-typed code; the defensive default keeps runtime sound.
        None => 0,
    }
}

/// Order two GC values through their descriptor's `compare` callback (ADR-045).
/// Returns `-1`, `0` or `1` — the caller turns that into the `<`/`<=`/`>`/`>=`
/// it wanted by comparing against zero.
///
/// This is the ordering counterpart of [`praxis_struct_eq`], and it exists for
/// the same reason: a `Text` is a pointer-and-length structure, so ordering one
/// by loading its first eight payload bytes compared *addresses* (P0-12).
///
/// Raises `FaultKind::TypeMismatch` and answers `0` when the two operands are
/// not the same runtime type, or when the type has no ordering. The type
/// checker rejects both in well-typed code (`Y006`), so reaching either is a
/// compiler bug — reported as a fault rather than a callback dispatched on a
/// foreign layout.
///
/// # Safety
/// `ctx` must be live and wired; `a` and `b` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_value_cmp(ctx: *mut RuntimeContext, a: GcRef, b: GcRef) -> i64 {
    // SAFETY: caller guarantees both are valid GcRefs; every object carries a
    // descriptor in its header.
    let desc = a.descriptor();
    if !std::ptr::eq(desc, b.descriptor()) {
        unsafe { set_fault(ctx, RaisedFault::TYPE_MISMATCH) };
        return 0;
    }
    let Some(compare) = desc.compare else {
        unsafe { set_fault(ctx, RaisedFault::TYPE_MISMATCH) };
        return 0;
    };
    // SAFETY: both values carry `desc` (checked above), so both payloads are
    // values of its type.
    let ordering = unsafe {
        compare(
            a.payload::<u8>() as *const u8,
            b.payload::<u8>() as *const u8,
        )
    };
    match ordering {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// Allocate a closure value (M7, §4.10) with `fn_ptr` as its entry point and
/// `n_captures` environment slots initialized to Unit. Captures are filled via
/// [`praxis_closure_set_capture`] after allocation. Returns the closure `GcRef`.
///
/// # Safety
/// `ctx` must be live and wired; `fn_ptr` must be a valid JIT'd function pointer
/// whose calling convention matches `fn(ctx, params..., env...) -> i64`.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_closure(
    ctx: *mut RuntimeContext,
    fn_ptr: *const u8,
    n_captures: i64,
) -> GcRef {
    let unit = unit_sentinel(ctx);
    let env = vec![unit; n_captures as usize];
    // SAFETY: ClosurePayload matches CLOSURE's size/align and is fully initialized.
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::closures::CLOSURE,
            std::mem::size_of::<crate::closures::ClosurePayload>(),
            std::mem::align_of::<crate::closures::ClosurePayload>(),
            |payload| {
                (payload as *mut crate::closures::ClosurePayload)
                    .write(crate::closures::ClosurePayload { fn_ptr, env });
            },
        )
    }
}

/// Set capture slot `idx` of `closure` to `value` (M7, §4.10). Returns the
/// closure (mutated in place).
///
/// # Safety
/// `ctx` must be live; `closure` must be a valid closure `GcRef`; `idx` in bounds.
#[no_mangle]
pub unsafe extern "C" fn praxis_closure_set_capture(
    ctx: *mut RuntimeContext,
    closure: GcRef,
    idx: i64,
    value: GcRef,
) -> GcRef {
    let _ = ctx;
    // SAFETY: caller guarantees closure is a valid closure GcRef.
    let payload = closure.payload::<u8>() as *mut crate::closures::ClosurePayload;
    let cp = unsafe { &mut *payload };
    if let Some(slot) = cp.env.get_mut(idx as usize) {
        *slot = value;
    }
    closure
}

/// Read the function pointer out of a closure `GcRef` (M7, §4.10). Used by the
/// indirect-call lowering to obtain the entry point before a native call.
///
/// `ctx` is accepted (and unused) for ABI uniformity with every other `praxis_*`
/// wrapper — generated code calls all wrappers as `fn(ctx, args...)`, so this
/// keeps the calling convention consistent. The returned `*const u8` is carried
/// as an `i64` (pointer-sized) back into the JIT'd code.
///
/// # Safety
/// `ctx` must be live; `closure` must be a valid closure `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_closure_fn_ptr(
    ctx: *mut RuntimeContext,
    closure: GcRef,
) -> *const u8 {
    let _ = ctx;
    // SAFETY: caller guarantees closure is a valid closure GcRef.
    let payload = closure.payload::<u8>() as *const crate::closures::ClosurePayload;
    unsafe { (*payload).fn_ptr }
}

/// Read capture slot `idx` out of a closure `GcRef` (M7, §4.10). Used by the
/// closure's synthetic function to load its captured values from the env.
///
/// # Safety
/// `ctx` must be live; `closure` must be a valid closure `GcRef`; `idx` in bounds.
#[no_mangle]
pub unsafe extern "C" fn praxis_closure_capture(
    ctx: *mut RuntimeContext,
    closure: GcRef,
    idx: i64,
) -> GcRef {
    // SAFETY: caller guarantees closure is a valid closure GcRef.
    let payload = closure.payload::<u8>() as *const crate::closures::ClosurePayload;
    let cp = unsafe { &*payload };
    cp.env
        .get(idx as usize)
        .copied()
        .unwrap_or_else(|| unsafe { unit_sentinel(ctx) })
}

/// Allocate a `VarCell` holding `value` (M7-WS7b, §4.10). The cell is the shared
/// mutable storage for a captured `var` binding: the binding site and every
/// closure that captures the `var` refer to the same cell. Returns the cell
/// `GcRef`.
///
/// # Safety
/// `ctx` must be live and wired; `value` must be a valid `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_var_cell(ctx: *mut RuntimeContext, value: GcRef) -> GcRef {
    // SAFETY: VarCellPayload matches VAR_CELL's size/align and is fully initialized.
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::var_cell::VAR_CELL,
            std::mem::size_of::<crate::var_cell::VarCellPayload>(),
            std::mem::align_of::<crate::var_cell::VarCellPayload>(),
            |payload| {
                (payload as *mut crate::var_cell::VarCellPayload)
                    .write(crate::var_cell::VarCellPayload { value });
            },
        )
    }
}

/// Read the current value out of a `VarCell` (M7-WS7b, §4.10). Used by `Path`
/// reads of a captured `var` (the local holds the cell; this derefs it).
///
/// # Safety
/// `ctx` must be live; `cell` must be a valid `VarCell` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_var_cell_get(ctx: *mut RuntimeContext, cell: GcRef) -> GcRef {
    let _ = ctx;
    // SAFETY: caller guarantees cell is a valid VarCell GcRef.
    let payload = cell.payload::<u8>() as *const crate::var_cell::VarCellPayload;
    unsafe { (*payload).value }
}

/// Store `value` into a `VarCell` (M7-WS7b, §4.10). Used by `Assign` to a
/// captured `var`. Returns the cell (mutated in place).
///
/// # Safety
/// `ctx` must be live; `cell` must be a valid `VarCell` `GcRef`; `value` valid.
#[no_mangle]
pub unsafe extern "C" fn praxis_var_cell_set(
    ctx: *mut RuntimeContext,
    cell: GcRef,
    value: GcRef,
) -> GcRef {
    let _ = ctx;
    // SAFETY: caller guarantees cell is a valid VarCell GcRef.
    let payload = cell.payload::<u8>() as *mut crate::var_cell::VarCellPayload;
    unsafe {
        (*payload).value = value;
    }
    cell
}

/// Append `value` to `vec` in place (§11.1). Returns the Unit sentinel — the
/// receiver is mutated directly, so the caller's `GcRef` remains valid (the
/// `VecPayload` object does not move; only its internal buffer may grow).
///
/// # Safety
/// `ctx` must be live and wired; `vec` must be a valid `Vec` `GcRef`; `value`
/// must be a valid `GcRef` whose type matches the vector's element descriptor.
#[no_mangle]
pub unsafe extern "C" fn praxis_vec_push(
    ctx: *mut RuntimeContext,
    vec: GcRef,
    value: GcRef,
) -> GcRef {
    // `push` may grow the Vec's backing buffer, which allocates Rust heap memory
    // (not GC memory). A GC collection during this would be safe (the vec
    // object is rooted by the caller's spilled `vec` local), but we trigger it
    // *before* the mutation to keep the rooting story simple: `value` is passed
    // by value and is not yet in the vec, so it must survive across this
    // collection via the caller's shadow frame.
    unsafe { maybe_collect(ctx) };
    // SAFETY: caller guarantees `vec` is a valid Vec.
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { vec_payload_mut(scope.root(vec)) };
    // A vector that was never told its element type adopts the first pushed
    // value's — the `forall T. () -> Vec[T]` builtin leaves `T` generalized
    // until first use, so construction genuinely has nothing to record. A
    // vector that *was* told rejects a mismatch instead of retagging itself:
    // retagging turned an explicitly typed `Vec[Int]` into a `Vec[Float]` on
    // one bad push, and every later `equals`/`hash`/`format` then read the
    // remaining `Int` payloads as `f64` (P0-11).
    if !unsafe { adopt_or_reject(ctx, &mut p.element_descriptor, value) } {
        return unsafe { unit_sentinel(ctx) };
    }
    p.items.push(value);
    unsafe { unit_sentinel(ctx) }
}

/// Reconcile a collection's element descriptor with a value about to be stored
/// in it: adopt the value's descriptor if the collection has none, accept if
/// they agree, and raise `TypeMismatch` if they do not.
///
/// Returns whether the store may proceed. Descriptors are `static`, so pointer
/// identity is the authoritative test (ADR-038).
///
/// # Safety
/// `ctx` must be live and wired; `value` must be a valid `GcRef`.
unsafe fn adopt_or_reject(
    ctx: *mut RuntimeContext,
    element_descriptor: &mut *const TypeDescriptor,
    value: GcRef,
) -> bool {
    let pushed = value.descriptor();
    if element_descriptor.is_null() {
        *element_descriptor = pushed;
        return true;
    }
    if std::ptr::eq(*element_descriptor, pushed) {
        return true;
    }
    unsafe { set_fault(ctx, RaisedFault::TYPE_MISMATCH) };
    false
}

/// The number of elements in `vec`, as a boxed `Int` (§11.1).
///
/// # Safety
/// `ctx` must be live and wired; `vec` must be a valid `Vec` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_vec_len(ctx: *mut RuntimeContext, vec: GcRef) -> GcRef {
    // SAFETY: caller guarantees `vec` is a valid Vec.
    let p = unsafe { vec_payload(vec) };
    let len = p.items.len() as i64;
    // len allocates the returned Int, but the input vec is still live via `vec`.
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, len) }
}

/// The element at `index`, or an `IndexOutOfBounds` fault if out of range
/// (§9.2, §11.1). Returns the Unit sentinel on fault.
///
/// # Safety
/// `ctx` must be live and wired; `vec` must be a valid `Vec` `GcRef`; `index`
/// must be a valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_vec_get(
    ctx: *mut RuntimeContext,
    vec: GcRef,
    index: GcRef,
) -> GcRef {
    // SAFETY: caller guarantees `vec` is a valid Vec.
    let p = unsafe { vec_payload(vec) };
    // SAFETY: caller guarantees `index` is a valid Int.
    let idx = unsafe { int_payload(index) };
    if idx < 0 || idx as usize >= p.items.len() {
        unsafe { set_fault(ctx, RaisedFault::INDEX_OUT_OF_BOUNDS) };
        return unsafe { unit_sentinel(ctx) };
    }
    // Return the element by value (a copy of the GcRef). No allocation, so no
    // collection is needed; the vec stays live via `vec`.
    p.items[idx as usize]
}

/// True iff `vec` has no elements, as a boxed `Bool` (§11.1).
///
/// # Safety
/// `ctx` must be live and wired; `vec` must be a valid `Vec` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_vec_is_empty(ctx: *mut RuntimeContext, vec: GcRef) -> GcRef {
    // SAFETY: caller guarantees `vec` is a valid Vec.
    let p = unsafe { vec_payload(vec) };
    let empty = p.items.is_empty();
    // SAFETY: ctx/heap valid; Bool immortal path.
    unsafe { bool_ref(ctx, empty) }
}

// ---------------------------------------------------------------------------
// Deque[T] methods (M8-WS2, §6.1). Mirrors the Vec surface but adds the
// front/back distinction: `push_front`/`push_back`/`pop_front`/`pop_back`.
// `pop_*` fault on an empty deque (§9.1 `EmptyCollection`).
// ---------------------------------------------------------------------------

use crate::collections::DequePayload;

/// Read the `DequePayload` out of a `GcRef` as a shared ref, asserting Deque.
///
/// # Safety
/// `r` must be a valid `Deque` `GcRef`.
unsafe fn deque_payload(r: GcRef) -> &'static DequePayload {
    // SAFETY: caller guarantees `r` is a Deque; non-moving GC keeps it stable.
    unsafe { &*r.payload::<DequePayload>() }
}

/// Read the `DequePayload` out of a `GcRef` as a mutable ref, asserting Deque.
///
/// # Safety
/// `r` must be a valid `Deque` `GcRef`, rooted for `'s`.
unsafe fn deque_payload_mut<'s>(r: Rooted<'s>) -> &'s mut DequePayload {
    // SAFETY: caller guarantees `r` is a Deque; non-moving GC keeps it stable.
    unsafe { &mut *r.get().payload::<DequePayload>() }
}

/// Allocate a new empty `Deque[T]` with the given element descriptor (§11.2).
/// A null descriptor stays null — "not told yet" — exactly as `praxis_vec_new`.
///
/// # Safety
/// `ctx` must be live and wired. `element_descriptor` must be a valid pointer to
/// a `'static TypeDescriptor` (or null).
#[no_mangle]
pub unsafe extern "C" fn praxis_deque_new(
    ctx: *mut RuntimeContext,
    element_descriptor: *const TypeDescriptor,
) -> GcRef {
    // SAFETY: DequePayload matches DEQUE's size/align and is fully initialized.
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::collections::DEQUE,
            std::mem::size_of::<DequePayload>(),
            std::mem::align_of::<DequePayload>(),
            |payload| {
                (payload as *mut DequePayload).write(DequePayload {
                    element_descriptor,
                    items: std::collections::VecDeque::new(),
                });
            },
        )
    }
}

/// Prepend `value` to the front of `deque`; returns Unit (§6.1).
///
/// # Safety
/// `ctx` must be live and wired; `deque` must be a valid `Deque` `GcRef`;
/// `value` must be a valid `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_deque_push_front(
    ctx: *mut RuntimeContext,
    deque: GcRef,
    value: GcRef,
) -> GcRef {
    unsafe { maybe_collect(ctx) };
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { deque_payload_mut(scope.root(deque)) };
    if !unsafe { adopt_or_reject(ctx, &mut p.element_descriptor, value) } {
        return unsafe { unit_sentinel(ctx) };
    }
    p.items.push_front(value);
    unsafe { unit_sentinel(ctx) }
}

/// Append `value` to the back of `deque`; returns Unit (§6.1).
///
/// # Safety
/// `ctx` must be live and wired; `deque` must be a valid `Deque` `GcRef`;
/// `value` must be a valid `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_deque_push_back(
    ctx: *mut RuntimeContext,
    deque: GcRef,
    value: GcRef,
) -> GcRef {
    unsafe { maybe_collect(ctx) };
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { deque_payload_mut(scope.root(deque)) };
    if !unsafe { adopt_or_reject(ctx, &mut p.element_descriptor, value) } {
        return unsafe { unit_sentinel(ctx) };
    }
    p.items.push_back(value);
    unsafe { unit_sentinel(ctx) }
}

/// Remove and return the front element; faults `EmptyCollection` if empty.
///
/// # Safety
/// `ctx` must be live and wired; `deque` must be a valid `Deque` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_deque_pop_front(ctx: *mut RuntimeContext, deque: GcRef) -> GcRef {
    // No allocation in the common case, but `pop_front` on a VecDeque does not
    // allocate Rust heap, so no collection is needed; `deque` stays live.
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { deque_payload_mut(scope.root(deque)) };
    match p.items.pop_front() {
        Some(v) => v,
        None => {
            unsafe { set_fault(ctx, RaisedFault::EMPTY_COLLECTION) };
            unsafe { unit_sentinel(ctx) }
        }
    }
}

/// Remove and return the back element; faults `EmptyCollection` if empty.
///
/// # Safety
/// `ctx` must be live and wired; `deque` must be a valid `Deque` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_deque_pop_back(ctx: *mut RuntimeContext, deque: GcRef) -> GcRef {
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { deque_payload_mut(scope.root(deque)) };
    match p.items.pop_back() {
        Some(v) => v,
        None => {
            unsafe { set_fault(ctx, RaisedFault::EMPTY_COLLECTION) };
            unsafe { unit_sentinel(ctx) }
        }
    }
}

/// The number of elements in `deque`, as a boxed `Int` (§6.1).
///
/// # Safety
/// `ctx` must be live and wired; `deque` must be a valid `Deque` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_deque_len(ctx: *mut RuntimeContext, deque: GcRef) -> GcRef {
    let p = unsafe { deque_payload(deque) };
    let len = p.items.len() as i64;
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, len) }
}

/// The element at `index` (0-based from the front); faults `IndexOutOfBounds`.
///
/// # Safety
/// `ctx` must be live and wired; `deque` must be a valid `Deque` `GcRef`;
/// `index` must be a valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_deque_get(
    ctx: *mut RuntimeContext,
    deque: GcRef,
    index: GcRef,
) -> GcRef {
    let p = unsafe { deque_payload(deque) };
    let idx = unsafe { int_payload(index) };
    if idx < 0 || idx as usize >= p.items.len() {
        unsafe { set_fault(ctx, RaisedFault::INDEX_OUT_OF_BOUNDS) };
        return unsafe { unit_sentinel(ctx) };
    }
    p.items[idx as usize]
}

/// True iff `deque` has no elements, as a boxed `Bool` (§6.1).
///
/// # Safety
/// `ctx` must be live and wired; `deque` must be a valid `Deque` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_deque_is_empty(ctx: *mut RuntimeContext, deque: GcRef) -> GcRef {
    let p = unsafe { deque_payload(deque) };
    let empty = p.items.is_empty();
    unsafe { bool_ref(ctx, empty) }
}

// ---------------------------------------------------------------------------
// Map[K, V] / Set[T] / Counter[T] (M8-WS3, §6.1, §11.3).
//
// All three reuse Rust hash collections behind opaque GC objects. Keys are
// wrapped in `DynamicKey`, which delegates Rust `Hash`/`Eq` to the descriptor's
// structural callbacks — this is what makes tuples/records/enums/nested
// collections work as keys (§19.7 criterion). Counter's absent keys read as
// zero (§6.2); `min=`/`max=` update a map entry in place (§6.2).
// ---------------------------------------------------------------------------

use crate::maps::{CounterPayload, MapPayload, SetPayload};

/// Read a `MapPayload` as a shared ref. See `vec_payload` for the safety model.
unsafe fn map_payload(r: GcRef) -> &'static MapPayload {
    unsafe { &*r.payload::<MapPayload>() }
}

unsafe fn map_payload_mut<'s>(r: Rooted<'s>) -> &'s mut MapPayload {
    unsafe { &mut *r.get().payload::<MapPayload>() }
}

unsafe fn set_payload(r: GcRef) -> &'static SetPayload {
    unsafe { &*r.payload::<SetPayload>() }
}

unsafe fn set_payload_mut<'s>(r: Rooted<'s>) -> &'s mut SetPayload {
    unsafe { &mut *r.get().payload::<SetPayload>() }
}

unsafe fn counter_payload(r: GcRef) -> &'static CounterPayload {
    unsafe { &*r.payload::<CounterPayload>() }
}

unsafe fn counter_payload_mut<'s>(r: Rooted<'s>) -> &'s mut CounterPayload {
    unsafe { &mut *r.get().payload::<CounterPayload>() }
}

/// Allocate an empty `Map[K, V]`. `key_descriptor` is the key type the
/// construction site knew, or **null** when it knew none — which is kept null
/// (REP-41), the way `praxis_vec_new` keeps it. Spelling an unknown type `INT`
/// is a claim, and every reader that believed it read the wrong type.
///
/// # Safety
/// `ctx` must be live and wired. `key_descriptor` must be a valid pointer to a
/// `'static TypeDescriptor` (or null).
#[no_mangle]
pub unsafe extern "C" fn praxis_map_new(
    ctx: *mut RuntimeContext,
    key_descriptor: *const TypeDescriptor,
) -> GcRef {
    // The `Map` row carries one type argument, so the value type never reaches
    // this wrapper at all — it is unknown here by construction, and says so
    // (REP-42). `praxis_map_insert` adopts the first inserted value's own
    // descriptor, which is how a `Vec` learns its element type.
    let value_descriptor: *const TypeDescriptor = std::ptr::null();
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::maps::MAP,
            std::mem::size_of::<MapPayload>(),
            std::mem::align_of::<MapPayload>(),
            |payload| {
                (payload as *mut MapPayload).write(MapPayload {
                    key_descriptor,
                    value_descriptor,
                    entries: std::collections::HashMap::new(),
                });
            },
        )
    }
}

/// Insert `(key, value)` into `map`, replacing any prior value; returns Unit.
///
/// # Safety
/// `ctx` must be live and wired; `map` must be a valid `Map` `GcRef`; `key` and
/// `value` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_map_insert(
    ctx: *mut RuntimeContext,
    map: GcRef,
    key: GcRef,
    value: GcRef,
) -> GcRef {
    unsafe { maybe_collect(ctx) };
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { map_payload_mut(scope.root(map)) };
    // Learn the value type from the first value inserted, the way a `Vec` learns
    // its element type from the first `push` (REP-42). The old rule was "adopt
    // if the label still says `INT`", which could not tell a `Map` that really
    // holds `Int`s from one that had never been told anything — because `INT`
    // was what "never been told" was spelled as.
    //
    // A later value of a different type un-learns it rather than faulting: the
    // type checker makes a `Map` homogeneous, so this is unreachable for a
    // well-typed program, and `praxis_map_insert` is a non-faulting row (its
    // caller emits no fault check). Null is now representable and means "the
    // value's own descriptor answers", so forgetting is the safe direction.
    let val_desc = value.descriptor();
    match p.value() {
        None => p.value_descriptor = val_desc,
        Some(known) if !std::ptr::eq(known, val_desc) => {
            p.value_descriptor = std::ptr::null();
        }
        Some(_) => {}
    }
    p.entries.insert(DynamicKey::new(key), value);
    unsafe { unit_sentinel(ctx) }
}

/// `Some(value)` for `key`, or `None` if absent (§4.7, §5.7).
///
/// §5.7 writes the signature `Map[K,V].get(K) -> Option[V]` and §4.7 opens
/// "Option[T] represents normal domain-level absence. It is not an error
/// channel." This used to answer the Unit sentinel under a `V` static type
/// (RT-14): the program had a value it could not distinguish from a real one
/// without `contains`, and the type system said it was a `V`.
///
/// The `Option` is built through the runtime's own `option_schema`, whose
/// `Some` slot is unknown — `V` is learned from the value found, never from a
/// static type — and which `EnumSchema::same_type` therefore recognizes as the
/// same type as the codegen's `Option[Int]`. That is what lets the result match
/// against arms the program wrote.
///
/// # Safety
/// `ctx` must be live and wired; `map` must be a valid `Map` `GcRef`; `key`
/// must be a valid `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_map_get(ctx: *mut RuntimeContext, map: GcRef, key: GcRef) -> GcRef {
    let found = {
        let p = unsafe { map_payload(map) };
        p.entries.get(&DynamicKey::new(key)).copied()
    };
    match found {
        Some(v) => unsafe { option_some(ctx, v) },
        None => unsafe { option_none(ctx) },
    }
}

/// `map[key]` (§4.7): the value for `key`, **faulting** if it is absent.
///
/// A different wrapper from [`praxis_map_get`] because the two answers are the
/// language's own choice, not an implementation detail: §4.7 says "indexing a
/// missing map key faults instead of returning an option… the user chooses
/// between explicit absence with `.get` and assertion-like access with
/// indexing". Sharing one wrapper would take that choice away from the user.
///
/// The fault is [`FaultKind::IndexOutOfBounds`](crate::FaultKind::IndexOutOfBounds)
/// — an index the collection does not hold, which is what its doc already
/// describes. A dedicated `MissingKey` kind would read better and is owed to the
/// next stage that spends an ABI bump, exactly as `clamp`'s empty range is.
///
/// # Safety
/// `ctx` must be live and wired; `map` must be a valid `Map` `GcRef`; `key`
/// must be a valid `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_map_index(
    ctx: *mut RuntimeContext,
    map: GcRef,
    key: GcRef,
) -> GcRef {
    let p = unsafe { map_payload(map) };
    match p.entries.get(&DynamicKey::new(key)) {
        Some(v) => *v,
        None => {
            unsafe { set_fault(ctx, RaisedFault::INDEX_OUT_OF_BOUNDS) };
            unsafe { unit_sentinel(ctx) }
        }
    }
}

/// True iff `key` is present, as a boxed Bool.
///
/// # Safety
/// `ctx` must be live and wired; `map` and `key` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_map_contains(
    ctx: *mut RuntimeContext,
    map: GcRef,
    key: GcRef,
) -> GcRef {
    let p = unsafe { map_payload(map) };
    let present = p.entries.contains_key(&DynamicKey::new(key));
    unsafe { bool_ref(ctx, present) }
}

/// Remove `key`; returns Unit (the removed value, if any, is dropped).
///
/// # Safety
/// `ctx` must be live and wired; `map` and `key` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_map_remove(
    ctx: *mut RuntimeContext,
    map: GcRef,
    key: GcRef,
) -> GcRef {
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { map_payload_mut(scope.root(map)) };
    p.entries.remove(&DynamicKey::new(key));
    unsafe { unit_sentinel(ctx) }
}

/// The number of entries, as a boxed Int.
///
/// # Safety
/// `ctx` must be live and wired; `map` must be a valid `Map` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_map_len(ctx: *mut RuntimeContext, map: GcRef) -> GcRef {
    let p = unsafe { map_payload(map) };
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, p.entries.len() as i64) }
}

/// `m.keys()` — every key, as a `Vec[K]` (REP-18). Ordered like
/// [`praxis_counter_keys`], and index-aligned with [`praxis_map_values`].
///
/// This and `values()` are the only way to enumerate a `Map` today: `for kv in m`
/// has no lowering at all (REP-15).
///
/// # Safety
/// `ctx` must be live and wired; `map` must be a valid `Map` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_map_keys(ctx: *mut RuntimeContext, map: GcRef) -> GcRef {
    let key_desc = unsafe { map_payload(map) }.key_descriptor;
    let rows = unsafe { crate::maps::ordered_entries(&map_payload(map).entries) };
    unsafe { vec_of(ctx, key_desc, rows.into_iter().map(|(k, _)| k)) }
}

/// `m.values()` — every value, as a `Vec[V]` (REP-18). See [`praxis_map_keys`].
///
/// # Safety
/// `ctx` must be live and wired; `map` must be a valid `Map` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_map_values(ctx: *mut RuntimeContext, map: GcRef) -> GcRef {
    let val_desc = unsafe { map_payload(map) }.value_descriptor;
    let rows = unsafe { crate::maps::ordered_entries(&map_payload(map).entries) };
    unsafe { vec_of(ctx, val_desc, rows.into_iter().map(|(_, v)| v)) }
}

/// True iff the map is empty, as a boxed Bool.
///
/// # Safety
/// `ctx` must be live and wired; `map` must be a valid `Map` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_map_is_empty(ctx: *mut RuntimeContext, map: GcRef) -> GcRef {
    let p = unsafe { map_payload(map) };
    unsafe { bool_ref(ctx, p.entries.is_empty()) }
}

/// `distance[key] min= candidate` (§6.2): keep the smaller value, or insert if
/// absent (an absent entry accepts the first value). The value must support
/// ordering (Int); returns Unit.
///
/// # Safety
/// `ctx` must be live and wired; `map`, `key`, `value` must be valid `GcRef`s
/// and `value` must be an `Int`.
#[no_mangle]
pub unsafe extern "C" fn praxis_map_update_min(
    ctx: *mut RuntimeContext,
    map: GcRef,
    key: GcRef,
    value: GcRef,
) -> GcRef {
    unsafe { maybe_collect(ctx) };
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { map_payload_mut(scope.root(map)) };
    let cand = unsafe { int_payload(value) };
    match p.entries.get_mut(&DynamicKey::new(key)) {
        Some(existing) => {
            let cur = unsafe { int_payload(*existing) };
            if cand < cur {
                *existing = value;
            }
        }
        None => {
            p.entries.insert(DynamicKey::new(key), value);
        }
    }
    unsafe { unit_sentinel(ctx) }
}

/// `best[key] max= score` (§6.2): keep the larger value, or insert if absent.
///
/// # Safety
/// `ctx` must be live and wired; `map`, `key`, `value` must be valid `GcRef`s
/// and `value` must be an `Int`.
#[no_mangle]
pub unsafe extern "C" fn praxis_map_update_max(
    ctx: *mut RuntimeContext,
    map: GcRef,
    key: GcRef,
    value: GcRef,
) -> GcRef {
    unsafe { maybe_collect(ctx) };
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { map_payload_mut(scope.root(map)) };
    let cand = unsafe { int_payload(value) };
    match p.entries.get_mut(&DynamicKey::new(key)) {
        Some(existing) => {
            let cur = unsafe { int_payload(*existing) };
            if cand > cur {
                *existing = value;
            }
        }
        None => {
            p.entries.insert(DynamicKey::new(key), value);
        }
    }
    unsafe { unit_sentinel(ctx) }
}

// --- Set[T] -----------------------------------------------------------------

/// Allocate an empty `Set[T]`. `element_descriptor` is the element type the
/// construction site knew, or **null** when it knew none — kept null (REP-41).
///
/// # Safety
/// `ctx` must be live and wired; `element_descriptor` must be a valid pointer to
/// a `'static TypeDescriptor` (or null).
#[no_mangle]
pub unsafe extern "C" fn praxis_set_new(
    ctx: *mut RuntimeContext,
    element_descriptor: *const TypeDescriptor,
) -> GcRef {
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::maps::SET,
            std::mem::size_of::<SetPayload>(),
            std::mem::align_of::<SetPayload>(),
            |payload| {
                (payload as *mut SetPayload).write(SetPayload {
                    element_descriptor,
                    entries: std::collections::HashSet::new(),
                });
            },
        )
    }
}

/// Insert `value` into `set`; returns Unit.
///
/// # Safety
/// `ctx` must be live and wired; `set` and `value` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_set_insert(
    ctx: *mut RuntimeContext,
    set: GcRef,
    value: GcRef,
) -> GcRef {
    unsafe { maybe_collect(ctx) };
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { set_payload_mut(scope.root(set)) };
    p.entries.insert(DynamicKey::new(value));
    unsafe { unit_sentinel(ctx) }
}

/// Remove `value` from `set`; returns Unit.
///
/// # Safety
/// `ctx` must be live and wired; `set` and `value` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_set_remove(
    ctx: *mut RuntimeContext,
    set: GcRef,
    value: GcRef,
) -> GcRef {
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { set_payload_mut(scope.root(set)) };
    p.entries.remove(&DynamicKey::new(value));
    unsafe { unit_sentinel(ctx) }
}

/// True iff `value` is in the set, as a boxed Bool.
///
/// # Safety
/// `ctx` must be live and wired; `set` and `value` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_set_contains(
    ctx: *mut RuntimeContext,
    set: GcRef,
    value: GcRef,
) -> GcRef {
    let p = unsafe { set_payload(set) };
    let present = p.entries.contains(&DynamicKey::new(value));
    unsafe { bool_ref(ctx, present) }
}

/// The number of elements, as a boxed Int.
///
/// # Safety
/// `ctx` must be live and wired; `set` must be a valid `Set` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_set_len(ctx: *mut RuntimeContext, set: GcRef) -> GcRef {
    let p = unsafe { set_payload(set) };
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, p.entries.len() as i64) }
}

/// True iff the set is empty, as a boxed Bool.
///
/// # Safety
/// `ctx` must be live and wired; `set` must be a valid `Set` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_set_is_empty(ctx: *mut RuntimeContext, set: GcRef) -> GcRef {
    let p = unsafe { set_payload(set) };
    unsafe { bool_ref(ctx, p.entries.is_empty()) }
}

/// Every member, as a `Vec[T]` in [`crate::maps::ordered_members`] order — the
/// snapshot `for x in s` iterates (REP-15, ADR-066).
///
/// There is no `praxis_set_get`, and this is why: a `HashSet` has no nth member,
/// so an indexed accessor would be a linear scan per step and the loop would be
/// quadratic. The snapshot is one pass, and it is what makes the order
/// deterministic — which for `for` is the program's *answer* and not only its
/// printing.
///
/// # Safety
/// `ctx` must be live and wired; `set` must be a valid `Set` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_set_items(ctx: *mut RuntimeContext, set: GcRef) -> GcRef {
    let elem_desc = unsafe { set_payload(set) }.element_descriptor;
    let members = unsafe { crate::maps::ordered_members(&set_payload(set).entries) };
    unsafe { vec_of(ctx, elem_desc, members.into_iter()) }
}

// --- Counter[T] -------------------------------------------------------------

/// Allocate an empty `Counter[T]`. `key_descriptor` is the key type the
/// construction site knew, or **null** when it knew none — kept null (REP-41).
///
/// # Safety
/// `ctx` must be live and wired; `key_descriptor` must be a valid pointer to a
/// `'static TypeDescriptor` (or null).
#[no_mangle]
pub unsafe extern "C" fn praxis_counter_new(
    ctx: *mut RuntimeContext,
    key_descriptor: *const TypeDescriptor,
) -> GcRef {
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::maps::COUNTER,
            std::mem::size_of::<CounterPayload>(),
            std::mem::align_of::<CounterPayload>(),
            |payload| {
                (payload as *mut CounterPayload).write(CounterPayload {
                    key_descriptor,
                    entries: std::collections::HashMap::new(),
                });
            },
        )
    }
}

/// The count for `key`, or zero if absent (§6.2: "absent values read as zero").
/// Never faults. Returns a boxed Int.
///
/// # Safety
/// `ctx` must be live and wired; `counter` and `key` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_counter_get(
    ctx: *mut RuntimeContext,
    counter: GcRef,
    key: GcRef,
) -> GcRef {
    let p = unsafe { counter_payload(counter) };
    let count = match p.entries.get(&DynamicKey::new(key)) {
        Some(v) => unsafe { int_payload(*v) },
        None => 0, // §6.2: absent reads as zero.
    };
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, count) }
}

/// Increment the count for `key` by one (inserting 1 if absent); returns Unit.
///
/// # Safety
/// `ctx` must be live and wired; `counter` and `key` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_counter_inc(
    ctx: *mut RuntimeContext,
    counter: GcRef,
    key: GcRef,
) -> GcRef {
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { counter_payload_mut(scope.root(counter)) };
    let dk = DynamicKey::new(key);
    match p.entries.get_mut(&dk) {
        Some(v) => {
            let cur = unsafe { int_payload(*v) };
            // Checked, like every other integer computation in this file
            // (§4.12): a raw `cur + 1` panics across `extern "C"` in debug — the
            // non-unwinding panic §10.4 forbids — and wraps to `i64::MIN` in
            // release, which is a silently wrong count. A `Counter`'s values are
            // set to arbitrary `Int`s by `c[k] = n`, so this is reachable from
            // source and not only from `i64::MAX` increments.
            let Some(next) = cur.checked_add(1) else {
                unsafe { set_fault(ctx, RaisedFault::INT_OVERFLOW) };
                return unsafe { unit_sentinel(ctx) };
            };
            // SAFETY: ctx is wired; alloc a fresh Int for the incremented value.
            *v = unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, next) };
        }
        None => {
            let one = unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, 1_i64) };
            p.entries.insert(dk, one);
        }
    }
    unsafe { unit_sentinel(ctx) }
}

/// `counts[key] = value` (§6.2): set the count for `key`, replacing any prior
/// one; returns Unit.
///
/// [`praxis_counter_inc`] adds exactly one, so it cannot express
/// `counts[key] += n` or `counts[key] = n`. A subscript assignment is a
/// read-modify-write over the pair (`praxis_counter_get`, this), which is what
/// makes every assignment operator work on a `Counter` rather than only `+= 1`.
///
/// # Safety
/// `ctx` must be live and wired; `counter` and `key` must be valid `GcRef`s and
/// `value` must be an `Int`.
#[no_mangle]
pub unsafe extern "C" fn praxis_counter_set(
    ctx: *mut RuntimeContext,
    counter: GcRef,
    key: GcRef,
    value: GcRef,
) -> GcRef {
    unsafe { maybe_collect(ctx) };
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { counter_payload_mut(scope.root(counter)) };
    p.entries.insert(DynamicKey::new(key), value);
    unsafe { unit_sentinel(ctx) }
}

/// `c.keys()` — every key, as a `Vec[T]` (REP-18).
///
/// Ordered by the key's rendered form, so it is the *same* order
/// [`praxis_counter_values`] uses and the two are index-aligned. A `HashMap`'s own
/// order is randomized per process, so returning it would make the same program
/// answer differently on two runs (RT-16 in a place where the value depends on it,
/// not only the printing).
///
/// # Safety
/// `ctx` must be live and wired; `counter` must be a valid `Counter` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_counter_keys(ctx: *mut RuntimeContext, counter: GcRef) -> GcRef {
    let key_desc = unsafe { counter_payload(counter) }.key_descriptor;
    let rows = unsafe { crate::maps::ordered_entries(&counter_payload(counter).entries) };
    unsafe { vec_of(ctx, key_desc, rows.into_iter().map(|(k, _)| k)) }
}

/// `c.values()` — every count, as a `Vec[Int]` (REP-18).
///
/// §3.3's representative program is `counts.values().count(|n| n >= 2)`. Ordered
/// like [`praxis_counter_keys`]; see it for why the order is fixed.
///
/// # Safety
/// `ctx` must be live and wired; `counter` must be a valid `Counter` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_counter_values(ctx: *mut RuntimeContext, counter: GcRef) -> GcRef {
    let rows = unsafe { crate::maps::ordered_entries(&counter_payload(counter).entries) };
    unsafe { vec_of(ctx, &scalars::INT, rows.into_iter().map(|(_, v)| v)) }
}

/// The number of distinct keys, as a boxed Int.
///
/// # Safety
/// `ctx` must be live and wired; `counter` must be a valid `Counter` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_counter_len(ctx: *mut RuntimeContext, counter: GcRef) -> GcRef {
    let p = unsafe { counter_payload(counter) };
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, p.entries.len() as i64) }
}

/// True iff the counter has no keys, as a boxed Bool.
///
/// # Safety
/// `ctx` must be live and wired; `counter` must be a valid `Counter` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_counter_is_empty(
    ctx: *mut RuntimeContext,
    counter: GcRef,
) -> GcRef {
    let p = unsafe { counter_payload(counter) };
    unsafe { bool_ref(ctx, p.entries.is_empty()) }
}

// ---------------------------------------------------------------------------
// MinHeap[T] / MaxHeap[T] (M8-WS4, §6.1, §11.2).
//
// `MaxHeap` maps directly to Rust's max-`BinaryHeap`; `MinHeap` wraps entries in
// `Reverse` so the smallest surfaces first. `pop`/`peek` fault `EmptyCollection`
// on an empty heap.
// ---------------------------------------------------------------------------

use crate::heaps::{HeapEntry, MaxHeapPayload, MinHeapPayload};
use std::collections::BinaryHeap;

unsafe fn max_heap_payload_mut<'s>(r: Rooted<'s>) -> &'s mut MaxHeapPayload {
    unsafe { &mut *r.get().payload::<MaxHeapPayload>() }
}

unsafe fn max_heap_payload(r: GcRef) -> &'static MaxHeapPayload {
    unsafe { &*r.payload::<MaxHeapPayload>() }
}

unsafe fn min_heap_payload_mut<'s>(r: Rooted<'s>) -> &'s mut MinHeapPayload {
    unsafe { &mut *r.get().payload::<MinHeapPayload>() }
}

unsafe fn min_heap_payload(r: GcRef) -> &'static MinHeapPayload {
    unsafe { &*r.payload::<MinHeapPayload>() }
}

/// Allocate an empty `MaxHeap[T]`. A null `element_descriptor` — the codegen's
/// "no static element type" — is kept null (REP-41).
///
/// # Safety
/// `ctx` must be live and wired; `element_descriptor` must be a valid pointer to
/// a `'static TypeDescriptor` (or null).
#[no_mangle]
pub unsafe extern "C" fn praxis_max_heap_new(
    ctx: *mut RuntimeContext,
    element_descriptor: *const TypeDescriptor,
) -> GcRef {
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::heaps::MAX_HEAP,
            std::mem::size_of::<MaxHeapPayload>(),
            std::mem::align_of::<MaxHeapPayload>(),
            |payload| {
                (payload as *mut MaxHeapPayload).write(MaxHeapPayload {
                    element_descriptor,
                    items: BinaryHeap::new(),
                });
            },
        )
    }
}

/// Push `value` onto the max-heap; returns Unit.
///
/// # Safety
/// `ctx` must be live and wired; `heap` and `value` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_max_heap_push(
    ctx: *mut RuntimeContext,
    heap_ref: GcRef,
    value: GcRef,
) -> GcRef {
    unsafe { maybe_collect(ctx) };
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { max_heap_payload_mut(scope.root(heap_ref)) };
    p.items.push(HeapEntry {
        value,
        descriptor: value.descriptor(),
    });
    unsafe { unit_sentinel(ctx) }
}

/// Remove and return the largest element; faults `EmptyCollection` if empty.
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MaxHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_max_heap_pop(ctx: *mut RuntimeContext, heap_ref: GcRef) -> GcRef {
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { max_heap_payload_mut(scope.root(heap_ref)) };
    match p.items.pop() {
        Some(e) => e.value,
        None => {
            unsafe { set_fault(ctx, RaisedFault::EMPTY_COLLECTION) };
            unsafe { unit_sentinel(ctx) }
        }
    }
}

/// The largest element without removing it; faults `EmptyCollection` if empty.
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MaxHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_max_heap_peek(ctx: *mut RuntimeContext, heap_ref: GcRef) -> GcRef {
    let p = unsafe { max_heap_payload(heap_ref) };
    match p.items.peek() {
        Some(e) => e.value,
        None => {
            unsafe { set_fault(ctx, RaisedFault::EMPTY_COLLECTION) };
            unsafe { unit_sentinel(ctx) }
        }
    }
}

/// The number of elements, as a boxed Int.
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MaxHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_max_heap_len(ctx: *mut RuntimeContext, heap_ref: GcRef) -> GcRef {
    let p = unsafe { max_heap_payload(heap_ref) };
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, p.items.len() as i64) }
}

/// Every element, as a `Vec[T]` in [`crate::heaps::in_pop_order`] — the snapshot
/// `for x in h` iterates (REP-15, ADR-066). The heap is **not** drained.
///
/// A heap's backing array is heap-ordered only at its root, so an indexed
/// accessor over it would answer in insertion-history order; that is what made
/// `for x in h` over `[3, 1, 2]` sum to a nine-digit number before this existed
/// — it was reading the array as a `Vec`'s.
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MaxHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_max_heap_items(ctx: *mut RuntimeContext, heap_ref: GcRef) -> GcRef {
    let p = unsafe { max_heap_payload(heap_ref) };
    let items = crate::heaps::in_pop_order(&p.items, |e| e.value);
    unsafe { vec_of(ctx, p.element_descriptor, items.into_iter()) }
}

/// True iff the heap is empty, as a boxed Bool.
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MaxHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_max_heap_is_empty(
    ctx: *mut RuntimeContext,
    heap_ref: GcRef,
) -> GcRef {
    let p = unsafe { max_heap_payload(heap_ref) };
    unsafe { bool_ref(ctx, p.items.is_empty()) }
}

// --- MinHeap (mirrors MaxHeap with Reverse wrapping) -----------------------

/// Allocate an empty `MinHeap[T]`. A null `element_descriptor` — the codegen's
/// "no static element type" — is kept null (REP-41).
///
/// # Safety
/// `ctx` must be live and wired; `element_descriptor` must be a valid pointer to
/// a `'static TypeDescriptor` (or null).
#[no_mangle]
pub unsafe extern "C" fn praxis_min_heap_new(
    ctx: *mut RuntimeContext,
    element_descriptor: *const TypeDescriptor,
) -> GcRef {
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::heaps::MIN_HEAP,
            std::mem::size_of::<MinHeapPayload>(),
            std::mem::align_of::<MinHeapPayload>(),
            |payload| {
                (payload as *mut MinHeapPayload).write(MinHeapPayload {
                    element_descriptor,
                    items: BinaryHeap::new(),
                });
            },
        )
    }
}

/// Push `value` onto the min-heap; returns Unit.
///
/// # Safety
/// `ctx` must be live and wired; `heap` and `value` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_min_heap_push(
    ctx: *mut RuntimeContext,
    heap_ref: GcRef,
    value: GcRef,
) -> GcRef {
    unsafe { maybe_collect(ctx) };
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { min_heap_payload_mut(scope.root(heap_ref)) };
    p.items.push(std::cmp::Reverse(HeapEntry {
        value,
        descriptor: value.descriptor(),
    }));
    unsafe { unit_sentinel(ctx) }
}

/// Remove and return the smallest element; faults `EmptyCollection` if empty.
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MinHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_min_heap_pop(ctx: *mut RuntimeContext, heap_ref: GcRef) -> GcRef {
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { min_heap_payload_mut(scope.root(heap_ref)) };
    match p.items.pop() {
        Some(e) => e.0.value,
        None => {
            unsafe { set_fault(ctx, RaisedFault::EMPTY_COLLECTION) };
            unsafe { unit_sentinel(ctx) }
        }
    }
}

/// The smallest element without removing it; faults `EmptyCollection` if empty.
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MinHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_min_heap_peek(ctx: *mut RuntimeContext, heap_ref: GcRef) -> GcRef {
    let p = unsafe { min_heap_payload(heap_ref) };
    match p.items.peek() {
        Some(e) => e.0.value,
        None => {
            unsafe { set_fault(ctx, RaisedFault::EMPTY_COLLECTION) };
            unsafe { unit_sentinel(ctx) }
        }
    }
}

/// The number of elements, as a boxed Int.
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MinHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_min_heap_len(ctx: *mut RuntimeContext, heap_ref: GcRef) -> GcRef {
    let p = unsafe { min_heap_payload(heap_ref) };
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, p.items.len() as i64) }
}

/// Every element, as a `Vec[T]` in [`crate::heaps::in_pop_order`] — ascending,
/// because the stored entry is a `Reverse<HeapEntry>`. See
/// [`praxis_max_heap_items`].
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MinHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_min_heap_items(ctx: *mut RuntimeContext, heap_ref: GcRef) -> GcRef {
    let p = unsafe { min_heap_payload(heap_ref) };
    let items = crate::heaps::in_pop_order(&p.items, |e| e.0.value);
    unsafe { vec_of(ctx, p.element_descriptor, items.into_iter()) }
}

/// True iff the heap is empty, as a boxed Bool.
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MinHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_min_heap_is_empty(
    ctx: *mut RuntimeContext,
    heap_ref: GcRef,
) -> GcRef {
    let p = unsafe { min_heap_payload(heap_ref) };
    unsafe { bool_ref(ctx, p.items.is_empty()) }
}

// ---------------------------------------------------------------------------
// BitSet (M8-WS5, §6.1). A compact set of non-negative integers.
// ---------------------------------------------------------------------------

use crate::bitset::{BitIndex, BitSetPayload};

unsafe fn bitset_payload(r: GcRef) -> &'static BitSetPayload {
    unsafe { &*r.payload::<BitSetPayload>() }
}

unsafe fn bitset_payload_mut<'s>(r: Rooted<'s>) -> &'s mut BitSetPayload {
    unsafe { &mut *r.get().payload::<BitSetPayload>() }
}

/// Allocate an empty `BitSet` (§6.1). Nullary — no element descriptor.
///
/// # Safety
/// `ctx` must be live and wired.
#[no_mangle]
pub unsafe extern "C" fn praxis_bitset_new(ctx: *mut RuntimeContext) -> GcRef {
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::bitset::BITSET,
            std::mem::size_of::<BitSetPayload>(),
            std::mem::align_of::<BitSetPayload>(),
            |payload| {
                (payload as *mut BitSetPayload).write(BitSetPayload { words: Vec::new() });
            },
        )
    }
}

/// Set bit `value`; returns Unit. Faults `InvalidSize` if `value` is negative
/// or above [`BitIndex::MAX`] — a member this set cannot hold (RT-07).
///
/// # Safety
/// `ctx` must be live and wired; `bs` must be a valid `BitSet` `GcRef`; `value`
/// must be a valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_bitset_insert(
    ctx: *mut RuntimeContext,
    bs: GcRef,
    value: GcRef,
) -> GcRef {
    unsafe { maybe_collect(ctx) };
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { bitset_payload_mut(scope.root(bs)) };
    let i = unsafe { int_payload(value) };
    // An insert that cannot be honoured is a fault, not a silent no-op: the
    // caller asked the set to contain something, and it will not.
    let Some(index) = BitIndex::new(i) else {
        unsafe { set_fault(ctx, RaisedFault::INVALID_SIZE) };
        return unsafe { unit_sentinel(ctx) };
    };
    p.insert(index);
    unsafe { unit_sentinel(ctx) }
}

/// Clear bit `value`; returns Unit. A value the set cannot hold is a value it
/// does not contain, so removing one is a no-op rather than a fault.
///
/// # Safety
/// `ctx` must be live and wired; `bs` and `value` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_bitset_remove(
    ctx: *mut RuntimeContext,
    bs: GcRef,
    value: GcRef,
) -> GcRef {
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { bitset_payload_mut(scope.root(bs)) };
    let i = unsafe { int_payload(value) };
    if let Some(index) = BitIndex::new(i) {
        p.remove(index);
    }
    unsafe { unit_sentinel(ctx) }
}

/// True iff bit `value` is set, as a boxed Bool. A value the set cannot hold is
/// simply absent — the query is total.
///
/// # Safety
/// `ctx` must be live and wired; `bs` and `value` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_bitset_contains(
    ctx: *mut RuntimeContext,
    bs: GcRef,
    value: GcRef,
) -> GcRef {
    let p = unsafe { bitset_payload(bs) };
    let i = unsafe { int_payload(value) };
    let present = BitIndex::new(i).is_some_and(|index| p.contains(index));
    unsafe { bool_ref(ctx, present) }
}

/// The number of set bits, as a boxed Int.
///
/// # Safety
/// `ctx` must be live and wired; `bs` must be a valid `BitSet` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_bitset_len(ctx: *mut RuntimeContext, bs: GcRef) -> GcRef {
    let p = unsafe { bitset_payload(bs) };
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, p.count() as i64) }
}

/// Every member, as a `Vec[Int]` **ascending** — the snapshot `for i in b`
/// iterates (REP-15, ADR-066).
///
/// This is the one iterable whose members are not objects: they are bit
/// positions, so each one is boxed here rather than copied from the payload.
///
/// # Safety
/// `ctx` must be live and wired; `bs` must be a valid `BitSet` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_bitset_items(ctx: *mut RuntimeContext, bs: GcRef) -> GcRef {
    // The members are read out before the first allocation: `vec_of` allocates
    // per element, and a collection during the walk would move nothing here
    // (the bits are not objects) but would leave the borrow of the payload
    // spanning a safepoint, which is the shape P0-07 forbids.
    let members: Vec<i64> = unsafe { bitset_payload(bs) }.members().collect();
    let result = unsafe { praxis_vec_new(ctx, &scalars::INT as *const _) };
    let scope = unsafe { NativeScope::new(ctx) };
    let rooted = scope.root(result);
    for value in members {
        let boxed = unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, value) };
        unsafe { vec_payload_mut(rooted) }.items.push(boxed);
    }
    result
}

/// True iff the bitset is empty, as a boxed Bool.
///
/// # Safety
/// `ctx` must be live and wired; `bs` must be a valid `BitSet` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_bitset_is_empty(ctx: *mut RuntimeContext, bs: GcRef) -> GcRef {
    let p = unsafe { bitset_payload(bs) };
    unsafe { bool_ref(ctx, p.count() == 0) }
}

// ---------------------------------------------------------------------------
// Grid[T] methods (M8-WS5, §6.4). The payload (GridPayload) already exists from
// M6 (row-major Vec<GcRef> + width). M8 adds the full method surface.
// Coordinates are (x, y) with x rightward, y downward (§6.4). Indexing stays
// behind runtime wrappers (§11.5 realloc safety).
// ---------------------------------------------------------------------------

use crate::collections::{GridExtent, GridPayload};

unsafe fn grid_payload(r: GcRef) -> &'static GridPayload {
    unsafe { &*r.payload::<GridPayload>() }
}

unsafe fn grid_payload_mut<'s>(r: Rooted<'s>) -> &'s mut GridPayload {
    unsafe { &mut *r.get().payload::<GridPayload>() }
}

/// Allocate a `(x, y)` point tuple from two `i64` coordinates. The schema is
/// the cached `(Int, Int)` point schema; elements are filled via
/// `praxis_tuple_set`. Returns the point `GcRef`.
///
/// Three allocations, and each one may collect: the tuple must survive the two
/// coordinate allocations, and the x coordinate must survive the y's. Nothing
/// generated is on the stack here — the caller is a runtime helper — so the
/// only thing that can root them is a native scope.
unsafe fn alloc_point(ctx: *mut RuntimeContext, x: i64, y: i64) -> GcRef {
    let scope = unsafe { NativeScope::new(ctx) };
    let schema = crate::tuples::point_schema();
    let schema_ptr = schema as *const crate::tuples::TupleSchema;
    let tup = scope.root(unsafe { praxis_alloc_tuple(ctx, schema_ptr) });
    let x_ref = scope.root(unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, x) });
    unsafe { praxis_tuple_set(ctx, tup.get(), 0, x_ref.get()) };
    let y_ref = unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, y) };
    unsafe { praxis_tuple_set(ctx, tup.get(), 1, y_ref) };
    tup.get()
}

/// The (x, y) coordinates of a flat `idx` in a grid of `width`.
fn grid_xy(idx: usize, width: usize) -> (i64, i64) {
    ((idx % width) as i64, (idx / width) as i64)
}

/// The height (row count) of a grid: `items.len() / width`, or 0 if width is 0
/// (avoids division by zero on a degenerate empty grid).
fn grid_height(items_len: usize, width: usize) -> usize {
    items_len.checked_div(width).unwrap_or(0)
}

/// The in-bounds neighbour at `(px + dx, py + dy)`, or `None` if it falls
/// outside a `width × height` grid.
///
/// The offsets are `checked_add` because `px`/`py` come out of a user-supplied
/// point tuple: `(i64::MAX, 0).neighbors4()` overflowed the addition and
/// panicked *inside* `extern "C"` (RT-07). A coordinate that overflows is
/// outside every grid — `GridExtent` bounds the extents far below `i64::MAX` —
/// so "outside" is the whole answer, not a special case.
fn grid_neighbor(
    px: i64,
    py: i64,
    dx: i64,
    dy: i64,
    width: usize,
    height: usize,
) -> Option<(i64, i64)> {
    let nx = px.checked_add(dx)?;
    let ny = py.checked_add(dy)?;
    // Both non-negative below, so the casts are exact.
    (nx >= 0 && ny >= 0 && (nx as usize) < width && (ny as usize) < height).then_some((nx, ny))
}

/// The zero value of the type `descriptor` names, or `None` if that type has no
/// natural default.
///
/// Only the scalars and `Unit` have one. A `Grid[Vec[Int]](3, 3)` would need
/// nine distinct empty vectors and, worse, no way to know their element type —
/// so it is refused rather than filled with something of the wrong type. A null
/// descriptor means the caller never said what the cells are, which is likewise
/// nothing this can invent.
///
/// # Safety
/// `ctx` must be live and wired.
unsafe fn default_cell(
    ctx: *mut RuntimeContext,
    descriptor: *const TypeDescriptor,
) -> Option<GcRef> {
    use crate::descriptor::BuiltinTypeId as B;
    // SAFETY: a non-null descriptor is a valid `&'static`.
    let builtin = unsafe { descriptor.as_ref() }?.as_builtin()?;
    unsafe {
        match builtin {
            B::Unit => Some(unit_sentinel(ctx)),
            B::Bool => Some(bool_ref(ctx, false)),
            B::Int => Some(gc_alloc(ctx, scalars::INT_PAYLOAD, 0_i64)),
            B::Byte => Some(gc_alloc(ctx, scalars::BYTE_PAYLOAD, 0_u8)),
            // `0_u32`, not `'\0'`: a `Char`'s payload is the scalar *value*, and
            // a Rust `char` only happened to fit because it shares `u32`'s
            // layout. REP-02's signature is what said so.
            B::Char => Some(gc_alloc(ctx, scalars::CHAR_PAYLOAD, 0_u32)),
            B::Float => Some(gc_alloc(ctx, scalars::FLOAT_PAYLOAD, 0.0_f64)),
            B::Text => Some(praxis_alloc_text(ctx, std::ptr::null(), 0)),
            // A composite has no zero value the runtime can invent: a
            // `Grid[Vec[Int]]` must be filled by the program that knows what its
            // cells are.
            B::Vec
            | B::Deque
            | B::Grid
            | B::Map
            | B::Set
            | B::Counter
            | B::MinHeap
            | B::MaxHeap
            | B::BitSet
            // A `Range`'s zero value would be a pair of bounds nobody chose;
            // `0..0` is *a* range but it is not "the empty one" in any sense a
            // `Grid[Range]` cell wants.
            | B::Range
            | B::Tuple
            | B::Record
            | B::Enum
            | B::Closure
            | B::VarCell => None,
        }
    }
}

/// Allocate an empty `Grid[T]` with the given element descriptor, width, and
/// height, all cells initialized to the cell type's zero value. (The input parser also constructs
/// grids directly; this wrapper is for source `Grid[T]()` + a follow-up fill.)
///
/// Faults `InvalidSize` if either extent is negative or the grid would exceed
/// [`GridExtent::MAX_CELLS`] — the sizes arrive from source and used to become a
/// `usize` cast (RT-07).
///
/// # Safety
/// `ctx` must be live and wired; `element_descriptor` must be a valid pointer to
/// a `'static TypeDescriptor` (or null).
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_new(
    ctx: *mut RuntimeContext,
    element_descriptor: *const TypeDescriptor,
    width: i64,
    height: i64,
) -> GcRef {
    let Some(extent) = GridExtent::new(width, height) else {
        unsafe { set_fault(ctx, RaisedFault::INVALID_SIZE) };
        return unsafe { unit_sentinel(ctx) };
    };
    // Every cell of a `Grid[T]` must *be* a `T`. Filling with the Unit sentinel
    // under a `T` element descriptor is the same lie as a mislabelled element
    // descriptor, one level down: `get`, `format`, `equals` and `hash` all
    // dispatch `T`'s callbacks against a zero-sized Unit payload (P0-11).
    let cells = if extent.cells() == 0 {
        Vec::new()
    } else {
        let Some(fill) = (unsafe { default_cell(ctx, element_descriptor) }) else {
            unsafe { set_fault(ctx, RaisedFault::TYPE_MISMATCH) };
            return unsafe { unit_sentinel(ctx) };
        };
        vec![fill; extent.cells()]
    };
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::collections::GRID,
            std::mem::size_of::<GridPayload>(),
            std::mem::align_of::<GridPayload>(),
            |payload| {
                (payload as *mut GridPayload).write(GridPayload {
                    element_descriptor,
                    items: cells,
                    width: extent.width(),
                });
            },
        )
    }
}

/// The grid width (number of columns), as a boxed Int.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_width(ctx: *mut RuntimeContext, grid: GcRef) -> GcRef {
    let p = unsafe { grid_payload(grid) };
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, p.width as i64) }
}

/// The grid height (number of rows), as a boxed Int.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_height(ctx: *mut RuntimeContext, grid: GcRef) -> GcRef {
    let p = unsafe { grid_payload(grid) };
    // height = items.len() / width.
    let height = grid_height(p.items.len(), p.width);
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, height as i64) }
}

/// The cell at `(x, y)`; faults `IndexOutOfBounds` if out of range.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`; `x`/`y`
/// must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_get(
    ctx: *mut RuntimeContext,
    grid: GcRef,
    x: GcRef,
    y: GcRef,
) -> GcRef {
    let p = unsafe { grid_payload(grid) };
    let (xi, yi) = (unsafe { int_payload(x) }, unsafe { int_payload(y) });
    let height = grid_height(p.items.len(), p.width);
    if xi < 0 || yi < 0 || xi as usize >= p.width || yi as usize >= height {
        unsafe { set_fault(ctx, RaisedFault::INDEX_OUT_OF_BOUNDS) };
        return unsafe { unit_sentinel(ctx) };
    }
    p.items[(yi as usize) * p.width + (xi as usize)]
}

/// Set the cell at `(x, y)`; faults `IndexOutOfBounds` if out of range.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`; `x`/`y`
/// must be valid `Int` `GcRef`s; `value` must be a valid `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_set(
    ctx: *mut RuntimeContext,
    grid: GcRef,
    x: GcRef,
    y: GcRef,
    value: GcRef,
) -> GcRef {
    let scope = unsafe { NativeScope::new(ctx) };
    let p = unsafe { grid_payload_mut(scope.root(grid)) };
    let (xi, yi) = (unsafe { int_payload(x) }, unsafe { int_payload(y) });
    let height = grid_height(p.items.len(), p.width);
    if xi < 0 || yi < 0 || xi as usize >= p.width || yi as usize >= height {
        unsafe { set_fault(ctx, RaisedFault::INDEX_OUT_OF_BOUNDS) };
        return unsafe { unit_sentinel(ctx) };
    }
    if !unsafe { adopt_or_reject(ctx, &mut p.element_descriptor, value) } {
        return unsafe { unit_sentinel(ctx) };
    }
    p.items[(yi as usize) * p.width + (xi as usize)] = value;
    unsafe { unit_sentinel(ctx) }
}

/// True iff `(x, y)` is within the grid, as a boxed Bool.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`; `x`/`y`
/// must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_contains(
    ctx: *mut RuntimeContext,
    grid: GcRef,
    x: GcRef,
    y: GcRef,
) -> GcRef {
    let p = unsafe { grid_payload(grid) };
    let (xi, yi) = (unsafe { int_payload(x) }, unsafe { int_payload(y) });
    let height = grid_height(p.items.len(), p.width);
    let inside = xi >= 0 && yi >= 0 && (xi as usize) < p.width && (yi as usize) < height;
    unsafe { bool_ref(ctx, inside) }
}

/// The 4 orthogonal neighbors of `point` that lie inside the grid, as a `Vec`.
///
/// # Safety
/// `ctx` must be live and wired; `grid` and `point` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_neighbors4(
    ctx: *mut RuntimeContext,
    grid: GcRef,
    point: GcRef,
) -> GcRef {
    let p = unsafe { grid_payload(grid) };
    // `point` is an `(Int, Int)` tuple; read its two elements.
    let tp = point.payload::<crate::tuples::TuplePayload>() as *const crate::tuples::TuplePayload;
    let pt = unsafe { &*tp };
    let (px, py) = unsafe { (int_payload(pt.items[0]), int_payload(pt.items[1])) };
    let height = grid_height(p.items.len(), p.width);
    let result = unsafe { praxis_vec_new(ctx, &crate::tuples::TUPLE as *const _) };
    let scope = unsafe { NativeScope::new(ctx) };
    let rp = unsafe { vec_payload_mut(scope.root(result)) };
    for (dx, dy) in [(0i64, -1), (0, 1), (-1, 0), (1, 0)] {
        if let Some((nx, ny)) = grid_neighbor(px, py, dx, dy, p.width, height) {
            let pt_ref = unsafe { alloc_point(ctx, nx, ny) };
            rp.items.push(pt_ref);
        }
    }
    result
}

/// The 8 neighbors of `point` that lie inside the grid, as a `Vec`.
///
/// # Safety
/// `ctx` must be live and wired; `grid` and `point` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_neighbors8(
    ctx: *mut RuntimeContext,
    grid: GcRef,
    point: GcRef,
) -> GcRef {
    let p = unsafe { grid_payload(grid) };
    let tp = point.payload::<crate::tuples::TuplePayload>() as *const crate::tuples::TuplePayload;
    let pt = unsafe { &*tp };
    let (px, py) = unsafe { (int_payload(pt.items[0]), int_payload(pt.items[1])) };
    let height = grid_height(p.items.len(), p.width);
    let result = unsafe { praxis_vec_new(ctx, &crate::tuples::TUPLE as *const _) };
    let scope = unsafe { NativeScope::new(ctx) };
    let rp = unsafe { vec_payload_mut(scope.root(result)) };
    for dy in -1i64..=1 {
        for dx in -1i64..=1 {
            if dx == 0 && dy == 0 {
                continue;
            }
            if let Some((nx, ny)) = grid_neighbor(px, py, dx, dy, p.width, height) {
                let pt_ref = unsafe { alloc_point(ctx, nx, ny) };
                rp.items.push(pt_ref);
            }
        }
    }
    result
}

/// All `(x, y)` positions in row-major order, as a `Vec`.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_positions(ctx: *mut RuntimeContext, grid: GcRef) -> GcRef {
    unsafe { maybe_collect(ctx) };
    let p = unsafe { grid_payload(grid) };
    let result = unsafe { praxis_vec_new(ctx, &crate::tuples::TUPLE as *const _) };
    let scope = unsafe { NativeScope::new(ctx) };
    let rp = unsafe { vec_payload_mut(scope.root(result)) };
    for i in 0..p.items.len() {
        let (x, y) = grid_xy(i, p.width);
        rp.items.push(unsafe { alloc_point(ctx, x, y) });
    }
    result
}

/// All cells in row-major order, as a `Vec`.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_cells(ctx: *mut RuntimeContext, grid: GcRef) -> GcRef {
    let p = unsafe { grid_payload(grid) };
    let result = unsafe { praxis_vec_new(ctx, p.element_descriptor) };
    let scope = unsafe { NativeScope::new(ctx) };
    let rp = unsafe { vec_payload_mut(scope.root(result)) };
    for cell in p.items.iter() {
        rp.items.push(*cell);
    }
    result
}

/// Row `y` as a `Vec`; faults `IndexOutOfBounds` if out of range.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`; `y`
/// must be a valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_row(ctx: *mut RuntimeContext, grid: GcRef, y: GcRef) -> GcRef {
    let p = unsafe { grid_payload(grid) };
    let yi = unsafe { int_payload(y) };
    let height = grid_height(p.items.len(), p.width);
    if yi < 0 || yi as usize >= height {
        unsafe { set_fault(ctx, RaisedFault::INDEX_OUT_OF_BOUNDS) };
        return unsafe { unit_sentinel(ctx) };
    }
    let start = (yi as usize) * p.width;
    let result = unsafe { praxis_vec_new(ctx, p.element_descriptor) };
    let scope = unsafe { NativeScope::new(ctx) };
    let rp = unsafe { vec_payload_mut(scope.root(result)) };
    for x in 0..p.width {
        rp.items.push(p.items[start + x]);
    }
    result
}

/// Column `x` as a `Vec`; faults `IndexOutOfBounds` if out of range.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`; `x`
/// must be a valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_column(
    ctx: *mut RuntimeContext,
    grid: GcRef,
    x: GcRef,
) -> GcRef {
    let p = unsafe { grid_payload(grid) };
    let xi = unsafe { int_payload(x) };
    if xi < 0 || xi as usize >= p.width {
        unsafe { set_fault(ctx, RaisedFault::INDEX_OUT_OF_BOUNDS) };
        return unsafe { unit_sentinel(ctx) };
    }
    let result = unsafe { praxis_vec_new(ctx, p.element_descriptor) };
    let scope = unsafe { NativeScope::new(ctx) };
    let rp = unsafe { vec_payload_mut(scope.root(result)) };
    let mut idx = xi as usize;
    while idx < p.items.len() {
        rp.items.push(p.items[idx]);
        idx += p.width;
    }
    result
}

/// `Some((x, y))` for the first position whose cell equals `value`, or `None`
/// (§4.7).
///
/// This used to answer the Unit sentinel under a `(Int, Int)` static type
/// (RT-15). `find_all` needs no equivalent: a `Vec` already encodes "nothing
/// matched" as emptiness.
///
/// # Safety
/// `ctx` must be live and wired; `grid` and `value` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_find(
    ctx: *mut RuntimeContext,
    grid: GcRef,
    value: GcRef,
) -> GcRef {
    let p = unsafe { grid_payload(grid) };
    let val_desc = value.descriptor();
    let eq = val_desc.equals;
    for (i, cell) in p.items.iter().enumerate() {
        let matches = match eq {
            Some(equals) => {
                let a = cell.payload::<u8>() as *const u8;
                let b = value.payload::<u8>() as *const u8;
                unsafe { equals(a, b) }
            }
            None => *cell == value,
        };
        if matches {
            let (x, y) = grid_xy(i, p.width);
            // `option_some` roots the point across the enum allocation.
            return unsafe { option_some(ctx, alloc_point(ctx, x, y)) };
        }
    }
    unsafe { option_none(ctx) }
}

/// All `(x, y)` positions whose cell equals `value`, as a `Vec`.
///
/// # Safety
/// `ctx` must be live and wired; `grid` and `value` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_find_all(
    ctx: *mut RuntimeContext,
    grid: GcRef,
    value: GcRef,
) -> GcRef {
    unsafe { maybe_collect(ctx) };
    let p = unsafe { grid_payload(grid) };
    let val_desc = value.descriptor();
    let eq = val_desc.equals;
    let result = unsafe { praxis_vec_new(ctx, &crate::tuples::TUPLE as *const _) };
    let scope = unsafe { NativeScope::new(ctx) };
    let rp = unsafe { vec_payload_mut(scope.root(result)) };
    for (i, cell) in p.items.iter().enumerate() {
        let matches = match eq {
            Some(equals) => {
                let a = cell.payload::<u8>() as *const u8;
                let b = value.payload::<u8>() as *const u8;
                unsafe { equals(a, b) }
            }
            None => *cell == value,
        };
        if matches {
            let (x, y) = grid_xy(i, p.width);
            rp.items.push(unsafe { alloc_point(ctx, x, y) });
        }
    }
    result
}

/// A transposed copy of the grid (rows ↔ columns), as a new `Grid`.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_transpose(ctx: *mut RuntimeContext, grid: GcRef) -> GcRef {
    let p = unsafe { grid_payload(grid) };
    let height = grid_height(p.items.len(), p.width);
    let new_width = height;
    let new_height = p.width;
    let mut cells = Vec::with_capacity(p.items.len());
    for y in 0..new_height {
        for x in 0..new_width {
            // new[x,y] = old[y,x]
            cells.push(p.items[x * p.width + y]);
        }
    }
    let _ = ctx;
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::collections::GRID,
            std::mem::size_of::<GridPayload>(),
            std::mem::align_of::<GridPayload>(),
            |payload| {
                (payload as *mut GridPayload).write(GridPayload {
                    element_descriptor: p.element_descriptor,
                    items: cells,
                    width: new_width,
                });
            },
        )
    }
}

/// A copy of the grid rotated 90° left (counter-clockwise), as a new `Grid`.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_rotate_left(ctx: *mut RuntimeContext, grid: GcRef) -> GcRef {
    let p = unsafe { grid_payload(grid) };
    let height = grid_height(p.items.len(), p.width);
    // Rotate left (90° CCW): result is H×W (width=height, height=width).
    // With x rightward and y downward, turning counter-clockwise carries the
    // *rightmost* column to the top row, top-to-bottom:
    // result[x, y] = original[width-1-y, x], for x in 0..height, y in 0..width.
    let new_width = height;
    let new_height = p.width;
    let mut cells = Vec::with_capacity(p.items.len());
    for y in 0..new_height {
        for x in 0..new_width {
            let ox = p.width - 1 - y;
            let oy = x;
            cells.push(p.items[oy * p.width + ox]);
        }
    }
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::collections::GRID,
            std::mem::size_of::<GridPayload>(),
            std::mem::align_of::<GridPayload>(),
            |payload| {
                (payload as *mut GridPayload).write(GridPayload {
                    element_descriptor: p.element_descriptor,
                    items: cells,
                    width: new_width,
                });
            },
        )
    }
}

/// A copy of the grid rotated 90° right (clockwise), as a new `Grid`.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_rotate_right(ctx: *mut RuntimeContext, grid: GcRef) -> GcRef {
    let p = unsafe { grid_payload(grid) };
    let height = grid_height(p.items.len(), p.width);
    // Rotate right (90° CW): result is H×W (width=height, height=width).
    // With x rightward and y downward, turning clockwise carries the *leftmost*
    // column to the top row, bottom-to-top:
    // result[x, y] = original[y, height-1-x], for x in 0..height, y in 0..width.
    let new_width = height;
    let new_height = p.width;
    let mut cells = Vec::with_capacity(p.items.len());
    for y in 0..new_height {
        for x in 0..new_width {
            let ox = y;
            let oy = height - 1 - x;
            cells.push(p.items[oy * p.width + ox]);
        }
    }
    unsafe {
        gc_alloc_with(
            ctx,
            &crate::collections::GRID,
            std::mem::size_of::<GridPayload>(),
            std::mem::align_of::<GridPayload>(),
            |payload| {
                (payload as *mut GridPayload).write(GridPayload {
                    element_descriptor: p.element_descriptor,
                    items: cells,
                    width: new_width,
                });
            },
        )
    }
}

// ---------------------------------------------------------------------------
// Text methods (§4.3, M5).
//
// `Text` is an immutable UTF-8 payload (`Box<str>`). The methods are pure
// (no allocation beyond the result object) and never fault.
// ---------------------------------------------------------------------------

/// Read the `Text` payload of a `GcRef` as a `&str`, following slice owners.
///
/// # Safety
/// `r` must be a valid `Text` `GcRef`. Non-moving GC keeps it stable.
unsafe fn text_str(r: GcRef) -> &'static str {
    // SAFETY: caller guarantees `r` is Text; payload is a TextPayload.
    let payload = r.payload::<crate::text::TextPayload>() as *const crate::text::TextPayload;
    unsafe { crate::text::text_str(payload) }
}

/// The number of Unicode scalar values (chars) in `text`, as a boxed `Int`.
///
/// # Safety
/// `ctx` must be live and wired; `text` must be a valid `Text` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_text_len(ctx: *mut RuntimeContext, text: GcRef) -> GcRef {
    // SAFETY: caller guarantees `text` is Text.
    let s = unsafe { text_str(text) };
    let len = s.chars().count() as i64;
    unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, len) }
}

/// True iff `text` has no chars, as a boxed `Bool`.
///
/// # Safety
/// `ctx` must be live and wired; `text` must be a valid `Text` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_text_is_empty(ctx: *mut RuntimeContext, text: GcRef) -> GcRef {
    // SAFETY: caller guarantees `text` is Text.
    let s = unsafe { text_str(text) };
    // SAFETY: ctx/heap valid; Bool immortal path.
    unsafe { bool_ref(ctx, s.is_empty()) }
}

/// The Unicode scalar value (as a boxed `Int`) of the char at `index`, or an
/// `IndexOutOfBounds` fault if out of range.
///
/// # Safety
/// `ctx` must be live and wired; `text` must be a valid `Text` `GcRef`; `index`
/// must be a valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_text_get(
    ctx: *mut RuntimeContext,
    text: GcRef,
    index: GcRef,
) -> GcRef {
    // SAFETY: caller guarantees `text` is Text.
    let s = unsafe { text_str(text) };
    // SAFETY: caller guarantees `index` is a valid Int.
    let idx = unsafe { int_payload(index) };
    if idx < 0 {
        unsafe { set_fault(ctx, RaisedFault::INDEX_OUT_OF_BOUNDS) };
        return unsafe { unit_sentinel(ctx) };
    }
    match s.chars().nth(idx as usize) {
        Some(ch) => {
            // Return the scalar value as an Int (Char is reserved; M5 uses Int).
            unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, ch as i64) }
        }
        None => {
            unsafe { set_fault(ctx, RaisedFault::INDEX_OUT_OF_BOUNDS) };
            unsafe { unit_sentinel(ctx) }
        }
    }
}

// ---------------------------------------------------------------------------
// `out(...)` — write a value to stdout followed by a newline (§16.1, M5).
// ---------------------------------------------------------------------------

/// Format `value` through its descriptor and write it to stdout followed by a
/// newline. Returns the Unit sentinel (§4.3), matching `out`'s `(T) -> Unit`
/// type. Never faults.
///
/// # Safety
/// `ctx` must be live and wired; `value` must be a valid `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_write_stdout(ctx: *mut RuntimeContext, value: GcRef) -> GcRef {
    use std::io::Write;
    let mut out = String::new();
    value.format(&mut out);
    let _ = std::io::stdout().write_all(out.as_bytes());
    let _ = std::io::stdout().write_all(b"\n");
    // `out` is `(T) -> Unit`: return the Unit sentinel so a Unit-typed value
    // flows out, not the printed argument (which would otherwise leak as the
    // function's result and be printed a second time by the host).
    unsafe { unit_sentinel(ctx) }
}

// ---------------------------------------------------------------------------
// `dbg(...)`, `panic(...)`, `assert(...)` — the rest of §16.1's control names.
// ---------------------------------------------------------------------------

/// Format `value` through its descriptor, write it to stderr followed by a
/// newline, and hand **the same reference back** (§8.1). `dbg` is `forall T.
/// (T) -> T`, so it can be wrapped around any subexpression without changing
/// what the program computes. Never faults, never allocates.
///
/// # Safety
/// `ctx` must be live and wired; `value` must be a valid `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_dbg(_ctx: *mut RuntimeContext, value: GcRef) -> GcRef {
    use std::io::Write;
    let mut rendered = String::new();
    value.format(&mut rendered);
    let _ = std::io::stderr().write_all(rendered.as_bytes());
    let _ = std::io::stderr().write_all(b"\n");
    value
}

/// Record `value` as the fault message and raise [`FaultKind::Panic`] (§9.1).
///
/// The message is rendered **here**, through the value's descriptor, exactly as
/// `out` renders its argument. It has to be: the host reads the message after
/// the heap the `GcRef` points into has been torn down, so a stored reference
/// would outlive what it names.
///
/// Returns the Unit sentinel. `panic` is `forall T. (T) -> Never`, so no caller
/// can use the result — but the ABI returns a `GcRef` on every path, and a
/// fault epilogue needs a defined value to carry out (§10.4).
///
/// # Safety
/// `ctx` must be live and wired; `value` must be a valid `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_panic(ctx: *mut RuntimeContext, value: GcRef) -> GcRef {
    let mut message = String::new();
    value.format(&mut message);
    unsafe { set_fault_message(ctx, message) };
    unsafe { set_fault(ctx, RaisedFault::PANIC) };
    unsafe { unit_sentinel(ctx) }
}

/// Raise [`FaultKind::AssertFailed`] when `condition` is false (§9.1), and do
/// nothing at all when it is true.
///
/// `assert` is `(Bool) -> Unit`, so the argument is one of the two `Bool`
/// immortals and reading its payload needs no descriptor check.
///
/// It sets **no** message: `assert` takes a condition and nothing else, so the
/// only text available would restate the fault kind. `panic` is the name that
/// carries words.
///
/// # Safety
/// `ctx` must be live and wired; `condition` must be a valid `Bool` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_assert(ctx: *mut RuntimeContext, condition: GcRef) -> GcRef {
    // SAFETY: `assert`'s scheme is `(Bool) -> Unit`, so the argument is a Bool.
    if !unsafe { crate::immortal::read_bool(condition) } {
        unsafe { set_fault(ctx, RaisedFault::ASSERT_FAILED) };
    }
    unsafe { unit_sentinel(ctx) }
}

// ---------------------------------------------------------------------------
// `Range` (§4.11, ADR-059).
//
// `a..b` and `a..=b` are two symbols rather than one symbol with a flag: the
// choice is already a syntactic fact the MIR builder holds, and a boolean
// smuggled through an `i64` parameter would have 2^64 spellings for two states.
// Both bounds arrive as `Int` `GcRef`s, because a bound is an arbitrary
// expression and every other wrapper takes its operands boxed.
// ---------------------------------------------------------------------------

/// Build the half-open range `start..end` (§4.11). A descending range is
/// **empty** — [`RangeVal::new`](crate::range::RangeVal::new) normalizes it, so
/// no range with a negative length exists.
///
/// # Safety
/// `ctx` must be live and wired; both bounds must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_range_new(
    ctx: *mut RuntimeContext,
    start: GcRef,
    end: GcRef,
) -> GcRef {
    let a = unsafe { int_payload(start) };
    let b = unsafe { int_payload(end) };
    unsafe {
        gc_alloc(
            ctx,
            crate::range::RANGE_PAYLOAD,
            crate::range::RangeVal::new(a, b),
        )
    }
}

/// Build the inclusive range `start..=end` (§4.11).
///
/// # Safety
/// `ctx` must be live and wired; both bounds must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_range_new_inclusive(
    ctx: *mut RuntimeContext,
    start: GcRef,
    end: GcRef,
) -> GcRef {
    let a = unsafe { int_payload(start) };
    let b = unsafe { int_payload(end) };
    unsafe {
        gc_alloc(
            ctx,
            crate::range::RANGE_PAYLOAD,
            crate::range::RangeVal::new_inclusive(a, b),
        )
    }
}

/// The number of integers in a range (§4.11) — what a `for` loop reads to
/// bound itself.
///
/// **Faults when the count does not fit an `Int`.** Only the very widest ranges
/// reach it (`Int::MIN..Int::MAX` holds `2^64 - 1` integers), and reporting a
/// wrapped negative length instead would be a `for` loop that ran zero times
/// over every integer there is.
///
/// The kind is `IntOverflow`, which is what `gcd`, `lcm` and A\*'s path cost
/// already answer for a result with no `Int`. ADR-059 wanted it in the
/// empty-range kind alongside `clamp`; S18 declined, because the range this
/// fires on is the *fullest* one there is and "empty range" would be a fault
/// message that lies about it. ADR-075 records the disagreement.
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Range` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_range_len(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    // SAFETY: the compiler only emits this with a Range-typed operand.
    let range = unsafe { &*r.payload::<crate::range::RangeVal>() };
    match i64::try_from(range.len()) {
        Ok(len) => unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, len) },
        Err(_) => {
            unsafe { set_fault(ctx, RaisedFault::INT_OVERFLOW) };
            unsafe { unit_sentinel(ctx) }
        }
    }
}

/// The `index`-th integer of a range (§4.11). Faults when `index` is outside
/// it, exactly as `Vec.get` does.
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Range` `GcRef` and
/// `index` a valid `Int` one.
#[no_mangle]
pub unsafe extern "C" fn praxis_range_get(
    ctx: *mut RuntimeContext,
    r: GcRef,
    index: GcRef,
) -> GcRef {
    // SAFETY: the compiler only emits this with a Range-typed receiver.
    let range = unsafe { &*r.payload::<crate::range::RangeVal>() };
    let i = unsafe { int_payload(index) };
    match range.get(i) {
        Some(value) => unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, value) },
        None => {
            unsafe { set_fault(ctx, RaisedFault::INDEX_OUT_OF_BOUNDS) };
            unsafe { unit_sentinel(ctx) }
        }
    }
}

// ---------------------------------------------------------------------------
// Input parser (§7, M6).
//
// `read` / `parse` lower to runtime calls that fetch the input buffer and run
// a compiled parser plan against it. The plan is compiled at HIR time and
// registered in a global slab; its index is passed as a boxed Int.
// ---------------------------------------------------------------------------

/// Return the process-input source buffer (§7.10). The CLI sets this from stdin
/// before executing the entry function; if unset, the immortal Unit is returned.
///
/// # Safety
/// `ctx` must be live and wired.
#[no_mangle]
pub unsafe extern "C" fn praxis_get_input(ctx: *mut RuntimeContext) -> GcRef {
    unsafe { (*ctx).input_source }
}

/// Run a compiled parser plan against `input`, returning the parsed result as a
/// `GcRef` (§7.1, M6). `plan_index_gc` is a boxed `Int` whose payload is the
/// plan's index in the HIR's global slab.
///
/// On a parse mismatch (or a non-Text `input`), sets `FaultKind::ParseFailed`
/// and returns the Unit sentinel (§7.11). No Rust panic crosses the ABI.
///
/// The non-Text guard is load-bearing (§6.3 host-safety gap): the parser
/// interpreter reinterprets `input`'s payload as a `TextPayload`, so a non-Text
/// `input` (e.g. the default Unit singleton when no input buffer was installed)
/// would be dereferenced as a Text buffer and segfault. Both `read` (whose
/// `input` comes from `praxis_get_input`) and `parse(text, expr)` (whose `input`
/// is an arbitrary expression) funnel through here, so guarding at this ABI
/// boundary closes the gap regardless of how the input was produced.
///
/// # Safety
/// `ctx` must be live and wired; `plan_index_gc` must be a valid `Int`; `input`
/// must be a valid `GcRef` (any descriptor — a non-Text descriptor faults cleanly
/// rather than dereferencing garbage).
#[no_mangle]
pub unsafe extern "C" fn praxis_run_parser(
    ctx: *mut RuntimeContext,
    plan_index_gc: GcRef,
    input: GcRef,
) -> GcRef {
    // Guard the parser interpreter against a non-Text input (§6.3). Reaching
    // `run_plan` with a non-Text payload would reinterpret foreign bytes as a
    // TextPayload and segfault; fault cleanly instead.
    if input.descriptor().id() != crate::text::TEXT.id() {
        unsafe { set_fault(ctx, RaisedFault::PARSE_FAILED) };
        return unsafe { unit_sentinel(ctx) };
    }
    let idx = unsafe { int_payload(plan_index_gc) };
    // Delegate to the parser interpreter (WS7). It validates the id, reads the
    // plan from the process-wide arena, runs it against the input bytes, and
    // allocates the result.
    match crate::parser::run_plan_by_id(ctx, idx, input) {
        Some(result) => result,
        None => {
            // A `None` return means the value named no registered plan (out of
            // range, negative, or zero) or the interpreter was not linked.
            // Treat as a parse fault.
            unsafe { set_fault(ctx, RaisedFault::PARSE_FAILED) };
            unsafe { unit_sentinel(ctx) }
        }
    }
}

// ---------------------------------------------------------------------------
// Graph helpers (§6.5, ADR-060).
//
// Six prelude names whose graph is a closure: the caller passes a start state
// and a function from a state to its neighbours, and the wrapper walks whatever
// that function describes. `crate::graph` owns the six algorithms and never
// touches a closure; `ClosureOracle` below is the one thing that does.
// ---------------------------------------------------------------------------

/// A [`GraphOracle`](crate::graph::GraphOracle) backed by the closures a
/// program passed, with every state it is handed rooted in a native frame.
///
/// The scope is what makes the walks safe: a state lives in a Rust visited set
/// or queue, which the collector cannot see, and every closure call may
/// allocate. `retain` roots each state the moment the walk decides to remember
/// it, so a collection triggered inside the *next* call finds it (P0-07).
struct ClosureOracle<'s, 'c> {
    ctx: *mut RuntimeContext,
    scope: &'s NativeScope<'c>,
    /// `(T) -> Vec[T]`.
    neighbours: GcRef,
    /// `(T, T) -> Int`, or the Unit sentinel for a helper that has no weights.
    weight: GcRef,
    /// `(T) -> Int`, or the Unit sentinel for a helper with no heuristic.
    heuristic: GcRef,
    /// `(T) -> Bool`, or the Unit sentinel for a helper with no goal test.
    goal: GcRef,
}

impl ClosureOracle<'_, '_> {
    /// Call `closure` with `args`, or `Err` if it faulted — or if it is not a
    /// closure at all.
    ///
    /// The type checker says every one of these operands is a function, and the
    /// only runtime representation of a function value is a closure object. The
    /// descriptor is checked anyway: the alternative to a `TypeMismatch` fault
    /// is transmuting whatever the payload's first word happens to be into a
    /// function pointer and jumping to it.
    unsafe fn call(
        &mut self,
        closure: GcRef,
        args: &[GcRef],
    ) -> Result<GcRef, crate::graph::Aborted> {
        if !std::ptr::eq(closure.descriptor(), &crate::closures::CLOSURE) {
            return Err(self.abort(crate::context::FaultKind::TypeMismatch));
        }
        // SAFETY: the descriptor check above proves the payload is a
        // `ClosurePayload`, so `fn_ptr` is the entry point the codegen wrote
        // there (`praxis_alloc_closure`).
        let fn_ptr = unsafe { (*closure.payload::<crate::closures::ClosurePayload>()).fn_ptr };
        // A closure's entry point is `fn(ctx, closure_self, params...) -> GcRef`
        // (§4.10, Approach B): the closure value itself is a hidden first
        // explicit argument, and the prologue loads its captures from it. The
        // arity is fixed by the helper's signature, which inference has already
        // checked, so only the shapes the six helpers use exist here.
        let result = match args {
            // SAFETY: `fn_ptr` is a finalized JIT entry whose parameter count is
            // the one the type checker enforced for this operand; every value
            // crossing is a `GcRef`, which is the ABI's only value kind.
            [a] => unsafe {
                let f: unsafe extern "C" fn(*mut RuntimeContext, GcRef, GcRef) -> GcRef =
                    std::mem::transmute(fn_ptr);
                f(self.ctx, closure, *a)
            },
            // SAFETY: as above, at the two-parameter shape.
            [a, b] => unsafe {
                let f: unsafe extern "C" fn(*mut RuntimeContext, GcRef, GcRef, GcRef) -> GcRef =
                    std::mem::transmute(fn_ptr);
                f(self.ctx, closure, *a, *b)
            },
            // Unreachable: `GraphParam` has no shape with another arity, and the
            // match on it in `seed_builtin_schemes` is exhaustive. Faulting is
            // still the only safe answer, because the alternative is calling
            // with the wrong number of arguments.
            _ => return Err(self.abort(crate::context::FaultKind::TypeMismatch)),
        };
        // The closure ran arbitrary Praxis code and may have faulted. Its result
        // on that path is the Unit sentinel, so continuing would walk a graph of
        // Units; stop instead, leaving the fault for the call site's own check.
        if unsafe { praxis_check_fault(self.ctx) } != 0 {
            return Err(crate::graph::Aborted);
        }
        Ok(self.scope.root(result).get())
    }

    /// The `i64` inside a boxed `Int` a closure returned, or a fault if it is
    /// not one.
    unsafe fn int_result(&mut self, value: GcRef) -> Result<i64, crate::graph::Aborted> {
        if !std::ptr::eq(value.descriptor(), &scalars::INT) {
            return Err(self.abort(crate::context::FaultKind::TypeMismatch));
        }
        Ok(unsafe { int_payload(value) })
    }
}

impl crate::graph::GraphOracle for ClosureOracle<'_, '_> {
    fn neighbours(&mut self, state: GcRef) -> Result<Vec<GcRef>, crate::graph::Aborted> {
        // SAFETY: `ctx` is live for the wrapper's duration and `neighbours` is
        // the operand the type checker typed `(T) -> Vec[T]`.
        let result = unsafe { self.call(self.neighbours, &[state])? };
        if !std::ptr::eq(result.descriptor(), &crate::collections::VEC) {
            return Err(self.abort(crate::context::FaultKind::TypeMismatch));
        }
        // SAFETY: the descriptor check proves the payload is a `VecPayload`, and
        // the result is rooted by `call`, so reading its items cannot race a
        // collection — nothing allocates between here and the copy.
        let items = unsafe { (*result.payload::<VecPayload>()).items.clone() };
        for item in &items {
            self.scope.root(*item);
        }
        Ok(items)
    }

    fn weight(&mut self, from: GcRef, to: GcRef) -> Result<i64, crate::graph::Aborted> {
        // SAFETY: as above, at the `(T, T) -> Int` operand.
        let result = unsafe { self.call(self.weight, &[from, to])? };
        // SAFETY: `result` is a live, rooted `GcRef`.
        unsafe { self.int_result(result) }
    }

    fn heuristic(&mut self, state: GcRef) -> Result<i64, crate::graph::Aborted> {
        // SAFETY: as above, at the `(T) -> Int` operand.
        let result = unsafe { self.call(self.heuristic, &[state])? };
        // SAFETY: `result` is a live, rooted `GcRef`.
        unsafe { self.int_result(result) }
    }

    fn is_goal(&mut self, state: GcRef) -> Result<bool, crate::graph::Aborted> {
        // SAFETY: as above, at the `(T) -> Bool` operand.
        let result = unsafe { self.call(self.goal, &[state])? };
        // `Bool` is an immortal singleton pair (RT-03), so the answer is which
        // singleton came back — but the payload is what the descriptor
        // describes, and reading it is what a non-`Bool` would corrupt.
        //
        // A `Bool`'s payload is **one byte**. Reading it as an `i64` — which is
        // what this did — takes seven further bytes of the block's alignment
        // padding, which the bump allocator never initialized, so the answer
        // was whatever malloc had left there and could differ between runs.
        // `read_scalar` checks the descriptor and takes the width from
        // `BOOL_PAYLOAD`'s own type, so neither half is written here any more.
        //
        // SAFETY: `result` is a `GcRef` the oracle's own call just produced.
        match unsafe { read_scalar(result, scalars::BOOL_PAYLOAD) } {
            Some(b) => Ok(b != 0),
            None => Err(self.abort(crate::context::FaultKind::TypeMismatch)),
        }
    }

    fn retain(&mut self, state: GcRef) {
        self.scope.root(state);
    }

    fn abort(&mut self, kind: crate::context::FaultKind) -> crate::graph::Aborted {
        if let Some(fault) = RaisedFault::new(kind) {
            // SAFETY: `ctx` is live and wired for the wrapper's duration.
            unsafe { set_fault(self.ctx, fault) };
        }
        crate::graph::Aborted
    }
}

/// The descriptor every state in this walk shares: the start state's own.
///
/// The type checker guarantees one state type per call, and a `GcRef` carries
/// its descriptor in its header — so the start state is the authority on what
/// the result collection holds, and no separate type argument has to cross the
/// ABI.
#[inline]
fn state_descriptor(start: GcRef) -> *const TypeDescriptor {
    start.descriptor() as *const TypeDescriptor
}

/// Build a `Vec[T]` holding `states`, in order.
///
/// # Safety
/// `ctx` must be live and wired; every state must be a valid, rooted `GcRef`.
unsafe fn states_as_vec(
    ctx: *mut RuntimeContext,
    element: *const TypeDescriptor,
    states: &[GcRef],
) -> GcRef {
    let result = unsafe { praxis_vec_new(ctx, element) };
    let scope = unsafe { NativeScope::new(ctx) };
    let rooted = scope.root(result);
    // SAFETY: `result` is the `Vec` just allocated, and `rooted` proves it is in
    // the collector's root set for the borrow.
    let payload = unsafe { vec_payload_mut(rooted) };
    payload.items.extend_from_slice(states);
    result
}

/// `bfs(start, neighbours)` — every reachable state, in breadth-first order
/// (§6.5).
///
/// # Safety
/// `ctx` must be live and wired; `start` must be a valid `GcRef` and
/// `neighbours` a closure value of type `(T) -> Vec[T]`.
#[no_mangle]
pub unsafe extern "C" fn praxis_bfs(
    ctx: *mut RuntimeContext,
    start: GcRef,
    neighbours: GcRef,
) -> GcRef {
    // SAFETY: the caller upholds ctx/operand validity.
    unsafe {
        let scope = NativeScope::new(ctx);
        let mut oracle = ClosureOracle {
            ctx,
            scope: &scope,
            neighbours,
            weight: unit_sentinel(ctx),
            heuristic: unit_sentinel(ctx),
            goal: unit_sentinel(ctx),
        };
        match crate::graph::bfs_order(&mut oracle, start) {
            Ok(states) => states_as_vec(ctx, state_descriptor(start), &states),
            Err(_) => unit_sentinel(ctx),
        }
    }
}

/// `dfs(start, neighbours)` — every reachable state, in depth-first pre-order
/// (§6.5).
///
/// # Safety
/// As [`praxis_bfs`].
#[no_mangle]
pub unsafe extern "C" fn praxis_dfs(
    ctx: *mut RuntimeContext,
    start: GcRef,
    neighbours: GcRef,
) -> GcRef {
    // SAFETY: the caller upholds ctx/operand validity.
    unsafe {
        let scope = NativeScope::new(ctx);
        let mut oracle = ClosureOracle {
            ctx,
            scope: &scope,
            neighbours,
            weight: unit_sentinel(ctx),
            heuristic: unit_sentinel(ctx),
            goal: unit_sentinel(ctx),
        };
        match crate::graph::dfs_order(&mut oracle, start) {
            Ok(states) => states_as_vec(ctx, state_descriptor(start), &states),
            Err(_) => unit_sentinel(ctx),
        }
    }
}

/// `flood_fill(start, neighbours)` — every reachable state, as a `Set` (§6.5).
///
/// # Safety
/// As [`praxis_bfs`].
#[no_mangle]
pub unsafe extern "C" fn praxis_flood_fill(
    ctx: *mut RuntimeContext,
    start: GcRef,
    neighbours: GcRef,
) -> GcRef {
    // SAFETY: the caller upholds ctx/operand validity.
    unsafe {
        let scope = NativeScope::new(ctx);
        let mut oracle = ClosureOracle {
            ctx,
            scope: &scope,
            neighbours,
            weight: unit_sentinel(ctx),
            heuristic: unit_sentinel(ctx),
            goal: unit_sentinel(ctx),
        };
        let states = match crate::graph::reachable(&mut oracle, start) {
            Ok(states) => states,
            Err(_) => return unit_sentinel(ctx),
        };
        let result = praxis_set_new(ctx, state_descriptor(start));
        let rooted = scope.root(result);
        let payload = set_payload_mut(rooted);
        for state in states {
            payload.entries.insert(DynamicKey::new(state));
        }
        result
    }
}

/// `bfs_distance(start, neighbours, is_goal)` — the fewest steps to a goal, or
/// `None` (§6.5).
///
/// # Safety
/// `ctx` must be live and wired; `start` must be a valid `GcRef`, `neighbours` a
/// `(T) -> Vec[T]` closure and `goal` a `(T) -> Bool` closure.
#[no_mangle]
pub unsafe extern "C" fn praxis_bfs_distance(
    ctx: *mut RuntimeContext,
    start: GcRef,
    neighbours: GcRef,
    goal: GcRef,
) -> GcRef {
    // SAFETY: the caller upholds ctx/operand validity.
    unsafe {
        let scope = NativeScope::new(ctx);
        let mut oracle = ClosureOracle {
            ctx,
            scope: &scope,
            neighbours,
            weight: unit_sentinel(ctx),
            heuristic: unit_sentinel(ctx),
            goal,
        };
        match crate::graph::bfs_distance(&mut oracle, start) {
            Ok(distance) => alloc_optional_int(ctx, distance),
            Err(_) => unit_sentinel(ctx),
        }
    }
}

/// `dijkstra(start, neighbours, weight)` — the least cost to every reachable
/// state, as a `Map[T, Int]` (§6.5).
///
/// # Safety
/// `ctx` must be live and wired; `start` must be a valid `GcRef`, `neighbours` a
/// `(T) -> Vec[T]` closure and `weight` a `(T, T) -> Int` closure.
#[no_mangle]
pub unsafe extern "C" fn praxis_dijkstra(
    ctx: *mut RuntimeContext,
    start: GcRef,
    neighbours: GcRef,
    weight: GcRef,
) -> GcRef {
    // SAFETY: the caller upholds ctx/operand validity.
    unsafe {
        let scope = NativeScope::new(ctx);
        let mut oracle = ClosureOracle {
            ctx,
            scope: &scope,
            neighbours,
            weight,
            heuristic: unit_sentinel(ctx),
            goal: unit_sentinel(ctx),
        };
        let costs = match crate::graph::dijkstra_costs(&mut oracle, start) {
            Ok(costs) => costs,
            Err(_) => return unit_sentinel(ctx),
        };
        let result = scope.root(praxis_map_new(ctx, state_descriptor(start)));
        for (state, cost) in costs {
            // Each boxed cost is allocated *before* the payload borrow: an
            // allocation while a `&mut MapPayload` is live is what `Rooted`
            // exists to make impossible, and taking the borrow per entry is
            // what keeps that true.
            let boxed = scope.root(gc_alloc(ctx, scalars::INT_PAYLOAD, cost));
            map_payload_mut(result)
                .entries
                .insert(DynamicKey::new(state), boxed.get());
        }
        result.get()
    }
}

/// `a_star(start, neighbours, weight, heuristic, is_goal)` — the cheapest cost
/// to a goal, or `None` (§6.5).
///
/// # Safety
/// `ctx` must be live and wired; `start` must be a valid `GcRef` and each of
/// `neighbours`, `weight`, `heuristic` and `goal` a closure value of the type
/// the helper's signature declares.
#[no_mangle]
pub unsafe extern "C" fn praxis_a_star(
    ctx: *mut RuntimeContext,
    start: GcRef,
    neighbours: GcRef,
    weight: GcRef,
    heuristic: GcRef,
    goal: GcRef,
) -> GcRef {
    // SAFETY: the caller upholds ctx/operand validity.
    unsafe {
        let scope = NativeScope::new(ctx);
        let mut oracle = ClosureOracle {
            ctx,
            scope: &scope,
            neighbours,
            weight,
            heuristic,
            goal,
        };
        match crate::graph::a_star_cost(&mut oracle, start) {
            Ok(cost) => alloc_optional_int(ctx, cost),
            Err(_) => unit_sentinel(ctx),
        }
    }
}

/// Allocate `Some(n)` or `None` for an `Option[Int]` result.
///
/// The tags are `Option`'s own declaration order — `Some` first, `None` second
/// (`TypeDb::new`) — which is the same order the codegen uses for a `Some(x)`
/// the program writes, so a runtime-built `Option` matches against the same
/// arms.
///
/// # Safety
/// `ctx` must be live and wired.
unsafe fn alloc_optional_int(ctx: *mut RuntimeContext, value: Option<i64>) -> GcRef {
    // SAFETY: the caller upholds ctx validity.
    unsafe {
        match value {
            Some(n) => {
                let boxed = gc_alloc(ctx, scalars::INT_PAYLOAD, n);
                option_some(ctx, boxed)
            }
            None => option_none(ctx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{Fault, FaultKind, Runtime};
    use crate::parse_detail::ParseFail;

    /// A wired context backed by a real runtime.
    fn wired_ctx(rt: &mut Runtime) -> *mut RuntimeContext {
        let ctx = Box::leak(Box::new(rt.context()));
        ctx as *mut RuntimeContext
    }

    unsafe fn drop_ctx(ctx: *mut RuntimeContext) {
        // Reclaim the leaked Box. The runtime outlives this call in tests.
        let _ = unsafe { Box::from_raw(ctx) };
    }

    /// Allocate through a safepointed ABI wrapper until its pre-allocation
    /// collection causes the live registry to shrink. Returns the live count
    /// immediately after that wrapper allocates its result.
    unsafe fn allocate_until_automatic_collection(rt: &Runtime, ctx: *mut RuntimeContext) -> usize {
        let mut before = rt.heap().stats().live_count;
        for i in 0..10_000_i64 {
            let _ = unsafe { praxis_alloc_int(ctx, i) };
            let after = rt.heap().stats().live_count;
            if after < before.saturating_add(1) {
                return after;
            }
            before = after;
        }
        panic!("automatic collection did not run after 10,000 allocations");
    }

    #[test]
    fn version_is_fourteen_after_the_enum_schema_pointer() {
        assert_eq!(RUNTIME_ABI_VERSION, 14);
    }

    #[test]
    fn assert_passes_within_a_single_build() {
        assert_abi_version();
    }

    #[test]
    fn alloc_int_and_load_round_trip() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx is wired to rt.
        let r = unsafe { praxis_alloc_int(ctx, 9001) };
        // SAFETY: r is a valid Int allocated above.
        assert_eq!(unsafe { praxis_int_load(ctx, r) }, 9001);
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn bool_and_unit_abi_allocations_reuse_runtime_singletons() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let (true_ref, false_ref, unit_ref) = unsafe {
            (
                praxis_alloc_bool(ctx, 1),
                praxis_alloc_bool(ctx, 0),
                praxis_alloc_unit(ctx),
            )
        };
        let expected = (
            rt.immortals().true_(),
            rt.immortals().false_(),
            rt.immortals().unit(),
        );
        unsafe { drop_ctx(ctx) };

        assert_eq!(true_ref.as_ptr(), expected.0.as_ptr());
        assert_eq!(false_ref.as_ptr(), expected.1.as_ptr());
        assert_eq!(unit_ref.as_ptr(), expected.2.as_ptr());
    }

    #[test]
    fn repeated_bool_allocation_mints_no_new_objects() {
        // RT-03's actual harm: every `praxis_alloc_bool` call allocated a fresh
        // *immortal*, which is unregistered storage no collection can reclaim.
        // A program evaluating a comparison in a loop leaked one Bool per
        // iteration. There are two Bools; a hundred calls must name two objects.
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let mut seen = std::collections::HashSet::new();
        // SAFETY: ctx wired.
        unsafe {
            for i in 0..100_i64 {
                seen.insert(praxis_alloc_bool(ctx, i % 2).as_ptr());
                seen.insert(praxis_alloc_unit(ctx).as_ptr());
            }
        }
        unsafe { drop_ctx(ctx) };
        assert_eq!(seen.len(), 3, "true, false and unit — and nothing else");
    }

    /// Every wrapper that answers a *predicate* hands back one of the two `Bool`
    /// singletons. This is the rest of RT-03: `praxis_alloc_bool` was only the
    /// most obvious of two dozen sites minting a fresh immortal per call, and
    /// the comparisons and `is_empty`/`contains` family are the ones a real
    /// program calls in a loop. It is also what makes their `Effect::Pure` rows
    /// honest — nothing here can collect, so the call site is not a safepoint.
    #[test]
    fn predicate_wrappers_return_bool_singletons_and_allocate_nothing() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let (immortal_true, immortal_false) = (rt.immortals().true_(), rt.immortals().false_());
        // SAFETY: ctx wired; every argument below is allocated through the ABI.
        unsafe {
            let one = praxis_alloc_int(ctx, 1);
            let two = praxis_alloc_int(ctx, 2);
            let empty_vec = praxis_vec_new(ctx, &scalars::INT);
            let empty_text = praxis_alloc_text(ctx, std::ptr::null(), 0);
            let live_before = rt.heap().stats().live_count;

            let answers = [
                (praxis_int_eq(ctx, one, two), false),
                (praxis_int_ne(ctx, one, two), true),
                (praxis_int_lt(ctx, one, two), true),
                (praxis_int_gt(ctx, one, two), false),
                (praxis_int_le(ctx, one, one), true),
                (praxis_int_ge(ctx, one, two), false),
                (praxis_vec_is_empty(ctx, empty_vec), true),
                (praxis_text_is_empty(ctx, empty_text), true),
            ];

            assert_eq!(
                rt.heap().stats().live_count,
                live_before,
                "a predicate wrapper must not allocate"
            );
            for (answer, expected) in answers {
                let want = if expected {
                    immortal_true
                } else {
                    immortal_false
                };
                assert_eq!(
                    answer.as_ptr(),
                    want.as_ptr(),
                    "predicate answered with a fresh Bool instead of the singleton"
                );
            }
        }
        unsafe { drop_ctx(ctx) };
    }

    /// The other half of P0-08b: the wrappers that box a *derived* scalar —
    /// `Text` construction, the `.len()` family, `Grid` extents, checked
    /// arithmetic — gc-allocated without ever calling `maybe_collect`. A program
    /// whose pressure came from those (a text-processing loop, say) could run
    /// arbitrarily long with the collector never offered a turn. Each is driven
    /// here until its own pacing collects.
    #[test]
    fn every_scalar_boxing_wrapper_paces_the_collector() {
        // (name, a closure that performs one allocating call)
        type Call = unsafe extern "C" fn(*mut RuntimeContext, GcRef) -> GcRef;
        let cases: [(&str, Call); 5] = [
            ("praxis_text_len", praxis_text_len),
            ("praxis_vec_len", praxis_vec_len),
            ("praxis_grid_width", praxis_grid_width),
            ("praxis_grid_height", praxis_grid_height),
            ("praxis_float_to_text", praxis_float_to_text),
        ];
        for (name, call) in cases {
            let mut rt = Runtime::new();
            let ctx = wired_ctx(&mut rt);
            // SAFETY: ctx wired; each receiver matches its wrapper.
            unsafe {
                let receiver = match name {
                    "praxis_text_len" => praxis_alloc_text(ctx, "hello".as_ptr(), 5),
                    "praxis_vec_len" => praxis_vec_new(ctx, &scalars::INT),
                    "praxis_float_to_text" => praxis_alloc_float(ctx, 1.5_f64.to_bits() as i64),
                    _ => praxis_grid_new(ctx, &scalars::INT, 2, 2),
                };
                let frame = crate::shadow_frame::praxis_push_shadow_frame(ctx, 1);
                (*frame).slots[0] = receiver.as_ptr();

                let mut before = rt.heap().stats().live_count;
                let mut paced = false;
                for _ in 0..10_000 {
                    let _ = call(ctx, receiver);
                    let after = rt.heap().stats().live_count;
                    if after < before.saturating_add(1) {
                        paced = true;
                        break;
                    }
                    before = after;
                }
                crate::shadow_frame::praxis_pop_shadow_frame(ctx, frame);
                assert!(paced, "{name} never gave the collector a turn");
            }
            unsafe { drop_ctx(ctx) };
        }
    }

    #[test]
    fn checked_add_returns_sum() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; operands allocated as Ints.
        unsafe {
            let a = praxis_alloc_int(ctx, 40);
            let b = praxis_alloc_int(ctx, 2);
            let s = praxis_int_add(ctx, a, b);
            assert_eq!(praxis_int_load(ctx, s), 42);
            assert!(!rt.has_pending_fault());
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn float_sign_of_zero_is_zero() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let signed = unsafe {
            let zero = praxis_alloc_float(ctx, 0.0_f64.to_bits() as i64);
            let result = praxis_float_sign(ctx, zero);
            f64::from_bits(praxis_float_load(ctx, result) as u64)
        };
        unsafe { drop_ctx(ctx) };

        assert_eq!(signed, 0.0);
    }

    /// `-0.0` is still zero: `signum` reports the sign *bit* and would answer
    /// `-1.0` here.
    #[test]
    fn float_sign_of_negative_zero_is_zero() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let signed = unsafe {
            let zero = praxis_alloc_float(ctx, (-0.0_f64).to_bits() as i64);
            let result = praxis_float_sign(ctx, zero);
            f64::from_bits(praxis_float_load(ctx, result) as u64)
        };
        unsafe { drop_ctx(ctx) };

        assert_eq!(signed, 0.0);
    }

    #[test]
    fn float_sign_of_nan_is_nan() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let signed = unsafe {
            let nan = praxis_alloc_float(ctx, f64::NAN.to_bits() as i64);
            let result = praxis_float_sign(ctx, nan);
            f64::from_bits(praxis_float_load(ctx, result) as u64)
        };
        unsafe { drop_ctx(ctx) };

        assert!(signed.is_nan());
    }

    /// `min`/`max`/`clamp` hand back **the reference they were given**, not an
    /// equal copy (ADR-058). That is what makes them `Effect::Pure` — no
    /// allocation, so their call site is not a safepoint — and a version that
    /// allocated would pass every value test while quietly making three of the
    /// seven helpers collect.
    #[test]
    fn the_selecting_helpers_return_an_operand_and_allocate_nothing() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; every operand is a valid Int.
        unsafe {
            let lo = praxis_alloc_int(ctx, 3);
            let hi = praxis_alloc_int(ctx, 7);
            assert_eq!(praxis_int_min(ctx, lo, hi).as_ptr(), lo.as_ptr());
            assert_eq!(praxis_int_min(ctx, hi, lo).as_ptr(), lo.as_ptr());
            assert_eq!(praxis_int_max(ctx, lo, hi).as_ptr(), hi.as_ptr());
            assert_eq!(praxis_int_max(ctx, hi, lo).as_ptr(), hi.as_ptr());
            // Equal operands pick the left one — arbitrary but fixed, so the
            // choice is a decision and not a coin flip.
            let three = praxis_alloc_int(ctx, 3);
            assert_eq!(praxis_int_min(ctx, lo, three).as_ptr(), lo.as_ptr());
            assert_eq!(praxis_int_max(ctx, lo, three).as_ptr(), lo.as_ptr());
            // `clamp` returns whichever of its three operands is the answer.
            let v = praxis_alloc_int(ctx, 5);
            assert_eq!(praxis_int_clamp(ctx, v, lo, hi).as_ptr(), v.as_ptr());
            let below = praxis_alloc_int(ctx, 1);
            assert_eq!(praxis_int_clamp(ctx, below, lo, hi).as_ptr(), lo.as_ptr());
            let above = praxis_alloc_int(ctx, 9);
            assert_eq!(praxis_int_clamp(ctx, above, lo, hi).as_ptr(), hi.as_ptr());
            assert!(!rt.has_pending_fault());
        }
        unsafe { drop_ctx(ctx) };
    }

    /// An inverted `clamp` range is empty, so there is no operand to return and
    /// no answer that is not invented. It faults (ADR-058) and returns the Unit
    /// sentinel, like every other faulting wrapper.
    #[test]
    fn an_inverted_clamp_range_faults_rather_than_guessing() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; every operand is a valid Int.
        unsafe {
            let v = praxis_alloc_int(ctx, 5);
            let lo = praxis_alloc_int(ctx, 10);
            let hi = praxis_alloc_int(ctx, 0);
            let r = praxis_int_clamp(ctx, v, lo, hi);
            assert!(rt.has_pending_fault());
            assert_eq!(rt.fault(), FaultKind::EmptyRange);
            assert_eq!(r.as_ptr(), rt.immortals().unit().as_ptr());
        }
        let _ = rt.take_fault();
        // A degenerate but *legal* range — one value wide — is not inverted.
        // SAFETY: ctx wired; every operand is a valid Int.
        unsafe {
            let v = praxis_alloc_int(ctx, 5);
            let same = praxis_alloc_int(ctx, 4);
            let r = praxis_int_clamp(ctx, v, same, same);
            assert!(!rt.has_pending_fault());
            assert_eq!(r.as_ptr(), same.as_ptr());
        }
        unsafe { drop_ctx(ctx) };
    }

    /// A range whose member count has no `Int` faults with `IntOverflow`,
    /// answering the Unit sentinel like every other faulting wrapper.
    ///
    /// There was no test for this at all until S18 re-pointed the kind. ADR-059
    /// assigned the case to the empty-range kind it and ADR-058 were both owed;
    /// S18 declined and gave it `IntOverflow` instead, because
    /// `Int::MIN..Int::MAX` is the *widest* range expressible and "empty range"
    /// would be a fault message that contradicts the input. `gcd`, `lcm` and
    /// A\*'s path cost already answer `IntOverflow` for a result with no `Int`
    /// (ADR-075).
    #[test]
    fn a_range_whose_count_has_no_int_faults_rather_than_wrapping_negative() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; both bounds are valid Ints.
        unsafe {
            let lo = praxis_alloc_int(ctx, i64::MIN);
            let hi = praxis_alloc_int(ctx, i64::MAX);
            let r = praxis_range_new(ctx, lo, hi);
            let len = praxis_range_len(ctx, r);
            assert!(rt.has_pending_fault());
            assert_eq!(rt.fault(), FaultKind::IntOverflow);
            assert_eq!(len.as_ptr(), rt.immortals().unit().as_ptr());
        }
        let _ = rt.take_fault();
        // A range one narrower is countable, so the refusal is the edge and not
        // the rule.
        // SAFETY: ctx wired; both bounds are valid Ints.
        unsafe {
            let lo = praxis_alloc_int(ctx, 0);
            let hi = praxis_alloc_int(ctx, i64::MAX);
            let r = praxis_range_new(ctx, lo, hi);
            let len = praxis_range_len(ctx, r);
            assert!(!rt.has_pending_fault());
            assert_eq!(praxis_int_load(ctx, len), i64::MAX);
        }
        unsafe { drop_ctx(ctx) };
    }

    /// `gcd` and `lcm` at the edges of what an `Int` can hold. Both are computed
    /// in `i128` and range-checked on the way out, so the only refusal is a
    /// result that genuinely has no `Int` — and `gcd`'s is exactly one input
    /// pair, which a naive `i64` implementation would have wrapped instead.
    #[test]
    fn gcd_and_lcm_are_non_negative_and_refuse_only_what_has_no_int() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; every operand is a valid Int.
        unsafe {
            let load = |r: GcRef| praxis_int_load(ctx, r);
            // `Int::MIN`'s divisors: |Int::MIN| is out of range, but every gcd
            // *with* it that is not itself is in range.
            let min = praxis_alloc_int(ctx, i64::MIN);
            let two = praxis_alloc_int(ctx, 2);
            assert_eq!(load(praxis_int_gcd(ctx, min, two)), 2);
            assert!(!rt.has_pending_fault());
            // …and the one pair whose answer is 2^63 faults.
            let min2 = praxis_alloc_int(ctx, i64::MIN);
            let _ = praxis_int_gcd(ctx, min, min2);
            assert!(rt.has_pending_fault());
            assert_eq!(rt.fault(), FaultKind::IntOverflow);
        }
        let _ = rt.take_fault();
        // SAFETY: ctx wired; every operand is a valid Int.
        unsafe {
            let load = |r: GcRef| praxis_int_load(ctx, r);
            // Both signs, one answer: the lcm is non-negative.
            let neg = praxis_alloc_int(ctx, -4);
            let six = praxis_alloc_int(ctx, 6);
            assert_eq!(load(praxis_int_lcm(ctx, neg, six)), 12);
            let neg6 = praxis_alloc_int(ctx, -6);
            assert_eq!(load(praxis_int_lcm(ctx, neg, neg6)), 12);
            // `lcm(n, 0)` is 0, and the pair `(0, 0)` does not divide by zero.
            let zero = praxis_alloc_int(ctx, 0);
            assert_eq!(load(praxis_int_lcm(ctx, six, zero)), 0);
            assert_eq!(load(praxis_int_lcm(ctx, zero, zero)), 0);
            assert_eq!(load(praxis_int_gcd(ctx, zero, zero)), 0);
            assert!(!rt.has_pending_fault());
            // An lcm that does not fit: two coprime halves of the range.
            let big = praxis_alloc_int(ctx, i64::MAX);
            let three = praxis_alloc_int(ctx, 3);
            let _ = praxis_int_lcm(ctx, big, three);
            assert!(rt.has_pending_fault());
            assert_eq!(rt.fault(), FaultKind::IntOverflow);
        }
        let _ = rt.take_fault();
        unsafe { drop_ctx(ctx) };
    }

    /// `abs` faults on the one input with no positive counterpart, and `sign` is
    /// total on the same input — the distinction the manifest records as
    /// `AllocatesAndFaults` versus `Allocates`.
    #[test]
    fn abs_faults_on_the_value_with_no_positive_and_sign_does_not() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; every operand is a valid Int.
        unsafe {
            let min = praxis_alloc_int(ctx, i64::MIN);
            let r = praxis_int_abs(ctx, min);
            assert!(rt.has_pending_fault());
            assert_eq!(rt.fault(), FaultKind::IntOverflow);
            assert_eq!(r.as_ptr(), rt.immortals().unit().as_ptr());
        }
        let _ = rt.take_fault();
        // SAFETY: ctx wired; every operand is a valid Int.
        unsafe {
            let min = praxis_alloc_int(ctx, i64::MIN);
            assert_eq!(praxis_int_load(ctx, praxis_int_sign(ctx, min)), -1);
            let max = praxis_alloc_int(ctx, i64::MAX);
            assert_eq!(praxis_int_load(ctx, praxis_int_abs(ctx, max)), i64::MAX);
            assert_eq!(praxis_int_load(ctx, praxis_int_sign(ctx, max)), 1);
            assert!(!rt.has_pending_fault());
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn overflow_sets_fault_and_returns_sentinel() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; operands are valid Ints.
        unsafe {
            let a = praxis_alloc_int(ctx, i64::MAX);
            let b = praxis_alloc_int(ctx, 1);
            let s = praxis_int_add(ctx, a, b);
            // The fault is set; the return is the Unit sentinel.
            assert!(rt.has_pending_fault());
            assert_eq!(rt.fault(), FaultKind::IntOverflow);
            assert_eq!(s.as_ptr(), rt.immortals().unit().as_ptr());
        }
        let _ = rt.take_fault();
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn division_by_zero_sets_fault() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let a = praxis_alloc_int(ctx, 10);
            let b = praxis_alloc_int(ctx, 0);
            let _ = praxis_int_div(ctx, a, b);
            assert!(rt.has_pending_fault());
            assert_eq!(rt.fault(), FaultKind::DivByZero);
        }
        let _ = rt.take_fault();
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn remainder_by_zero_sets_fault() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let a = praxis_alloc_int(ctx, 10);
            let b = praxis_alloc_int(ctx, 0);
            let _ = praxis_int_rem(ctx, a, b);
            assert!(rt.has_pending_fault());
            assert_eq!(rt.fault(), FaultKind::DivByZero);
        }
        let _ = rt.take_fault();
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn subtraction_overflow_sets_fault() {
        // The add/sub/mul overflow paths are symmetric; only `add` was exercised
        // before. Sub: `Int::MIN - 1` overflows.
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; operands are valid Ints.
        unsafe {
            let a = praxis_alloc_int(ctx, i64::MIN);
            let b = praxis_alloc_int(ctx, 1);
            let _ = praxis_int_sub(ctx, a, b);
            assert!(rt.has_pending_fault());
            assert_eq!(rt.fault(), FaultKind::IntOverflow);
        }
        let _ = rt.take_fault();
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn multiplication_overflow_sets_fault() {
        // `Int::MIN * -1` is the canonical mul overflow (same magnitude as
        // `Int::MAX + 1`).
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; operands are valid Ints.
        unsafe {
            let a = praxis_alloc_int(ctx, i64::MIN);
            let b = praxis_alloc_int(ctx, -1);
            let _ = praxis_int_mul(ctx, a, b);
            assert!(rt.has_pending_fault());
            assert_eq!(rt.fault(), FaultKind::IntOverflow);
        }
        let _ = rt.take_fault();
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn division_truncates_toward_zero() {
        // §4.12 / abi.rs comment: division truncates toward zero, so -7 / 2 == -3
        // (not -4 as floor division would give). Remainder takes the sign of the
        // dividend.
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let a = praxis_alloc_int(ctx, -7);
            let b = praxis_alloc_int(ctx, 2);
            let q = praxis_int_div(ctx, a, b);
            assert!(!rt.has_pending_fault());
            assert_eq!(praxis_int_load(ctx, q), -3);
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn remainder_truncates_toward_zero() {
        // -7 % 2 == -1 (remainder takes the dividend's sign under truncation).
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let a = praxis_alloc_int(ctx, -7);
            let b = praxis_alloc_int(ctx, 2);
            let r = praxis_int_rem(ctx, a, b);
            assert!(!rt.has_pending_fault());
            assert_eq!(praxis_int_load(ctx, r), -1);
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn division_min_div_minus_one_overflows() {
        // Regression for the §10.4 no-panic-across-ABI contract: `Int::MIN / -1`
        // is the sole signed-division case that overflows. The raw `/` panics in
        // debug builds; the wrapper must instead fault `IntOverflow`.
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; operands are valid Ints.
        unsafe {
            let a = praxis_alloc_int(ctx, i64::MIN);
            let b = praxis_alloc_int(ctx, -1);
            let _ = praxis_int_div(ctx, a, b);
            assert!(rt.has_pending_fault());
            assert_eq!(rt.fault(), FaultKind::IntOverflow);
        }
        let _ = rt.take_fault();
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn remainder_min_div_minus_one_overflows() {
        // Companion to the division regression: `Int::MIN % -1` traps in debug
        // builds even though the mathematical remainder is 0, because the
        // corresponding quotient overflows. The wrapper must fault instead.
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; operands are valid Ints.
        unsafe {
            let a = praxis_alloc_int(ctx, i64::MIN);
            let b = praxis_alloc_int(ctx, -1);
            let _ = praxis_int_rem(ctx, a, b);
            assert!(rt.has_pending_fault());
            assert_eq!(rt.fault(), FaultKind::IntOverflow);
        }
        let _ = rt.take_fault();
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn comparisons_yield_bools() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let one = praxis_alloc_int(ctx, 1);
            let two = praxis_alloc_int(ctx, 2);
            assert_eq!(praxis_bool_load(ctx, praxis_int_lt(ctx, one, two)), 1);
            assert_eq!(praxis_bool_load(ctx, praxis_int_gt(ctx, one, two)), 0);
            assert_eq!(praxis_bool_load(ctx, praxis_int_eq(ctx, one, one)), 1);
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn neg_of_min_overflows() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let min = praxis_alloc_int(ctx, i64::MIN);
            let _ = praxis_int_neg(ctx, min);
            assert!(rt.has_pending_fault());
            assert_eq!(rt.fault(), FaultKind::IntOverflow);
        }
        let _ = rt.take_fault();
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn check_fault_reports_pending() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            assert_eq!(praxis_check_fault(ctx), 0);
            let a = praxis_alloc_int(ctx, 1);
            let b = praxis_alloc_int(ctx, 0);
            let _ = praxis_int_div(ctx, a, b);
            assert_eq!(praxis_check_fault(ctx), 1);
        }
        let _ = rt.take_fault();
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn alloc_text_round_trips() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let s = "héllo";
        // SAFETY: ctx wired; `bytes` is a valid UTF-8 buffer for the call.
        unsafe {
            let r = praxis_alloc_text(ctx, s.as_ptr(), s.len());
            assert_eq!(r.as_text(), "héllo");
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn alloc_bool_round_trips_value() {
        // The ABI wrapper allocates a Bool through the immortal path (never
        // reclaimed); its value is what matters. Pointer-identity with the
        // pre-allocated singleton is a runtime-internal optimization the
        // wrappers need not preserve (Bool equality is structural, §5.5).
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let t = praxis_alloc_bool(ctx, 1);
            let f = praxis_alloc_bool(ctx, 0);
            assert_eq!(praxis_bool_load(ctx, t), 1);
            assert_eq!(praxis_bool_load(ctx, f), 0);
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn fault_clear_default_is_none() {
        let f = Fault::clear();
        assert!(!f.is_pending());
        assert_eq!(f.kind(), FaultKind::None);
    }

    // --- Vec[T] collection wrappers (M5) -----------------------------------

    #[test]
    fn vec_new_is_empty() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; INT is a valid static descriptor.
        unsafe {
            let v = praxis_vec_new(ctx, &crate::scalars::INT as *const _);
            assert_eq!(praxis_bool_load(ctx, praxis_vec_is_empty(ctx, v)), 1);
            assert_eq!(praxis_int_load(ctx, praxis_vec_len(ctx, v)), 0);
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn vec_push_grows_and_get_reads_back() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; push mutates the vec in place (returns Unit), so we
        // keep using the same `v` GcRef throughout.
        unsafe {
            let v = praxis_vec_new(ctx, &crate::scalars::INT as *const _);
            let a = praxis_alloc_int(ctx, 10);
            let b = praxis_alloc_int(ctx, 20);
            let c = praxis_alloc_int(ctx, 30);
            let _ = praxis_vec_push(ctx, v, a);
            let _ = praxis_vec_push(ctx, v, b);
            let _ = praxis_vec_push(ctx, v, c);
            assert_eq!(praxis_int_load(ctx, praxis_vec_len(ctx, v)), 3);
            let i0 = praxis_alloc_int(ctx, 0);
            let i2 = praxis_alloc_int(ctx, 2);
            assert_eq!(praxis_int_load(ctx, praxis_vec_get(ctx, v, i0)), 10);
            assert_eq!(praxis_int_load(ctx, praxis_vec_get(ctx, v, i2)), 30);
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn vec_get_out_of_bounds_faults() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let v = praxis_vec_new(ctx, &crate::scalars::INT as *const _);
            let one = praxis_alloc_int(ctx, 1);
            let _ = praxis_vec_get(ctx, v, one); // empty vec, index 0
            assert!(rt.has_pending_fault());
            assert_eq!(rt.fault(), FaultKind::IndexOutOfBounds);
        }
        let _ = rt.take_fault();
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn vec_push_many_survive_collection() {
        // Stress: root the receiver/current element exactly as generated code
        // does, push enough elements to force multiple automatic collections,
        // and leave one unrooted allocation per iteration so collection is
        // observable as a live-registry shrink.
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; push mutates in place so `v` stays valid throughout.
        unsafe {
            let v = praxis_vec_new(ctx, &crate::scalars::INT as *const _);
            let frame = crate::shadow_frame::praxis_push_shadow_frame(ctx, 2);
            (*frame).slots[0] = v.as_ptr();
            let mut observed_reclamation = false;
            for i in 0..5000_i64 {
                let before_alloc = rt.heap().stats().live_count;
                let elem = praxis_alloc_int(ctx, i);
                if rt.heap().stats().live_count < before_alloc.saturating_add(1) {
                    observed_reclamation = true;
                }
                (*frame).slots[1] = elem.as_ptr();
                let before_push = rt.heap().stats().live_count;
                let _ = praxis_vec_push(ctx, v, elem);
                if rt.heap().stats().live_count < before_push {
                    observed_reclamation = true;
                }
                (*frame).slots[1] = std::ptr::null_mut();
                let _ = rt.alloc_int(-i - 1);
            }
            assert!(
                observed_reclamation,
                "the test must observe an automatic collection, not merely allocation pressure"
            );
            assert_eq!(praxis_int_load(ctx, praxis_vec_len(ctx, v)), 5000);
            // Spot-check first/middle/last.
            let zero = praxis_alloc_int(ctx, 0);
            assert_eq!(praxis_int_load(ctx, praxis_vec_get(ctx, v, zero)), 0);
            let middle = praxis_alloc_int(ctx, 2500);
            assert_eq!(praxis_int_load(ctx, praxis_vec_get(ctx, v, middle)), 2500);
            let last = praxis_alloc_int(ctx, 4999);
            assert_eq!(praxis_int_load(ctx, praxis_vec_get(ctx, v, last)), 4999);
            crate::shadow_frame::praxis_pop_shadow_frame(ctx, frame);
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn vec_get_negative_index_faults() {
        // The `idx < 0` guard in `praxis_vec_get` (the documented IndexOutOfBounds
        // path) was never exercised — only the `idx >= len` path was.
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let v = praxis_vec_new(ctx, &crate::scalars::INT as *const _);
            let a = praxis_alloc_int(ctx, 1);
            let _ = praxis_vec_push(ctx, v, a); // non-empty vec, so only the sign can fail
            let neg = praxis_alloc_int(ctx, -1);
            let _ = praxis_vec_get(ctx, v, neg);
            assert!(rt.has_pending_fault());
            assert_eq!(rt.fault(), FaultKind::IndexOutOfBounds);
        }
        let _ = rt.take_fault();
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn text_get_negative_index_faults() {
        // Companion to vec_get_negative_index_faults: the `idx < 0` guard in
        // `praxis_text_get` was likewise untested.
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let s = "ab";
            let text = praxis_alloc_text(ctx, s.as_ptr(), s.len());
            let neg = praxis_alloc_int(ctx, -1);
            let _ = praxis_text_get(ctx, text, neg);
            assert!(rt.has_pending_fault());
            assert_eq!(rt.fault(), FaultKind::IndexOutOfBounds);
        }
        let _ = rt.take_fault();
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn alloc_text_empty_string_round_trips() {
        // The `len == 0` branch in `praxis_alloc_text` (treats an empty buffer as
        // the empty slice) was not exercised. An empty Text must format as "".
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; null pointer + zero length is the documented empty path.
        unsafe {
            let r = praxis_alloc_text(ctx, std::ptr::null(), 0);
            assert_eq!(r.as_text(), "");
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn vec_new_with_null_descriptor_defaults_to_int() {
        // A null element descriptor to `praxis_vec_new` falls back to Int (the
        // documented `Vec()` construction path). The vec must be usable.
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; null descriptor is the handled default case.
        unsafe {
            let v = praxis_vec_new(ctx, std::ptr::null());
            assert_eq!(praxis_bool_load(ctx, praxis_vec_is_empty(ctx, v)), 1);
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn vec_push_rejects_a_value_with_the_wrong_descriptor() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let length_after;
        unsafe {
            let ints = praxis_vec_new(ctx, &crate::scalars::INT as *const _);
            let float = praxis_alloc_float(ctx, 1.5_f64.to_bits() as i64);
            let _ = praxis_vec_push(ctx, ints, float);
            length_after = ints.as_vec().len();
        }
        unsafe { drop_ctx(ctx) };

        assert_eq!(
            length_after, 0,
            "an ABI type mismatch must not silently retag and mutate an explicitly typed Vec[Int]"
        );
    }

    #[test]
    fn alloc_char_rejects_values_that_only_become_valid_after_truncation() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let result = unsafe { praxis_alloc_char(ctx, 0x1_0000_0041) };
        let unit = rt.immortals().unit();
        unsafe { drop_ctx(ctx) };

        assert_eq!(
            result.as_ptr(),
            unit.as_ptr(),
            "the ABI must range-check the i64 code point before converting it to u32"
        );
        // And the fault it raises must name itself. `praxis_alloc_char` used to
        // raise `FaultKind::None`, so the host reported "no fault" while
        // generated code took its fault path (RT-17).
        assert_eq!(rt.fault(), FaultKind::InvalidChar);
        assert!(rt.has_pending_fault());
    }

    /// A negative code point is out of range for the same reason a too-large
    /// one is, and `as u32` wraps it into the valid range just as silently.
    #[test]
    fn alloc_char_rejects_a_negative_code_point() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let result = unsafe { praxis_alloc_char(ctx, -1) };
        let unit = rt.immortals().unit();
        unsafe { drop_ctx(ctx) };

        assert_eq!(result.as_ptr(), unit.as_ptr());
        assert_eq!(rt.fault(), FaultKind::InvalidChar);
    }

    /// Malformed UTF-8 recovers lossily rather than panicking across the ABI —
    /// but the recovery is a fault, and it used to raise `FaultKind::None`
    /// (RT-17).
    #[test]
    fn alloc_text_reports_invalid_utf8_as_its_own_fault_kind() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let bad = [0xF0_u8, 0x28, 0x8C, 0x28];
        let result = unsafe { praxis_alloc_text(ctx, bad.as_ptr(), bad.len()) };
        unsafe { drop_ctx(ctx) };

        assert_eq!(result.descriptor().name, "Text");
        assert_eq!(rt.fault(), FaultKind::InvalidText);
        assert!(rt.has_pending_fault());
    }

    #[test]
    fn grid_cell_vectors_preserve_the_grid_element_descriptor() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let cell = rt.alloc_text("x");
        let grid = rt.alloc_grid(&crate::text::TEXT, vec![cell], 1);
        let descriptors;
        unsafe {
            let zero = praxis_alloc_int(ctx, 0);
            let cells = praxis_grid_cells(ctx, grid);
            let row = praxis_grid_row(ctx, grid, zero);
            let column = praxis_grid_column(ctx, grid, zero);
            descriptors = [
                (*vec_payload(cells).element_descriptor).id(),
                (*vec_payload(row).element_descriptor).id(),
                (*vec_payload(column).element_descriptor).id(),
            ];
        }
        unsafe { drop_ctx(ctx) };

        assert!(
            descriptors.iter().all(|id| *id == crate::text::TEXT.id()),
            "cells(), row(), and column() must return Vec values tagged with the Grid cell type"
        );
    }

    #[test]
    fn constructed_grid_cells_satisfy_the_declared_element_descriptor() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let cell_descriptor;
        unsafe {
            let grid = praxis_grid_new(ctx, &crate::scalars::INT as *const _, 1, 1);
            cell_descriptor = grid_payload(grid).items[0].descriptor().id();
        }
        unsafe { drop_ctx(ctx) };

        assert_eq!(
            cell_descriptor,
            crate::scalars::INT.id(),
            "a live Grid[Int] must never contain a Unit placeholder observable through get/format/hash"
        );
    }

    #[test]
    fn grid_position_vectors_use_the_point_tuple_descriptor() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let cell = rt.alloc_int(1);
        let grid = rt.alloc_grid(&crate::scalars::INT, vec![cell], 1);
        let descriptors;
        unsafe {
            let point = alloc_point(ctx, 0, 0);
            let positions = praxis_grid_positions(ctx, grid);
            let neighbors4 = praxis_grid_neighbors4(ctx, grid, point);
            let neighbors8 = praxis_grid_neighbors8(ctx, grid, point);
            let matches = praxis_grid_find_all(ctx, grid, cell);
            descriptors = [
                (*vec_payload(positions).element_descriptor).id(),
                (*vec_payload(neighbors4).element_descriptor).id(),
                (*vec_payload(neighbors8).element_descriptor).id(),
                (*vec_payload(matches).element_descriptor).id(),
            ];
        }
        unsafe { drop_ctx(ctx) };

        assert!(
            descriptors
                .iter()
                .all(|id| *id == crate::tuples::TUPLE.id()),
            "position-producing Grid methods must return Vec[Tuple[Int, Int]] at runtime"
        );
    }

    /// RT-07. Each of these extents used to reach `vec![unit; (w as usize) * (h
    /// as usize)]`: `-1` became `usize::MAX`, and the products either overflowed
    /// (a capacity panic across `extern "C"`) or asked the host for terabytes (an
    /// OOM abort). The wrapper must answer with a fault, and the heap must be
    /// untouched — a partly-built grid is as bad as a crash.
    #[test]
    fn a_negative_or_absurd_grid_extent_faults_instead_of_allocating() {
        let absurd = GridExtent::MAX_CELLS as i64 + 1;
        for (width, height) in [
            (-1_i64, 4_i64),
            (4, -1),
            (-1, -1),
            (i64::MIN, 1),
            // Overflows the `usize` multiplication outright.
            (i64::MAX, 2),
            (1 << 40, 1 << 40),
            // Multiplies cleanly and is still an allocation no host can serve.
            (absurd, 1),
            (1, absurd),
        ] {
            let mut rt = Runtime::new();
            let ctx = wired_ctx(&mut rt);
            let live_before = rt.heap().stats().live_count;
            let result =
                unsafe { praxis_grid_new(ctx, &crate::scalars::INT as *const _, width, height) };
            let live_after = rt.heap().stats().live_count;
            let unit = rt.immortals().unit();
            unsafe { drop_ctx(ctx) };

            assert_eq!(
                rt.fault(),
                FaultKind::InvalidSize,
                "Grid[Int]({width}, {height}) must fault"
            );
            assert_eq!(
                result.as_ptr(),
                unit.as_ptr(),
                "a faulted Grid[Int]({width}, {height}) returns the Unit sentinel"
            );
            assert_eq!(
                live_after, live_before,
                "a rejected Grid[Int]({width}, {height}) allocates nothing"
            );
        }
    }

    /// The other side of the same gate: an extent the runtime *can* serve still
    /// builds the grid it asked for, including the degenerate zero cases.
    #[test]
    fn an_in_range_grid_extent_still_builds_its_cells() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let shapes: Vec<(i64, i64, usize, usize)> = vec![(0, 0, 0, 0), (0, 5, 0, 0), (3, 2, 6, 3)];
        let mut observed = Vec::new();
        for (width, height, _, _) in &shapes {
            let grid =
                unsafe { praxis_grid_new(ctx, &crate::scalars::INT as *const _, *width, *height) };
            let p = unsafe { grid_payload(grid) };
            observed.push((p.items.len(), p.width));
        }
        unsafe { drop_ctx(ctx) };

        assert_eq!(rt.fault(), FaultKind::None, "no in-range extent faults");
        for ((w, h, cells, width), (got_cells, got_width)) in shapes.iter().zip(observed) {
            assert_eq!(
                (got_cells, got_width),
                (*cells, *width),
                "Grid[Int]({w}, {h}) shape"
            );
        }
    }

    /// RT-07. `bs.insert(10^18)` asked `Vec::resize` for 10^16 words — an OOM
    /// abort from inside `extern "C"`. A member the set cannot hold is now a
    /// fault, and a negative one no longer vanishes silently.
    #[test]
    fn a_bitset_member_outside_the_representable_range_faults() {
        for member in [-1_i64, i64::MIN, i64::MAX, BitIndex::MAX + 1] {
            let mut rt = Runtime::new();
            let ctx = wired_ctx(&mut rt);
            let words;
            unsafe {
                let bs = praxis_bitset_new(ctx);
                let value = praxis_alloc_int(ctx, member);
                let _ = praxis_bitset_insert(ctx, bs, value);
                words = bitset_payload(bs).words.len();
            }
            unsafe { drop_ctx(ctx) };

            assert_eq!(
                rt.fault(),
                FaultKind::InvalidSize,
                "BitSet.insert({member}) must fault"
            );
            assert_eq!(words, 0, "BitSet.insert({member}) must allocate no words");
        }
    }

    /// Queries stay total: a value the set cannot hold is a value it does not
    /// contain, and removing one is a no-op. Neither may fault, and neither may
    /// grow the word vector.
    #[test]
    fn bitset_queries_outside_the_range_are_absent_rather_than_faults() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let (present, words) = unsafe {
            let bs = praxis_bitset_new(ctx);
            let huge = praxis_alloc_int(ctx, i64::MAX);
            let _ = praxis_bitset_remove(ctx, bs, huge);
            let answer = praxis_bitset_contains(ctx, bs, huge);
            (
                praxis_bool_load(ctx, answer) != 0,
                bitset_payload(bs).words.len(),
            )
        };
        unsafe { drop_ctx(ctx) };

        assert!(!present, "an unrepresentable member is absent");
        assert_eq!(words, 0, "a query allocates no words");
        assert_eq!(rt.fault(), FaultKind::None, "a query does not fault");
    }

    /// RT-07. `(i64::MAX, i64::MAX).neighbors4()` overflowed the offset addition
    /// and panicked across `extern "C"`. Every such neighbour is outside every
    /// grid, so the answer is an empty Vec.
    #[test]
    fn neighbors_of_an_extreme_point_are_empty_rather_than_a_panic() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let cell = rt.alloc_int(1);
        let grid = rt.alloc_grid(&crate::scalars::INT, vec![cell], 1);
        let counts = unsafe {
            let mut counts = Vec::new();
            for (x, y) in [
                (i64::MAX, i64::MAX),
                (i64::MIN, i64::MIN),
                (i64::MAX, 0),
                (0, i64::MIN),
            ] {
                let point = alloc_point(ctx, x, y);
                counts.push((
                    vec_payload(praxis_grid_neighbors4(ctx, grid, point))
                        .items
                        .len(),
                    vec_payload(praxis_grid_neighbors8(ctx, grid, point))
                        .items
                        .len(),
                ));
            }
            counts
        };
        unsafe { drop_ctx(ctx) };

        assert!(
            counts.iter().all(|(n4, n8)| *n4 == 0 && *n8 == 0),
            "an out-of-range point has no in-grid neighbours: {counts:?}"
        );
        assert_eq!(rt.fault(), FaultKind::None);
    }

    /// **REP-16 at the unit level.** `map[key]` faults on an absent key where
    /// `.get` answers, and `praxis_counter_set` replaces a count where
    /// `praxis_counter_inc` only adds one.
    ///
    /// Here as well as in the JIT tests because §4.7's choice is the *runtime's*
    /// to make: the two map wrappers differ in one line, and a compiler that
    /// pointed both rows at `praxis_map_get` would still pass every type test.
    #[test]
    fn a_map_index_faults_where_get_answers_and_a_counter_set_replaces() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let (present, absent_get) = unsafe {
            let map = praxis_map_new(ctx, &crate::scalars::INT as *const _);
            let key = praxis_alloc_int(ctx, 1);
            let val = praxis_alloc_int(ctx, 42);
            praxis_map_insert(ctx, map, key, val);
            let present = int_payload(praxis_map_index(ctx, map, key));
            assert_eq!(rt.fault(), FaultKind::None, "a present key does not fault");
            // `.get` on an absent key is `None` and no fault…
            let other = praxis_alloc_int(ctx, 2);
            let absent_get = praxis_map_get(ctx, map, other);
            assert_eq!(rt.fault(), FaultKind::None, "`.get` does not fault");
            // …and the subscript on the same key faults.
            praxis_map_index(ctx, map, other);
            (present, absent_get)
        };
        assert_eq!(present, 42);
        assert_eq!(
            absent_get.descriptor().id(),
            crate::enums::ENUM.id(),
            "`.get` answers with absence, and absence is an `Option` value"
        );
        assert_eq!(
            rt.fault(),
            FaultKind::IndexOutOfBounds,
            "§4.7: indexing a missing key faults"
        );
        unsafe { drop_ctx(ctx) };

        // The counter half: `set` replaces, where `inc` adds one.
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let (after_inc, after_set, len) = unsafe {
            let c = praxis_counter_new(ctx, &crate::scalars::INT as *const _);
            let key = praxis_alloc_int(ctx, 7);
            praxis_counter_inc(ctx, c, key);
            let after_inc = int_payload(praxis_counter_get(ctx, c, key));
            let five = praxis_alloc_int(ctx, 5);
            praxis_counter_set(ctx, c, key, five);
            let after_set = int_payload(praxis_counter_get(ctx, c, key));
            let len = int_payload(praxis_counter_len(ctx, c));
            (after_inc, after_set, len)
        };
        unsafe { drop_ctx(ctx) };
        assert_eq!(after_inc, 1);
        assert_eq!(after_set, 5, "a set replaces rather than adds");
        assert_eq!(len, 1, "and does not add a second entry for the same key");
        assert_eq!(rt.fault(), FaultKind::None);
    }

    /// S18 exit criterion (RT-14). `Map.get` is statically value-typed, so an
    /// absent key cannot answer the Unit sentinel — a value whose static type is
    /// `V` and whose runtime descriptor is `Unit` is exactly the confusion the
    /// repair exists to close.
    ///
    /// Strengthened past the `!= UNIT` it was ignored with: the answer is an
    /// `Option`, so the test says which one — the `None` variant of the
    /// runtime's own `option_schema`, which is what makes it match a program's
    /// `None` arm.
    #[test]
    fn absent_map_get_does_not_return_an_untyped_unit_sentinel() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let (missing, found);
        unsafe {
            let map = praxis_map_new(ctx, &crate::scalars::INT as *const _);
            let key = praxis_alloc_int(ctx, 1);
            missing = praxis_map_get(ctx, map, key);
            let value = praxis_alloc_int(ctx, 42);
            praxis_map_insert(ctx, map, key, value);
            found = praxis_map_get(ctx, map, key);
        }

        assert_ne!(
            missing.descriptor().id(),
            crate::scalars::UNIT.id(),
            "Map.get is statically value-typed; absence needs Option or a checked fault, not Unit"
        );
        assert_eq!(missing.descriptor().id(), crate::enums::ENUM.id());
        assert_eq!(
            enum_tag_of(missing),
            crate::enums::OPTION_NONE_TAG as u32,
            "absence is `None`"
        );
        assert_eq!(enum_tag_of(found), crate::enums::OPTION_SOME_TAG as u32);
        // …and the `Some` carries the value, rather than merely not being Unit.
        let payload = unsafe { praxis_enum_payload(ctx, found, 0) };
        assert_eq!(unsafe { praxis_int_load(ctx, payload) }, 42);
        unsafe { drop_ctx(ctx) };
    }

    /// A goal predicate that is `false` at every state, as a closure entry
    /// point. Rust-side `extern "C"` stands in for a JIT'd closure body: the
    /// oracle only cares that the pointer has the closure calling convention.
    ///
    /// # Safety
    /// Called only through [`ClosureOracle::call`], which upholds the ABI.
    unsafe extern "C" fn always_false(
        ctx: *mut RuntimeContext,
        _closure: GcRef,
        _state: GcRef,
    ) -> GcRef {
        // SAFETY: the oracle passes its own wired ctx.
        unsafe { bool_ref(ctx, false) }
    }

    /// A neighbour function with no neighbours: the walk visits the start and
    /// stops, so the only thing that can decide the answer is the goal test.
    ///
    /// # Safety
    /// As [`always_false`].
    unsafe extern "C" fn no_neighbours(
        ctx: *mut RuntimeContext,
        _closure: GcRef,
        _state: GcRef,
    ) -> GcRef {
        // SAFETY: the oracle passes its own wired ctx.
        unsafe { praxis_vec_new(ctx, &crate::scalars::INT as *const _) }
    }

    /// **REP-37's gate.** `ClosureOracle::is_goal` read the closure's `Bool`
    /// answer with `int_payload` — an eight-byte read — but a `Bool`'s payload
    /// is **one** byte (`BoolPayload = u8`, and `BOOL` is built from it). The
    /// other seven bytes are the block's alignment padding, which the bump
    /// allocator never initializes, so `false` read as an `i64` was whatever
    /// malloc had left there and the answer could differ between runs.
    ///
    /// Every `bfs_distance` / `a_star` / `flood_fill` goal predicate goes
    /// through that read, so this walk is the whole class: the goal says
    /// `false` at the only reachable state, and the answer must be `None`. Read
    /// eight bytes and a non-zero padding byte makes it `Some(0)`.
    ///
    /// The failure is **deterministic** rather than padding-dependent because
    /// the same fix teaches `int_payload` to `debug_assert` that its operand is
    /// eight bytes wide — tests are a debug build, which is the CI profile —
    /// so restoring the old reader fails here every run instead of one in
    /// however-many. That assertion is the durable half: the next site that
    /// reaches for `int_payload` on a narrower payload fails loudly rather than
    /// reading past the object.
    #[test]
    fn a_graph_goal_predicate_reads_a_bool_at_a_bool_s_width() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let answer = unsafe {
            let goal = praxis_alloc_closure(ctx, always_false as *const u8, 0);
            let neighbours = praxis_alloc_closure(ctx, no_neighbours as *const u8, 0);
            let start = praxis_alloc_int(ctx, 0);
            praxis_bfs_distance(ctx, start, neighbours, goal)
        };
        assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
        assert_eq!(answer.descriptor().id(), crate::enums::ENUM.id());
        assert_eq!(
            enum_tag_of(answer),
            crate::enums::OPTION_NONE_TAG as u32,
            "the goal answered `false` at every state, so no distance was found"
        );
        unsafe { drop_ctx(ctx) };
    }

    /// The reader itself, both directions. `read_scalar` is what makes the two
    /// mistakes `is_goal` made unspellable, so its own contract is pinned here:
    /// the right type reads at the right width, and a foreign type answers
    /// `None` instead of reinterpreting the bytes.
    #[test]
    fn read_scalar_answers_none_for_a_foreign_descriptor() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        unsafe {
            let t = bool_ref(ctx, true);
            let f = bool_ref(ctx, false);
            assert_eq!(read_scalar(t, crate::scalars::BOOL_PAYLOAD), Some(1u8));
            assert_eq!(read_scalar(f, crate::scalars::BOOL_PAYLOAD), Some(0u8));
            // An `Int` is not a `Bool`, and the answer is absence rather than
            // the first byte of the `i64`.
            let n = praxis_alloc_int(ctx, 1);
            assert_eq!(read_scalar(n, crate::scalars::BOOL_PAYLOAD), None);
            assert_eq!(read_scalar(n, crate::scalars::INT_PAYLOAD), Some(1i64));
            drop_ctx(ctx);
        }
    }

    /// The variant tag of an enum value, read the way the runtime's own
    /// `enum_format` reads it.
    fn enum_tag_of(value: GcRef) -> u32 {
        // SAFETY: the caller passes an ENUM-descriptor object.
        unsafe { (*(value.payload::<u8>() as *const crate::enums::EnumPayload)).tag }
    }

    /// S18 exit criterion (RT-15). The same rule under a *tuple* static type:
    /// `Grid.find` answers `(Int, Int)`, so "nothing matched" cannot be the Unit
    /// sentinel wearing that type.
    #[test]
    fn absent_grid_find_does_not_return_an_untyped_unit_sentinel() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let (missing, found);
        unsafe {
            let cell = praxis_alloc_int(ctx, 1);
            let sought = praxis_alloc_int(ctx, 2);
            let grid = rt.alloc_grid(&crate::scalars::INT, vec![cell], 1);
            missing = praxis_grid_find(ctx, grid, sought);
            let present = praxis_alloc_int(ctx, 1);
            found = praxis_grid_find(ctx, grid, present);
        }

        assert_ne!(
            missing.descriptor().id(),
            crate::scalars::UNIT.id(),
            "Grid.find is statically point-typed; absence needs Option or a checked fault, not Unit"
        );
        assert_eq!(missing.descriptor().id(), crate::enums::ENUM.id());
        assert_eq!(enum_tag_of(missing), crate::enums::OPTION_NONE_TAG as u32);
        // …and a hit is `Some((x, y))`, still a real point inside the option.
        assert_eq!(enum_tag_of(found), crate::enums::OPTION_SOME_TAG as u32);
        let point = unsafe { praxis_enum_payload(ctx, found, 0) };
        assert_eq!(point.descriptor().id(), crate::tuples::TUPLE.id());
        unsafe { drop_ctx(ctx) };
    }

    // --- GC pacing (§12.4, ADR-019) ----------------------------------------
    //
    // `maybe_collect` is the load-bearing mechanism for the M5 shadow-stack
    // spill: the alloc wrappers call it so collection happens automatically
    // inside JIT'd code. The threshold/doubling logic was only tested
    // indirectly (through the heavy-allocation integration tests).

    #[test]
    fn maybe_collect_skips_below_threshold() {
        // A fresh heap with a single small allocation is well under the 64 KiB
        // threshold, so `maybe_collect` must report no collection ran.
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let _ = praxis_alloc_int(ctx, 1);
            // Nothing live matters here; we only ask whether collection *ran*.
            let roots = crate::roots::RuntimeRoots::from_context(ctx);
            let ran = rt.heap().maybe_collect(&roots);
            assert!(
                !ran,
                "a single small Int must not trip the 64 KiB threshold"
            );
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn maybe_collect_runs_under_pressure() {
        // Allocating past the 64 KiB threshold collects. This used to allocate
        // 3000 Ints and *then* call `maybe_collect` by hand, because the
        // automatic path returned early whenever `ctx.roots` was null — which
        // is exactly the case here, with no generated frame on the stack. With
        // that early return gone (P0-06) the allocation loop collects on its
        // own, and the helper asserts it happens within 10,000 allocations.
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let _ = allocate_until_automatic_collection(&rt, ctx);
            // After a collection the pacing counter resets, so an immediate
            // call (no new allocations) does not collect again.
            let roots = crate::roots::RuntimeRoots::from_context(ctx);
            assert!(
                !rt.heap().maybe_collect(&roots),
                "counter must reset after a collection"
            );
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn checked_int_add_is_an_automatic_gc_safepoint() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let collected;
        unsafe {
            let lhs = praxis_alloc_int(ctx, 20);
            let rhs = praxis_alloc_int(ctx, 22);
            let frame = crate::shadow_frame::praxis_push_shadow_frame(ctx, 2);
            (*frame).slots[0] = lhs.as_ptr();
            (*frame).slots[1] = rhs.as_ptr();

            let mut before = rt.heap().stats().live_count;
            let mut observed = false;
            for _ in 0..10_000 {
                let _ = praxis_int_add(ctx, lhs, rhs);
                let after = rt.heap().stats().live_count;
                if after < before.saturating_add(1) {
                    observed = true;
                    break;
                }
                before = after;
            }
            collected = observed;
            crate::shadow_frame::praxis_pop_shadow_frame(ctx, frame);
        }
        unsafe { drop_ctx(ctx) };

        assert!(
            collected,
            "every allocating ABI wrapper must participate in automatic GC pacing"
        );
    }

    #[test]
    fn automatic_gc_roots_the_ambient_input_buffer() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let live_after_collection;
        unsafe {
            (*ctx).input_source = rt.alloc_text("input that main has not read yet");
            let frame = crate::shadow_frame::praxis_push_shadow_frame(ctx, 0);
            live_after_collection = allocate_until_automatic_collection(&rt, ctx);
            crate::shadow_frame::praxis_pop_shadow_frame(ctx, frame);
        }
        unsafe { drop_ctx(ctx) };

        assert!(
            live_after_collection >= 2,
            "the ambient input Text and the allocation returned after collection must both remain live"
        );
    }

    #[test]
    fn automatic_gc_roots_parse_failure_partial_values() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let partial = rt.alloc_int(99);
        rt.parse_detail_mut()
            .consider(ParseFail::here(0, "test").with_partial(Some(partial)), b"");
        let live_after_collection;
        unsafe {
            let frame = crate::shadow_frame::praxis_push_shadow_frame(ctx, 0);
            live_after_collection = allocate_until_automatic_collection(&rt, ctx);
            crate::shadow_frame::praxis_pop_shadow_frame(ctx, frame);
        }
        unsafe { drop_ctx(ctx) };

        assert!(
            live_after_collection >= 2,
            "ParseDetail.partial is runtime-owned and must be included in every automatic root set"
        );
    }

    #[test]
    fn automatic_gc_roots_runtime_owned_crash_snapshots() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let captured = rt.alloc_int(7);
        let local_name = b"value";
        let meta = crate::debug::DebugLocalMeta {
            source_name: local_name.as_ptr(),
            name_len: local_name.len() as u32,
            symbol_id: 1,
            descriptor: &crate::scalars::INT as *const _,
            type_id: 0,
            kind: crate::debug::LOCAL_KIND_USER,
            span_start: 0,
            span_end: 0,
        };
        let live_after_collection;
        unsafe {
            let debug_frame =
                crate::debug::praxis_push_debug_frame(ctx, b"main".as_ptr(), 4, 1, &meta);
            (*(*debug_frame).locals).value = Some(captured);
            crate::crash_snapshot::praxis_snapshot_debug_chain(ctx);
            crate::debug::praxis_pop_debug_frame(ctx, debug_frame);
            assert!(rt.crash_snapshot().is_some());

            let shadow_frame = crate::shadow_frame::praxis_push_shadow_frame(ctx, 0);
            live_after_collection = allocate_until_automatic_collection(&rt, ctx);
            crate::shadow_frame::praxis_pop_shadow_frame(ctx, shadow_frame);
        }
        unsafe { drop_ctx(ctx) };

        assert!(
            live_after_collection >= 2,
            "a runtime-owned CrashSnapshot must root its copied local values during automatic GC"
        );
    }

    #[test]
    fn nested_allocating_helpers_root_intermediate_results() {
        // `Grid.positions` builds its result Vec in a Rust local and fills it by
        // calling `alloc_point`, which allocates three times per point. Every
        // one of those is a safepoint, and until P0-07 nothing rooted the result
        // Vec, the points already in it, or the tuple `alloc_point` was midway
        // through filling — the shadow stack only sees what generated code
        // spilled, and this is all native code.
        //
        // This calls the real helper rather than inlining a sketch of it: the
        // fix is that the helper opens a `NativeScope`, not that a bare Rust
        // local became magically visible to the collector, and reading the
        // points back afterwards is what proves nothing was reclaimed.
        // The grid is wide enough that the helper's own point allocations cross
        // the pacing threshold partway through the loop — the collection has to
        // happen *inside* the helper for this to test anything.
        const W: usize = 40;
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let mut coords: Vec<(i64, i64)> = Vec::new();
        let collections_inside_the_helper;
        unsafe {
            let cells: Vec<GcRef> = (0..(W * W) as i64).map(|i| rt.alloc_int(i)).collect();
            let grid = rt.alloc_grid(&scalars::INT, cells, W);
            let frame = crate::shadow_frame::praxis_push_shadow_frame(ctx, 1);
            (*frame).slots[0] = grid.as_ptr();

            let before = rt.heap().stats().live_count;
            let positions = praxis_grid_positions(ctx, grid);
            // Every point survived, so the live count only grew; a collection
            // that reclaimed the half-built result would show up as a drop.
            collections_inside_the_helper = rt.heap().stats().live_count > before;

            let items = &(*positions.payload::<VecPayload>()).items;
            assert_eq!(items.len(), W * W, "one position per cell");
            for point in items {
                let tuple = &*point.payload::<crate::tuples::TuplePayload>();
                coords.push((int_payload(tuple.items[0]), int_payload(tuple.items[1])));
            }

            crate::shadow_frame::praxis_pop_shadow_frame(ctx, frame);
        }
        unsafe { drop_ctx(ctx) };

        assert!(collections_inside_the_helper);
        let expected: Vec<(i64, i64)> = (0..W * W)
            .map(|i| ((i % W) as i64, (i / W) as i64))
            .collect();
        assert_eq!(
            coords, expected,
            "every point and coordinate the helper allocated must survive the \
             collections the helper itself triggers"
        );
    }

    // --- null-context safety (defensive guards, §10.4 spirit) --------------

    #[test]
    fn check_fault_on_null_context_is_zero() {
        // A null/unwired context must report no fault rather than dereferencing
        // the null pointer (the guard at `praxis_check_fault`).
        // SAFETY: passing a null context is the exact case the guard handles.
        assert_eq!(unsafe { praxis_check_fault(std::ptr::null_mut()) }, 0);
    }

    #[test]
    fn push_shadow_frame_on_null_context_returns_null() {
        // A null context must return a null frame rather than dereferencing it
        // (the guard at `praxis_push_shadow_frame`).
        // SAFETY: passing a null context is the exact case the guard handles.
        let frame =
            unsafe { crate::shadow_frame::praxis_push_shadow_frame(std::ptr::null_mut(), 0) };
        assert!(frame.is_null());
    }

    // --- REP-41: a null element type stays unknown -------------------------

    /// **REP-41.** A collection built with no static element type must not
    /// claim to hold `Int`s, and what it holds must render as what it is.
    ///
    /// The codegen passes a **null** descriptor for `let c = Counter()` — its
    /// contract above `collection_element_descriptor_for` says so, and says
    /// every `praxis_*_new` wrapper reads it that way. Five did not: `Set`,
    /// `Counter`, `Map`'s key and the two heaps each replaced the null with
    /// `&INT`, which is not a default but a false claim, and unlike `Vec` none
    /// of them ever corrected it. The label was then dispatched through — a
    /// `Text` key hashed and printed as an `i64`, a `Float` element printed as
    /// the integer its bits spell.
    ///
    /// Both halves are asserted because either alone passes a wrong fix: the
    /// absent label is the representation, the rendering is the answer.
    #[test]
    fn a_collection_with_no_static_element_type_does_not_claim_int() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: `ctx` is wired; every argument below is a wrapper's own.
        unsafe {
            // Exactly what the codegen passes when the element type is still an
            // inference variable.
            let null = std::ptr::null::<TypeDescriptor>();
            let counter = praxis_counter_new(ctx, null);
            let set = praxis_set_new(ctx, null);
            let map = praxis_map_new(ctx, null);
            let min_heap = praxis_min_heap_new(ctx, null);
            let max_heap = praxis_max_heap_new(ctx, null);

            assert!(
                counter_payload(counter).key().is_none(),
                "a Counter with no static key type must not claim one"
            );
            assert!(set_payload(set).element().is_none());
            assert!(map_payload(map).key().is_none());
            assert!(min_heap_payload(min_heap).element().is_none());
            assert!(max_heap_payload(max_heap).element().is_none());

            // …and the values come back out as themselves. A `Text` key through
            // a `Counter`, a `Float` through a `MinHeap`: neither is an `Int`,
            // and both used to print as one.
            let key = praxis_alloc_text(ctx, "ab".as_ptr(), 2);
            praxis_counter_inc(ctx, counter, key);
            let keys = praxis_counter_keys(ctx, counter);
            let mut rendered = String::new();
            keys.format(&mut rendered);
            assert_eq!(rendered, "[ab]", "a Counter's keys are its keys");

            let half = praxis_alloc_float(ctx, 1.5f64.to_bits() as i64);
            praxis_min_heap_push(ctx, min_heap, half);
            let mut rendered = String::new();
            min_heap.format(&mut rendered);
            assert_eq!(rendered, "[1.5]", "a MinHeap prints the elements it holds");

            let member = praxis_alloc_text(ctx, "zz".as_ptr(), 2);
            praxis_set_insert(ctx, set, member);
            let items = praxis_set_items(ctx, set);
            let mut rendered = String::new();
            items.format(&mut rendered);
            assert_eq!(rendered, "[zz]");
        }
        // SAFETY: the context was leaked by `wired_ctx` and is unused after this.
        unsafe { drop_ctx(ctx) };
    }

    /// **REP-42.** A `Map` does not claim its values are `Int`s.
    ///
    /// `praxis_map_new` takes one descriptor — the key's — because the `MapNew`
    /// row carries one type argument, and it wrote `INT` into the value slot
    /// unconditionally with a comment calling that "sound for now". It was not a
    /// default but a claim, and it was the same word as "unknown", so the
    /// adoption that followed could not tell a `Map` that really holds `Int`s
    /// from one that had never been told anything. The progress doc records the
    /// consequence that shaped REP-18: a `Map[Text, Text]`'s pair read its value
    /// as an `i64`, which is why `keys()`/`values()` are built in MIR.
    #[test]
    fn a_map_does_not_claim_its_values_are_ints() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: `ctx` is wired; every argument below is a wrapper's own.
        unsafe {
            let empty = praxis_map_new(ctx, &crate::text::TEXT);
            assert!(
                map_payload(empty).value().is_none(),
                "an empty Map has been told nothing about its values"
            );
            // …so the `Vec` its `values()` answers is not labelled `Int` either,
            // which is what made an empty `Map[Text, Text]`'s values unequal to
            // an empty `Vec[Text]` (`vec_equals` compares element labels).
            let none_yet = praxis_map_values(ctx, empty);
            assert!(vec_payload(none_yet).element().is_none());

            // The first insert is what the map learns from.
            let k = praxis_alloc_text(ctx, "k".as_ptr(), 1);
            let v = praxis_alloc_text(ctx, "vv".as_ptr(), 2);
            praxis_map_insert(ctx, empty, k, v);
            assert!(
                std::ptr::eq(map_payload(empty).value().unwrap(), &crate::text::TEXT),
                "a Map learns its value type from the first value inserted"
            );
            let mut rendered = String::new();
            praxis_map_values(ctx, empty).format(&mut rendered);
            assert_eq!(rendered, "[vv]");

            // A `Map` that really does hold `Int`s says so — the assertion the
            // old spelling could not make, because `INT` was also its "unknown".
            let ints = praxis_map_new(ctx, &crate::text::TEXT);
            let ik = praxis_alloc_text(ctx, "n".as_ptr(), 1);
            praxis_map_insert(ctx, ints, ik, praxis_alloc_int(ctx, 7));
            assert!(std::ptr::eq(
                map_payload(ints).value().unwrap(),
                &scalars::INT
            ));
        }
        // SAFETY: the context was leaked by `wired_ctx` and is unused after this.
        unsafe { drop_ctx(ctx) };
    }

    // --- REP-45: the manifest's fault column is checked against the code ----

    /// Every function defined in this file, as `(name, body)`.
    ///
    /// Line-based on purpose: a definition is a line whose first tokens are one
    /// of Rust's `fn` spellings, and its body runs to the line where the brace
    /// depth opened by that definition returns to zero. Anything cleverer would
    /// be a Rust parser, and anything looser — matching `fn` anywhere — reads
    /// the word out of doc comments and glues unrelated bodies together.
    fn functions_in_this_file() -> Vec<(String, String)> {
        const PREFIXES: [&str; 6] = [
            "fn ",
            "pub fn ",
            "pub(crate) fn ",
            "unsafe fn ",
            "pub unsafe fn ",
            "pub unsafe extern \"C\" fn ",
        ];
        let mut out: Vec<(String, String)> = Vec::new();
        // (name, body so far, brace depth, whether the body has opened at all —
        // a multi-line signature spends several lines at depth zero before its
        // `{`, and closing there would give every such wrapper a one-line body).
        let mut open: Option<(String, String, i32, bool)> = None;
        for line in include_str!("abi.rs").lines() {
            let depth_change = |l: &str| -> i32 {
                l.chars().filter(|c| *c == '{').count() as i32
                    - l.chars().filter(|c| *c == '}').count() as i32
            };
            if let Some((name, body, depth, opened)) = open.as_mut() {
                body.push_str(line);
                body.push('\n');
                *depth += depth_change(line);
                *opened |= line.contains('{');
                if *opened && *depth <= 0 {
                    out.push((std::mem::take(name), std::mem::take(body)));
                    open = None;
                }
                continue;
            }
            let trimmed = line.trim_start();
            let Some(rest) = PREFIXES
                .iter()
                .find_map(|p| trimmed.strip_prefix(p).filter(|_| trimmed.starts_with(p)))
            else {
                continue;
            };
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if name.is_empty() {
                continue;
            }
            let depth = depth_change(line);
            let opened = line.contains('{');
            // A one-line definition has already opened and closed its body.
            if opened && depth <= 0 {
                out.push((name, line.to_string()));
            } else {
                open = Some((name, format!("{line}\n"), depth, opened));
            }
        }
        out
    }

    /// **REP-45.** A wrapper that can raise a fault says so in the manifest.
    ///
    /// `VecPush` and the two `DequePush*` rows were declared `Allocates` — which
    /// makes `RuntimeSymbol::faults()` answer `false` — while all three call
    /// `adopt_or_reject`, which ends in `set_fault(ctx, TYPE_MISMATCH)`. The
    /// identical wrapper `praxis_grid_set` makes the same call and *is* declared
    /// `Faults`, so the manifest was internally inconsistent about one
    /// operation. The consequence is not cosmetic: on a rejection the element is
    /// silently dropped, the wrapper answers the Unit sentinel, and the fault is
    /// observed by some later unrelated check — at the wrong source location,
    /// after the program has computed and possibly printed an answer from a
    /// collection that quietly lost a value.
    ///
    /// A hand-corrected row drifts again, so this is the invariant instead, in
    /// the shape S18 used for the catalog: the file is read at compile time,
    /// each `praxis_*` wrapper's body is walked, and any body that can reach
    /// `set_fault` — directly or through a helper defined here, transitively —
    /// must belong to a symbol whose row says `faults()`.
    ///
    /// One direction only. A row may declare a fault the reader cannot see: the
    /// arithmetic wrappers are generated by `checked_int_binop!` and have no
    /// textual definition at all, and a future wrapper may fault through a
    /// helper in another module. Those are false negatives — this test is weaker
    /// than the truth, never stricter — and the direction it does check is the
    /// one that produces wrong answers.
    #[test]
    fn a_wrapper_that_can_raise_a_fault_declares_that_it_faults() {
        let defs = functions_in_this_file();
        // Fixed point: a function faults if it calls `set_fault`, or calls
        // something that does.
        let mut faulting: std::collections::HashSet<String> =
            ["set_fault".to_string()].into_iter().collect();
        loop {
            let mut grew = false;
            for (name, body) in &defs {
                if faulting.contains(name) {
                    continue;
                }
                if faulting.iter().any(|f| body.contains(&format!("{f}("))) {
                    faulting.insert(name.clone());
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }

        let mut checked = 0usize;
        for (name, _) in &defs {
            let Some(sym) = praxis_stdlib::abi::RuntimeSymbol::from_name(name) else {
                continue;
            };
            if !faulting.contains(name) {
                continue;
            }
            assert!(
                sym.faults(),
                "{name} can reach `set_fault`, but its manifest row says it \
                 cannot fault — so no `CheckFault` follows the call and the \
                 fault is observed somewhere else entirely"
            );
            checked += 1;
        }
        // If the scan stopped finding wrappers, the assertion above would be
        // vacuous and this test would pass while saying nothing.
        assert!(
            checked >= 20,
            "expected the fault-raising wrappers to be found; saw {checked}"
        );
        // And the three rows this test was written for must be among the
        // wrappers it can see, or it would have been green on the defect.
        for name in [
            "praxis_vec_push",
            "praxis_deque_push_front",
            "praxis_deque_push_back",
        ] {
            assert!(
                faulting.contains(name),
                "{name} reaches `set_fault` through `adopt_or_reject`; a scan \
                 that cannot see that cannot hold the invariant"
            );
        }
    }
}
