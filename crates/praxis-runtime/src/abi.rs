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
    repr_c_vec::ReprCVec,
};
pub use praxis_stdlib::abi::{AbiKind, AbiRet, AbiSig, Effect, RuntimeSymbol};

/// The runtime ABI version for this build. Bump it whenever a program compiled
/// against one version could be misled by a runtime of another.
///
/// Three classes of change owe a bump:
///
/// * **Layout, calling convention, or signature.** Any move of a field
///   generated code reads; any change to a `praxis_*` wrapper's parameters or
///   return type; and any change to the *size* of
///   [`RuntimeContext`](crate::RuntimeContext), because a host that built a
///   context of the previous size would have this runtime read past its end.
/// * **Meaning, with the layout unchanged.** A field or wrapper whose bits stay
///   where they were but stand for something else: a slot whose "absent" value
///   moves from a sentinel to all-zero `None`; a counter that counted calls up
///   to a limit now counting a native-stack budget down; a wrapper whose
///   manifest [`Effect`] row changes, and with it whether the caller must emit a
///   `CheckFault` after it.
/// * **A new dependency of generated code.** Nothing moves, but the compiler
///   starts reading a field it never read, so repacking that field becomes a
///   generated-code change from then on.
///
/// All three are about what generated code or a host can observe, so a field
/// with no reader outside `praxis-runtime` whose displacement and width do not
/// move owes nothing, however much the thing it points at changes.
/// `RuntimeContext.native_roots` is the one such field: its writers are
/// `Runtime::context` and `NativeScope`, its only reader is
/// `RuntimeRoots::from_context`, and no `offset_of!` in
/// `crates/praxis-codegen-cranelift/src/lower.rs` names it (ADR-114).
///
/// What generated code reads today — and therefore what the third class now
/// covers — is: [`RuntimeContext`](crate::RuntimeContext)'s own `shadow`,
/// `stack_left`, `pending_fault`, `unit_ref`, `true_ref`, `false_ref`,
/// `small_ints`, `small_chars`, `descriptors`, `debug_frames`, `debug_values`
/// and `heap`; [`Fault::KIND_OFFSET`](crate::Fault::KIND_OFFSET) and
/// [`GcHeader::DESCRIPTOR_OFFSET`](crate::GcHeader::DESCRIPTOR_OFFSET);
/// `EnumPayload::tag`; the two pacing words at
/// [`Heap::BYTES_SINCE_COLLECT_OFFSET`](crate::Heap::BYTES_SINCE_COLLECT_OFFSET)
/// and [`Heap::COLLECT_THRESHOLD_OFFSET`](crate::Heap::COLLECT_THRESHOLD_OFFSET),
/// plus `Heap::live_count`; `PageHeader`'s and `GcHeader`'s fields, which the
/// inline claim path both reads and writes; and `VecPayload`'s and
/// `BitSetPayload`'s leading words, reached through
/// [`ReprCVec`](crate::ReprCVec). A debug value slot's word is an
/// `Option<GcRef>` only when the `DebugLocalMeta` beside it says so: a temp
/// whose box was elided stores its payload raw (ADR-120).
///
/// One numeral per build, not one per change — a version is a statement about a
/// build, so several packages landing in the same round share a single bump,
/// owned by one of them, rather than taking four bumps in four worktrees that a
/// merge would silently reduce to the last.
pub const RUNTIME_ABI_VERSION: u32 = 20;

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
const COMPILER_EXPECTED_ABI_VERSION: u32 = 20;

// ---------------------------------------------------------------------------
// The runtime symbol table.
// ---------------------------------------------------------------------------

/// The address of a runtime wrapper, for the JIT to resolve an import to.
///
/// This match is the **only** symbol→address table in the workspace, and it is
/// exhaustive over [`RuntimeSymbol`]: adding a row to the manifest without
/// giving it an address here is a compile error. There is no fallback — the JIT
/// never reaches `dlsym`, so a symbol the compiler failed to register cannot
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
        RuntimeSymbol::Breakpoint => praxis_breakpoint as *const (),
        RuntimeSymbol::BitsetRemove => praxis_bitset_remove as *const (),
        RuntimeSymbol::BoolLoad => praxis_bool_load as *const (),
        RuntimeSymbol::CharLoad => praxis_char_load as *const (),
        RuntimeSymbol::CharToInt => praxis_char_to_int as *const (),
        RuntimeSymbol::CharToText => praxis_char_to_text as *const (),
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
        RuntimeSymbol::DequeSet => praxis_deque_set as *const (),
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
        RuntimeSymbol::GridFilled => praxis_grid_filled as *const (),
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
        RuntimeSymbol::IntCheckedAdd => praxis_int_checked_add as *const (),
        RuntimeSymbol::IntCheckedMul => praxis_int_checked_mul as *const (),
        RuntimeSymbol::IntCheckedSub => praxis_int_checked_sub as *const (),
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
        RuntimeSymbol::IntSaturatingAdd => praxis_int_saturating_add as *const (),
        RuntimeSymbol::IntSaturatingMul => praxis_int_saturating_mul as *const (),
        RuntimeSymbol::IntSaturatingSub => praxis_int_saturating_sub as *const (),
        RuntimeSymbol::IntSign => praxis_int_sign as *const (),
        RuntimeSymbol::IntSub => praxis_int_sub as *const (),
        RuntimeSymbol::IntToChar => praxis_int_to_char as *const (),
        RuntimeSymbol::IntToFloat => praxis_int_to_float as *const (),
        RuntimeSymbol::IntToText => praxis_int_to_text as *const (),
        RuntimeSymbol::IntWrappingAdd => praxis_int_wrapping_add as *const (),
        RuntimeSymbol::IntWrappingMul => praxis_int_wrapping_mul as *const (),
        RuntimeSymbol::IntWrappingSub => praxis_int_wrapping_sub as *const (),
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
        RuntimeSymbol::RaiseDivByZeroIf => praxis_raise_div_by_zero_if as *const (),
        RuntimeSymbol::RaiseEmptyCollection => praxis_raise_empty_collection as *const (),
        RuntimeSymbol::RaiseIntOverflowIf => praxis_raise_int_overflow_if as *const (),
        RuntimeSymbol::RaiseStackOverflow => praxis_raise_stack_overflow as *const (),
        RuntimeSymbol::RecordField => praxis_record_field as *const (),
        RuntimeSymbol::RecordSetField => praxis_record_set_field as *const (),
        RuntimeSymbol::RunParser => praxis_run_parser as *const (),
        RuntimeSymbol::SetContains => praxis_set_contains as *const (),
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
        RuntimeSymbol::TextConcat => praxis_text_concat as *const (),
        RuntimeSymbol::TextGet => praxis_text_get as *const (),
        RuntimeSymbol::TextFloat => praxis_text_float as *const (),
        RuntimeSymbol::TextInt => praxis_text_int as *const (),
        RuntimeSymbol::TextIsEmpty => praxis_text_is_empty as *const (),
        RuntimeSymbol::TextLen => praxis_text_len as *const (),
        RuntimeSymbol::TupleGet => praxis_tuple_get as *const (),
        RuntimeSymbol::TupleSet => praxis_tuple_set as *const (),
        RuntimeSymbol::ValueCmp => praxis_value_cmp as *const (),
        RuntimeSymbol::ValueToText => praxis_value_to_text as *const (),
        RuntimeSymbol::VarCellGet => praxis_var_cell_get as *const (),
        RuntimeSymbol::VarCellSet => praxis_var_cell_set as *const (),
        RuntimeSymbol::VecFrequencies => praxis_vec_frequencies as *const (),
        RuntimeSymbol::VecGet => praxis_vec_get as *const (),
        RuntimeSymbol::VecSet => praxis_vec_set as *const (),
        RuntimeSymbol::VecIsEmpty => praxis_vec_is_empty as *const (),
        RuntimeSymbol::VecJoin => praxis_vec_join as *const (),
        RuntimeSymbol::VecLen => praxis_vec_len as *const (),
        RuntimeSymbol::VecChunks => praxis_vec_chunks as *const (),
        RuntimeSymbol::VecFilled => praxis_vec_filled as *const (),
        RuntimeSymbol::VecNew => praxis_vec_new as *const (),
        RuntimeSymbol::VecPush => praxis_vec_push as *const (),
        RuntimeSymbol::VecReversed => praxis_vec_reversed as *const (),
        RuntimeSymbol::VecSorted => praxis_vec_sorted as *const (),
        RuntimeSymbol::VecSortedByKey => praxis_vec_sorted_by_key as *const (),
        RuntimeSymbol::VecToText => praxis_vec_to_text as *const (),
        RuntimeSymbol::VecUnique => praxis_vec_unique as *const (),
        RuntimeSymbol::VecWindows => praxis_vec_windows as *const (),
        RuntimeSymbol::WriteStdout => praxis_write_stdout as *const (),
    };
    ptr as *const u8
}

// ---------------------------------------------------------------------------
// The panic backstop (§9.2, §10.4)
// ---------------------------------------------------------------------------

/// The defined dummy a wrapper returns when it has raised a fault and has no
/// real answer (§10.4).
///
/// A wrapper's return type is part of the ABI, so "return nothing" is not
/// available; every type generated code can receive needs a value that is safe
/// to hold and never read. `GcRef` is `NonNull`, so its dummy is the context's
/// `Unit` — the same sentinel the fault epilogue returns — and integer zero
/// would be an invalid reference, not a dummy.
pub(crate) trait AbiSentinel {
    /// # Safety
    /// `ctx` must be null or point at a live, wired `RuntimeContext`.
    unsafe fn sentinel(ctx: *mut RuntimeContext) -> Self;
}

impl AbiSentinel for () {
    unsafe fn sentinel(_ctx: *mut RuntimeContext) {}
}

impl AbiSentinel for i64 {
    unsafe fn sentinel(_ctx: *mut RuntimeContext) -> i64 {
        0
    }
}

impl AbiSentinel for GcRef {
    unsafe fn sentinel(ctx: *mut RuntimeContext) -> GcRef {
        // SAFETY: the caller guarantees a live, wired context. A null one
        // cannot produce a `GcRef` at all, and `abi_panic_escaped` refuses to
        // reach here with one.
        unsafe { unit_sentinel(ctx) }
    }
}

impl<T> AbiSentinel for *mut T {
    unsafe fn sentinel(_ctx: *mut RuntimeContext) -> *mut T {
        std::ptr::null_mut()
    }
}

impl<T> AbiSentinel for *const T {
    unsafe fn sentinel(_ctx: *mut RuntimeContext) -> *const T {
        std::ptr::null()
    }
}

/// Translate a panic that reached an `extern "C"` boundary into a fault, and
/// return the boundary's defined dummy.
///
/// **This must never fire.** Totality is the contract — a wrapper validates its
/// arguments and reports a bad one as a fault — and this exists because a
/// contract that cannot be checked is a hope. A Rust panic unwinding out of
/// `extern "C"` into Cranelift frames is undefined behaviour; the guard turns
/// the one outcome nobody can reason about into the one §10.4 already
/// specifies, and does it uniformly so no future wrapper has to remember.
///
/// The kind is [`FaultKind::Panic`](crate::FaultKind::Panic), with a message
/// naming the wrapper. It is deliberately not a new `FaultKind::Internal`: a
/// new kind is a `#[repr(C)]` layout change that costs an ABI bump (ADR-075),
/// and `Panic` plus a message that names the function carries strictly more
/// information for the crash report (§9.4) than a bare kind would.
///
/// # Safety
/// `ctx` must be null or point at a live, wired `RuntimeContext`.
#[cold]
#[inline(never)]
pub(crate) unsafe fn abi_panic_escaped<T: AbiSentinel>(
    ctx: *mut RuntimeContext,
    wrapper: &'static str,
) -> T {
    if ctx.is_null() {
        // There is no fault slot to write and no `Unit` to return. Aborting is
        // the only defined answer left, and it is still better than unwinding
        // into generated frames.
        std::process::abort();
    }
    // SAFETY: the caller guarantees a live, wired context.
    unsafe { set_fault(ctx, RaisedFault::PANIC) };
    let message = format!("internal error: a panic escaped the runtime wrapper `{wrapper}`");
    // SAFETY: as above.
    unsafe { set_fault_message(ctx, message.clone()) };
    if !panic_fault_is_observable(wrapper) {
        // **The dummy has to be unreachable where nobody will look at the
        // fault.** Generated code tests the fault slot only where MIR emitted
        // a `CheckFault`, and MIR emits one only after a call it classifies as
        // faultable — so for a wrapper the manifest declares non-faulting there
        // is *no* check by construction, and returning `unit_sentinel` would
        // hand a `Unit` into a slot generated code believes holds a Record, a
        // Tuple or a closure. Aborting with the message is the only answer that
        // does not introduce a descriptor/payload confusion.
        eprintln!("{message}");
        std::process::abort();
    }
    // SAFETY: as above.
    unsafe { T::sentinel(ctx) }
}

/// Whether generated code can be expected to observe a `Panic` fault raised by
/// `wrapper` — i.e. whether the wrapper's defined dummy is ever consumed under
/// a fault check rather than as a value.
///
/// The manifest is the authority: a symbol declared [`Effect::Pure`] or
/// [`Effect::Allocates`] cannot be followed by a `CheckFault`. **That is
/// `praxis_mir::verify`'s rule, not a claim restated here** — its
/// `RedundantFaultCheck` variant rejects a check after an instruction that
/// cannot fault, so this function's premise is enforced rather than assumed
/// (ADR-088). A wrapper the manifest does not name at all is in the same
/// position and is treated the same way.
///
/// The converse is *not* claimed here: a declared-faulting wrapper's call sites
/// are MIR's business. What this rules out is the class where the check is
/// impossible.
fn panic_fault_is_observable(wrapper: &str) -> bool {
    praxis_stdlib::abi::RuntimeSymbol::from_name(wrapper).is_some_and(|s| s.faults())
}

/// Wrap an `extern "C"` wrapper's body so a panic becomes a fault.
///
/// Every `#[no_mangle] extern "C" fn` in this crate has its body inside one of
/// these, and `every_no_mangle_wrapper_is_behind_the_panic_guard` is the test
/// that keeps it that way — a new wrapper that forgets is a failing test rather
/// than a latent abort.
macro_rules! abi_guard {
    ($wrapper:expr, $ctx:expr, $body:block) => {{
        // `AssertUnwindSafe`: the body's captures are the wrapper's own
        // arguments, which are `Copy` C types, and the fault protocol is how
        // the runtime already communicates a half-finished operation.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || $body)) {
            Ok(value) => value,
            // SAFETY: `ctx` is the wrapper's own context argument, whose
            // validity every wrapper's `# Safety` section already requires.
            Err(_) => unsafe { crate::abi::abi_panic_escaped($ctx, $wrapper) },
        }
    }};
}

pub(crate) use abi_guard;

// ---------------------------------------------------------------------------
// Internals the wrappers share.
// ---------------------------------------------------------------------------

/// Raise `fault` on `ctx`'s fault slot (§10.4). Does nothing if the context's
/// fault pointer is null (a misuse, but never panics across the ABI).
///
/// Takes a [`RaisedFault`], not a `FaultKind`: every raise names a kind that
/// describes it, and "no fault" is not spellable here.
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

#[inline]
/// Charge the pacer for a collection's buffer growing (ADR-121).
///
/// `before` and `after` are the same payload's `owned_bytes()`, read either
/// side of a mutation that may reallocate. Nothing is charged when the buffer
/// did not grow, which is the overwhelmingly common case: amortized doubling
/// means a `push` reallocates once every *n* pushes, so this is a compare and a
/// not-taken branch on the hot path.
///
/// # Why every growing wrapper has to call this
///
/// `Heap::alloc_raw` charges `stride + owned_bytes_of(payload)` once, at
/// construction. Leaving later growth uncharged relies on the elements
/// themselves being paced allocations, so that the residual under-count is only
/// the spine — and scalar promotion deletes exactly those element allocations,
/// so an allocation-light program that grows a large buffer paces nothing.
/// Uncharged, `bfs` runs 6 collections instead of 41 and reaches a peak
/// resident set of 224 MiB against 61 (ADR-121).
///
/// So the rule is: **a wrapper that can grow a buffer charges the growth.** The
/// `growth_charging_tests` module below has one case per such wrapper, because
/// the failure mode is silent — a program that simply stops collecting, which
/// reads as a leak nobody connects to the wrapper that was added.
fn charge_growth(ctx: *mut RuntimeContext, before: usize, after: usize) {
    let Some(grown) = after.checked_sub(before).filter(|g| *g != 0) else {
        return;
    };
    if ctx.is_null() {
        return;
    }
    // SAFETY: every caller is inside `abi_guard!`, which established that `ctx`
    // is live and wired; the null check above covers the guard's own edge.
    unsafe { heap(ctx).charge_owned_growth(grown) };
}

/// Trigger a collection on allocation pressure, rooting from the context
/// (§12.4, ADR-019, ADR-101). Called by every allocating `praxis_*` wrapper.
/// Safe to call with a null/unwired context (no-op).
///
/// The roots are every **strong** arm of
/// [`RuntimeRoots`](crate::roots::RuntimeRoots) — the shadow stack, the ambient
/// input buffer, a parse failure's partial value, a runtime-owned crash
/// snapshot, and the native root store. All five, not the shadow stack alone:
/// host-driven allocation and the parser interpreter push no shadow frame, and
/// the other four owners are reachable regardless. The debug arm is
/// deliberately *weak* (ADR-106): it names storage without keeping it alive, so
/// it is cleared after the sweep rather than traced here.
unsafe fn maybe_collect(ctx: *mut RuntimeContext) {
    if ctx.is_null() {
        return;
    }
    // SAFETY: ctx is live and wired.
    let roots = unsafe { crate::roots::RuntimeRoots::from_context(ctx) };
    unsafe { heap(ctx).maybe_collect(&roots) };
}

/// Pace the collector and mint the token one allocation needs.
///
/// Every `praxis_*` wrapper reaches the heap through [`gc_alloc`] or
/// [`gc_alloc_owned`], which call this — and even a wrapper that reached
/// `Heap::alloc` directly would have to come through here, because the token
/// has no other producer. That is the whole point: an allocation that skipped
/// the pacer would let a program whose pressure comes from `Text`, `.len()` or
/// checked arithmetic run arbitrarily long without the collector ever being
/// offered a turn.
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
/// call: a value of the wrong type is an `E0308`, and an *untyped* literal
/// infers as the payload type instead of defaulting to `i32`. A bare descriptor
/// reference with `T` free would let a width mismatch reach the heap and abort
/// the process from inside `extern "C"`, which §10.4 forbids.
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
/// [`gc_alloc`]'s counterpart for the payloads no [`Payload<T>`] can describe.
/// The type is named **once**, as `P`: [`Heap::alloc_payload`] derives the size,
/// the alignment and the write from it, so no wrapper restates all three and
/// keeps them in agreement by hand.
///
/// The payload arrives as a producer rather than a value, and that is
/// load-bearing: `init` runs *after* [`safepoint`] has given the collector its
/// turn, so a payload built out of bare `GcRef`s — `vec![fill; n]` in
/// [`praxis_vec_filled`] and [`praxis_grid_filled`] — is never a `Vec<GcRef>`
/// live across a collection with no root set able to see it.
///
/// # Safety
/// `ctx` must be live and wired, and `descriptor` must be `P`'s own descriptor
/// ([`Heap::alloc_payload`]'s contract).
#[inline]
unsafe fn gc_alloc_owned<P>(
    ctx: *mut RuntimeContext,
    descriptor: &'static TypeDescriptor,
    init: impl FnOnce() -> P,
) -> GcRef {
    // SAFETY: caller upholds ctx validity.
    let (h, sp) = unsafe { safepoint(ctx) };
    // `init()` is evaluated here, downstream of the safepoint above — see the
    // ordering note in this function's doc.
    // SAFETY: forwarded from this function's contract.
    unsafe { h.alloc_payload(sp, descriptor, init()) }
}

/// The immortal `Bool` for `value`, off the context's cached singletons.
///
/// Never an allocation: there are exactly two `Bool` values and the runtime
/// minted both at startup, so every comparison, `contains` and `is_empty` a
/// program evaluates answers with a singleton rather than consuming arena
/// storage permanently. It is also what makes those manifest rows honestly
/// `Effect::Pure`: nothing here can collect, so the call site is not a
/// safepoint.
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

/// The `Int` for `value`: the interned immortal when it is small
/// ([`crate::small_int`]), a fresh allocation otherwise.
///
/// [`bool_ref`]'s shape, one step less absolute: `Bool` has two values so it is
/// always the singleton, while `Int` has a *range* that is interned and an
/// unbounded remainder that is not. Every wrapper that answers an `Int` reaches
/// the heap through here, so the interning covers not just literals but
/// `Vec.len()`, a `Counter` bump, an enum tag, a comparison's index and the
/// result of arithmetic — which is where most of a real program's small `Int`s
/// come from.
///
/// # It paces even when it does not allocate, and that is deliberate
///
/// The manifest declares `VecLen`, `MapLen`, `EnumTag`, `TextLen`, `CounterGet`
/// and two dozen more `Effect::Allocates`, which is generated code's contract
/// that the call site is a GC safepoint. If this returned before [`safepoint`],
/// a loop whose only allocations were small `Int`s would never offer the
/// collector a turn — the collector's *only* trigger is the pacing counter, and
/// nothing else in such a loop touches it. So the token is minted and then
/// dropped: [`Safepoint`] is `#[must_use]`, so `drop(sp)` is the honest spelling
/// of "the collector got its turn and we allocated nothing", and it is a
/// compile error to forget which of the two happened.
///
/// The interned path therefore costs a threshold compare and a range test rather
/// than an allocation. `Inst::ConstGc` is what removes even that, but only for a
/// *literal*, where the compiler knows the value and no manifest row applies.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[inline]
unsafe fn int_ref(ctx: *mut RuntimeContext, value: i64) -> GcRef {
    // SAFETY: caller upholds ctx validity.
    let (h, sp) = unsafe { safepoint(ctx) };
    match crate::small_int::index_of(value) {
        Some(i) => {
            drop(sp);
            // SAFETY: `index_of` bounds `i` by `SMALL_INT_COUNT`, and
            // `Runtime::context` points `small_ints` at a table of exactly that
            // length whose slot `i` holds `SMALL_INT_MIN + i`.
            unsafe { *(*ctx).small_ints.add(i) }
        }
        None => h.alloc(sp, scalars::INT_PAYLOAD, value),
    }
}

/// The `Char` for `code`: the interned immortal when it is ASCII
/// ([`crate::small_char`]), a fresh allocation otherwise.
///
/// [`int_ref`]'s shape and its argument (ADR-107). Every wrapper that answers a
/// `Char` reaches the heap through here — [`checked_alloc_char`], which is both
/// `praxis_alloc_char` and `praxis_int_to_char`; [`praxis_text_get`], which is
/// both `t[i]` and every step of `for c in t`; and [`default_cell`], which is a
/// `Grid[Char]`'s fill. `praxis_text_get` is the one that matters for real code:
/// an AoC-shaped program that walks a line of text would otherwise box a fresh
/// object per character read, and every character of such a line is ASCII.
///
/// # It paces even when it does not allocate
///
/// The manifest declares `TextGet`, `AllocChar` and `IntToChar`
/// `Effect::AllocatesAndFaults`, which is generated code's contract that the call
/// site is a GC safepoint. If this returned before [`safepoint`], `for c in text`
/// over an ASCII line would never offer the collector a turn — the collector's
/// *only* trigger is the pacing counter, and a loop that reads characters and
/// compares them touches nothing else that would bump it. So the token is minted
/// and then dropped: [`Safepoint`] is `#[must_use]`, so `drop(sp)` is the honest
/// spelling of "the collector got its turn and we allocated nothing", and it is a
/// compile error to forget which of the two happened. Pinned by
/// `char_ref_paces_the_collector_even_when_it_answers_from_the_table`.
///
/// Unlike `int_ref` there is no `Inst::ConstGc` that removes even the pacing
/// check: that instruction exists for a *literal*, and the language has no
/// character literal (ADR-107 Decision 2).
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`, and `code` must be a valid
/// Unicode scalar value — every caller has already established this, either by
/// [`checked_alloc_char`]'s range check or by starting from a Rust `char`.
#[inline]
unsafe fn char_ref(ctx: *mut RuntimeContext, code: u32) -> GcRef {
    debug_assert!(
        crate::scalars::is_valid_char(code),
        "char_ref's callers validate first"
    );
    // SAFETY: caller upholds ctx validity.
    let (h, sp) = unsafe { safepoint(ctx) };
    match crate::small_char::index_of(code) {
        Some(i) => {
            drop(sp);
            // SAFETY: `index_of` bounds `i` by `SMALL_CHAR_COUNT`, and
            // `Runtime::context` points `small_chars` at a table of exactly that
            // length whose slot `i` holds code point `i`.
            unsafe { *(*ctx).small_chars.add(i) }
        }
        None => h.alloc(sp, scalars::CHAR_PAYLOAD, code),
    }
}

/// A fresh owned `Text` holding `s`.
///
/// [`bool_ref`]/[`int_ref`]/[`char_ref`]'s place in the file but not their
/// shape: there is nothing interned to answer from — every `Text` is a distinct
/// object — so this always allocates, and always paces.
///
/// `impl Into<Box<str>>` is what lets every text-producing wrapper share this
/// one allocation without a defensive `.clone()`: a `String` from a renderer, a
/// `Box<str>` the caller already built ([`praxis_alloc_text`]), a `&str`.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[inline]
unsafe fn text_ref(ctx: *mut RuntimeContext, s: impl Into<Box<str>>) -> GcRef {
    // SAFETY: caller upholds ctx validity; `TextPayload` is `TEXT`'s payload type.
    unsafe {
        gc_alloc_owned(ctx, &crate::text::TEXT, || {
            crate::text::TextPayload::owned(s)
        })
    }
}

/// Read `r`'s payload through a [`Payload`] handle, first checking that `r`
/// really is that handle's type.
///
/// This is the reader to reach for whenever a wrapper receives a `GcRef` it did
/// not itself allocate — a value handed back by a program's closure, most of
/// all. Two mistakes are impossible through it: reading a value of the wrong
/// *type* (the identity check answers `None`, and the caller decides whether
/// that is a `TypeMismatch` fault), and reading the right type at the wrong
/// *width* (the width is `size_of::<T>()`, which [`Payload::new`] proved is the
/// descriptor's width when the handle was declared). Reading a one-byte `Bool`
/// payload with an eight-byte `int_payload` is the class it rules out.
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
/// this reads eight bytes, and the descriptor check is all that stands between
/// it and a narrower payload.
///
/// # The width check is a branch, not a `debug_assert`
///
/// A `debug_assert` is not a bound — it compiles out of a release build, leaving
/// an eight-byte read against a descriptor that may be narrower or zero bytes
/// wide, so the two profiles would answer differently and the wrong one is the
/// one users get. As an ordinary branch it holds in every profile: the read
/// cannot happen. What happens instead is ADR-080's defined panic path —
/// `abi_guard` catches it, raises `RaisedFault::PANIC` with a message naming
/// the wrapper, and either faults into the crash debugger (a wrapper the
/// manifest declares faultable) or prints that message and aborts (one it does
/// not, which is `praxis_int_load`'s case). The guard is a memory-safety check
/// on a raw read, not a stand-in for a type system.
///
/// This stays `-> i64` rather than becoming fallible: sixty-odd wrappers read
/// through it, and a `ctx`-threading signature change is a larger edit than the
/// one memory safety needs.
#[inline]
unsafe fn int_payload(r: GcRef) -> i64 {
    // SAFETY: `read_scalar` proves `r`'s descriptor *is* `INT` before reading,
    // so the eight bytes are in bounds and are an `i64`. The compiler only emits
    // these calls with Int-typed operands, and a fault that would feed a
    // non-`Int` (the Unit sentinel, say) into an arithmetic wrapper is diverted
    // by `Inst::CheckFault` before it gets here (§10.4).
    unsafe { read_scalar(r, scalars::INT_PAYLOAD) }
        .unwrap_or_else(|| scalar_type_mismatch("int_payload", "Int", r.descriptor().name))
}

/// The refusal every scalar reader shares, out of line so the check costs a
/// never-taken branch on the hot path.
///
/// `#[cold]` and `#[inline(never)]` are what let the check be unconditional, and
/// it must be unconditional: a `debug_assert` compiles out, so a release build
/// would do an out-of-bounds heap read where a debug build aborted.
///
/// A panic here is ADR-080's defined path: `abi_guard!` catches it, raises
/// `RaisedFault::PANIC` naming the wrapper, and either faults into the crash
/// debugger or prints the message and aborts. That is the backstop, not the
/// primary defence — a raw scalar read must prove its own width whatever the
/// type system believes.
#[cold]
#[inline(never)]
fn scalar_type_mismatch(what: &'static str, want: &'static str, found: &'static str) -> ! {
    panic!("{what} wants a `{want}` payload; this value is a `{found}` (REP-56)");
}

/// [`praxis_alloc_text`]'s refusal when its buffer is not UTF-8 — a violated
/// precondition, not a runtime condition (ADR-111).
///
/// **Why this is a panic and not a fault.** The precondition is the one that is
/// actually true: the compiler's bytes are a Rust `&str` unbroken from
/// `Lit::Text(String)` through `AllocKind::Text { value: String }` to
/// `Generation::alloc_str`, and the one caller in this crate that holds raw
/// *host* bytes — [`praxis_get_input`] — validates them itself and raises
/// `InvalidText` there, where the `read` can observe it. Spelling it as a fault
/// instead would cost a `CheckFault` after every text literal for a fault no
/// generated call site can raise.
///
/// **Why the `from_utf8` call above stays, in every profile.** This is
/// [`scalar_type_mismatch`]'s argument verbatim and it is why that function is
/// the neighbour: a `debug_assert` is not a bound, because it compiles out of a
/// release build. What would be left in release is a `Box<str>` built from bytes
/// that are not UTF-8, which [`crate::text::text_str`] later hands out as a
/// `&str` — so the two profiles would answer differently and the wrong one is
/// the one users get. `from_utf8_unchecked` is the same hole with the check
/// deleted rather than compiled out. The unconditional branch costs a
/// never-taken jump to this cold callee, which is the price ADR-102 §1 already
/// established for the inline scalar loads.
///
/// **It must not reach `set_fault`, and that is enforced.**
/// `a_wrapper_that_can_raise_a_fault_declares_that_it_faults` computes a textual
/// fixed point over this file: a body that can reach `set_fault`, directly or
/// through a helper defined here, must belong to a symbol whose manifest row
/// says it faults. `AllocText`'s row is `Effect::Allocates`, so a refusal
/// spelled as a fault would fail that test.
///
/// The end-to-end path on a violation is ADR-080's: panic → `abi_guard!`
/// catches → `panic_fault_is_observable("praxis_alloc_text")` reads the
/// `Allocates` row and answers `false` → the message is printed and the process
/// aborts. That is the same outcome `praxis_int_load` gives a wrong descriptor,
/// and it falls out of the row change with no code of its own.
#[cold]
#[inline(never)]
fn text_bytes_are_not_utf8(len: usize) -> ! {
    panic!(
        "praxis_alloc_text was handed {len} bytes that are not valid UTF-8; its \
         `# Safety` contract requires them to be (ADR-111). A host with untrusted \
         bytes must validate them first, as `praxis_get_input` does."
    );
}

/// The Unit `GcRef` returned on fault paths as the "defined dummy" (§10.4).
/// Reads the cached immortal `unit_ref` from the context, which is stable for
/// the program's lifetime.
#[inline]
unsafe fn unit_sentinel(ctx: *mut RuntimeContext) -> GcRef {
    unsafe { (*ctx).unit_ref }
}

/// The slot a source index `i` names in a container of `len` elements, or
/// `None` when it is outside `0..len` (§9.2, §11.1).
///
/// **The one place a source index becomes a `usize`.** The `< 0` test has to
/// run before the cast: a negative `i64` casts to a value near `usize::MAX` and
/// would sail straight past a bare length comparison. Every accessor shares this
/// one copy, so no single site can drop the guard invisibly. The construction
/// side guards the same hazard with a named
/// [`GridExtent`](crate::collections::GridExtent).
///
/// `Vec` and `Deque` indexing is one language rule applied to two containers,
/// so they share this rather than each spelling it out.
fn linear_index(i: i64, len: usize) -> Option<usize> {
    if i < 0 {
        return None;
    }
    // Non-negative above, so the cast is exact.
    let i = i as usize;
    (i < len).then_some(i)
}

/// The row-major slot `(x, y)` names in a `width × height` grid, or `None` when
/// either axis falls outside it.
///
/// Both axes go through [`linear_index`], so the 2-D rule is the 1-D rule twice
/// and the signed-to-`usize` cast still exists in exactly one place. The
/// product cannot overflow: `y < height` and `x < width` with
/// `height = items.len() / width`, so the result is below `items.len()`.
fn cell_index(x: i64, y: i64, width: usize, height: usize) -> Option<usize> {
    let x = linear_index(x, width)?;
    let y = linear_index(y, height)?;
    Some(y * width + x)
}

/// [`linear_index`], raising `IndexOutOfBounds` when the index is out of range.
/// `None` means the fault is already set and the caller owes only its sentinel
/// return.
///
/// The raising wrapper is deliberately separate from the pure predicate,
/// because not every bounds question may fault. [`praxis_grid_contains`] is
/// declared `Pure` in the ABI manifest while `GridGet` is `Faults`, and MIR's
/// `RedundantFaultCheck` emits no `CheckFault` after a `Pure` call — so a fault
/// raised on every legitimate `false` would sit pending until some later check
/// mistook it for its own. Bounds-testing sites take [`cell_index`] /
/// [`linear_index`]; only sites the manifest says can fault take these.
///
/// # Safety
/// `ctx` must be live and wired.
unsafe fn checked_index(ctx: *mut RuntimeContext, i: i64, len: usize) -> Option<usize> {
    let idx = linear_index(i, len);
    if idx.is_none() {
        unsafe { set_fault(ctx, RaisedFault::INDEX_OUT_OF_BOUNDS) };
    }
    idx
}

/// [`cell_index`], raising `IndexOutOfBounds` when `(x, y)` is off the grid.
/// See [`checked_index`] for why the raising and the predicate are two
/// functions.
///
/// # Safety
/// `ctx` must be live and wired.
unsafe fn checked_cell(
    ctx: *mut RuntimeContext,
    x: i64,
    y: i64,
    width: usize,
    height: usize,
) -> Option<usize> {
    let idx = cell_index(x, y, width, height);
    if idx.is_none() {
        unsafe { set_fault(ctx, RaisedFault::INDEX_OUT_OF_BOUNDS) };
    }
    idx
}

// ---------------------------------------------------------------------------
// Allocation wrappers.
// ---------------------------------------------------------------------------

/// The `Int` for `value` (§4.3, §11.1) — the interned immortal when it is small
/// ([`crate::small_int`]), a fresh box otherwise.
///
/// The row stays `Effect::Allocates`, not `Pure` as `AllocBool`'s is: this
/// wrapper still allocates for an out-of-range value, so the call site is still
/// a GC safepoint and generated code must still spill its roots across it. The
/// interning is invisible to the caller by design.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext` whose `heap` is valid.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_int(ctx: *mut RuntimeContext, value: i64) -> GcRef {
    abi_guard!("praxis_alloc_int", ctx, {
        // `int_ref` paces first, rooted at the whole `RuntimeRoots`. The new object
        // is not yet a root, but it is returned by value to the caller, which spills
        // it — so it is safe across this collection (the *previous* allocation's
        // result was already spilled by the backend before this wrapper was called).
        // SAFETY: caller upholds the ctx/heap validity.
        unsafe { int_ref(ctx, value) }
    })
}

/// Allocate a boxed `Bool` from a 0/1 value (§4.3). Returns the immortal
/// singleton, never a fresh allocation.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_bool(ctx: *mut RuntimeContext, value: i64) -> GcRef {
    abi_guard!("praxis_alloc_bool", ctx, {
        // There are two `Bool` values, and the runtime allocated both at startup.
        // `value != 0` is true; `0` is false.
        // SAFETY: caller upholds ctx validity.
        let c = unsafe { &*ctx };
        if value != 0 {
            c.true_ref
        } else {
            c.false_ref
        }
    })
}

/// Allocate the `Unit` singleton (§4.3).
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_unit(ctx: *mut RuntimeContext) -> GcRef {
    abi_guard!("praxis_alloc_unit", ctx, {
        // The one `Unit` value, cached on the context for the fault path.
        // SAFETY: caller upholds ctx validity.
        unsafe { (*ctx).unit_ref }
    })
}

/// Allocate a boxed `Char` from a Unicode scalar value (§4.3). The `value`
/// is the `u32` code point carried as `i64` (the uniform scalar ABI width). If
/// the code point is not a valid scalar, the fault is set and the Unit sentinel
/// is returned (no panic crosses the ABI).
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_char(ctx: *mut RuntimeContext, value: i64) -> GcRef {
    abi_guard!("praxis_alloc_char", ctx, {
        // SAFETY: caller upholds ctx/heap validity.
        unsafe { checked_alloc_char(ctx, value) }
    })
}

/// Box an `i64` as a `Char`, or raise `InvalidChar` and answer the Unit sentinel.
///
/// The one place the `i64`-to-code-point rule is enforced, because there are two
/// doors into it — `praxis_alloc_char` (the parser and codegen's `AllocKind::Char`)
/// and `praxis_int_to_char` (`Int.to_char()`, ADR-086) — and a rule stated at both
/// goes stale at one.
///
/// **Range-check before narrowing.** `value as u32` truncates, so
/// `0x1_0000_0041` would silently become `'A'`. The scalar ABI is 64 bits wide;
/// a code point is not, and the conversion has to say so rather than wrap. The
/// surrogate range is rejected for the same reason: `char::from_u32` is what
/// decides, not a width.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
unsafe fn checked_alloc_char(ctx: *mut RuntimeContext, value: i64) -> GcRef {
    let Ok(code) = u32::try_from(value) else {
        unsafe { set_fault(ctx, RaisedFault::INVALID_CHAR) };
        return unsafe { unit_sentinel(ctx) };
    };
    if !crate::scalars::is_valid_char(code) {
        unsafe { set_fault(ctx, RaisedFault::INVALID_CHAR) };
        return unsafe { unit_sentinel(ctx) };
    }
    // SAFETY: caller upholds ctx/heap validity; code is a validated scalar.
    unsafe { char_ref(ctx, code) }
}

/// Allocate an owned `Text` from a UTF-8 byte buffer (§4.3, ADR-013).
///
/// **UTF-8 is the caller's precondition, and this wrapper cannot fault**
/// (ADR-111). Its row is `Effect::Allocates`, so `Inst::Alloc { AllocKind::Text }`
/// is followed by no `CheckFault` — `praxis_mir::verify` rejects one — and a
/// `Text` literal in a loop is hoisted into the preheader like a `Float` one
/// (ADR-108 §3). Handing this bytes that are not UTF-8 is a violated contract,
/// not a runtime condition, and it aborts through `text_bytes_are_not_utf8`
/// (whose doc carries the argument) rather than raising `InvalidText`.
///
/// A host that holds *untrusted* bytes validates them before calling. There is
/// exactly one such caller in this crate — [`praxis_get_input`], whose row is
/// `AllocatesAndFaults` — and it raises `InvalidText` itself, so the fault a
/// `read` can observe still lands at the `read` (§4.3, §7.10).
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
    abi_guard!("praxis_alloc_text", ctx, {
        let slice = if bytes.is_null() || len == 0 {
            &[]
        } else {
            // SAFETY: caller guarantees `bytes..bytes+len` is a valid, UTF-8 buffer.
            unsafe { std::slice::from_raw_parts(bytes, len) }
        };
        // The check is unconditional in every profile: it is the backstop on a
        // raw read, the same standing `read_scalar` has, and its argument is
        // written out at `text_bytes_are_not_utf8`. A violation refuses rather
        // than recovering lossily behind a fault nobody at a generated call site
        // could observe (ADR-111).
        let owned: Box<str> = match std::str::from_utf8(slice) {
            Ok(s) => s.into(),
            Err(_) => text_bytes_are_not_utf8(len),
        };
        // SAFETY: ctx/heap valid.
        unsafe { text_ref(ctx, owned) }
    })
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
    abi_guard!("praxis_int_load", _ctx, {
        // SAFETY: caller guarantees `r` is an Int.
        unsafe { int_payload(r) }
    })
}

/// Read a `Bool` payload as 0/1 (§10.3 transient scalar).
///
/// # Safety
/// `r` must be a valid `Bool` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_bool_load(_ctx: *mut RuntimeContext, r: GcRef) -> i64 {
    abi_guard!("praxis_bool_load", _ctx, {
        // Read the byte, then decide — never `*r.payload::<bool>()`. A Rust
        // `bool` whose byte is not 0 or 1 is an *invalid value*, and
        // materializing one is undefined behaviour whatever the read's bounds
        // are; `BoolPayload` is a `u8` precisely so the runtime never has to.
        // SAFETY: `read_scalar` bounds the read against `r`'s own descriptor.
        let byte = unsafe { read_scalar(r, scalars::BOOL_PAYLOAD) }.unwrap_or_else(|| {
            scalar_type_mismatch("praxis_bool_load", "Bool", r.descriptor().name)
        });
        i64::from(byte != 0)
    })
}

/// Read a `Char` payload as its `u32` code point widened to `i64` (§4.3).
///
/// # Safety
/// `r` must be a valid `Char` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_char_load(_ctx: *mut RuntimeContext, r: GcRef) -> i64 {
    abi_guard!("praxis_char_load", _ctx, {
        // SAFETY: `read_scalar` bounds the read against `r`'s own descriptor.
        let code = unsafe { read_scalar(r, scalars::CHAR_PAYLOAD) }.unwrap_or_else(|| {
            scalar_type_mismatch("praxis_char_load", "Char", r.descriptor().name)
        });
        i64::from(code)
    })
}

/// Allocate a boxed `Float` from an `i64` carrying the IEEE-754 binary64 bit
/// pattern (§4.3, §4.12). The uniform scalar ABI carries every payload as
/// `i64`; a float is transported as `f64::to_bits()` and reassembled here.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_float(ctx: *mut RuntimeContext, value: i64) -> GcRef {
    abi_guard!("praxis_alloc_float", ctx, {
        let f = f64::from_bits(value as u64);
        // SAFETY: caller upholds ctx/heap validity; all f64 values are valid Floats.
        unsafe { gc_alloc(ctx, scalars::FLOAT_PAYLOAD, f) }
    })
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
    abi_guard!("praxis_float_load", _ctx, {
        // Through `float_payload`, which goes through `read_scalar`: the read
        // proves its own width rather than taking the caller's word for it.
        //
        // It matters more since ADR-102: generated code reads a `Float`
        // payload inline behind a descriptor check, and this wrapper is the
        // cold path that check branches to. If it read unchecked, the two
        // would disagree about what a wrong descriptor means — the inline
        // path would refuse and the fallback would read anyway.
        //
        // SAFETY: caller guarantees `r` is a valid `GcRef`; `float_payload`
        // proves it is a `Float` before reading.
        unsafe { float_payload(r) }.to_bits() as i64
    })
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
    // SAFETY: `read_scalar` proves `r`'s descriptor is `FLOAT` before reading.
    unsafe { read_scalar(r, scalars::FLOAT_PAYLOAD) }
        .unwrap_or_else(|| scalar_type_mismatch("float_payload", "Float", r.descriptor().name))
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
    abi_guard!("praxis_int_to_float", ctx, {
        let i = unsafe { int_payload(r) };
        // SAFETY: ctx/heap valid; every widened int is a valid Float payload.
        unsafe { gc_alloc(ctx, scalars::FLOAT_PAYLOAD, i as f64) }
    })
}

/// `Char.to_int()` — the Unicode scalar value, as an `Int` (ADR-086). Never
/// faults: every valid scalar fits an `i64`.
///
/// This reads through [`read_scalar`] with the `Char` handle rather than
/// `int_payload`, because a `Char` payload is **four** bytes and an `i64` read
/// would take eight of them.
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Char` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_char_to_int(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    abi_guard!("praxis_char_to_int", ctx, {
        // SAFETY: caller guarantees `r` is a valid `GcRef`; `read_scalar` proves
        // the descriptor is `CHAR` before reading its four bytes.
        let code = unsafe { read_scalar(r, scalars::CHAR_PAYLOAD) }.unwrap_or_else(|| {
            scalar_type_mismatch("praxis_char_to_int", "Char", r.descriptor().name)
        });
        // SAFETY: ctx/heap valid; every scalar value is a valid Int payload.
        unsafe { int_ref(ctx, i64::from(code)) }
    })
}

/// `Int.to_char()` — the `Char` with this Unicode scalar value (ADR-086).
/// Faults (`InvalidChar`) on a negative value, one above `0x10FFFF`, or one in
/// the surrogate range: those are not scalar values and have no `Char`.
///
/// It is `Char.to_int()`'s partial half exactly as `Float.to_int()` is
/// `Int.to_float()`'s — the narrowing direction is the one that can fail. The
/// check lives in [`checked_alloc_char`] and is not restated here.
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_to_char(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    abi_guard!("praxis_int_to_char", ctx, {
        let value = unsafe { int_payload(r) };
        // SAFETY: caller upholds ctx/heap validity.
        unsafe { checked_alloc_char(ctx, value) }
    })
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
    abi_guard!("praxis_float_to_int", ctx, {
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
        unsafe { int_ref(ctx, f as i64) }
    })
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
    abi_guard!("praxis_float_abs", ctx, {
        let f = unsafe { float_payload(r) };
        unsafe { rebox_float(ctx, f.abs()) }
    })
}

/// `Float.sqrt()` — square root (§4.12). Negative inputs yield NaN (IEEE-754);
/// this never faults.
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Float` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_sqrt(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    abi_guard!("praxis_float_sqrt", ctx, {
        let f = unsafe { float_payload(r) };
        unsafe { rebox_float(ctx, f.sqrt()) }
    })
}

/// `Float.floor()` — round toward negative infinity (§4.12).
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Float` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_floor(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    abi_guard!("praxis_float_floor", ctx, {
        let f = unsafe { float_payload(r) };
        unsafe { rebox_float(ctx, f.floor()) }
    })
}

/// `Float.ceil()` — round toward positive infinity (§4.12).
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Float` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_ceil(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    abi_guard!("praxis_float_ceil", ctx, {
        let f = unsafe { float_payload(r) };
        unsafe { rebox_float(ctx, f.ceil()) }
    })
}

/// `Float.round()` — round half away from zero (§4.12, matches Rust's `f64::round`).
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Float` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_round(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    abi_guard!("praxis_float_round", ctx, {
        let f = unsafe { float_payload(r) };
        unsafe { rebox_float(ctx, f.round()) }
    })
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
    abi_guard!("praxis_float_sign", ctx, {
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
    })
}

/// `Float.is_nan()` — true iff NaN (§4.12).
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Float` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_is_nan(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    abi_guard!("praxis_float_is_nan", ctx, {
        let result = unsafe { float_payload(r) }.is_nan();
        // SAFETY: ctx valid; Bool immortal path.
        unsafe { bool_ref(ctx, result) }
    })
}

/// `Float.is_infinite()` — true iff ±infinity (§4.12).
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Float` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_is_infinite(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    abi_guard!("praxis_float_is_infinite", ctx, {
        let result = unsafe { float_payload(r) }.is_infinite();
        // SAFETY: ctx valid; Bool immortal path.
        unsafe { bool_ref(ctx, result) }
    })
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
    abi_guard!("praxis_float_min", ctx, {
        let a = unsafe { float_payload(lhs) };
        let b = unsafe { float_payload(rhs) };
        unsafe { rebox_float(ctx, a.min(b)) }
    })
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
    abi_guard!("praxis_float_max", ctx, {
        let a = unsafe { float_payload(lhs) };
        let b = unsafe { float_payload(rhs) };
        unsafe { rebox_float(ctx, a.max(b)) }
    })
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
    abi_guard!("praxis_float_to_text", ctx, {
        let f = unsafe { float_payload(r) };
        let mut s = String::new();
        scalars::write_float(&mut s, f);
        // SAFETY: `s` is valid UTF-8 for the duration of the call; ctx/heap valid.
        unsafe { text_ref(ctx, s) }
    })
}

/// `Int.to_text()` — the same digits `out()` writes (ADR-143).
///
/// It goes through `scalars::write_int` rather than restating the rendering,
/// because `to_text()` and `out()` disagreeing is a defect in itself: a program
/// that prints a value and a program that builds a string from it must produce
/// the same characters. That is the guarantee, and the shared writer is what
/// makes it structural rather than a thing a test happens to check.
///
/// Never faults: every `i64` renders, `i64::MIN` included.
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_to_text(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    abi_guard!("praxis_int_to_text", ctx, {
        let v = unsafe { int_payload(r) };
        let mut s = String::new();
        scalars::write_int(&mut s, v);
        // SAFETY: `s` is valid UTF-8; ctx/heap valid.
        unsafe { text_ref(ctx, s) }
    })
}

/// `Char.to_text()` — the one-character `Text` holding this scalar, which is the
/// same character `out()` writes (ADR-143).
///
/// Shares `scalars::write_char` with the descriptor's `format` callback for
/// [`praxis_int_to_text`]'s reason. Never faults: a `CharPayload` is a validated
/// Unicode scalar value by construction (ADR-086).
///
/// Reads through [`read_scalar`] with the `Char` handle rather than
/// `int_payload`, because a `Char` payload is **four** bytes and an `i64` read
/// would take eight of them — the same care [`praxis_char_to_int`] takes.
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Char` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_char_to_text(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    abi_guard!("praxis_char_to_text", ctx, {
        // SAFETY: caller guarantees `r` is a valid `GcRef`; `read_scalar` proves
        // the descriptor is `CHAR` before reading its four bytes.
        let code = unsafe { read_scalar(r, scalars::CHAR_PAYLOAD) }.unwrap_or_else(|| {
            scalar_type_mismatch("praxis_char_to_text", "Char", r.descriptor().name)
        });
        let mut s = String::new();
        scalars::write_char(&mut s, code);
        // SAFETY: `s` is valid UTF-8; ctx/heap valid.
        unsafe { text_ref(ctx, s) }
    })
}

/// `pi()` — the constant π as a `Float` (§4.12 prelude free function).
///
/// # Safety
/// `ctx` must be live and wired.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_pi(ctx: *mut RuntimeContext) -> GcRef {
    abi_guard!("praxis_float_pi", ctx, {
        // SAFETY: ctx/heap valid.
        unsafe { gc_alloc(ctx, scalars::FLOAT_PAYLOAD, core::f64::consts::PI) }
    })
}

/// `e()` — Euler's number as a `Float` (§4.12 prelude free function).
///
/// # Safety
/// `ctx` must be live and wired.
#[no_mangle]
pub unsafe extern "C" fn praxis_float_e(ctx: *mut RuntimeContext) -> GcRef {
    abi_guard!("praxis_float_e", ctx, {
        // SAFETY: ctx/heap valid.
        unsafe { gc_alloc(ctx, scalars::FLOAT_PAYLOAD, core::f64::consts::E) }
    })
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
            abi_guard!(stringify!($name), ctx, {
                let a = unsafe { int_payload(lhs) };
                let b = unsafe { int_payload(rhs) };
                match a.$op(b) {
                    Some(result) => unsafe { int_ref(ctx, result) },
                    None => {
                        unsafe { set_fault(ctx, $fault) };
                        unsafe { unit_sentinel(ctx) }
                    }
                }
            })
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
    abi_guard!("praxis_int_div", ctx, {
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
        unsafe { int_ref(ctx, a / b) }
    })
}

/// Checked `Int` remainder (§4.12). Faults on division by zero, and on overflow
/// (`Int::MIN % -1`, whose result is not representable under the §4.12 rule).
///
/// # Safety
/// `ctx` must be live and wired; both operands must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_rem(ctx: *mut RuntimeContext, lhs: GcRef, rhs: GcRef) -> GcRef {
    abi_guard!("praxis_int_rem", ctx, {
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
        unsafe { int_ref(ctx, a % b) }
    })
}

/// Negate an `Int` (§4.12). Faults on overflow (`Int::MIN`).
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_neg(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    abi_guard!("praxis_int_neg", ctx, {
        let a = unsafe { int_payload(r) };
        match a.checked_neg() {
            Some(result) => unsafe { int_ref(ctx, result) },
            None => {
                unsafe { set_fault(ctx, RaisedFault::INT_OVERFLOW) };
                unsafe { unit_sentinel(ctx) }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// §4.12's explicit overflow alternatives: three modes —
// `wrapping_`, `saturating_`, `checked_` — over `add`, `sub` and `mul`.
//
// §4.12 states the family and its two closures (no `_div`/`_rem`, no
// `_neg`/`_abs`) and is the only place that rule is written; the catalog test
// `the_overflow_alternative_family_is_three_modes_over_three_operators` is what
// enforces it. Do not restate it here.
//
// **None of the nine can fault, and that is the whole point of them** — their
// manifest rows are `Allocates`, so ADR-088's verifier rule means no
// `CheckFault` follows the call. They allocate, like every other wrapper that
// answers a fresh number.
// ---------------------------------------------------------------------------

/// `a.wrapping_add(b)` (§4.12): two's-complement wraparound instead of a fault.
///
/// # Safety
/// `ctx` must be live and wired; `a` and `b` must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_wrapping_add(
    ctx: *mut RuntimeContext,
    a: GcRef,
    b: GcRef,
) -> GcRef {
    abi_guard!("praxis_int_wrapping_add", ctx, {
        let (x, y) = unsafe { (int_payload(a), int_payload(b)) };
        unsafe { int_ref(ctx, x.wrapping_add(y)) }
    })
}

/// `a.saturating_add(b)` (§4.12): clamp to `Int`'s ends instead of faulting.
///
/// # Safety
/// `ctx` must be live and wired; `a` and `b` must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_saturating_add(
    ctx: *mut RuntimeContext,
    a: GcRef,
    b: GcRef,
) -> GcRef {
    abi_guard!("praxis_int_saturating_add", ctx, {
        let (x, y) = unsafe { (int_payload(a), int_payload(b)) };
        unsafe { int_ref(ctx, x.saturating_add(y)) }
    })
}

/// `a.checked_add(b)` (§4.12): `Option[Int]` — `None` where the checked `+`
/// would fault.
///
/// It answers a real `Option` (ADR-076): the absence is the *answer* here, not
/// an error channel, which is exactly §4.7's distinction.
///
/// # Safety
/// `ctx` must be live and wired; `a` and `b` must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_checked_add(
    ctx: *mut RuntimeContext,
    a: GcRef,
    b: GcRef,
) -> GcRef {
    abi_guard!("praxis_int_checked_add", ctx, {
        let (x, y) = unsafe { (int_payload(a), int_payload(b)) };
        match x.checked_add(y) {
            Some(sum) => unsafe {
                let scope = NativeScope::new(ctx);
                let boxed = int_ref(ctx, sum);
                let rooted = scope.root(boxed);
                option_some(ctx, rooted.get())
            },
            None => unsafe { option_none(ctx) },
        }
    })
}

/// `a.wrapping_sub(b)` (§4.12): two's-complement wraparound instead of a fault.
///
/// # Safety
/// `ctx` must be live and wired; `a` and `b` must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_wrapping_sub(
    ctx: *mut RuntimeContext,
    a: GcRef,
    b: GcRef,
) -> GcRef {
    abi_guard!("praxis_int_wrapping_sub", ctx, {
        let (x, y) = unsafe { (int_payload(a), int_payload(b)) };
        unsafe { int_ref(ctx, x.wrapping_sub(y)) }
    })
}

/// `a.saturating_sub(b)` (§4.12): clamp to `Int`'s ends instead of faulting.
///
/// # Safety
/// `ctx` must be live and wired; `a` and `b` must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_saturating_sub(
    ctx: *mut RuntimeContext,
    a: GcRef,
    b: GcRef,
) -> GcRef {
    abi_guard!("praxis_int_saturating_sub", ctx, {
        let (x, y) = unsafe { (int_payload(a), int_payload(b)) };
        unsafe { int_ref(ctx, x.saturating_sub(y)) }
    })
}

/// `a.checked_sub(b)` (§4.12): `Option[Int]` — `None` where the checked `-`
/// would fault.
///
/// # Safety
/// `ctx` must be live and wired; `a` and `b` must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_checked_sub(
    ctx: *mut RuntimeContext,
    a: GcRef,
    b: GcRef,
) -> GcRef {
    abi_guard!("praxis_int_checked_sub", ctx, {
        let (x, y) = unsafe { (int_payload(a), int_payload(b)) };
        match x.checked_sub(y) {
            Some(difference) => unsafe {
                let scope = NativeScope::new(ctx);
                let boxed = int_ref(ctx, difference);
                let rooted = scope.root(boxed);
                option_some(ctx, rooted.get())
            },
            None => unsafe { option_none(ctx) },
        }
    })
}

/// `a.wrapping_mul(b)` (§4.12): two's-complement wraparound instead of a fault.
///
/// This is the one of the nine a program could not write for itself: with every
/// arithmetic operator checked and no bitwise operators in the language, there
/// is no in-language spelling of modular multiplication (§4.12).
///
/// # Safety
/// `ctx` must be live and wired; `a` and `b` must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_wrapping_mul(
    ctx: *mut RuntimeContext,
    a: GcRef,
    b: GcRef,
) -> GcRef {
    abi_guard!("praxis_int_wrapping_mul", ctx, {
        let (x, y) = unsafe { (int_payload(a), int_payload(b)) };
        unsafe { int_ref(ctx, x.wrapping_mul(y)) }
    })
}

/// `a.saturating_mul(b)` (§4.12): clamp to `Int`'s ends instead of faulting.
///
/// # Safety
/// `ctx` must be live and wired; `a` and `b` must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_saturating_mul(
    ctx: *mut RuntimeContext,
    a: GcRef,
    b: GcRef,
) -> GcRef {
    abi_guard!("praxis_int_saturating_mul", ctx, {
        let (x, y) = unsafe { (int_payload(a), int_payload(b)) };
        unsafe { int_ref(ctx, x.saturating_mul(y)) }
    })
}

/// `a.checked_mul(b)` (§4.12): `Option[Int]` — `None` where the checked `*`
/// would fault.
///
/// # Safety
/// `ctx` must be live and wired; `a` and `b` must be valid `Int` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_checked_mul(
    ctx: *mut RuntimeContext,
    a: GcRef,
    b: GcRef,
) -> GcRef {
    abi_guard!("praxis_int_checked_mul", ctx, {
        let (x, y) = unsafe { (int_payload(a), int_payload(b)) };
        match x.checked_mul(y) {
            Some(product) => unsafe {
                let scope = NativeScope::new(ctx);
                let boxed = int_ref(ctx, product);
                let rooted = scope.root(boxed);
                option_some(ctx, rooted.get())
            },
            None => unsafe { option_none(ctx) },
        }
    })
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
    abi_guard!("praxis_int_abs", ctx, {
        let a = unsafe { int_payload(r) };
        match a.checked_abs() {
            Some(result) => unsafe { int_ref(ctx, result) },
            None => {
                unsafe { set_fault(ctx, RaisedFault::INT_OVERFLOW) };
                unsafe { unit_sentinel(ctx) }
            }
        }
    })
}

/// `sign(n)` (§16.1): `-1`, `0` or `1`. Total — every `Int`, `Int::MIN`
/// included, has a sign in range.
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_sign(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    abi_guard!("praxis_int_sign", ctx, {
        let a = unsafe { int_payload(r) };
        unsafe { int_ref(ctx, a.signum()) }
    })
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
    abi_guard!("praxis_int_min", _ctx, {
        let a = unsafe { int_payload(lhs) };
        let b = unsafe { int_payload(rhs) };
        if b < a {
            rhs
        } else {
            lhs
        }
    })
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
    abi_guard!("praxis_int_max", _ctx, {
        let a = unsafe { int_payload(lhs) };
        let b = unsafe { int_payload(rhs) };
        if b > a {
            rhs
        } else {
            lhs
        }
    })
}

/// `clamp(value, low, high)` (§16.1): `value` confined to the inclusive range
/// `low..=high`, returned as one of the three references passed in.
///
/// **Faults when `low > high`.** The range is empty, so there is no value to
/// return and no answer that is not a guess — clamping to an empty range is a
/// mistake in the program, not in the data, and a mistake is reported rather
/// than answered with an invented number. (Rust's `Ord::clamp` panics on the
/// same input; a panic across `extern "C"` is what §10.4 forbids, so it is a
/// fault.) The kind is `EmptyRange` (ADR-058).
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
    abi_guard!("praxis_int_clamp", ctx, {
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
    })
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
    abi_guard!("praxis_int_gcd", ctx, {
        let a = unsafe { int_payload(lhs) };
        let b = unsafe { int_payload(rhs) };
        match checked_gcd(a, b) {
            Some(result) => unsafe { int_ref(ctx, result) },
            None => {
                unsafe { set_fault(ctx, RaisedFault::INT_OVERFLOW) };
                unsafe { unit_sentinel(ctx) }
            }
        }
    })
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
    abi_guard!("praxis_int_lcm", ctx, {
        let a = unsafe { int_payload(lhs) };
        let b = unsafe { int_payload(rhs) };
        // `lcm(n, 0)` is 0 for every n: 0 is a multiple of everything, and dividing
        // by the gcd below would divide by zero when both are 0.
        if a == 0 || b == 0 {
            return unsafe { int_ref(ctx, 0i64) };
        }
        // |a / gcd * b| in i128, which cannot overflow: both operands fit i64, so
        // the product fits i128 with room to spare. The range check is the only
        // thing that can refuse.
        let result = checked_gcd(a, b)
            .map(|g| ((a as i128) / (g as i128) * (b as i128)).abs())
            .and_then(|m| i64::try_from(m).ok());
        match result {
            Some(result) => unsafe { int_ref(ctx, result) },
            None => {
                unsafe { set_fault(ctx, RaisedFault::INT_OVERFLOW) };
                unsafe { unit_sentinel(ctx) }
            }
        }
    })
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
            abi_guard!(stringify!($name), ctx, {
                let a = unsafe { int_payload(lhs) };
                let b = unsafe { int_payload(rhs) };
                let result = a $op b;
                // SAFETY: ctx/heap valid; Bool immortal path.
                unsafe { bool_ref(ctx, result) }
            })
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

/// Return 1 if a fault is pending on `ctx`, else 0 (§10.4).
///
/// **Generated code does not call this.** An `Inst::CheckFault` is a load of
/// `ctx.pending_fault`, a load of the kind at
/// [`Fault::KIND_OFFSET`](crate::Fault::KIND_OFFSET) and a `brif` (ADR-102) —
/// the same question, without the call, the `catch_unwind` region and the
/// `Result` discriminant, on a path that runs once per faultable instruction.
///
/// The wrapper stays: it is the named ABI entry point for a host asking the
/// question from Rust (the JIT test harness does), it keeps its manifest row and
/// its address-table arm so `RuntimeSymbol` stays a bijection onto real
/// addresses, and deleting it would churn ADR-080's source-reading test for no
/// gain. Its two null tests are the difference between it and the inline form,
/// and they are why *this* is what a host with a possibly-unwired context calls.
///
/// # Safety
/// `ctx` must point at a live `RuntimeContext` (a null/unwired context reports
/// no fault rather than panicking).
#[no_mangle]
pub unsafe extern "C" fn praxis_check_fault(ctx: *mut RuntimeContext) -> i64 {
    abi_guard!("praxis_check_fault", ctx, {
        if ctx.is_null() {
            return 0;
        }
        if let Some(fault) = unsafe { (*ctx).pending_fault.as_ref() } {
            return fault.is_pending().into();
        }
        0
    })
}

/// Stop the program at a `:bp` marker and show the host its frame chain (§9.8).
///
/// `span_start`/`span_end` are the marker's own source span, passed as
/// immediates: this is a call with no operands from the program, and boxing a
/// span so it could ride a `GcRef` argument would put an allocation at the one
/// site whose cost has to stay a single call.
///
/// Everything that makes a stop *not* a fault lives in
/// [`crate::breakpoint::stop`]: the host handler is given a snapshot and no
/// context, so it cannot allocate, cannot collect and cannot raise. That is what
/// lets this be declared [`Effect::Pure`](praxis_stdlib::abi::Effect::Pure), and
/// therefore what lets generated code emit no root spill before it and no fault
/// check after.
///
/// A program with no handler installed — every JIT test, every embedder that
/// wants none — finds nothing to call and returns.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext` whose claimed debug frames
/// satisfy `copy_stack`'s contract, which every generated prologue establishes.
#[no_mangle]
pub unsafe extern "C" fn praxis_breakpoint(
    ctx: *mut RuntimeContext,
    span_start: u32,
    span_end: u32,
) {
    abi_guard!("praxis_breakpoint", ctx, {
        if ctx.is_null() {
            return;
        }
        // SAFETY: `ctx` is non-null and the caller guarantees it is live and
        // wired; the debug frames are the ones its prologue chain claimed.
        unsafe { crate::breakpoint::stop(ctx, (span_start, span_end)) };
    })
}

/// Raise a [`FaultKind::StackOverflow`] fault on `ctx` (§9.2, §17.4). Called by
/// the generated prologue guard when `ctx.stack_left` is less than this frame's
/// [`frame_cost`](crate::frame_cost), so the host survives deep recursion
/// instead of overflowing the native stack. The prologue then unwinds to its
/// fault epilogue (pop frame + return Unit) — same path as any other fault.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[no_mangle]
pub unsafe extern "C" fn praxis_raise_stack_overflow(ctx: *mut RuntimeContext) {
    abi_guard!("praxis_raise_stack_overflow", ctx, {
        unsafe { set_fault(ctx, RaisedFault::STACK_OVERFLOW) };
    })
}

/// Raise a [`FaultKind::EmptyCollection`] fault on `ctx` (§9.2).
///
/// `reduce`, `min_by` and `max_by` have no answer for an empty sequence: they
/// seed their accumulator from the first element, and there is no first
/// element. Handing back an unwritten accumulator slot would materialize
/// whatever the register held as a `GcRef` that is `NonNull` by type and
/// arbitrary in fact; this is the defined failure instead, and a fault is what
/// the other empty-collection accessors (`Deque.pop_front`, heap `pop`/`peek`)
/// already raise for the same reason.
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
    abi_guard!("praxis_raise_empty_collection", ctx, {
        unsafe { set_fault(ctx, RaisedFault::EMPTY_COLLECTION) };
        unsafe { unit_sentinel(ctx) }
    })
}

/// Raise a [`FaultKind::IntOverflow`] fault on `ctx` iff `condition` is
/// non-zero (§4.12).
///
/// Generated code lowers `Int` arithmetic natively — `iadd`/`isub`/`imul` on
/// the raw scalar channel — and computes the overflow predicate inline. This is
/// how it reports one. It allocates nothing, so an arithmetic site is not a GC
/// safepoint and spills no roots.
///
/// **The call site branches; this is the cold path.** Calling unconditionally
/// and letting `condition` decide would keep arithmetic to a single basic
/// block, but a branch does not clobber registers and a call does, so it would
/// force a spill and reload of every live value around an arithmetic op that
/// never faults. The site is a `brif` to a cold block (ADR-102);
/// `raise_on_cold_path` in the backend carries the full argument.
///
/// The cold block passes a constant `1` — honest, since it is reached only when
/// the predicate held, and it keeps the test below a true statement rather than
/// dead code.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[no_mangle]
pub unsafe extern "C" fn praxis_raise_int_overflow_if(ctx: *mut RuntimeContext, condition: i64) {
    abi_guard!("praxis_raise_int_overflow_if", ctx, {
        if condition != 0 {
            unsafe { set_fault(ctx, RaisedFault::INT_OVERFLOW) };
        }
    })
}

/// Raise a [`FaultKind::DivByZero`] fault on `ctx` iff `condition` is non-zero
/// (§4.12). The division counterpart of [`praxis_raise_int_overflow_if`].
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[no_mangle]
pub unsafe extern "C" fn praxis_raise_div_by_zero_if(ctx: *mut RuntimeContext, condition: i64) {
    abi_guard!("praxis_raise_div_by_zero_if", ctx, {
        if condition != 0 {
            unsafe { set_fault(ctx, RaisedFault::DIV_BY_ZERO) };
        }
    })
}

// ---------------------------------------------------------------------------
// Collection payload accessors (§11.1, §11.4).
//
// Every collection below reaches its payload through a named accessor —
// `vec_payload`, `map_payload_mut`, … — and the name is the descriptor
// assertion the wrapper is making, which is why the nine kinds keep nine
// (shared, mut) pairs rather than calling `payload::<P>()` inline. The two
// casts underneath live here once, and each named accessor is the one line
// that spells its payload type.
// ---------------------------------------------------------------------------

/// Read a `P` payload out of a `GcRef` as a shared ref.
///
/// # Safety
/// `r` must be a valid `GcRef` whose descriptor's payload type is `P`.
unsafe fn payload_ref<P>(r: GcRef) -> &'static P {
    // SAFETY: caller guarantees `r`'s payload is a `P`; the non-moving GC
    // (ADR-011) keeps the payload address stable for the object's lifetime. The
    // `'static` is unbounded because the raw FFI boundary has no lifetime to
    // carry; the caller (a wrapper that holds `ctx`) ensures the object outlives
    // the use.
    unsafe { &*r.payload::<P>() }
}

/// Read a `P` payload out of a rooted `GcRef` as a mutable ref — the accessor
/// the wrappers that mutate in place go through (§11.1).
///
/// # Safety
/// `r` must be a valid `GcRef` whose descriptor's payload type is `P`, rooted
/// for `'s`.
unsafe fn payload_mut<'s, P>(r: Rooted<'s>) -> &'s mut P {
    // SAFETY: caller guarantees `r`'s payload is a `P`; the non-moving GC
    // (ADR-011) keeps the payload address stable for the object's lifetime, and
    // `Rooted` proves the object is in the collector's root set for `'s`, so a
    // collection triggered while this reference is held cannot reclaim what it
    // points at.
    unsafe { &mut *r.get().payload::<P>() }
}

// ---------------------------------------------------------------------------
// Vec[T] collection methods (§11.1, §11.2, §11.5).
//
// `VecPayload` stores a growable [`ReprCVec<GcRef>`](crate::ReprCVec), so
// `push` mutates the existing payload in place and the receiver's `GcRef` stays
// valid across it. Per §11.5 reallocation safety, no interior pointer into that
// buffer is retained across a capacity-mutating op.
// ---------------------------------------------------------------------------

/// Read the `VecPayload` out of a `GcRef` as a shared ref, asserting it is a Vec.
///
/// # Safety
/// `r` must be a valid `Vec` `GcRef`.
unsafe fn vec_payload(r: GcRef) -> &'static VecPayload {
    // SAFETY: caller guarantees `r` is a Vec; see `payload_ref`.
    unsafe { payload_ref::<VecPayload>(r) }
}

/// Read the `VecPayload` out of a `GcRef` as a mutable ref, asserting it is a
/// Vec. Used by `push` to mutate the vector in place (§11.1).
///
/// # Safety
/// `r` must be a valid `Vec` `GcRef`, rooted for `'s`.
unsafe fn vec_payload_mut<'s>(r: Rooted<'s>) -> &'s mut VecPayload {
    // SAFETY: caller guarantees `r` is a Vec; see `payload_mut`.
    unsafe { payload_mut::<VecPayload>(r) }
}

/// Build a `Vec[T]` holding `items`, with `element_descriptor` as its element
/// type — the shape every wrapper that answers with a collection needs.
///
/// The `Vec` is rooted across the pushes, which is the part worth having in one
/// place: `praxis_vec_new` allocates, and so may the caller's own iteration, so a
/// collection between the allocation and the last push would reclaim it.
///
/// `element_descriptor` may be **null**: the source collection's label is what
/// its own construction site knew, and that may have been nothing. A
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
    abi_guard!("praxis_vec_new", ctx, {
        // A null descriptor is kept null: it means "the caller has no static
        // element type", which is a thing this payload can hold. Spelling it
        // `INT` instead would make an empty `Vec[Float]` claim to hold `Int`s.
        // SAFETY: VecPayload is VEC's payload type.
        unsafe {
            gc_alloc_owned(ctx, &crate::collections::VEC, || VecPayload {
                element_descriptor,
                items: ReprCVec::new(),
            })
        }
    })
}

/// Allocate a `Vec[T]` of `count` slots, every one holding `fill` (ADR-146's
/// `Vec(n, fill)`).
///
/// Faults `InvalidSize` if `count` is negative or exceeds
/// [`VecExtent::MAX_ITEMS`](crate::collections::VecExtent::MAX_ITEMS): the count
/// arrives from source and would otherwise become a `usize` cast, where a
/// negative value lands near `usize::MAX` (ADR-041 decision 1).
///
/// Faults `TypeMismatch` if the caller declared an element type that the fill is
/// not, through the same [`adopt_or_reject`] `push` uses — a `Vec[Int]` filled
/// with a `Float` is a mislabelled element descriptor, and every later
/// `equals`/`hash`/`format` would read the payloads as the wrong type. A null
/// static descriptor adopts the fill's, which is what "the caller has no static
/// element type" already means here.
///
/// **`fill` is stored `count` times, not copied `count` times.** Every slot is
/// the same `GcRef`, so `Vec(3, Vec())` is three names for one inner `Vec`.
/// That is the language's existing reference semantics — `outer.push(a)` twice
/// aliases too — stated at a new site rather than a new rule (ADR-146 decision
/// 4).
///
/// `count` arrives boxed rather than as a `RawI64` like [`praxis_grid_new`]'s
/// extents: MIR lowers an argument expression to a `Gc` local, and unboxing it
/// there would cost an `ExtractScalar` and a second shape in the codegen's
/// allocation arm. `praxis_grid_new`'s two are `iconst` immediates with no local
/// to unbox, which is why the two wrappers differ.
///
/// # Safety
/// `ctx` must be live and wired; `element_descriptor` must be a valid pointer to
/// a `'static TypeDescriptor` (or null); `count` must be a valid `Int` `GcRef`;
/// `fill` must be a valid `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_vec_filled(
    ctx: *mut RuntimeContext,
    element_descriptor: *const TypeDescriptor,
    count: GcRef,
    fill: GcRef,
) -> GcRef {
    abi_guard!("praxis_vec_filled", ctx, {
        // SAFETY: caller guarantees `count` is a valid Int.
        let n = unsafe { int_payload(count) };
        let Some(extent) = crate::collections::VecExtent::new(n) else {
            unsafe { set_fault(ctx, RaisedFault::INVALID_SIZE) };
            return unsafe { unit_sentinel(ctx) };
        };
        let mut descriptor = element_descriptor;
        if !unsafe { adopt_or_reject(ctx, &mut descriptor, fill) } {
            return unsafe { unit_sentinel(ctx) };
        }
        // `fill` is a bare `GcRef` argument, and `gc_alloc_owned` may collect.
        // Rooting it in a native scope is what keeps it addressable across the
        // allocation — the caller's shadow frame roots it up to the call, and
        // this roots it through it.
        let scope = unsafe { NativeScope::new(ctx) };
        let fill = scope.root(fill).get();
        // The items are built inside the initializer, which `gc_alloc_owned`
        // runs *after* the safepoint: no untraced `Vec<GcRef>` is ever live
        // across a collection.
        // SAFETY: VecPayload is VEC's payload type.
        unsafe {
            gc_alloc_owned(ctx, &crate::collections::VEC, || VecPayload {
                element_descriptor: descriptor,
                items: ReprCVec::from_vec(vec![fill; extent.len()]),
            })
        }
    })
}

/// Allocate a nominal record (§4.5) with all fields initialized to Unit.
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
    abi_guard!("praxis_alloc_record", ctx, {
        if schema_ptr.is_null() {
            return unit_sentinel(ctx);
        }
        // SAFETY: caller guarantees schema_ptr is a valid 'static pointer.
        let schema = unsafe { &*schema_ptr };
        let arity = schema.fields.len();
        let unit = unit_sentinel(ctx);
        // SAFETY: RecordPayload is RECORD's payload type.
        // Every field slot starts as Unit (a valid GcRef), keeping the GC sound
        // before the caller fills them in via praxis_record_set_field.
        unsafe {
            gc_alloc_owned(ctx, &crate::records::RECORD, || {
                crate::records::RecordPayload {
                    schema: schema_ptr,
                    items: vec![unit; arity],
                }
            })
        }
    })
}

/// Set field `idx` of `record` to `value` (§4.5). Used by the codegen to
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
    abi_guard!("praxis_record_set_field", ctx, {
        let _ = ctx;
        // SAFETY: caller guarantees record is a valid record GcRef.
        let payload = record.payload::<u8>() as *mut crate::records::RecordPayload;
        // SAFETY: the payload is a RecordPayload for any RECORD-descriptor object.
        let rp = unsafe { &mut *payload };
        if let Some(slot) = rp.items.get_mut(idx as usize) {
            *slot = value;
        }
        record
    })
}

/// Read field `idx` out of a record `GcRef` (§4.5). Returns the field's
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
    abi_guard!("praxis_record_field", ctx, {
        // SAFETY: caller guarantees record is a valid record GcRef; the payload is
        // a RecordPayload for any RECORD-descriptor object.
        let payload = record.payload::<u8>() as *const crate::records::RecordPayload;
        let rp = &*payload;
        rp.items
            .get(idx as usize)
            .copied()
            .unwrap_or_else(|| unsafe { unit_sentinel(ctx) })
    })
}

/// Allocate an enum value (§4.6) of the type `schema_ptr` describes, with
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
    abi_guard!("praxis_alloc_enum", ctx, {
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
        // SAFETY: EnumPayload is ENUM's payload type.
        unsafe {
            gc_alloc_owned(ctx, &crate::enums::ENUM, || crate::enums::EnumPayload {
                schema: schema_ptr,
                tag: tag as u32,
                items,
            })
        }
    })
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

/// Set payload slot `idx` of `enum_value` to `value` (§4.6). Returns the
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
    abi_guard!("praxis_enum_set_payload", ctx, {
        let _ = ctx;
        // SAFETY: caller guarantees enum_value is a valid enum GcRef.
        let payload = enum_value.payload::<u8>() as *mut crate::enums::EnumPayload;
        let ep = unsafe { &mut *payload };
        if let Some(slot) = ep.items.get_mut(idx as usize) {
            *slot = value;
        }
        enum_value
    })
}

/// Read the variant tag (discriminant) of an enum value (§4.6). Returns the
/// tag as a boxed `Int` `GcRef` (the uniform ABI convention), so the `match`
/// lowering can extract the scalar and compare. Used by `match` to branch.
///
/// # Safety
/// `ctx` must be live; `enum_value` must be a valid enum `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_enum_tag(ctx: *mut RuntimeContext, enum_value: GcRef) -> GcRef {
    abi_guard!("praxis_enum_tag", ctx, {
        // SAFETY: caller guarantees enum_value is a valid enum GcRef.
        // Read the tag BEFORE allocating — the alloc below may trigger GC, and
        // enum_value is not explicitly rooted (it's only in a Cranelift local).
        let payload = enum_value.payload::<u8>() as *const crate::enums::EnumPayload;
        let tag = unsafe { (*payload).tag as i64 };
        // SAFETY: alloc boxes the i64 into a fresh Int object. The tag value is
        // already in a register, so GC collecting enum_value here is safe.
        unsafe { int_ref(ctx, tag) }
    })
}

/// Read payload slot `idx` of an enum value (§4.6). Returns the slot's
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
    abi_guard!("praxis_enum_payload", ctx, {
        // SAFETY: caller guarantees enum_value is a valid enum GcRef.
        let payload = enum_value.payload::<u8>() as *const crate::enums::EnumPayload;
        let ep = unsafe { &*payload };
        ep.items
            .get(idx as usize)
            .copied()
            .unwrap_or_else(|| unsafe { unit_sentinel(ctx) })
    })
}

/// Allocate a tuple (§4.5 structural tuples) with all element slots
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
    abi_guard!("praxis_alloc_tuple", ctx, {
        if schema_ptr.is_null() {
            return unit_sentinel(ctx);
        }
        // SAFETY: caller guarantees schema_ptr is a valid 'static pointer.
        let schema = unsafe { &*schema_ptr };
        let arity = schema.descriptors.len();
        let unit = unit_sentinel(ctx);
        // SAFETY: TuplePayload is TUPLE's payload type.
        // Every element slot starts as Unit (a valid GcRef), keeping the GC sound
        // before the caller fills them in via praxis_tuple_set.
        unsafe {
            gc_alloc_owned(ctx, &crate::tuples::TUPLE, || crate::tuples::TuplePayload {
                schema: schema_ptr,
                items: vec![unit; arity],
            })
        }
    })
}

/// Set element `idx` of `tuple` to `value` (§4.5). Used by the codegen to
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
    abi_guard!("praxis_tuple_set", ctx, {
        let _ = ctx;
        // SAFETY: caller guarantees tuple is a valid tuple GcRef.
        let payload = tuple.payload::<u8>() as *mut crate::tuples::TuplePayload;
        // SAFETY: the payload is a TuplePayload for any TUPLE-descriptor object.
        let tp = unsafe { &mut *payload };
        if let Some(slot) = tp.items.get_mut(idx as usize) {
            *slot = value;
        }
        tuple
    })
}

/// Read element `idx` out of a tuple `GcRef` (§4.5). Returns the element's
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
    abi_guard!("praxis_tuple_get", ctx, {
        // SAFETY: caller guarantees tuple is a valid tuple GcRef; the payload is a
        // TuplePayload for any TUPLE-descriptor object.
        let payload = tuple.payload::<u8>() as *const crate::tuples::TuplePayload;
        let tp = unsafe { &*payload };
        tp.items
            .get(idx as usize)
            .copied()
            .unwrap_or_else(|| unsafe { unit_sentinel(ctx) })
    })
}

/// Structural equality between two GC values (§5.5). Reads the descriptor
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
    abi_guard!("praxis_struct_eq", ctx, {
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
    })
}

/// Order two GC values through their descriptor's `compare` callback (ADR-045).
/// Returns `-1`, `0` or `1` — the caller turns that into the `<`/`<=`/`>`/`>=`
/// it wanted by comparing against zero.
///
/// This is the ordering counterpart of [`praxis_struct_eq`], and it exists for
/// the same reason: a `Text` is a pointer-and-length structure, so ordering one
/// by loading its first eight payload bytes would compare *addresses*.
///
/// Raises `FaultKind::TypeMismatch` and answers `0` when the two operands are
/// not the same runtime type, or when the type has no `compare`. The type
/// checker rejects both in well-typed code (`Y006`), so reaching either is a
/// compiler bug — reported as a fault rather than a callback dispatched on a
/// foreign layout.
///
/// The second guard is a weak backstop, and deliberately named as one: ADR-138
/// populated `compare` on every type a `Map` key can be, including tuples and
/// records, so a *miscompile* that lowered `(1, 2) < (1, 3)` to this wrapper
/// would be answered rather than faulted. `capability::supports_ord` refuses it
/// at `praxis check`, so no well-typed program reaches here either way.
///
/// # Safety
/// `ctx` must be live and wired; `a` and `b` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_value_cmp(ctx: *mut RuntimeContext, a: GcRef, b: GcRef) -> i64 {
    abi_guard!("praxis_value_cmp", ctx, {
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
    })
}

/// Allocate a closure value (§4.10) with `fn_ptr` as its entry point and
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
    abi_guard!("praxis_alloc_closure", ctx, {
        let unit = unit_sentinel(ctx);
        let env = vec![unit; n_captures as usize];
        // SAFETY: ClosurePayload is CLOSURE's payload type.
        unsafe {
            gc_alloc_owned(ctx, &crate::closures::CLOSURE, || {
                crate::closures::ClosurePayload { fn_ptr, env }
            })
        }
    })
}

/// Set capture slot `idx` of `closure` to `value` (§4.10). Returns the
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
    abi_guard!("praxis_closure_set_capture", ctx, {
        let _ = ctx;
        // SAFETY: caller guarantees closure is a valid closure GcRef.
        let payload = closure.payload::<u8>() as *mut crate::closures::ClosurePayload;
        let cp = unsafe { &mut *payload };
        if let Some(slot) = cp.env.get_mut(idx as usize) {
            *slot = value;
        }
        closure
    })
}

/// Read the function pointer out of a closure `GcRef` (§4.10). Used by the
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
    abi_guard!("praxis_closure_fn_ptr", ctx, {
        let _ = ctx;
        // SAFETY: caller guarantees closure is a valid closure GcRef.
        let payload = closure.payload::<u8>() as *const crate::closures::ClosurePayload;
        unsafe { (*payload).fn_ptr }
    })
}

/// Read capture slot `idx` out of a closure `GcRef` (§4.10). Used by the
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
    abi_guard!("praxis_closure_capture", ctx, {
        // SAFETY: caller guarantees closure is a valid closure GcRef.
        let payload = closure.payload::<u8>() as *const crate::closures::ClosurePayload;
        let cp = unsafe { &*payload };
        cp.env
            .get(idx as usize)
            .copied()
            .unwrap_or_else(|| unsafe { unit_sentinel(ctx) })
    })
}

/// Allocate a `VarCell` holding `value` (§4.10). The cell is the shared
/// mutable storage for a captured `var` binding: the binding site and every
/// closure that captures the `var` refer to the same cell. Returns the cell
/// `GcRef`.
///
/// # Safety
/// `ctx` must be live and wired; `value` must be a valid `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_var_cell(ctx: *mut RuntimeContext, value: GcRef) -> GcRef {
    abi_guard!("praxis_alloc_var_cell", ctx, {
        // SAFETY: VarCellPayload is VAR_CELL's payload type.
        unsafe {
            gc_alloc_owned(ctx, &crate::var_cell::VAR_CELL, || {
                crate::var_cell::VarCellPayload { value }
            })
        }
    })
}

/// Read the current value out of a `VarCell` (§4.10). Used by `Path`
/// reads of a captured `var` (the local holds the cell; this derefs it).
///
/// # Safety
/// `ctx` must be live; `cell` must be a valid `VarCell` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_var_cell_get(ctx: *mut RuntimeContext, cell: GcRef) -> GcRef {
    abi_guard!("praxis_var_cell_get", ctx, {
        let _ = ctx;
        // SAFETY: caller guarantees cell is a valid VarCell GcRef.
        let payload = cell.payload::<u8>() as *const crate::var_cell::VarCellPayload;
        unsafe { (*payload).value }
    })
}

/// Store `value` into a `VarCell` (§4.10). Used by `Assign` to a
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
    abi_guard!("praxis_var_cell_set", ctx, {
        let _ = ctx;
        // SAFETY: caller guarantees cell is a valid VarCell GcRef.
        let payload = cell.payload::<u8>() as *mut crate::var_cell::VarCellPayload;
        unsafe {
            (*payload).value = value;
        }
        cell
    })
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
    abi_guard!("praxis_vec_push", ctx, {
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
        // retagging would turn an explicitly typed `Vec[Int]` into a `Vec[Float]`
        // on one bad push, and every later `equals`/`hash`/`format` would then
        // read the remaining `Int` payloads as `f64`.
        if !unsafe { adopt_or_reject(ctx, &mut p.element_descriptor, value) } {
            return unsafe { unit_sentinel(ctx) };
        }
        // Charge the spine when it grows (ADR-121; see
        // `Heap::charge_owned_growth`). Measured either side of the mutation
        // through the payload's own `owned_bytes`, so the growth policy stays
        // `RawVec`'s and the size formula stays the descriptor's.
        let before = p.owned_bytes();
        p.items.push(value);
        charge_growth(ctx, before, p.owned_bytes());
        unsafe { unit_sentinel(ctx) }
    })
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
    abi_guard!("praxis_vec_len", ctx, {
        // SAFETY: caller guarantees `vec` is a valid Vec.
        let p = unsafe { vec_payload(vec) };
        let len = p.items.len() as i64;
        // len allocates the returned Int, but the input vec is still live via `vec`.
        unsafe { int_ref(ctx, len) }
    })
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
    abi_guard!("praxis_vec_get", ctx, {
        // SAFETY: caller guarantees `vec` is a valid Vec.
        let p = unsafe { vec_payload(vec) };
        // SAFETY: caller guarantees `index` is a valid Int.
        let idx = unsafe { int_payload(index) };
        // SAFETY: `abi_guard!` established that `ctx` is live and wired.
        let Some(idx) = (unsafe { checked_index(ctx, idx, p.items.len()) }) else {
            return unsafe { unit_sentinel(ctx) };
        };
        // Return the element by value (a copy of the GcRef). No allocation, so no
        // collection is needed; the vec stays live via `vec`.
        p.items[idx]
    })
}

/// Replace the element at `index`; faults `IndexOutOfBounds` if out of range
/// (§9.2, §11.1). Returns the Unit sentinel.
///
/// **Replaces, and never appends.** `v[v.len()] = x` is out of range rather than
/// a push, which is `praxis_vec_push`'s job: a store whose index decides between
/// the two operations makes an off-by-one grow the vector instead of reporting.
///
/// The element descriptor goes through the same [`adopt_or_reject`] every push
/// does, so a store into a vector that was never told its element type adopts
/// the first value's, and one into a `Vec[Int]` raises `TypeMismatch` rather
/// than retagging the collection — `push`'s rule, at the second door.
///
/// # Safety
/// `ctx` must be live and wired; `vec` must be a valid `Vec` `GcRef`; `index`
/// must be a valid `Int` `GcRef`; `value` must be a valid `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_vec_set(
    ctx: *mut RuntimeContext,
    vec: GcRef,
    index: GcRef,
    value: GcRef,
) -> GcRef {
    abi_guard!("praxis_vec_set", ctx, {
        // SAFETY: caller guarantees `vec` is a valid Vec.
        let scope = unsafe { NativeScope::new(ctx) };
        let p = unsafe { vec_payload_mut(scope.root(vec)) };
        // SAFETY: caller guarantees `index` is a valid Int.
        let idx = unsafe { int_payload(index) };
        // SAFETY: `abi_guard!` established that `ctx` is live and wired.
        let Some(idx) = (unsafe { checked_index(ctx, idx, p.items.len()) }) else {
            return unsafe { unit_sentinel(ctx) };
        };
        if !unsafe { adopt_or_reject(ctx, &mut p.element_descriptor, value) } {
            return unsafe { unit_sentinel(ctx) };
        }
        // No allocation: the slot takes a `GcRef` the caller already holds.
        p.items[idx] = value;
        unsafe { unit_sentinel(ctx) }
    })
}

/// True iff `vec` has no elements, as a boxed `Bool` (§11.1).
///
/// # Safety
/// `ctx` must be live and wired; `vec` must be a valid `Vec` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_vec_is_empty(ctx: *mut RuntimeContext, vec: GcRef) -> GcRef {
    abi_guard!("praxis_vec_is_empty", ctx, {
        // SAFETY: caller guarantees `vec` is a valid Vec.
        let p = unsafe { vec_payload(vec) };
        let empty = p.items.is_empty();
        // SAFETY: ctx/heap valid; Bool immortal path.
        unsafe { bool_ref(ctx, empty) }
    })
}

// --- the §6.3 barrier combinators ------------------------------------------
//
// A *barrier* is a pipeline stage that cannot be fused into the loop feeding it
// because it needs the whole sequence before it can answer its first element:
// `sorted` has to see the largest element before it knows the smallest is first.
// So each is a real runtime call over a materialized `Vec` rather than an
// intrinsic the MIR fuser expands, and the fuser's own recognizer already knows
// to end a chain at one and start a fresh chain from its result.
//
// All three rebuild through [`vec_of`] rather than mutating the receiver, which
// is the shape `praxis_set_items` and `praxis_counter_keys` already use.
// `v.sorted()` is an expression, not a statement: §6.3 lists it beside `map` and
// `filter`, and a caller that also holds `v` must still see `v`'s own order.

/// `v.sorted()` — the elements of `vec` in ascending order, as a **new** `Vec`
/// (§6.3). The receiver is not touched.
///
/// Ordering goes through the element descriptor's `compare` callback — the same
/// callback [`praxis_value_cmp`] uses, and for the same reason: a `Text` is a
/// pointer-and-length structure, so ordering one by its first eight payload
/// bytes compares *addresses*. That sorts `Vec[Int]` correctly and `Vec[Text]`
/// into allocation order, which is the failure that looks like it works.
///
/// The sort is **stable**, so equal elements keep their input order and the
/// answer is a function of the input alone.
///
/// Raises `FaultKind::TypeMismatch` and answers Unit when the elements are not
/// all one type, or when that type has no `compare`. The catalog row's `Ord`
/// bound (ADR-093, `Bound::Kind`) rejects both at `praxis check`, so reaching
/// either is a compiler bug — reported as a fault rather than as a callback
/// dispatched on a foreign layout. As [`praxis_value_cmp`], the second guard
/// covers fewer types since ADR-138 populated `compare` on the composites.
///
/// It is the callback a `Set` and a `Map` order their keys through too
/// (ADR-138), which is what makes `out(s)` and `out(s.sorted())` print one
/// sequence rather than one numeric and one lexicographic.
///
/// # Safety
/// `ctx` must be live and wired; `vec` must be a valid `Vec` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_vec_sorted(ctx: *mut RuntimeContext, vec: GcRef) -> GcRef {
    abi_guard!("praxis_vec_sorted", ctx, {
        // SAFETY: caller guarantees `vec` is a valid Vec.
        let p = unsafe { vec_payload(vec) };
        let mut items: Vec<GcRef> = p.items.to_vec();
        // Nothing to order, and nothing to check: a zero- or one-element Vec has
        // no pair to compare, so an empty `Vec[fn(Int) -> Int]` sorts rather than
        // faulting on a `compare` it would never have called.
        if items.len() > 1 {
            // The elements' *own* descriptors decide, not the Vec's label: the
            // label may be null (the construction site knew no element type)
            // while every member is a perfectly good `Text`.
            let desc = items[0].descriptor();
            if !items.iter().all(|i| std::ptr::eq(i.descriptor(), desc)) {
                unsafe { set_fault(ctx, RaisedFault::TYPE_MISMATCH) };
                return unsafe { unit_sentinel(ctx) };
            }
            let Some(compare) = desc.compare else {
                unsafe { set_fault(ctx, RaisedFault::TYPE_MISMATCH) };
                return unsafe { unit_sentinel(ctx) };
            };
            items.sort_by(|a, b| {
                // SAFETY: every element carries `desc` (checked above), so both
                // payloads are values of its type; the non-moving GC keeps them
                // stable, and `sort_by` allocates nothing that could collect.
                unsafe {
                    compare(
                        a.payload::<u8>() as *const u8,
                        b.payload::<u8>() as *const u8,
                    )
                }
            });
        }
        unsafe { vec_of(ctx, p.element_descriptor, items.into_iter()) }
    })
}

/// `v.sorted_by_key(f)` — the elements of `vec` ordered by the key `f` extracts,
/// as a **new** `Vec` (§6.3, ADR-127 decision 5). The receiver is not touched.
///
/// # Why this row exists, and why it is not `sorted_by`
///
/// ADR-045 decided that no composite is orderable, so the moment a pipeline's
/// item is a pair — which is the moment its source is a `Map` or a `Counter` —
/// `sorted` is unavailable and "the five most common values" has no spelling.
/// The closure extracts an orderable key from an item that is not.
///
/// Not a `(T, T) -> Bool` comparator: `min_by`/`max_by` already own the
/// less-than-predicate shape, and a comparator is O(n log n) calls back into
/// JIT'd code where a key extractor is n.
///
/// **Decorate–sort–undecorate.** Every key is extracted once, up front, and the
/// sort orders the (key, element) pairs — which is what makes it n calls. The
/// keys are held in a `Vec<GcRef>` the collector cannot see, so they are rooted
/// in a native scope: extraction allocates, and a collection triggered by the
/// *next* call would otherwise free the key the previous one produced.
///
/// Ordering goes through the same `compare` callback [`praxis_value_cmp`] uses,
/// so the `Ord` bound the catalog row puts on the *key* is the rule this
/// enforces. The sort is **stable**, so items with equal keys keep their input
/// order and the answer is a function of the input alone.
///
/// Raises `FaultKind::TypeMismatch` and answers Unit when the keys are not all
/// one type, or when that type has no ordering; a fault the closure itself
/// raised stops the sort and is left for the call site's own check.
///
/// # Safety
/// `ctx` must be live and wired; `vec` must be a valid `Vec` `GcRef` and `key`
/// a valid closure `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_vec_sorted_by_key(
    ctx: *mut RuntimeContext,
    vec: GcRef,
    key: GcRef,
) -> GcRef {
    abi_guard!("praxis_vec_sorted_by_key", ctx, {
        let scope = unsafe { NativeScope::new(ctx) };
        // The receiver is rooted explicitly: its items are read into a Rust
        // `Vec` and held across one closure call *per element*, which is a far
        // longer window than a single-allocation wrapper's.
        let _receiver = scope.root(vec);
        // SAFETY: caller guarantees `vec` is a valid Vec.
        let p = unsafe { vec_payload(vec) };
        let element_descriptor = p.element_descriptor;
        let items: Vec<GcRef> = p.items.to_vec();
        for item in &items {
            scope.root(*item);
        }
        // Decorate: one call per element, keys rooted as they arrive.
        let mut decorated: Vec<(GcRef, GcRef)> = Vec::with_capacity(items.len());
        for item in items {
            let Some(k) = (unsafe { call_unary_closure(ctx, key, item) }) else {
                // The closure faulted (or is not a closure, which the type
                // checker already refused). Its answer is the Unit sentinel, so
                // sorting on it would order garbage; stop and leave the fault
                // for the call site.
                return unsafe { unit_sentinel(ctx) };
            };
            decorated.push((scope.root(k).get(), item));
        }
        if decorated.len() > 1 {
            // The keys' *own* descriptors decide, as `praxis_vec_sorted`'s
            // elements' do: the source Vec's label says nothing about them.
            let desc = decorated[0].0.descriptor();
            if !decorated
                .iter()
                .all(|(k, _)| std::ptr::eq(k.descriptor(), desc))
            {
                unsafe { set_fault(ctx, RaisedFault::TYPE_MISMATCH) };
                return unsafe { unit_sentinel(ctx) };
            }
            let Some(compare) = desc.compare else {
                unsafe { set_fault(ctx, RaisedFault::TYPE_MISMATCH) };
                return unsafe { unit_sentinel(ctx) };
            };
            decorated.sort_by(|(a, _), (b, _)| {
                // SAFETY: every key carries `desc` (checked above), so both
                // payloads are values of its type; the non-moving GC keeps them
                // stable, and `sort_by` allocates nothing that could collect.
                unsafe {
                    compare(
                        a.payload::<u8>() as *const u8,
                        b.payload::<u8>() as *const u8,
                    )
                }
            });
        }
        // Undecorate.
        unsafe {
            vec_of(
                ctx,
                element_descriptor,
                decorated.into_iter().map(|(_, item)| item),
            )
        }
    })
}

/// Call a `(T) -> U` Praxis closure with one argument, or `None` if it faulted —
/// or if it is not a closure at all.
///
/// The descriptor is checked rather than assumed: the type checker says the
/// operand is a function and the only runtime representation of one is a closure
/// object, but the alternative to a `TypeMismatch` fault is transmuting whatever
/// the payload's first word happens to be into a function pointer and jumping to
/// it.
///
/// # Safety
/// `ctx` must be live and wired; `closure` and `arg` must be valid `GcRef`s.
unsafe fn call_unary_closure(
    ctx: *mut RuntimeContext,
    closure: GcRef,
    arg: GcRef,
) -> Option<GcRef> {
    if !std::ptr::eq(closure.descriptor(), &crate::closures::CLOSURE) {
        unsafe { set_fault(ctx, RaisedFault::TYPE_MISMATCH) };
        return None;
    }
    // SAFETY: the descriptor check proves the payload is a `ClosurePayload`, so
    // `fn_ptr` is the entry point the codegen wrote there.
    let fn_ptr = unsafe { (*closure.payload::<crate::closures::ClosurePayload>()).fn_ptr };
    // A closure's entry point is `fn(ctx, closure_self, params...) -> GcRef`
    // (§4.10, Approach B): the closure value itself is a hidden first argument,
    // and the prologue loads its captures from it.
    //
    // SAFETY: `fn_ptr` is a finalized JIT entry whose parameter count is the one
    // the type checker enforced for this operand; every value crossing is a
    // `GcRef`, which is the ABI's only value kind.
    let result = unsafe {
        let f: unsafe extern "C" fn(*mut RuntimeContext, GcRef, GcRef) -> GcRef =
            std::mem::transmute(fn_ptr);
        f(ctx, closure, arg)
    };
    // The closure ran arbitrary Praxis code and may have faulted; its result on
    // that path is the Unit sentinel.
    if unsafe { praxis_check_fault(ctx) } != 0 {
        return None;
    }
    Some(result)
}

/// `v.unique()` — the elements of `vec` with later duplicates dropped, as a
/// **new** `Vec`, in first-occurrence order (§6.3). The receiver is not touched.
///
/// First-occurrence order rather than sorted-and-deduped: `unique` is listed
/// separately from `sorted` in §6.3, so composing them has to be the user's
/// choice, and an order that depends on a hash map's iteration would make the
/// same program answer differently on two runs — and here the order is the
/// program's *answer*, not only its printing.
///
/// Sameness is [`DynamicKey`]'s — the descriptor's `hash` and `equals`
/// callbacks, which is what "the same value" means everywhere else in this
/// runtime (§5.5, §11.3). The catalog row's `HashStable` bound is what keeps a
/// mutable element out; a key that can change after it is stored cannot be found
/// again.
///
/// # Safety
/// `ctx` must be live and wired; `vec` must be a valid `Vec` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_vec_unique(ctx: *mut RuntimeContext, vec: GcRef) -> GcRef {
    abi_guard!("praxis_vec_unique", ctx, {
        // SAFETY: caller guarantees `vec` is a valid Vec.
        let p = unsafe { vec_payload(vec) };
        let mut seen: std::collections::HashSet<DynamicKey> = std::collections::HashSet::new();
        let mut kept: Vec<GcRef> = Vec::new();
        for item in &p.items {
            if seen.insert(DynamicKey::new(*item)) {
                kept.push(*item);
            }
        }
        unsafe { vec_of(ctx, p.element_descriptor, kept.into_iter()) }
    })
}

/// `v.reversed()` — the elements of `vec` in the opposite order, as a **new**
/// `Vec` (ADR-145). The receiver is not touched.
///
/// A barrier for `praxis_vec_sorted`'s reason and not a fused stage: reversal
/// cannot answer its first element until it has seen the last one.
///
/// It reads **no descriptor callback** — not `compare`, not `equals`, not
/// `hash` — so unlike `sorted` and `unique` there is no element it can be handed
/// that it cannot reverse, and its catalog row carries no capability bound. That
/// is why the manifest row is `Allocates` and there is no `TypeMismatch` path
/// here to read.
///
/// The element label is copied through unchanged, the null a construction site
/// that knew no element type leaves included.
///
/// # Safety
/// `ctx` must be live and wired; `vec` must be a valid `Vec` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_vec_reversed(ctx: *mut RuntimeContext, vec: GcRef) -> GcRef {
    abi_guard!("praxis_vec_reversed", ctx, {
        // SAFETY: caller guarantees `vec` is a valid Vec.
        let p = unsafe { vec_payload(vec) };
        let items: Vec<GcRef> = p.items.iter().rev().copied().collect();
        unsafe { vec_of(ctx, p.element_descriptor, items.into_iter()) }
    })
}

/// The group size `chunks(n)` and `windows(n)` share, or `None` when `n` names no
/// group at all (ADR-149).
///
/// **The only thing either wrapper refuses.** A run of zero elements is not a
/// short run — chunking a non-empty sequence into them has no finite answer, and
/// sliding one along it has no useful one — and a negative run names nothing.
/// Every other `n` has an answer, including one larger than the receiver: a
/// `chunks` wider than the sequence is one short chunk, a `windows` wider than it
/// is no windows. So this returns an `Option` of a size rather than clamping to
/// one, and the two callers spell those two answers themselves.
///
/// There is no upper bound here and none is missing. Both results are *shorter*
/// than the receiver — one group per start position at most — so neither can
/// ask for an extent [`VecExtent`](crate::collections::VecExtent) would refuse,
/// which is the bound `praxis_vec_filled` needs and these do not.
fn group_size(n: i64) -> Option<usize> {
    if n <= 0 {
        return None;
    }
    usize::try_from(n).ok()
}

/// The `Vec[Vec[T]]` both groupings answer, built from the half-open source
/// ranges `groups` names (ADR-149).
///
/// **The outer label is `collections::VEC` at every length, and it is *passed*
/// rather than inferred** (ADR-149 decision 1). Which label belongs there is not
/// this wrapper's choice — `outer.push(inner)` builds a `Vec[Vec[T]]` today and
/// `adopt_or_reject` labels it `VEC`, so anything else would disagree with
/// `push`. What is chosen here is only that it is written down: letting
/// [`vec_of`] infer it from the first group would answer `VEC` for
/// `[1].chunks(2)` and *null* for `[].chunks(2)` — one type with two labels, and
/// the null is the one `vec_format` renders as `[]`.
///
/// That is [`praxis_grid_positions`]'s argument, not a new one: it passes
/// `&tuples::TUPLE` for the same reason, and `Grid(0, 0, 1).positions()` is the
/// same empty case. Naming the label is what a wrapper does whenever its result's
/// element kind is not its receiver's.
///
/// The inner labels *are* the receiver's own, passed through unchanged the way
/// `praxis_vec_reversed` passes its one through, null included.
///
/// # Safety
/// `ctx` must be live and wired; `vec` must be a valid `Vec` `GcRef`; every
/// range `groups` yields must lie within its length.
unsafe fn vec_of_groups(
    ctx: *mut RuntimeContext,
    vec: GcRef,
    groups: impl Iterator<Item = (usize, usize)>,
) -> GcRef {
    // SAFETY: caller guarantees `vec` is a valid Vec.
    let element_descriptor = unsafe { vec_payload(vec) }.element_descriptor;
    let outer = unsafe { praxis_vec_new(ctx, &crate::collections::VEC as *const _) };
    let scope = unsafe { NativeScope::new(ctx) };
    let op = unsafe { vec_payload_mut(scope.root(outer)) };
    for (start, end) in groups {
        // The elements are read out of the receiver, which the caller's shadow
        // frame roots across this call, so the untraced `Vec<GcRef>` below holds
        // nothing a collection inside `vec_of` could reclaim — the receiver
        // holds every one of them too. That is `praxis_vec_unique`'s argument at
        // a second site, and it is why the *groups* are what need rooting and
        // the items are not: an inner `Vec` is reachable from nothing until it
        // is pushed, which is why it is pushed before the next one is built.
        //
        // SAFETY: caller guarantees `vec` is a valid Vec and that `start..end`
        // lies within its length.
        let items: Vec<GcRef> = unsafe { vec_payload(vec) }.items[start..end].to_vec();
        let inner = unsafe { vec_of(ctx, element_descriptor, items.into_iter()) };
        op.items.push(inner);
    }
    outer
}

/// `seq.chunks(n)` — these elements in consecutive non-overlapping runs of `n`,
/// the last short if the length does not divide (ADR-149). The receiver is not
/// touched.
///
/// `[1, 2, 3, 4, 5].chunks(2)` is `[[1, 2], [3, 4], [5]]`. An empty receiver
/// answers `[]` at any size, and an `n` at or above the length answers one chunk
/// holding everything.
///
/// Raises `FaultKind::InvalidSize` and answers Unit when `n` is not positive —
/// [`group_size`] has the reason, and it is the wrapper's whole faulting
/// surface. It reads **no descriptor callback**, so unlike `sorted` there is no
/// element it can be handed that it cannot group.
///
/// # Safety
/// `ctx` must be live and wired; `vec` must be a valid `Vec` `GcRef` and `n` a
/// valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_vec_chunks(
    ctx: *mut RuntimeContext,
    vec: GcRef,
    n: GcRef,
) -> GcRef {
    abi_guard!("praxis_vec_chunks", ctx, {
        // SAFETY: caller guarantees `n` is a valid Int.
        let Some(size) = group_size(unsafe { int_payload(n) }) else {
            unsafe { set_fault(ctx, RaisedFault::INVALID_SIZE) };
            return unsafe { unit_sentinel(ctx) };
        };
        // SAFETY: caller guarantees `vec` is a valid Vec.
        let len = unsafe { vec_payload(vec) }.items.len();
        // Every `size`th position starts a chunk; the last one stops at the end
        // rather than past it, which is the short tail.
        let groups = (0..len)
            .step_by(size)
            .map(move |s| (s, (s + size).min(len)));
        unsafe { vec_of_groups(ctx, vec, groups) }
    })
}

/// `seq.windows(n)` — every consecutive run of exactly `n`, each starting one
/// element after the last (ADR-149). The receiver is not touched.
///
/// `[1, 2, 3, 4].windows(2)` is `[[1, 2], [2, 3], [3, 4]]`. Elements are shared,
/// not copied: the `2` in the first window and the `2` in the second are one
/// object, which is the language's reference semantics rather than a rule of
/// this wrapper.
///
/// **A window that does not fit is dropped rather than shortened**, which is the
/// one place this and [`praxis_vec_chunks`] answer differently: `[1, 2].windows(5)`
/// is `[]`, because a run of five is a run of five. It is not the fault below
/// arriving late — "which runs of five are there" has an answer for a sequence
/// of two, and that answer is none.
///
/// Raises `FaultKind::InvalidSize` and answers Unit when `n` is not positive,
/// for [`praxis_vec_chunks`]'s reason.
///
/// # Safety
/// `ctx` must be live and wired; `vec` must be a valid `Vec` `GcRef` and `n` a
/// valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_vec_windows(
    ctx: *mut RuntimeContext,
    vec: GcRef,
    n: GcRef,
) -> GcRef {
    abi_guard!("praxis_vec_windows", ctx, {
        // SAFETY: caller guarantees `n` is a valid Int.
        let Some(size) = group_size(unsafe { int_payload(n) }) else {
            unsafe { set_fault(ctx, RaisedFault::INVALID_SIZE) };
            return unsafe { unit_sentinel(ctx) };
        };
        // SAFETY: caller guarantees `vec` is a valid Vec.
        let len = unsafe { vec_payload(vec) }.items.len();
        // Written as a subtraction guarded by its own comparison rather than a
        // `saturating_sub`: `len - size` saturating to zero would answer *one*
        // window for a receiver too short to hold any, and the empty answer is
        // the whole point of the branch.
        let starts = if size <= len { len - size + 1 } else { 0 };
        let groups = (0..starts).map(move |s| (s, s + size));
        unsafe { vec_of_groups(ctx, vec, groups) }
    })
}

/// `seq.join(sep)` — these `Text` elements concatenated with `sep` between them
/// (ADR-144). An empty sequence answers `""`; a one-element sequence answers
/// that element's characters and no separator.
///
/// Raises `FaultKind::TypeMismatch` and answers Unit when an element is not a
/// `Text`. The catalog row bounds the item to `Text`, so reaching that is a
/// compiler bug — reported the way `praxis_vec_sorted` reports its own, rather
/// than reading a foreign payload as a pointer-and-length pair.
///
/// This does **not** render: a `Vec[Int]` is refused at `praxis check` rather
/// than stringified here, which is what keeps `join` from being a back door
/// around ADR-143's decision about which types have a `to_text`.
///
/// # Safety
/// `ctx` must be live and wired; `vec` must be a valid `Vec` `GcRef` and `sep` a
/// valid `Text` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_vec_join(
    ctx: *mut RuntimeContext,
    vec: GcRef,
    sep: GcRef,
) -> GcRef {
    abi_guard!("praxis_vec_join", ctx, {
        // SAFETY: caller guarantees `vec` is a valid Vec and `sep` a valid Text.
        let p = unsafe { vec_payload(vec) };
        if !p
            .items
            .iter()
            .all(|item| std::ptr::eq(item.descriptor(), &crate::text::TEXT))
        {
            unsafe { set_fault(ctx, RaisedFault::TYPE_MISMATCH) };
            return unsafe { unit_sentinel(ctx) };
        }
        let separator = unsafe { text_str(sep) };
        let mut joined = String::new();
        for (i, item) in p.items.iter().enumerate() {
            if i > 0 {
                joined.push_str(separator);
            }
            // SAFETY: the loop above proved every element's descriptor is TEXT.
            joined.push_str(unsafe { text_str(*item) });
        }
        // SAFETY: `joined` is valid UTF-8; ctx/heap valid.
        unsafe { text_ref(ctx, joined) }
    })
}

/// `chars.to_text()` — these `Char`s as one `Text`, with nothing between them
/// (ADR-144). The inverse of walking a `Text`, and what renders a `Grid` row
/// back as the line it was read from.
///
/// Each code point is read through [`read_scalar`] with the `Char` handle, never
/// a bare payload read: the payload is **four** bytes and an `i64` read would
/// take eight of them. A foreign element is `TypeMismatch` and the Unit
/// sentinel, for [`praxis_vec_join`]'s reason.
///
/// # Safety
/// `ctx` must be live and wired; `vec` must be a valid `Vec` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_vec_to_text(ctx: *mut RuntimeContext, vec: GcRef) -> GcRef {
    abi_guard!("praxis_vec_to_text", ctx, {
        // SAFETY: caller guarantees `vec` is a valid Vec.
        let p = unsafe { vec_payload(vec) };
        let mut rendered = String::new();
        for item in &p.items {
            // SAFETY: `read_scalar` proves the descriptor is `CHAR` before
            // reading its four bytes, and answers `None` otherwise.
            let Some(code) = (unsafe { read_scalar(*item, scalars::CHAR_PAYLOAD) }) else {
                unsafe { set_fault(ctx, RaisedFault::TYPE_MISMATCH) };
                return unsafe { unit_sentinel(ctx) };
            };
            // The descriptor's own writer, so a line rebuilt from a `Grid` row
            // holds the characters `out` would have written one at a time.
            scalars::write_char(&mut rendered, code);
        }
        // SAFETY: `rendered` is valid UTF-8; ctx/heap valid.
        unsafe { text_ref(ctx, rendered) }
    })
}

/// `v.frequencies()` — a `Counter[T]` holding how many times each element of
/// `vec` occurs (§6.3, §6.2).
///
/// The first combinator whose result is a **keyed** collection, which is why the
/// catalog row carries a `HashStable` bound of its own:
/// `require_collection_invariants` is applied to a method's *receiver*, and the
/// receiver here is an ordinary `Vec` that may legitimately hold anything. It is
/// the result that has a key rule.
///
/// The counter's key descriptor is the source `Vec`'s element label, which may
/// be null when the construction site knew no element type — the same null
/// [`praxis_counter_new`] already accepts and means "not told yet".
///
/// # Safety
/// `ctx` must be live and wired; `vec` must be a valid `Vec` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_vec_frequencies(ctx: *mut RuntimeContext, vec: GcRef) -> GcRef {
    abi_guard!("praxis_vec_frequencies", ctx, {
        let scope = unsafe { NativeScope::new(ctx) };
        // The receiver is rooted **explicitly**, unlike the one-allocation
        // wrappers around it. The keys below are `GcRef`s into this `Vec`'s
        // items and they are held across one allocation *per distinct element*,
        // which is a much longer window than `praxis_set_items`' single
        // `vec_of`; relying on the caller's shadow frame alone for that long is
        // an assumption worth not making.
        let _receiver = scope.root(vec);
        // SAFETY: caller guarantees `vec` is a valid Vec.
        let p = unsafe { vec_payload(vec) };
        // Count first, with no allocation at all, so the tally cannot be
        // disturbed by a collection mid-loop. The tally is a `Vec` with a side
        // index rather than a bare map so the counts come out in
        // first-occurrence order, which makes the *allocation* order a function
        // of the input; the `Counter` itself is unordered either way.
        let mut counts: Vec<(DynamicKey, i64)> = Vec::new();
        let mut index: std::collections::HashMap<DynamicKey, usize> =
            std::collections::HashMap::new();
        for item in &p.items {
            let key = DynamicKey::new(*item);
            match index.get(&key) {
                Some(at) => counts[*at].1 += 1,
                None => {
                    index.insert(key, counts.len());
                    counts.push((key, 1));
                }
            }
        }
        let counter = unsafe { praxis_counter_new(ctx, p.element_descriptor) };
        let rooted = scope.root(counter);
        for (key, count) in counts {
            // Allocate first, then take the payload borrow — the boxed `Int`
            // allocation can collect, and the counter has to be reachable
            // through the native root store rather than through a `&mut` this
            // frame is holding across it.
            let boxed = unsafe { int_ref(ctx, count) };
            unsafe { counter_payload_mut(rooted) }
                .entries
                .insert(key, boxed);
        }
        counter
    })
}

// ---------------------------------------------------------------------------
// Deque[T] methods (§6.1). Mirrors the Vec surface but adds the
// front/back distinction: `push_front`/`push_back`/`pop_front`/`pop_back`.
// `pop_*` fault on an empty deque (§9.1 `EmptyCollection`).
// ---------------------------------------------------------------------------

use crate::collections::DequePayload;

/// Read the `DequePayload` out of a `GcRef` as a shared ref, asserting Deque.
///
/// # Safety
/// `r` must be a valid `Deque` `GcRef`.
unsafe fn deque_payload(r: GcRef) -> &'static DequePayload {
    // SAFETY: caller guarantees `r` is a Deque; see `payload_ref`.
    unsafe { payload_ref::<DequePayload>(r) }
}

/// Read the `DequePayload` out of a `GcRef` as a mutable ref, asserting Deque.
///
/// # Safety
/// `r` must be a valid `Deque` `GcRef`, rooted for `'s`.
unsafe fn deque_payload_mut<'s>(r: Rooted<'s>) -> &'s mut DequePayload {
    // SAFETY: caller guarantees `r` is a Deque; see `payload_mut`.
    unsafe { payload_mut::<DequePayload>(r) }
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
    abi_guard!("praxis_deque_new", ctx, {
        // SAFETY: DequePayload is DEQUE's payload type.
        unsafe {
            gc_alloc_owned(ctx, &crate::collections::DEQUE, || DequePayload {
                element_descriptor,
                items: std::collections::VecDeque::new(),
            })
        }
    })
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
    abi_guard!("praxis_deque_push_front", ctx, {
        unsafe { maybe_collect(ctx) };
        let scope = unsafe { NativeScope::new(ctx) };
        let p = unsafe { deque_payload_mut(scope.root(deque)) };
        if !unsafe { adopt_or_reject(ctx, &mut p.element_descriptor, value) } {
            return unsafe { unit_sentinel(ctx) };
        }
        let before = p.owned_bytes();
        p.items.push_front(value);
        charge_growth(ctx, before, p.owned_bytes());
        unsafe { unit_sentinel(ctx) }
    })
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
    abi_guard!("praxis_deque_push_back", ctx, {
        unsafe { maybe_collect(ctx) };
        let scope = unsafe { NativeScope::new(ctx) };
        let p = unsafe { deque_payload_mut(scope.root(deque)) };
        if !unsafe { adopt_or_reject(ctx, &mut p.element_descriptor, value) } {
            return unsafe { unit_sentinel(ctx) };
        }
        let before = p.owned_bytes();
        p.items.push_back(value);
        charge_growth(ctx, before, p.owned_bytes());
        unsafe { unit_sentinel(ctx) }
    })
}

/// Remove and return the front element; faults `EmptyCollection` if empty.
///
/// # Safety
/// `ctx` must be live and wired; `deque` must be a valid `Deque` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_deque_pop_front(ctx: *mut RuntimeContext, deque: GcRef) -> GcRef {
    abi_guard!("praxis_deque_pop_front", ctx, {
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
    })
}

/// Remove and return the back element; faults `EmptyCollection` if empty.
///
/// # Safety
/// `ctx` must be live and wired; `deque` must be a valid `Deque` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_deque_pop_back(ctx: *mut RuntimeContext, deque: GcRef) -> GcRef {
    abi_guard!("praxis_deque_pop_back", ctx, {
        let scope = unsafe { NativeScope::new(ctx) };
        let p = unsafe { deque_payload_mut(scope.root(deque)) };
        match p.items.pop_back() {
            Some(v) => v,
            None => {
                unsafe { set_fault(ctx, RaisedFault::EMPTY_COLLECTION) };
                unsafe { unit_sentinel(ctx) }
            }
        }
    })
}

/// The number of elements in `deque`, as a boxed `Int` (§6.1).
///
/// # Safety
/// `ctx` must be live and wired; `deque` must be a valid `Deque` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_deque_len(ctx: *mut RuntimeContext, deque: GcRef) -> GcRef {
    abi_guard!("praxis_deque_len", ctx, {
        let p = unsafe { deque_payload(deque) };
        let len = p.items.len() as i64;
        unsafe { int_ref(ctx, len) }
    })
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
    abi_guard!("praxis_deque_get", ctx, {
        let p = unsafe { deque_payload(deque) };
        let idx = unsafe { int_payload(index) };
        let Some(idx) = (unsafe { checked_index(ctx, idx, p.items.len()) }) else {
            return unsafe { unit_sentinel(ctx) };
        };
        p.items[idx]
    })
}

/// Replace the element at `index` (0-based from the front); faults
/// `IndexOutOfBounds` if out of range. Returns the Unit sentinel.
///
/// A replacement and never an insertion, and the element descriptor is
/// reconciled the same way, for [`praxis_vec_set`]'s reasons.
///
/// # Safety
/// `ctx` must be live and wired; `deque` must be a valid `Deque` `GcRef`;
/// `index` must be a valid `Int` `GcRef`; `value` must be a valid `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_deque_set(
    ctx: *mut RuntimeContext,
    deque: GcRef,
    index: GcRef,
    value: GcRef,
) -> GcRef {
    abi_guard!("praxis_deque_set", ctx, {
        let scope = unsafe { NativeScope::new(ctx) };
        let p = unsafe { deque_payload_mut(scope.root(deque)) };
        let idx = unsafe { int_payload(index) };
        let Some(idx) = (unsafe { checked_index(ctx, idx, p.items.len()) }) else {
            return unsafe { unit_sentinel(ctx) };
        };
        if !unsafe { adopt_or_reject(ctx, &mut p.element_descriptor, value) } {
            return unsafe { unit_sentinel(ctx) };
        }
        p.items[idx] = value;
        unsafe { unit_sentinel(ctx) }
    })
}

/// True iff `deque` has no elements, as a boxed `Bool` (§6.1).
///
/// # Safety
/// `ctx` must be live and wired; `deque` must be a valid `Deque` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_deque_is_empty(ctx: *mut RuntimeContext, deque: GcRef) -> GcRef {
    abi_guard!("praxis_deque_is_empty", ctx, {
        let p = unsafe { deque_payload(deque) };
        let empty = p.items.is_empty();
        unsafe { bool_ref(ctx, empty) }
    })
}

// ---------------------------------------------------------------------------
// Map[K, V] / Set[T] / Counter[T] (§6.1, §11.3).
//
// All three reuse Rust hash collections behind opaque GC objects. Keys are
// wrapped in `DynamicKey`, which delegates Rust `Hash`/`Eq` to the descriptor's
// structural callbacks — this is what makes tuples/records/enums/nested
// collections work as keys (§19.7 criterion). Counter's absent keys read as
// zero (§6.2); `min=`/`max=` update a map entry in place (§6.2).
// ---------------------------------------------------------------------------

use crate::maps::{CounterPayload, MapPayload, SetPayload};

/// Read a `MapPayload` as a shared ref. See `payload_ref` for the safety model.
unsafe fn map_payload(r: GcRef) -> &'static MapPayload {
    unsafe { payload_ref::<MapPayload>(r) }
}

unsafe fn map_payload_mut<'s>(r: Rooted<'s>) -> &'s mut MapPayload {
    unsafe { payload_mut::<MapPayload>(r) }
}

unsafe fn set_payload(r: GcRef) -> &'static SetPayload {
    unsafe { payload_ref::<SetPayload>(r) }
}

unsafe fn set_payload_mut<'s>(r: Rooted<'s>) -> &'s mut SetPayload {
    unsafe { payload_mut::<SetPayload>(r) }
}

unsafe fn counter_payload(r: GcRef) -> &'static CounterPayload {
    unsafe { payload_ref::<CounterPayload>(r) }
}

unsafe fn counter_payload_mut<'s>(r: Rooted<'s>) -> &'s mut CounterPayload {
    unsafe { payload_mut::<CounterPayload>(r) }
}

/// Allocate an empty `Map[K, V]`. `key_descriptor` is the key type the
/// construction site knew, or **null** when it knew none — which is kept null,
/// the way `praxis_vec_new` keeps it. Spelling an unknown type `INT` is a claim,
/// and every reader that believed it would read the wrong type.
///
/// # Safety
/// `ctx` must be live and wired. `key_descriptor` must be a valid pointer to a
/// `'static TypeDescriptor` (or null).
#[no_mangle]
pub unsafe extern "C" fn praxis_map_new(
    ctx: *mut RuntimeContext,
    key_descriptor: *const TypeDescriptor,
) -> GcRef {
    abi_guard!("praxis_map_new", ctx, {
        // The `Map` row carries one type argument, so the value type never reaches
        // this wrapper at all — it is unknown here by construction, and says so.
        // `praxis_map_insert` adopts the first inserted value's own descriptor,
        // which is how a `Vec` learns its element type.
        let value_descriptor: *const TypeDescriptor = std::ptr::null();
        // SAFETY: MapPayload is MAP's payload type.
        unsafe {
            gc_alloc_owned(ctx, &crate::maps::MAP, || MapPayload {
                key_descriptor,
                value_descriptor,
                entries: std::collections::HashMap::new(),
            })
        }
    })
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
    abi_guard!("praxis_map_insert", ctx, {
        unsafe { maybe_collect(ctx) };
        let scope = unsafe { NativeScope::new(ctx) };
        let p = unsafe { map_payload_mut(scope.root(map)) };
        // Learn the value type from the first value inserted, the way a `Vec`
        // learns its element type from the first `push`. Null is the encoding of
        // "never been told", so it is distinguishable from a `Map` that really
        // holds `Int`s.
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
        let before = p.owned_bytes();
        p.entries.insert(DynamicKey::new(key), value);
        charge_growth(ctx, before, p.owned_bytes());
        unsafe { unit_sentinel(ctx) }
    })
}

/// `Some(value)` for `key`, or `None` if absent (§4.7, §5.7).
///
/// §5.7 writes the signature `Map[K,V].get(K) -> Option[V]` and §4.7 opens
/// "Option[T] represents normal domain-level absence. It is not an error
/// channel." Answering the Unit sentinel under a `V` static type instead would
/// hand the program a value it could not distinguish from a real one without
/// `contains`, while the type system insisted it was a `V`.
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
    abi_guard!("praxis_map_get", ctx, {
        let found = {
            let p = unsafe { map_payload(map) };
            p.entries.get(&DynamicKey::new(key)).copied()
        };
        match found {
            Some(v) => unsafe { option_some(ctx, v) },
            None => unsafe { option_none(ctx) },
        }
    })
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
/// describes. A dedicated `MissingKey` kind would read better, and adding one is
/// a `#[repr(C)]` change that costs an ABI bump (ADR-075).
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
    abi_guard!("praxis_map_index", ctx, {
        let p = unsafe { map_payload(map) };
        match p.entries.get(&DynamicKey::new(key)) {
            Some(v) => *v,
            None => {
                unsafe { set_fault(ctx, RaisedFault::INDEX_OUT_OF_BOUNDS) };
                unsafe { unit_sentinel(ctx) }
            }
        }
    })
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
    abi_guard!("praxis_map_contains", ctx, {
        let p = unsafe { map_payload(map) };
        let present = p.entries.contains_key(&DynamicKey::new(key));
        unsafe { bool_ref(ctx, present) }
    })
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
    abi_guard!("praxis_map_remove", ctx, {
        let scope = unsafe { NativeScope::new(ctx) };
        let p = unsafe { map_payload_mut(scope.root(map)) };
        p.entries.remove(&DynamicKey::new(key));
        unsafe { unit_sentinel(ctx) }
    })
}

/// The number of entries, as a boxed Int.
///
/// # Safety
/// `ctx` must be live and wired; `map` must be a valid `Map` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_map_len(ctx: *mut RuntimeContext, map: GcRef) -> GcRef {
    abi_guard!("praxis_map_len", ctx, {
        let p = unsafe { map_payload(map) };
        unsafe { int_ref(ctx, p.entries.len() as i64) }
    })
}

/// `m.keys()` — every key, as a `Vec[K]`. Ordered like
/// [`praxis_counter_keys`], and index-aligned with [`praxis_map_values`].
///
/// This and `values()` are the only way to enumerate a `Map`: `for kv in m` has
/// no lowering.
///
/// # Safety
/// `ctx` must be live and wired; `map` must be a valid `Map` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_map_keys(ctx: *mut RuntimeContext, map: GcRef) -> GcRef {
    abi_guard!("praxis_map_keys", ctx, {
        let key_desc = unsafe { map_payload(map) }.key_descriptor;
        let rows = unsafe { crate::maps::ordered_entries(&map_payload(map).entries) };
        unsafe { vec_of(ctx, key_desc, rows.into_iter().map(|(k, _)| k)) }
    })
}

/// `m.values()` — every value, as a `Vec[V]`. See [`praxis_map_keys`].
///
/// # Safety
/// `ctx` must be live and wired; `map` must be a valid `Map` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_map_values(ctx: *mut RuntimeContext, map: GcRef) -> GcRef {
    abi_guard!("praxis_map_values", ctx, {
        let val_desc = unsafe { map_payload(map) }.value_descriptor;
        let rows = unsafe { crate::maps::ordered_entries(&map_payload(map).entries) };
        unsafe { vec_of(ctx, val_desc, rows.into_iter().map(|(_, v)| v)) }
    })
}

/// True iff the map is empty, as a boxed Bool.
///
/// # Safety
/// `ctx` must be live and wired; `map` must be a valid `Map` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_map_is_empty(ctx: *mut RuntimeContext, map: GcRef) -> GcRef {
    abi_guard!("praxis_map_is_empty", ctx, {
        let p = unsafe { map_payload(map) };
        unsafe { bool_ref(ctx, p.entries.is_empty()) }
    })
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
    abi_guard!("praxis_map_update_min", ctx, {
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
    })
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
    abi_guard!("praxis_map_update_max", ctx, {
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
    })
}

// --- Set[T] -----------------------------------------------------------------

/// Allocate an empty `Set[T]`. `element_descriptor` is the element type the
/// construction site knew, or **null** when it knew none — kept null.
///
/// # Safety
/// `ctx` must be live and wired; `element_descriptor` must be a valid pointer to
/// a `'static TypeDescriptor` (or null).
#[no_mangle]
pub unsafe extern "C" fn praxis_set_new(
    ctx: *mut RuntimeContext,
    element_descriptor: *const TypeDescriptor,
) -> GcRef {
    abi_guard!("praxis_set_new", ctx, {
        // SAFETY: SetPayload is SET's payload type.
        unsafe {
            gc_alloc_owned(ctx, &crate::maps::SET, || SetPayload {
                element_descriptor,
                entries: std::collections::HashSet::new(),
            })
        }
    })
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
    abi_guard!("praxis_set_insert", ctx, {
        unsafe { maybe_collect(ctx) };
        let scope = unsafe { NativeScope::new(ctx) };
        let p = unsafe { set_payload_mut(scope.root(set)) };
        let before = p.owned_bytes();
        p.entries.insert(DynamicKey::new(value));
        charge_growth(ctx, before, p.owned_bytes());
        unsafe { unit_sentinel(ctx) }
    })
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
    abi_guard!("praxis_set_remove", ctx, {
        let scope = unsafe { NativeScope::new(ctx) };
        let p = unsafe { set_payload_mut(scope.root(set)) };
        p.entries.remove(&DynamicKey::new(value));
        unsafe { unit_sentinel(ctx) }
    })
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
    abi_guard!("praxis_set_contains", ctx, {
        let p = unsafe { set_payload(set) };
        let present = p.entries.contains(&DynamicKey::new(value));
        unsafe { bool_ref(ctx, present) }
    })
}

/// The number of elements, as a boxed Int.
///
/// # Safety
/// `ctx` must be live and wired; `set` must be a valid `Set` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_set_len(ctx: *mut RuntimeContext, set: GcRef) -> GcRef {
    abi_guard!("praxis_set_len", ctx, {
        let p = unsafe { set_payload(set) };
        unsafe { int_ref(ctx, p.entries.len() as i64) }
    })
}

/// True iff the set is empty, as a boxed Bool.
///
/// # Safety
/// `ctx` must be live and wired; `set` must be a valid `Set` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_set_is_empty(ctx: *mut RuntimeContext, set: GcRef) -> GcRef {
    abi_guard!("praxis_set_is_empty", ctx, {
        let p = unsafe { set_payload(set) };
        unsafe { bool_ref(ctx, p.entries.is_empty()) }
    })
}

/// Every member, as a `Vec[T]` in [`crate::maps::ordered_members`] order — the
/// snapshot `for x in s` iterates (ADR-066).
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
    abi_guard!("praxis_set_items", ctx, {
        let elem_desc = unsafe { set_payload(set) }.element_descriptor;
        let members = unsafe { crate::maps::ordered_members(&set_payload(set).entries) };
        unsafe { vec_of(ctx, elem_desc, members.into_iter()) }
    })
}

// --- Counter[T] -------------------------------------------------------------

/// Allocate an empty `Counter[T]`. `key_descriptor` is the key type the
/// construction site knew, or **null** when it knew none — kept null.
///
/// # Safety
/// `ctx` must be live and wired; `key_descriptor` must be a valid pointer to a
/// `'static TypeDescriptor` (or null).
#[no_mangle]
pub unsafe extern "C" fn praxis_counter_new(
    ctx: *mut RuntimeContext,
    key_descriptor: *const TypeDescriptor,
) -> GcRef {
    abi_guard!("praxis_counter_new", ctx, {
        // SAFETY: CounterPayload is COUNTER's payload type.
        unsafe {
            gc_alloc_owned(ctx, &crate::maps::COUNTER, || CounterPayload {
                key_descriptor,
                entries: std::collections::HashMap::new(),
            })
        }
    })
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
    abi_guard!("praxis_counter_get", ctx, {
        let p = unsafe { counter_payload(counter) };
        let count = match p.entries.get(&DynamicKey::new(key)) {
            Some(v) => unsafe { int_payload(*v) },
            None => 0, // §6.2: absent reads as zero.
        };
        unsafe { int_ref(ctx, count) }
    })
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
    abi_guard!("praxis_counter_inc", ctx, {
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
                *v = unsafe { int_ref(ctx, next) };
            }
            None => {
                let one = unsafe { int_ref(ctx, 1_i64) };
                p.entries.insert(dk, one);
            }
        }
        unsafe { unit_sentinel(ctx) }
    })
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
    abi_guard!("praxis_counter_set", ctx, {
        unsafe { maybe_collect(ctx) };
        let scope = unsafe { NativeScope::new(ctx) };
        let p = unsafe { counter_payload_mut(scope.root(counter)) };
        let before = p.owned_bytes();
        p.entries.insert(DynamicKey::new(key), value);
        charge_growth(ctx, before, p.owned_bytes());
        unsafe { unit_sentinel(ctx) }
    })
}

/// `c.keys()` — every key, as a `Vec[T]`.
///
/// Ordered by the key's own `compare` (ADR-138), so it is the *same* order
/// [`praxis_counter_values`] uses and the two are index-aligned. A `HashMap`'s
/// own order is randomized per process, so returning it would make the same
/// program answer differently on two runs — and here the order is the program's
/// *answer*, not only its printing.
///
/// # Safety
/// `ctx` must be live and wired; `counter` must be a valid `Counter` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_counter_keys(ctx: *mut RuntimeContext, counter: GcRef) -> GcRef {
    abi_guard!("praxis_counter_keys", ctx, {
        let key_desc = unsafe { counter_payload(counter) }.key_descriptor;
        let rows = unsafe { crate::maps::ordered_entries(&counter_payload(counter).entries) };
        unsafe { vec_of(ctx, key_desc, rows.into_iter().map(|(k, _)| k)) }
    })
}

/// `c.values()` — every count, as a `Vec[Int]`.
///
/// §3.3's representative program is `counts.values().count(|n| n >= 2)`. Ordered
/// like [`praxis_counter_keys`]; see it for why the order is fixed.
///
/// # Safety
/// `ctx` must be live and wired; `counter` must be a valid `Counter` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_counter_values(ctx: *mut RuntimeContext, counter: GcRef) -> GcRef {
    abi_guard!("praxis_counter_values", ctx, {
        let rows = unsafe { crate::maps::ordered_entries(&counter_payload(counter).entries) };
        unsafe { vec_of(ctx, &scalars::INT, rows.into_iter().map(|(_, v)| v)) }
    })
}

/// The number of distinct keys, as a boxed Int.
///
/// # Safety
/// `ctx` must be live and wired; `counter` must be a valid `Counter` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_counter_len(ctx: *mut RuntimeContext, counter: GcRef) -> GcRef {
    abi_guard!("praxis_counter_len", ctx, {
        let p = unsafe { counter_payload(counter) };
        unsafe { int_ref(ctx, p.entries.len() as i64) }
    })
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
    abi_guard!("praxis_counter_is_empty", ctx, {
        let p = unsafe { counter_payload(counter) };
        unsafe { bool_ref(ctx, p.entries.is_empty()) }
    })
}

// ---------------------------------------------------------------------------
// MinHeap[T] / MaxHeap[T] (§6.1, §11.2).
//
// `MaxHeap` maps directly to Rust's max-`BinaryHeap`; `MinHeap` wraps entries in
// `Reverse` so the smallest surfaces first. `pop`/`peek` fault `EmptyCollection`
// on an empty heap.
// ---------------------------------------------------------------------------

use crate::heaps::{HeapEntry, MaxHeapPayload, MinHeapPayload};
use std::collections::BinaryHeap;

unsafe fn max_heap_payload_mut<'s>(r: Rooted<'s>) -> &'s mut MaxHeapPayload {
    unsafe { payload_mut::<MaxHeapPayload>(r) }
}

unsafe fn max_heap_payload(r: GcRef) -> &'static MaxHeapPayload {
    unsafe { payload_ref::<MaxHeapPayload>(r) }
}

unsafe fn min_heap_payload_mut<'s>(r: Rooted<'s>) -> &'s mut MinHeapPayload {
    unsafe { payload_mut::<MinHeapPayload>(r) }
}

unsafe fn min_heap_payload(r: GcRef) -> &'static MinHeapPayload {
    unsafe { payload_ref::<MinHeapPayload>(r) }
}

/// Allocate an empty `MaxHeap[T]`. A null `element_descriptor` — the codegen's
/// "no static element type" — is kept null.
///
/// # Safety
/// `ctx` must be live and wired; `element_descriptor` must be a valid pointer to
/// a `'static TypeDescriptor` (or null).
#[no_mangle]
pub unsafe extern "C" fn praxis_max_heap_new(
    ctx: *mut RuntimeContext,
    element_descriptor: *const TypeDescriptor,
) -> GcRef {
    abi_guard!("praxis_max_heap_new", ctx, {
        // SAFETY: MaxHeapPayload is MAX_HEAP's payload type.
        unsafe {
            gc_alloc_owned(ctx, &crate::heaps::MAX_HEAP, || MaxHeapPayload {
                element_descriptor,
                items: BinaryHeap::new(),
            })
        }
    })
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
    abi_guard!("praxis_max_heap_push", ctx, {
        unsafe { maybe_collect(ctx) };
        let scope = unsafe { NativeScope::new(ctx) };
        let p = unsafe { max_heap_payload_mut(scope.root(heap_ref)) };
        let before = p.owned_bytes();
        p.items.push(HeapEntry {
            value,
            descriptor: value.descriptor(),
        });
        charge_growth(ctx, before, p.owned_bytes());
        unsafe { unit_sentinel(ctx) }
    })
}

/// Remove and return the largest element; faults `EmptyCollection` if empty.
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MaxHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_max_heap_pop(ctx: *mut RuntimeContext, heap_ref: GcRef) -> GcRef {
    abi_guard!("praxis_max_heap_pop", ctx, {
        let scope = unsafe { NativeScope::new(ctx) };
        let p = unsafe { max_heap_payload_mut(scope.root(heap_ref)) };
        match p.items.pop() {
            Some(e) => e.value,
            None => {
                unsafe { set_fault(ctx, RaisedFault::EMPTY_COLLECTION) };
                unsafe { unit_sentinel(ctx) }
            }
        }
    })
}

/// The largest element without removing it; faults `EmptyCollection` if empty.
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MaxHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_max_heap_peek(ctx: *mut RuntimeContext, heap_ref: GcRef) -> GcRef {
    abi_guard!("praxis_max_heap_peek", ctx, {
        let p = unsafe { max_heap_payload(heap_ref) };
        match p.items.peek() {
            Some(e) => e.value,
            None => {
                unsafe { set_fault(ctx, RaisedFault::EMPTY_COLLECTION) };
                unsafe { unit_sentinel(ctx) }
            }
        }
    })
}

/// The number of elements, as a boxed Int.
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MaxHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_max_heap_len(ctx: *mut RuntimeContext, heap_ref: GcRef) -> GcRef {
    abi_guard!("praxis_max_heap_len", ctx, {
        let p = unsafe { max_heap_payload(heap_ref) };
        unsafe { int_ref(ctx, p.items.len() as i64) }
    })
}

/// Every element, as a `Vec[T]` in [`crate::heaps::in_pop_order`] — the snapshot
/// `for x in h` iterates (ADR-066). The heap is **not** drained.
///
/// A heap's backing array is heap-ordered only at its root, so an indexed
/// accessor over it would answer in insertion-history order — reading the array
/// as if it were a `Vec`'s.
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MaxHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_max_heap_items(ctx: *mut RuntimeContext, heap_ref: GcRef) -> GcRef {
    abi_guard!("praxis_max_heap_items", ctx, {
        let p = unsafe { max_heap_payload(heap_ref) };
        let items = crate::heaps::in_pop_order(&p.items, |e| e.value);
        unsafe { vec_of(ctx, p.element_descriptor, items.into_iter()) }
    })
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
    abi_guard!("praxis_max_heap_is_empty", ctx, {
        let p = unsafe { max_heap_payload(heap_ref) };
        unsafe { bool_ref(ctx, p.items.is_empty()) }
    })
}

// --- MinHeap (mirrors MaxHeap with Reverse wrapping) -----------------------

/// Allocate an empty `MinHeap[T]`. A null `element_descriptor` — the codegen's
/// "no static element type" — is kept null.
///
/// # Safety
/// `ctx` must be live and wired; `element_descriptor` must be a valid pointer to
/// a `'static TypeDescriptor` (or null).
#[no_mangle]
pub unsafe extern "C" fn praxis_min_heap_new(
    ctx: *mut RuntimeContext,
    element_descriptor: *const TypeDescriptor,
) -> GcRef {
    abi_guard!("praxis_min_heap_new", ctx, {
        // SAFETY: MinHeapPayload is MIN_HEAP's payload type.
        unsafe {
            gc_alloc_owned(ctx, &crate::heaps::MIN_HEAP, || MinHeapPayload {
                element_descriptor,
                items: BinaryHeap::new(),
            })
        }
    })
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
    abi_guard!("praxis_min_heap_push", ctx, {
        unsafe { maybe_collect(ctx) };
        let scope = unsafe { NativeScope::new(ctx) };
        let p = unsafe { min_heap_payload_mut(scope.root(heap_ref)) };
        let before = p.owned_bytes();
        p.items.push(std::cmp::Reverse(HeapEntry {
            value,
            descriptor: value.descriptor(),
        }));
        charge_growth(ctx, before, p.owned_bytes());
        unsafe { unit_sentinel(ctx) }
    })
}

/// Remove and return the smallest element; faults `EmptyCollection` if empty.
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MinHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_min_heap_pop(ctx: *mut RuntimeContext, heap_ref: GcRef) -> GcRef {
    abi_guard!("praxis_min_heap_pop", ctx, {
        let scope = unsafe { NativeScope::new(ctx) };
        let p = unsafe { min_heap_payload_mut(scope.root(heap_ref)) };
        match p.items.pop() {
            Some(e) => e.0.value,
            None => {
                unsafe { set_fault(ctx, RaisedFault::EMPTY_COLLECTION) };
                unsafe { unit_sentinel(ctx) }
            }
        }
    })
}

/// The smallest element without removing it; faults `EmptyCollection` if empty.
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MinHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_min_heap_peek(ctx: *mut RuntimeContext, heap_ref: GcRef) -> GcRef {
    abi_guard!("praxis_min_heap_peek", ctx, {
        let p = unsafe { min_heap_payload(heap_ref) };
        match p.items.peek() {
            Some(e) => e.0.value,
            None => {
                unsafe { set_fault(ctx, RaisedFault::EMPTY_COLLECTION) };
                unsafe { unit_sentinel(ctx) }
            }
        }
    })
}

/// The number of elements, as a boxed Int.
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MinHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_min_heap_len(ctx: *mut RuntimeContext, heap_ref: GcRef) -> GcRef {
    abi_guard!("praxis_min_heap_len", ctx, {
        let p = unsafe { min_heap_payload(heap_ref) };
        unsafe { int_ref(ctx, p.items.len() as i64) }
    })
}

/// Every element, as a `Vec[T]` in [`crate::heaps::in_pop_order`] — ascending,
/// because the stored entry is a `Reverse<HeapEntry>`. See
/// [`praxis_max_heap_items`].
///
/// # Safety
/// `ctx` must be live and wired; `heap_ref` must be a valid `MinHeap` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_min_heap_items(ctx: *mut RuntimeContext, heap_ref: GcRef) -> GcRef {
    abi_guard!("praxis_min_heap_items", ctx, {
        let p = unsafe { min_heap_payload(heap_ref) };
        let items = crate::heaps::in_pop_order(&p.items, |e| e.0.value);
        unsafe { vec_of(ctx, p.element_descriptor, items.into_iter()) }
    })
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
    abi_guard!("praxis_min_heap_is_empty", ctx, {
        let p = unsafe { min_heap_payload(heap_ref) };
        unsafe { bool_ref(ctx, p.items.is_empty()) }
    })
}

// ---------------------------------------------------------------------------
// BitSet (§6.1). A compact set of non-negative integers.
// ---------------------------------------------------------------------------

use crate::bitset::{BitIndex, BitSetPayload};

unsafe fn bitset_payload(r: GcRef) -> &'static BitSetPayload {
    unsafe { payload_ref::<BitSetPayload>(r) }
}

unsafe fn bitset_payload_mut<'s>(r: Rooted<'s>) -> &'s mut BitSetPayload {
    unsafe { payload_mut::<BitSetPayload>(r) }
}

/// Allocate an empty `BitSet` (§6.1). Nullary — no element descriptor.
///
/// # Safety
/// `ctx` must be live and wired.
#[no_mangle]
pub unsafe extern "C" fn praxis_bitset_new(ctx: *mut RuntimeContext) -> GcRef {
    abi_guard!("praxis_bitset_new", ctx, {
        // SAFETY: BitSetPayload is BITSET's payload type.
        unsafe {
            gc_alloc_owned(ctx, &crate::bitset::BITSET, || BitSetPayload {
                words: ReprCVec::new(),
            })
        }
    })
}

/// Set bit `value`; returns Unit. Faults `InvalidSize` if `value` is negative
/// or above [`BitIndex::MAX`] — a member this set cannot hold.
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
    abi_guard!("praxis_bitset_insert", ctx, {
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
        // A `BitSet` grows its word vector to reach the index, so an insert far
        // past the current high-water is a large uncharged allocation — the
        // shape `bfs` has, one visited-set per search.
        let before = p.owned_bytes();
        p.insert(index);
        charge_growth(ctx, before, p.owned_bytes());
        unsafe { unit_sentinel(ctx) }
    })
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
    abi_guard!("praxis_bitset_remove", ctx, {
        let scope = unsafe { NativeScope::new(ctx) };
        let p = unsafe { bitset_payload_mut(scope.root(bs)) };
        let i = unsafe { int_payload(value) };
        if let Some(index) = BitIndex::new(i) {
            p.remove(index);
        }
        unsafe { unit_sentinel(ctx) }
    })
}

/// True iff bit `value` is set, as a raw `0`/`1` in the scalar channel. A value
/// the set cannot hold is simply absent — the query is total.
///
/// **It answers an `i64` and not a boxed `Bool` (ADR-118 decision 6.)** A boxed
/// answer would be unboxed again on the next instruction — `if bs.contains(x)`
/// as a `Materialize{Bool}`, an `ExtractScalar{Bool}` and then the branch that
/// wanted the predicate. `praxis_struct_eq` and `praxis_value_cmp` answer the
/// scalar channel for the same reason, and MIR carries this one as
/// [`Inst::BitsetContains`](praxis_mir::Inst::BitsetContains) — a
/// `Scalar(Bool)` result, and, because it neither allocates nor faults, not a
/// GC safepoint.
///
/// `0` and `1` and nothing else, which is what the `Bool` payload byte holds
/// and what `emit_inline_bool` re-boxes with a `!= 0` test.
///
/// # Safety
/// `ctx` must be live and wired; `bs` and `value` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_bitset_contains(
    ctx: *mut RuntimeContext,
    bs: GcRef,
    value: GcRef,
) -> i64 {
    abi_guard!("praxis_bitset_contains", ctx, {
        let p = unsafe { bitset_payload(bs) };
        let i = unsafe { int_payload(value) };
        let present = BitIndex::new(i).is_some_and(|index| p.contains(index));
        i64::from(present)
    })
}

/// The number of set bits, as a boxed Int.
///
/// # Safety
/// `ctx` must be live and wired; `bs` must be a valid `BitSet` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_bitset_len(ctx: *mut RuntimeContext, bs: GcRef) -> GcRef {
    abi_guard!("praxis_bitset_len", ctx, {
        let p = unsafe { bitset_payload(bs) };
        unsafe { int_ref(ctx, p.count() as i64) }
    })
}

/// Every member, as a `Vec[Int]` **ascending** — the snapshot `for i in b`
/// iterates (ADR-066).
///
/// This is the one iterable whose members are not objects: they are bit
/// positions, so each one is boxed here rather than copied from the payload.
///
/// # Safety
/// `ctx` must be live and wired; `bs` must be a valid `BitSet` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_bitset_items(ctx: *mut RuntimeContext, bs: GcRef) -> GcRef {
    abi_guard!("praxis_bitset_items", ctx, {
        // The members are read out before the first allocation: `vec_of` allocates
        // per element, and a collection during the walk would move nothing here
        // (the bits are not objects) but would leave the borrow of the payload
        // spanning a safepoint, which is not allowed.
        let members: Vec<i64> = unsafe { bitset_payload(bs) }.members().collect();
        let result = unsafe { praxis_vec_new(ctx, &scalars::INT as *const _) };
        let scope = unsafe { NativeScope::new(ctx) };
        let rooted = scope.root(result);
        for value in members {
            let boxed = unsafe { int_ref(ctx, value) };
            unsafe { vec_payload_mut(rooted) }.items.push(boxed);
        }
        result
    })
}

/// True iff the bitset is empty, as a boxed Bool.
///
/// # Safety
/// `ctx` must be live and wired; `bs` must be a valid `BitSet` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_bitset_is_empty(ctx: *mut RuntimeContext, bs: GcRef) -> GcRef {
    abi_guard!("praxis_bitset_is_empty", ctx, {
        let p = unsafe { bitset_payload(bs) };
        unsafe { bool_ref(ctx, p.count() == 0) }
    })
}

// ---------------------------------------------------------------------------
// Grid[T] methods (§6.4). `GridPayload` is a row-major `Vec<GcRef>` plus a
// width. Coordinates are (x, y) with x rightward, y downward (§6.4). Indexing
// stays behind runtime wrappers (§11.5 realloc safety).
// ---------------------------------------------------------------------------

use crate::collections::{GridExtent, GridPayload};

unsafe fn grid_payload(r: GcRef) -> &'static GridPayload {
    unsafe { payload_ref::<GridPayload>(r) }
}

unsafe fn grid_payload_mut<'s>(r: Rooted<'s>) -> &'s mut GridPayload {
    unsafe { payload_mut::<GridPayload>(r) }
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
    let x_ref = scope.root(unsafe { int_ref(ctx, x) });
    unsafe { praxis_tuple_set(ctx, tup.get(), 0, x_ref.get()) };
    let y_ref = unsafe { int_ref(ctx, y) };
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
/// point tuple, so `(i64::MAX, 0).neighbors4()` would otherwise overflow the
/// addition and panic *inside* `extern "C"`. A coordinate that overflows is
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
            B::Int => Some(int_ref(ctx, 0_i64)),
            B::Byte => Some(gc_alloc(ctx, scalars::BYTE_PAYLOAD, 0_u8)),
            // `0_u32`, not `'\0'`: a `Char`'s payload is the scalar *value*,
            // and a Rust `char` only fits because it shares `u32`'s layout. NUL
            // is inside the interned range, so this is the immortal, like the
            // `Int` arm above.
            B::Char => Some(char_ref(ctx, 0_u32)),
            B::Float => Some(gc_alloc(ctx, scalars::FLOAT_PAYLOAD, 0.0_f64)),
            // `(null, 0)` meets `praxis_alloc_text`'s UTF-8 precondition
            // trivially: the wrapper's own `bytes.is_null() || len == 0` branch
            // turns it into the empty slice, and the empty slice is UTF-8.
            // The precondition is load-bearing (ADR-111): a violation here
            // would abort, not fault. `alloc_text_empty_string_round_trips`
            // pins the branch this depends on.
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
/// [`GridExtent::MAX_CELLS`] — the sizes arrive from source, where a negative
/// value would otherwise land near `usize::MAX` on the cast.
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
    abi_guard!("praxis_grid_new", ctx, {
        let Some(extent) = GridExtent::new(width, height) else {
            unsafe { set_fault(ctx, RaisedFault::INVALID_SIZE) };
            return unsafe { unit_sentinel(ctx) };
        };
        // Every cell of a `Grid[T]` must *be* a `T`. Filling with the Unit sentinel
        // under a `T` element descriptor is the same lie as a mislabelled element
        // descriptor, one level down: `get`, `format`, `equals` and `hash` all
        // dispatch `T`'s callbacks against a zero-sized Unit payload.
        let cells = if extent.cells() == 0 {
            Vec::new()
        } else {
            let Some(fill) = (unsafe { default_cell(ctx, element_descriptor) }) else {
                unsafe { set_fault(ctx, RaisedFault::TYPE_MISMATCH) };
                return unsafe { unit_sentinel(ctx) };
            };
            vec![fill; extent.cells()]
        };
        // SAFETY: GridPayload is GRID's payload type.
        unsafe {
            gc_alloc_owned(ctx, &crate::collections::GRID, || GridPayload {
                element_descriptor,
                items: cells,
                width: extent.width(),
            })
        }
    })
}

/// Allocate a `Grid[T]` of `width` × `height` cells, every one holding `fill`
/// (ADR-146's `Grid(w, h, fill)`) — the working grid an algorithm allocates for
/// itself: an occupancy board, a visited mask, a distance table.
///
/// Faults `InvalidSize` on the extents [`praxis_grid_new`] refuses, through the
/// same [`GridExtent::new`], since this is the very allocation ADR-041 was
/// written about and a fill changes nothing about the arithmetic.
///
/// **It does not call [`default_cell`], and that is the whole difference.**
/// `praxis_grid_new` has to invent a zero value for the cell type and has none
/// for a composite, so it raises `TypeMismatch` for a `Grid[Vec[Int]]` rather
/// than filling it with Unit sentinels under a `Vec` descriptor. An explicit
/// fill removes the question — the caller supplied a value of the cell type —
/// so a grid of collections is constructible here and not there. The descriptor
/// is still reconciled through [`adopt_or_reject`], so a *declared* cell type
/// the fill does not match is `TypeMismatch` rather than a silent retag.
///
/// Every cell is the same `GcRef`, exactly as [`praxis_vec_filled`]'s are; see
/// its comment for why that is the language's existing rule rather than a new
/// one. The extents arrive boxed for the reason stated there too.
///
/// # Safety
/// `ctx` must be live and wired; `element_descriptor` must be a valid pointer to
/// a `'static TypeDescriptor` (or null); `width` and `height` must be valid
/// `Int` `GcRef`s; `fill` must be a valid `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_filled(
    ctx: *mut RuntimeContext,
    element_descriptor: *const TypeDescriptor,
    width: GcRef,
    height: GcRef,
    fill: GcRef,
) -> GcRef {
    abi_guard!("praxis_grid_filled", ctx, {
        // SAFETY: caller guarantees `width` and `height` are valid Ints.
        let (w, h) = unsafe { (int_payload(width), int_payload(height)) };
        let Some(extent) = GridExtent::new(w, h) else {
            unsafe { set_fault(ctx, RaisedFault::INVALID_SIZE) };
            return unsafe { unit_sentinel(ctx) };
        };
        let mut descriptor = element_descriptor;
        if !unsafe { adopt_or_reject(ctx, &mut descriptor, fill) } {
            return unsafe { unit_sentinel(ctx) };
        }
        let scope = unsafe { NativeScope::new(ctx) };
        let fill = scope.root(fill).get();
        // The cells are built inside the initializer, which `gc_alloc_owned` runs
        // *after* the safepoint — `praxis_vec_filled`'s rule, for the same
        // untraced `Vec<GcRef>`.
        // SAFETY: GridPayload is GRID's payload type.
        unsafe {
            gc_alloc_owned(ctx, &crate::collections::GRID, || GridPayload {
                element_descriptor: descriptor,
                items: vec![fill; extent.cells()],
                width: extent.width(),
            })
        }
    })
}

/// The grid width (number of columns), as a boxed Int.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_width(ctx: *mut RuntimeContext, grid: GcRef) -> GcRef {
    abi_guard!("praxis_grid_width", ctx, {
        let p = unsafe { grid_payload(grid) };
        unsafe { int_ref(ctx, p.width as i64) }
    })
}

/// The grid height (number of rows), as a boxed Int.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_height(ctx: *mut RuntimeContext, grid: GcRef) -> GcRef {
    abi_guard!("praxis_grid_height", ctx, {
        let p = unsafe { grid_payload(grid) };
        // height = items.len() / width.
        let height = grid_height(p.items.len(), p.width);
        unsafe { int_ref(ctx, height as i64) }
    })
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
    abi_guard!("praxis_grid_get", ctx, {
        let p = unsafe { grid_payload(grid) };
        let (xi, yi) = (unsafe { int_payload(x) }, unsafe { int_payload(y) });
        let height = grid_height(p.items.len(), p.width);
        let Some(idx) = (unsafe { checked_cell(ctx, xi, yi, p.width, height) }) else {
            return unsafe { unit_sentinel(ctx) };
        };
        p.items[idx]
    })
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
    abi_guard!("praxis_grid_set", ctx, {
        let scope = unsafe { NativeScope::new(ctx) };
        let p = unsafe { grid_payload_mut(scope.root(grid)) };
        let (xi, yi) = (unsafe { int_payload(x) }, unsafe { int_payload(y) });
        let height = grid_height(p.items.len(), p.width);
        let Some(idx) = (unsafe { checked_cell(ctx, xi, yi, p.width, height) }) else {
            return unsafe { unit_sentinel(ctx) };
        };
        if !unsafe { adopt_or_reject(ctx, &mut p.element_descriptor, value) } {
            return unsafe { unit_sentinel(ctx) };
        }
        p.items[idx] = value;
        unsafe { unit_sentinel(ctx) }
    })
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
    abi_guard!("praxis_grid_contains", ctx, {
        let p = unsafe { grid_payload(grid) };
        let (xi, yi) = (unsafe { int_payload(x) }, unsafe { int_payload(y) });
        let height = grid_height(p.items.len(), p.width);
        // The **pure** [`cell_index`], never `checked_cell`: this wrapper's
        // manifest row is `Pure`, so generated code emits no `CheckFault` after
        // it and a fault raised on every legitimate `false` would sit pending
        // until an unrelated check picked it up.
        let inside = cell_index(xi, yi, p.width, height).is_some();
        unsafe { bool_ref(ctx, inside) }
    })
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
    abi_guard!("praxis_grid_neighbors4", ctx, {
        let p = unsafe { grid_payload(grid) };
        // `point` is an `(Int, Int)` tuple; read its two elements.
        let tp =
            point.payload::<crate::tuples::TuplePayload>() as *const crate::tuples::TuplePayload;
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
    })
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
    abi_guard!("praxis_grid_neighbors8", ctx, {
        let p = unsafe { grid_payload(grid) };
        let tp =
            point.payload::<crate::tuples::TuplePayload>() as *const crate::tuples::TuplePayload;
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
    })
}

/// All `(x, y)` positions in row-major order, as a `Vec`.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_positions(ctx: *mut RuntimeContext, grid: GcRef) -> GcRef {
    abi_guard!("praxis_grid_positions", ctx, {
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
    })
}

/// All cells in row-major order, as a `Vec`.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_cells(ctx: *mut RuntimeContext, grid: GcRef) -> GcRef {
    abi_guard!("praxis_grid_cells", ctx, {
        let p = unsafe { grid_payload(grid) };
        let result = unsafe { praxis_vec_new(ctx, p.element_descriptor) };
        let scope = unsafe { NativeScope::new(ctx) };
        let rp = unsafe { vec_payload_mut(scope.root(result)) };
        for cell in p.items.iter() {
            rp.items.push(*cell);
        }
        result
    })
}

/// Row `y` as a `Vec`; faults `IndexOutOfBounds` if out of range.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`; `y`
/// must be a valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_row(ctx: *mut RuntimeContext, grid: GcRef, y: GcRef) -> GcRef {
    abi_guard!("praxis_grid_row", ctx, {
        let p = unsafe { grid_payload(grid) };
        let yi = unsafe { int_payload(y) };
        let height = grid_height(p.items.len(), p.width);
        // One axis of [`cell_index`]'s rule: a row is bounded by the height
        // alone, and every `x` in it is in range by construction.
        let Some(row) = (unsafe { checked_index(ctx, yi, height) }) else {
            return unsafe { unit_sentinel(ctx) };
        };
        let start = row * p.width;
        let result = unsafe { praxis_vec_new(ctx, p.element_descriptor) };
        let scope = unsafe { NativeScope::new(ctx) };
        let rp = unsafe { vec_payload_mut(scope.root(result)) };
        for x in 0..p.width {
            rp.items.push(p.items[start + x]);
        }
        result
    })
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
    abi_guard!("praxis_grid_column", ctx, {
        let p = unsafe { grid_payload(grid) };
        let xi = unsafe { int_payload(x) };
        // The other axis: a column is bounded by the width alone, and the
        // stride below walks only the rows that exist.
        let Some(col) = (unsafe { checked_index(ctx, xi, p.width) }) else {
            return unsafe { unit_sentinel(ctx) };
        };
        let result = unsafe { praxis_vec_new(ctx, p.element_descriptor) };
        let scope = unsafe { NativeScope::new(ctx) };
        let rp = unsafe { vec_payload_mut(scope.root(result)) };
        let mut idx = col;
        while idx < p.items.len() {
            rp.items.push(p.items[idx]);
            idx += p.width;
        }
        result
    })
}

/// `Some((x, y))` for the first position whose cell equals `value`, or `None`
/// (§4.7).
///
/// An `Option` rather than a sentinel: the Unit sentinel under a `(Int, Int)`
/// static type is indistinguishable from a real answer. `find_all` needs no
/// equivalent — a `Vec` already encodes "nothing matched" as emptiness.
///
/// # Safety
/// `ctx` must be live and wired; `grid` and `value` must be valid `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_find(
    ctx: *mut RuntimeContext,
    grid: GcRef,
    value: GcRef,
) -> GcRef {
    abi_guard!("praxis_grid_find", ctx, {
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
    })
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
    abi_guard!("praxis_grid_find_all", ctx, {
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
    })
}

/// A transposed copy of the grid (rows ↔ columns), as a new `Grid`.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_transpose(ctx: *mut RuntimeContext, grid: GcRef) -> GcRef {
    abi_guard!("praxis_grid_transpose", ctx, {
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
        // SAFETY: GridPayload is GRID's payload type.
        unsafe {
            gc_alloc_owned(ctx, &crate::collections::GRID, || GridPayload {
                element_descriptor: p.element_descriptor,
                items: cells,
                width: new_width,
            })
        }
    })
}

/// A copy of the grid rotated 90° left (counter-clockwise), as a new `Grid`.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_rotate_left(ctx: *mut RuntimeContext, grid: GcRef) -> GcRef {
    abi_guard!("praxis_grid_rotate_left", ctx, {
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
        // SAFETY: GridPayload is GRID's payload type.
        unsafe {
            gc_alloc_owned(ctx, &crate::collections::GRID, || GridPayload {
                element_descriptor: p.element_descriptor,
                items: cells,
                width: new_width,
            })
        }
    })
}

/// A copy of the grid rotated 90° right (clockwise), as a new `Grid`.
///
/// # Safety
/// `ctx` must be live and wired; `grid` must be a valid `Grid` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_grid_rotate_right(ctx: *mut RuntimeContext, grid: GcRef) -> GcRef {
    abi_guard!("praxis_grid_rotate_right", ctx, {
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
        // SAFETY: GridPayload is GRID's payload type.
        unsafe {
            gc_alloc_owned(ctx, &crate::collections::GRID, || GridPayload {
                element_descriptor: p.element_descriptor,
                items: cells,
                width: new_width,
            })
        }
    })
}

// ---------------------------------------------------------------------------
// Text methods (§4.3).
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

/// The `Text` payload behind a `GcRef`.
///
/// # Safety
/// `r` must be a valid `Text` `GcRef`. Non-moving GC keeps it stable.
#[inline]
unsafe fn text_payload(r: GcRef) -> *const crate::text::TextPayload {
    r.payload::<crate::text::TextPayload>() as *const crate::text::TextPayload
}

/// The number of Unicode scalar values (chars) in `text`, as a boxed `Int`.
///
/// **O(1) after the text or its owner has been counted once** (ADR-115). The
/// count is cached rather than recomputed as `text_str(text).chars().count()`,
/// which is two passes over every byte — `text_str` re-validates the UTF-8 the
/// payload is already known to hold, and `chars().count()` then decodes it —
/// and this is called *once per iteration* of `for c in t`, because `lower_for`
/// puts the plan's `len` call in the loop **header**
/// (`praxis-mir/src/build.rs`, `lower_for`).
///
/// # Safety
/// `ctx` must be live and wired; `text` must be a valid `Text` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_text_len(ctx: *mut RuntimeContext, text: GcRef) -> GcRef {
    abi_guard!("praxis_text_len", ctx, {
        // SAFETY: caller guarantees `text` is Text.
        let len = unsafe { crate::text::text_char_count(text_payload(text)) } as i64;
        unsafe { int_ref(ctx, len) }
    })
}

/// The whole of `text`, trimmed, if `run` accepts all of it — the shared half of
/// [`praxis_text_int`] and [`praxis_text_float`] (ADR-136).
///
/// **`run` is the input parser's own scanner** (`parser::take_int_run`,
/// `parser::take_float_run`), and that is the point rather than a convenience.
/// `parse(t, int)` and `t.int()` are two spellings of "read a number out of
/// text", and a program that gets different answers from them has found a defect
/// in one of them. Sharing the scanner makes the disagreement unrepresentable.
///
/// The difference between the method and the atomic is *how much* must match,
/// not what: an atomic stops where its run stops and hands the rest of the line
/// to the template, and a method has no rest to hand anywhere — so a run that
/// covers less than the whole trimmed text is `None`. That is what makes
/// `"1 2"`, `"12abc"` and `"1."` rejections rather than partial answers.
///
/// Trimming is the one liberty taken, and it is what makes a line read off input
/// usable without a second call.
fn whole_trimmed(s: &str, run: fn(&[u8]) -> (&str, usize)) -> Option<&str> {
    let trimmed = s.trim();
    let (text, len) = run(trimmed.as_bytes());
    (!text.is_empty() && len == trimmed.len()).then_some(trimmed)
}

/// The `Int` `text` spells, as `Some(n)`, or `None` when it spells no `Int`
/// (ADR-136).
///
/// `Y001`'s help on `var count: Int = raw` names `.int()`, so this is the method
/// that help sends the reader to.
///
/// `Option[Int]` and not `Int`, for §4.7's reason: a text that is not a number
/// is *absence*, not a fault. Input arrives as text and is routinely not what
/// the program hoped, so a panicking conversion would make `"abc".int()` a crash
/// the program has no way to prevent — where `read lines(int)`, the other half
/// of that help, reports at the parser and never produces the value at all.
///
/// The accepted spelling is **§7.4's `int` atomic** over the whole trimmed text:
/// an optional `-` and then digits (see [`whole_trimmed`]). `"1 2"`, `"0x10"`,
/// `"1_000"`, `"+5"` and `""` are all `None`, and so is a value outside `Int`'s
/// range — for the reason `Y013` exists: a saturated answer is a number nobody
/// wrote.
///
/// # Safety
/// `ctx` must be live and wired; `text` must be a valid `Text` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_text_int(ctx: *mut RuntimeContext, text: GcRef) -> GcRef {
    abi_guard!("praxis_text_int", ctx, {
        // SAFETY: caller guarantees `text` is Text.
        let s = unsafe { text_str(text) };
        match whole_trimmed(s, crate::parser::take_int_run).and_then(|t| t.parse::<i64>().ok()) {
            // SAFETY: `ctx` is live and wired; `int_ref` allocates the payload
            // and `option_some` roots it across the enum allocation.
            Some(n) => unsafe {
                let boxed = int_ref(ctx, n);
                option_some(ctx, boxed)
            },
            // SAFETY: `ctx` is live and wired.
            None => unsafe { option_none(ctx) },
        }
    })
}

/// The `Float` `text` spells, as `Some(x)`, or `None` when it spells no `Float`
/// (ADR-136).
///
/// [`praxis_text_int`]'s twin, over §7.4's `float` atomic: an optional sign,
/// digits, an optional `.` **with** a fraction, and an optional complete
/// exponent. `"1.5"`, `"-2"`, `"+5.0"` and `"1e10"` are values; `"1."`, `"1e"`,
/// `"inf"`, `"nan"` and `""` are `None`, because none of them is a token the
/// input parser reads either.
///
/// `inf` and `nan` are the answer worth stating: Rust's `f64::from_str` accepts
/// both, §7.4's `float` accepts neither, and a method that took them would be a
/// second opinion about what a number is. `Float` still *has* those values —
/// `1.0 / 0.0` is one — and `Float.to_text()` prints them; what has no spelling
/// is reading one back out of arbitrary text.
///
/// The leading `+` this accepts and [`praxis_text_int`] does not is §7.4's own
/// asymmetry, carried over rather than papered over: changing an atomic's
/// accepted set is a change to the input language.
///
/// # Safety
/// `ctx` must be live and wired; `text` must be a valid `Text` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_text_float(ctx: *mut RuntimeContext, text: GcRef) -> GcRef {
    abi_guard!("praxis_text_float", ctx, {
        // SAFETY: caller guarantees `text` is Text.
        let s = unsafe { text_str(text) };
        match whole_trimmed(s, crate::parser::take_float_run).and_then(|t| t.parse::<f64>().ok()) {
            // SAFETY: `ctx` is live and wired. `praxis_alloc_float` takes the
            // bit pattern the uniform scalar ABI carries (§4.3), and
            // `option_some` roots the box across the enum allocation.
            Some(x) => unsafe {
                let boxed = praxis_alloc_float(ctx, x.to_bits() as i64);
                option_some(ctx, boxed)
            },
            // SAFETY: `ctx` is live and wired.
            None => unsafe { option_none(ctx) },
        }
    })
}

/// True iff `text` has no chars, as a boxed `Bool`.
///
/// Asks the bytes rather than a `&str`: `text_str` validates the whole payload
/// to hand back a `&str`, which would make an O(1) question O(n) (ADR-115). A
/// text is empty iff it has no bytes — no scalar encodes to zero of them.
///
/// # Safety
/// `ctx` must be live and wired; `text` must be a valid `Text` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_text_is_empty(ctx: *mut RuntimeContext, text: GcRef) -> GcRef {
    abi_guard!("praxis_text_is_empty", ctx, {
        // SAFETY: caller guarantees `text` is Text.
        let empty = unsafe { crate::text::text_bytes(text_payload(text)) }.is_empty();
        // SAFETY: ctx/heap valid; Bool immortal path.
        unsafe { bool_ref(ctx, empty) }
    })
}

/// `a + b` on two `Text`s — a new owned `Text` holding their concatenation
/// (ADR-085).
///
/// Declared `Allocates` rather than `AllocatesAndFaults`, which is
/// `praxis_float_to_text`'s row and for the same reason: both payloads are
/// UTF-8 by construction, so their concatenation is too, and there is nothing
/// for the `InvalidText` fault to check. Since ADR-111 `praxis_alloc_text` is
/// `Allocates` on the same footing — every wrapper here trusts its caller about
/// encoding, and the one place that cannot (`praxis_get_input`, which holds the
/// host's raw bytes) validates and faults there.
///
/// The result is `Owned` and never a `Slice`: a concatenation has no single
/// owner to point into, and a slice of one would be a lie about its extent.
///
/// # Safety
/// `ctx` must be live and wired; `a` and `b` must be valid `Text` `GcRef`s.
#[no_mangle]
pub unsafe extern "C" fn praxis_text_concat(ctx: *mut RuntimeContext, a: GcRef, b: GcRef) -> GcRef {
    abi_guard!("praxis_text_concat", ctx, {
        // SAFETY: caller guarantees both are Text.
        let left = unsafe { text_str(a) };
        let right = unsafe { text_str(b) };
        let mut joined = String::with_capacity(left.len() + right.len());
        joined.push_str(left);
        joined.push_str(right);
        // SAFETY: TextPayload matches TEXT's size/align and is fully initialized.
        unsafe { text_ref(ctx, joined) }
    })
}

/// Render `value` into a fresh `Text`, **exactly as `out` renders it** (§8.1,
/// ADR-147).
///
/// This is the whole of an interpolation hole. `"{v}"` on a `Vec[Int]` is
/// `[1, 2, 3]` because this function and [`praxis_write_stdout`] are the same
/// two lines with a different destination: both call [`GcRef::format`], which
/// dispatches through the value's type descriptor. There is no second renderer
/// here and there must never be one — writing a `write!` inline instead of
/// calling `format` is the mistake this wrapper exists to make unnecessary, and
/// it is the mistake ADR-143 decision 2 records for the three scalar rows.
///
/// That is also why a hole may hold **any** type (ADR-147 decision 2). Every
/// `GcRef` has a descriptor and every descriptor has a `format` callback, so
/// there is no value this can be handed that it cannot render — which is what
/// lets inference impose no requirement on a hole at all.
///
/// Declared `Allocates`, never `AllocatesAndFaults`: nothing above can fail, and
/// a `String` built by `format` is valid UTF-8 by construction, so there is
/// nothing for an `InvalidText` fault to check. That is `praxis_text_concat`'s
/// row exactly.
///
/// # Safety
/// `ctx` must be live and wired; `value` must be a valid `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_value_to_text(ctx: *mut RuntimeContext, value: GcRef) -> GcRef {
    abi_guard!("praxis_value_to_text", ctx, {
        let mut s = String::new();
        value.format(&mut s);
        // SAFETY: `s` is valid UTF-8; ctx/heap valid.
        unsafe { text_ref(ctx, s) }
    })
}

/// The `Char` at `index`, or an `IndexOutOfBounds` fault if out of range
/// (ADR-086). `index` counts Unicode scalar values, not bytes.
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
    abi_guard!("praxis_text_get", ctx, {
        // SAFETY: caller guarantees `text` is Text.
        let payload = unsafe { text_payload(text) };
        // SAFETY: caller guarantees `index` is a valid Int.
        let idx = unsafe { int_payload(index) };
        if idx < 0 {
            unsafe { set_fault(ctx, RaisedFault::INDEX_OUT_OF_BOUNDS) };
            return unsafe { unit_sentinel(ctx) };
        }
        // **The byte index is the character index exactly when every scalar is
        // one byte, and `text_ascii_bytes` answers that in O(1)** (ADR-115).
        // The fallback is `chars().nth(i)`, which is O(i): a multi-byte text
        // has no random access without either a wider representation or a
        // cursor, and ADR-115 declines the cursor with its arithmetic. `idx` is
        // non-negative above, so the `as usize` cannot wrap.
        // SAFETY: caller guarantees `text` is Text.
        if let Some(bytes) = unsafe { crate::text::text_ascii_bytes(payload) } {
            return match bytes.get(idx as usize) {
                // One-byte scalars are exactly the ASCII range, so the byte
                // *is* the code point (§4.3, ADR-086).
                Some(&b) => unsafe { char_ref(ctx, u32::from(b)) },
                None => {
                    unsafe { set_fault(ctx, RaisedFault::INDEX_OUT_OF_BOUNDS) };
                    unsafe { unit_sentinel(ctx) }
                }
            };
        }
        // SAFETY: caller guarantees `text` is Text.
        let s = unsafe { text_str(text) };
        match s.chars().nth(idx as usize) {
            Some(ch) => {
                // No validity check, and none belongs here: `ch` is a Rust `char`,
                // so `ch as u32` is a valid Unicode scalar by construction. The
                // check `praxis_int_to_char` needs is for the values that did not
                // come from one — which is why this goes to `char_ref` directly
                // rather than through `checked_alloc_char`.
                //
                // This is the interning's largest site (ADR-107): the same call
                // is `t[i]` and every step of `for c in t` (the `iter_plan`
                // lowering), so a program that walks a line of ASCII text would
                // otherwise box one object per character.
                unsafe { char_ref(ctx, ch as u32) }
            }
            None => {
                unsafe { set_fault(ctx, RaisedFault::INDEX_OUT_OF_BOUNDS) };
                unsafe { unit_sentinel(ctx) }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// `out(...)` — write a value to stdout followed by a newline (§16.1).
// ---------------------------------------------------------------------------

/// Format `value` through its descriptor and write it to stdout followed by a
/// newline. Returns the Unit sentinel (§4.3), matching `out`'s `(T) -> Unit`
/// type. Never faults.
///
/// # Safety
/// `ctx` must be live and wired; `value` must be a valid `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_write_stdout(ctx: *mut RuntimeContext, value: GcRef) -> GcRef {
    abi_guard!("praxis_write_stdout", ctx, {
        use std::io::Write;
        let mut out = String::new();
        value.format(&mut out);
        let _ = std::io::stdout().write_all(out.as_bytes());
        let _ = std::io::stdout().write_all(b"\n");
        // `out` is `(T) -> Unit`: return the Unit sentinel so a Unit-typed value
        // flows out, not the printed argument (which would otherwise leak as the
        // function's result and be printed a second time by the host).
        unsafe { unit_sentinel(ctx) }
    })
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
    abi_guard!("praxis_dbg", _ctx, {
        use std::io::Write;
        let mut rendered = String::new();
        value.format(&mut rendered);
        let _ = std::io::stderr().write_all(rendered.as_bytes());
        let _ = std::io::stderr().write_all(b"\n");
        value
    })
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
    abi_guard!("praxis_panic", ctx, {
        let mut message = String::new();
        value.format(&mut message);
        unsafe { set_fault_message(ctx, message) };
        unsafe { set_fault(ctx, RaisedFault::PANIC) };
        unsafe { unit_sentinel(ctx) }
    })
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
    abi_guard!("praxis_assert", ctx, {
        // SAFETY: `assert`'s scheme is `(Bool) -> Unit`, so the argument is a Bool.
        if !unsafe { crate::immortal::read_bool(condition) } {
            unsafe { set_fault(ctx, RaisedFault::ASSERT_FAILED) };
        }
        unsafe { unit_sentinel(ctx) }
    })
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
    abi_guard!("praxis_range_new", ctx, {
        let a = unsafe { int_payload(start) };
        let b = unsafe { int_payload(end) };
        unsafe {
            gc_alloc(
                ctx,
                crate::range::RANGE_PAYLOAD,
                crate::range::RangeVal::new(a, b),
            )
        }
    })
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
    abi_guard!("praxis_range_new_inclusive", ctx, {
        let a = unsafe { int_payload(start) };
        let b = unsafe { int_payload(end) };
        unsafe {
            gc_alloc(
                ctx,
                crate::range::RANGE_PAYLOAD,
                crate::range::RangeVal::new_inclusive(a, b),
            )
        }
    })
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
/// already answer for a result with no `Int`. It is deliberately not
/// `EmptyRange`: the range this fires on is the *fullest* one there is, so that
/// message would lie about it (ADR-059, ADR-075).
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Range` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_range_len(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    abi_guard!("praxis_range_len", ctx, {
        // SAFETY: the compiler only emits this with a Range-typed operand.
        let range = unsafe { &*r.payload::<crate::range::RangeVal>() };
        match i64::try_from(range.len()) {
            Ok(len) => unsafe { int_ref(ctx, len) },
            Err(_) => {
                unsafe { set_fault(ctx, RaisedFault::INT_OVERFLOW) };
                unsafe { unit_sentinel(ctx) }
            }
        }
    })
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
    abi_guard!("praxis_range_get", ctx, {
        // SAFETY: the compiler only emits this with a Range-typed receiver.
        let range = unsafe { &*r.payload::<crate::range::RangeVal>() };
        let i = unsafe { int_payload(index) };
        match range.get(i) {
            Some(value) => unsafe { int_ref(ctx, value) },
            None => {
                unsafe { set_fault(ctx, RaisedFault::INDEX_OUT_OF_BOUNDS) };
                unsafe { unit_sentinel(ctx) }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Input parser (§7).
//
// `read` / `parse` lower to runtime calls that fetch the input buffer and run
// a compiled parser plan against it. The plan is compiled at HIR time and
// registered in a global slab; its index is passed as a boxed Int.
// ---------------------------------------------------------------------------

/// Return the process-input source buffer (§7.10), reading it the **first**
/// time a program asks.
///
/// A `read` lowers to this call and then to `praxis_run_parser`, so this is
/// where §7.10's "the first `read` lazily reads standard input once" happens.
/// The host installs a [`crate::input::InputReader`] rather than a buffer; it
/// is called at most once — [`crate::input::take_input_reader`] removes it, so
/// "once" is structural rather than a flag — and the result is installed as
/// `input_source`, which every later `read` reuses.
///
/// Nothing before a program's first `read` touches the host's input. Reading it
/// up front would make a program with no `read` in it still consume standard
/// input, so `praxis run` against an open pipe would block forever.
///
/// A host that installs no reader — every JIT test, and the crash debugger's
/// re-run path, which installs the buffer directly to keep re-runs identical
/// (§9.7) — reaches the plain `input_source` read below.
///
/// **A reader that answers zero bytes has given empty input, not no input.** Its
/// answer is installed as `input_source` whatever its length, so `read` runs
/// against a zero-length buffer and the parser constructors answer from their own
/// rules — `lines(int)` over it is `[]` by `split_lines`'s rule, and one that
/// requires content faults at `0..0` naming what it expected. That is what §7.11
/// asks a mismatch to carry, and a fault raised before any buffer existed can
/// carry none of it: it has no input span to name. A zero-byte `--input` file is
/// the same decision, made at `praxis-cli/src/run.rs` (ADR-087).
///
/// The one remaining Unit-source state belongs to a host that installs **neither**
/// a buffer nor a reader — every JIT test, every embedder. `praxis_run_parser`'s
/// descriptor guard (§6.3) is what keeps that state survivable; no `praxis run`
/// reaches it.
///
/// **This wrapper owns the UTF-8 judgement, and it is the only producer of
/// [`FaultKind::InvalidText`](crate::FaultKind::InvalidText)** (ADR-111). A
/// reader's bytes are the host's, not the compiler's, so they are checked here
/// and `INVALID_TEXT` is raised here — where `lower_read`'s `CheckFault` makes
/// it divert at the `read`. Raising it inside `praxis_alloc_text` instead would
/// cost a check after every text *literal* for a fault a literal cannot
/// produce; that wrapper trusts its caller, and this is the caller that has to
/// earn the trust.
///
/// `praxis run` cannot reach the fault: `lazy_stdin::read` goes through
/// `std::io::read_to_string` and exits 2 on non-UTF-8 stdin before the runtime
/// sees a byte. An embedder installing its own reader can.
///
/// # Safety
/// `ctx` must be live and wired.
#[no_mangle]
pub unsafe extern "C" fn praxis_get_input(ctx: *mut RuntimeContext) -> GcRef {
    abi_guard!("praxis_get_input", ctx, {
        if let Some(read) = crate::input::take_input_reader() {
            let bytes = read();
            // **This is the one place in the runtime that holds raw host bytes,
            // so it is the one place the UTF-8 judgement §4.3 assigns belongs**
            // (ADR-111). Here the fault is real: a host's `InputReader` is
            // infallible about I/O by design (`crate::input`) and says nothing
            // about encoding, so these bytes are exactly as trustworthy as the
            // host. `GetInput`'s row is `AllocatesAndFaults` and `lower_read`
            // emits the check, so `InvalidText` diverts *at the `read`*.
            //
            // The Unit sentinel is the defined dummy (§10.4); `input_source`
            // holds it until a buffer is installed, so answering it below is
            // the same value by a shorter route.
            let Ok(text) = std::str::from_utf8(&bytes) else {
                unsafe { set_fault(ctx, RaisedFault::INVALID_TEXT) };
                return unsafe { (*ctx).input_source };
            };
            // **The validation is strictly before the allocation, and must
            // stay there.** SAFETY: `text` borrows a live, initialized buffer
            // for this call, and `ctx` is the caller's live context. The result
            // is stored into `input_source` — a root (`RuntimeRoots`) — with no
            // allocation in between, so the collection this allocation paces
            // cannot reclaim it. `praxis_alloc_text` takes `&[]` for
            // `len == 0`, so the empty answer needs no special case here and
            // must not get one.
            let text = unsafe { praxis_alloc_text(ctx, text.as_ptr(), text.len()) };
            unsafe { (*ctx).input_source = text };
        }
        unsafe { (*ctx).input_source }
    })
}

/// Run a compiled parser plan against `input`, returning the parsed result as a
/// `GcRef` (§7.1). `plan_index_gc` is a boxed `Int` whose payload is the
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
/// The guard **clears** the parse detail and records none of its own. It runs no
/// parse, so it has nothing to report — and fabricating a [`ParseFail`] there
/// would be worse than silence: with no buffer there is no input span, and an
/// invented `expected` would make an embedder's host bug read as a parse failure
/// at an offset that does not exist. Clearing is also what stops it reporting a
/// *previous* parse's offset: this is the one entry into the parser that does
/// not go through `run_plan`'s own clear.
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
    abi_guard!("praxis_run_parser", ctx, {
        // Guard the parser interpreter against a non-Text input (§6.3). Reaching
        // `run_plan` with a non-Text payload would reinterpret foreign bytes as a
        // TextPayload and segfault; fault cleanly instead.
        if input.descriptor().id() != crate::text::TEXT.id() {
            unsafe { crate::parser::clear_parse_detail(ctx) };
            unsafe { set_fault(ctx, RaisedFault::PARSE_FAILED) };
            return unsafe { unit_sentinel(ctx) };
        }
        let idx = unsafe { int_payload(plan_index_gc) };
        // Delegate to the parser interpreter. It validates the id, reads the
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
    })
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
/// it, so a collection triggered inside the *next* call finds it.
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
        let items = unsafe { (*result.payload::<VecPayload>()).items.to_vec() };
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
        // A `Bool`'s payload is **one byte**. Reading it as an `i64` would take
        // seven further bytes of the block's alignment padding, which the bump
        // allocator never initialized. `read_scalar` checks the descriptor and
        // takes the width from `BOOL_PAYLOAD`'s own type, so neither half is
        // written here.
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
    abi_guard!("praxis_bfs", ctx, {
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
    })
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
    abi_guard!("praxis_dfs", ctx, {
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
    })
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
    abi_guard!("praxis_flood_fill", ctx, {
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
    })
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
    abi_guard!("praxis_bfs_distance", ctx, {
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
    })
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
    abi_guard!("praxis_dijkstra", ctx, {
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
                let boxed = scope.root(int_ref(ctx, cost));
                map_payload_mut(result)
                    .entries
                    .insert(DynamicKey::new(state), boxed.get());
            }
            result.get()
        }
    })
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
    abi_guard!("praxis_a_star", ctx, {
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
    })
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
                let boxed = int_ref(ctx, n);
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
    use crate::shadow_stack::{push_frame, SlotCount};

    /// A wired context backed by a real runtime.
    pub(super) fn wired_ctx(rt: &mut Runtime) -> *mut RuntimeContext {
        let ctx = Box::leak(Box::new(rt.context()));
        ctx as *mut RuntimeContext
    }

    pub(super) unsafe fn drop_ctx(ctx: *mut RuntimeContext) {
        // Reclaim the leaked Box. The runtime outlives this call in tests.
        let _ = unsafe { Box::from_raw(ctx) };
    }

    /// The first `Int` value the runtime does **not** intern.
    ///
    /// Every test below that detects a collection by watching the live registry
    /// *shrink* must allocate above this. An interned `Int` never enters the
    /// registry, so `praxis_alloc_int(ctx, 5)` in such a loop makes
    /// `after < before + 1` true on the first iteration and the test reports
    /// success without a collection ever having run — a false pass, which is
    /// strictly worse than the failure it replaces.
    const UNINTERNED: i64 = crate::small_int::SMALL_INT_MAX + 1;

    /// Allocate through a safepointed ABI wrapper until its pre-allocation
    /// collection causes the live registry to shrink. Returns the live count
    /// immediately after that wrapper allocates its result.
    unsafe fn allocate_until_automatic_collection(rt: &Runtime, ctx: *mut RuntimeContext) -> usize {
        let mut before = rt.heap().stats().live_count;
        for i in 0..10_000_i64 {
            // Above the interned range: see `UNINTERNED`.
            let _ = unsafe { praxis_alloc_int(ctx, UNINTERNED + i) };
            let after = rt.heap().stats().live_count;
            if after < before.saturating_add(1) {
                return after;
            }
            before = after;
        }
        panic!("automatic collection did not run after 10,000 allocations");
    }

    /// The version number this build declares.
    ///
    /// Named for the version rather than for any one change, because a version
    /// is a statement about a build and several changes share one bump. This
    /// pins the numeral so a build cannot ship a layout change without moving
    /// it.
    ///
    /// `gc::tests::the_folded_payload_offset_moved_at_v19_and_is_pinned_here`
    /// asserts the other direction, pinning the payload offset *to* a version
    /// number, so a layout change and the version that declares it cannot drift
    /// apart.
    #[test]
    fn version_is_twenty_for_the_batch_this_build_ships() {
        assert_eq!(RUNTIME_ABI_VERSION, 20);
    }

    #[test]
    fn assert_passes_within_a_single_build() {
        assert_abi_version();
    }

    /// [`int_payload`]'s width check must be a real branch, not a
    /// `debug_assert` — the read has to be bounded in the profile users
    /// actually run.
    ///
    /// `debug_assert_eq!` is compiled out of a release build, leaving
    /// `unsafe { *r.payload::<i64>() }` against a descriptor that may be zero
    /// bytes wide: an 8-byte out-of-bounds heap read, reachable from a program
    /// that passes `praxis check`.
    ///
    /// **This is a source gate on purpose, and it is the only kind that works
    /// here.** The defect is a difference *between profiles*, and `cargo test`
    /// builds exactly one of them — a behavioural test is green under
    /// `debug_assertions` whether the check is conditional or not. The companion
    /// below asserts the branch actually refuses; this asserts it is still
    /// *there* at `-O`.
    ///
    /// It reads the file rather than the function because there is nothing in a
    /// compiled artifact to ask. `every_no_mangle_wrapper_is_behind_the_panic_guard`
    /// is the same technique for the same reason.
    #[test]
    fn every_scalar_payload_read_goes_through_the_bounded_reader() {
        let source = include_str!("abi.rs");

        // 1. The reader itself checks before it reads, and the check is an
        //    ordinary branch — not a `debug_assert`, which compiles out of a
        //    release build. That distinction is the point: with the check
        //    compiled out, a `praxis check`-clean program does an out-of-bounds
        //    read where a debug build aborts cleanly.
        const SIGNATURE: &str =
            "unsafe fn read_scalar<T: Copy>(r: GcRef, handle: crate::descriptor::Payload<T>) -> Option<T> {";
        let at = source
            .find(SIGNATURE)
            .expect("`read_scalar`'s definition moved; this gate names it by signature");
        let body_start = at + SIGNATURE.len();
        let body_len = source[body_start..]
            .find("\n}")
            .expect("`read_scalar` has no closing brace in the first column");
        let body = &source[body_start..body_start + body_len];

        assert!(
            !body.contains("debug_assert"),
            "`read_scalar`'s type check is a `debug_assert`, which is compiled out of a \
             release build — and what is left is an unchecked read off a payload that may \
             be narrower (REP-56). Make it an ordinary branch.\nbody was:{body}"
        );
        assert!(
            body.contains("std::ptr::eq(r.descriptor(), handle.descriptor())"),
            "`read_scalar` no longer proves the value is the handle's type before reading \
             it (REP-37, REP-56).\nbody was:{body}"
        );

        // 2. Nothing else in this file reads a scalar payload directly. This
        //    is the half that matters: a gate that names one function can only
        //    ever gate that function, and every scalar reader needs bounding.
        //
        //    Scanned over the crate's own code only: `include_str!` hands us this
        //    test too, whose list below would otherwise match itself, and
        //    comments naming the pattern are describing it rather than doing it.
        let code: String = source[..source
            .find("#[cfg(test)]")
            .expect("abi.rs has no test module marker")]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        //
        //    The patterns are the *bare* calls, not the dereferenced ones:
        //    binding `r.payload::<f64>()` to a local and dereferencing it on the
        //    next line breaks the spelling `*r.payload::<f64>()` without
        //    breaking the defect. Forbidding the call means no phrasing of it
        //    passes. `payload::<u8>()` stays legal: it is how
        //    `read_scalar` itself reaches the bytes, and how every *compound*
        //    payload (record, tuple, closure) is reached — those are cast to a
        //    struct the descriptor already vouched for, not read at a width.
        for forbidden in [
            "r.payload::<i64>()",
            "r.payload::<f64>()",
            "r.payload::<u32>()",
            "r.payload::<bool>()",
        ] {
            assert!(
                !code.contains(forbidden),
                "a scalar payload is read directly as `{forbidden}` instead of through \
                 `read_scalar`, so its type is unchecked in release (REP-56). Route it \
                 through `read_scalar(r, scalars::…_PAYLOAD)` instead."
            );
        }

        // 3. And no Rust `bool` is ever materialized from a payload byte: a
        //    `bool` whose byte is not 0 or 1 is an *invalid value*, which is
        //    undefined behaviour independently of whether the read was in
        //    bounds. `BoolPayload` is a `u8` precisely so it never has to be.
        assert!(
            !code.contains("Payload<bool>") && !code.contains("read_scalar::<bool>"),
            "a `bool` is read straight out of a payload; read `scalars::BOOL_PAYLOAD` \
             (a `u8`) and compare it instead (REP-56)."
        );
    }

    /// **ADR-111.** `praxis_alloc_text`'s UTF-8 backstop is unconditional in
    /// every profile, and it never becomes an unchecked read.
    ///
    /// The same source-gate technique as
    /// [`every_scalar_payload_read_goes_through_the_bounded_reader`], for the
    /// same reason and against a sharper temptation. Making the row `Allocates`
    /// says the caller promises UTF-8; the next tidy-up reads that as licence to
    /// delete the check — either into a `debug_assert` (which compiles out of a
    /// release build, so debug aborts and release builds a `Box<str>` of
    /// non-UTF-8 bytes that `text_str` later hands out as a `&str`) or into
    /// `from_utf8_unchecked` (the same hole, with the check deleted rather than
    /// compiled out). Both give two profiles two answers, and `just ci` never
    /// builds the one users get.
    ///
    /// A behavioural test cannot see this: under `cfg(debug_assertions)` a
    /// `debug_assert` version passes every test the branch version passes.
    #[test]
    fn the_text_precondition_backstop_is_unconditional_in_every_profile() {
        let source = include_str!("abi.rs");
        const SIGNATURE: &str = "pub unsafe extern \"C\" fn praxis_alloc_text(";
        let at = source
            .find(SIGNATURE)
            .expect("`praxis_alloc_text`'s definition moved; this gate names it by signature");
        let body_len = source[at..]
            .find("\n}")
            .expect("`praxis_alloc_text` has no closing brace in the first column");
        let body = &source[at..at + body_len];

        assert!(
            body.contains("std::str::from_utf8(slice)"),
            "`praxis_alloc_text` no longer validates its buffer. The check is the \
             backstop on a raw read, not an optimization the `Allocates` row traded \
             away (ADR-111).\nbody was:{body}"
        );
        assert!(
            !body.contains("debug_assert"),
            "`praxis_alloc_text`'s UTF-8 check is a `debug_assert`, which is compiled \
             out of a release build — leaving a `Box<str>` built from bytes that are \
             not UTF-8 (REP-56's shape). Make it an ordinary branch.\nbody was:{body}"
        );
        assert!(
            !body.contains("from_utf8_unchecked"),
            "`praxis_alloc_text` skips the check outright. A precondition is not a \
             licence to read unvalidated bytes as a `str` — the refusal is \
             `text_bytes_are_not_utf8`, which costs a never-taken branch \
             (ADR-111).\nbody was:{body}"
        );
        // And the refusal is not a fault. If it were, the fault sweep would
        // classify the wrapper as faulting and correctly refuse the
        // `Allocates` row — this says so at the site rather than leaving the
        // failure to be diagnosed three tests away.
        assert!(
            !body.contains("set_fault"),
            "`praxis_alloc_text` sets a fault. Its row is `Effect::Allocates`, so no \
             `CheckFault` follows the call and nothing would ever observe it \
             (ADR-088, ADR-111).\nbody was:{body}"
        );
    }

    /// The companion to the source gate above: the branch it insists on is real,
    /// and it refuses rather than reading.
    ///
    /// A `Unit` is zero bytes wide, which is the shape that must be refused.
    /// The refusal is a panic, which is ADR-080's defined path — inside a
    /// wrapper `abi_guard!` turns it into a `Panic` fault (or a message and an
    /// abort where the manifest makes that fault unobservable). What must not
    /// happen, in any profile, is the read.
    #[test]
    fn a_scalar_read_refuses_a_value_that_is_not_its_type() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx is wired to rt; the Unit immortal is a valid GcRef.
        let unit = unsafe { praxis_alloc_unit(ctx) };
        assert_eq!(unit.descriptor().size(), 0, "Unit is a zero-width payload");

        // The panic is the refusal. `catch_unwind` here is the test standing in
        // for `abi_guard!`, which is what catches it in a real wrapper.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        // `AssertUnwindSafe` for the same reason `abi_guard!` uses it: the
        // capture is a `Copy` C type and nothing observes a half-finished read.
        // SAFETY: `unit` is a valid GcRef into rt's live heap.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            int_payload(unit)
        }));
        std::panic::set_hook(previous);
        unsafe { drop_ctx(ctx) };

        let payload = outcome.expect_err("a zero-width payload must not be read as eight bytes");
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            message.contains("int_payload wants a `Int` payload")
                && message.contains("this value is a `Unit`"),
            "unexpected panic message: {message:?}"
        );
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

    /// The `Int` counterpart of
    /// [`bool_and_unit_abi_allocations_reuse_runtime_singletons`]: a small `Int`
    /// is one object per value, and an out-of-range one is still a fresh box.
    ///
    /// Both halves matter. The first is the optimization; the second is the
    /// branch a regression would silently delete, leaving every large `Int` in
    /// the language reading slot `value - SMALL_INT_MIN` of a table that ends
    /// long before it.
    #[test]
    fn small_ints_are_one_object_per_value_and_large_ones_are_not() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx is wired to rt throughout.
        unsafe {
            // In range: two calls, one object — and it is the runtime's own
            // table entry, not some other cache.
            let a = praxis_alloc_int(ctx, 7);
            let b = praxis_alloc_int(ctx, 7);
            assert_eq!(a.as_ptr(), b.as_ptr());
            assert_eq!(a.as_ptr(), rt.immortals().small_int(7).unwrap().as_ptr());
            assert_eq!(praxis_int_load(ctx, a), 7);

            // The four boundary cases, through the ABI: the exact endpoints are
            // interned and one step outside either is not.
            for v in [
                crate::small_int::SMALL_INT_MIN,
                crate::small_int::SMALL_INT_MAX,
            ] {
                assert_eq!(
                    praxis_alloc_int(ctx, v).as_ptr(),
                    praxis_alloc_int(ctx, v).as_ptr(),
                    "{v} is the edge of the range and must be interned"
                );
            }
            for v in [
                crate::small_int::SMALL_INT_MIN - 1,
                crate::small_int::SMALL_INT_MAX + 1,
            ] {
                let x = praxis_alloc_int(ctx, v);
                let y = praxis_alloc_int(ctx, v);
                assert_ne!(
                    x.as_ptr(),
                    y.as_ptr(),
                    "{v} is outside the range and must still allocate"
                );
                assert_eq!(praxis_int_load(ctx, x), v, "and still hold its value");
                assert_eq!(praxis_int_load(ctx, y), v);
            }

            // Distinct in-range values are distinct objects: interning shares
            // an object across *calls*, never across values.
            assert_ne!(
                praxis_alloc_int(ctx, 7).as_ptr(),
                praxis_alloc_int(ctx, 8).as_ptr()
            );

            // The host helper and the ABI wrapper answer the same object, as
            // `Runtime::alloc_bool` and `praxis_alloc_bool` already do.
            assert_eq!(rt.alloc_int(7).as_ptr(), a.as_ptr());
        }
        unsafe { drop_ctx(ctx) };
    }

    /// The `Char` counterpart of
    /// [`small_ints_are_one_object_per_value_and_large_ones_are_not`] (ADR-107).
    ///
    /// Both halves matter. The first is the optimization; the second is the
    /// branch a regression would silently delete, leaving every non-ASCII `Char`
    /// in the language reading slot `code` of a table that ends at 128.
    #[test]
    fn alloc_char_answers_one_object_per_ascii_code_point_and_a_large_one_still_allocates() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx is wired to rt throughout.
        unsafe {
            // In range: two calls, one object — and it is the runtime's own
            // table entry, not some other cache.
            let a = praxis_alloc_char(ctx, i64::from('a' as u32));
            let b = praxis_alloc_char(ctx, i64::from('a' as u32));
            assert_eq!(a.as_ptr(), b.as_ptr());
            assert_eq!(
                a.as_ptr(),
                rt.immortals().small_char('a' as u32).unwrap().as_ptr()
            );
            assert_eq!(a.as_char(), 'a');

            // The ceiling is interned; one above it is not. There is no floor
            // case — the payload is unsigned and NUL is interned.
            let max = i64::from(crate::small_char::SMALL_CHAR_MAX);
            assert_eq!(
                praxis_alloc_char(ctx, max).as_ptr(),
                praxis_alloc_char(ctx, max).as_ptr(),
                "the last ASCII scalar is the edge of the range and must be interned"
            );
            assert_eq!(
                praxis_alloc_char(ctx, 0).as_ptr(),
                praxis_alloc_char(ctx, 0).as_ptr(),
                "NUL is the floor and must be interned"
            );
            for code in [max + 1, i64::from('é' as u32), 0x10_FFFF] {
                let x = praxis_alloc_char(ctx, code);
                let y = praxis_alloc_char(ctx, code);
                assert_ne!(
                    x.as_ptr(),
                    y.as_ptr(),
                    "{code:#x} is outside the range and must still allocate"
                );
                assert_eq!(
                    u32::from(x.as_char()),
                    code as u32,
                    "and still hold its code point"
                );
            }

            // Distinct in-range code points are distinct objects: interning
            // shares an object across *calls*, never across values.
            assert_ne!(
                praxis_alloc_char(ctx, i64::from('a' as u32)).as_ptr(),
                praxis_alloc_char(ctx, i64::from('b' as u32)).as_ptr()
            );

            // The validity rule is untouched by the table: an interned slot is
            // reached only after `checked_alloc_char` has approved the value, so
            // a code point that is not a scalar still faults.
            for bad in [-1_i64, 0xD800, 0x11_0000, 0x1_0000_0041] {
                let _ = praxis_alloc_char(ctx, bad);
                assert_eq!(
                    rt.take_fault(),
                    Some(FaultKind::InvalidChar),
                    "{bad:#x} is not a scalar value"
                );
            }

            // The host helper and the ABI wrapper answer the same object, as
            // `Runtime::alloc_int`/`praxis_alloc_int` already do. This is what
            // makes "a `Char` is interned" one fact rather than two.
            assert_eq!(rt.alloc_char('a' as u32).as_ptr(), a.as_ptr());
            // …and the out-of-range halves still disagree as objects, which is
            // the same statement from the other side.
            assert_ne!(
                rt.alloc_char('é' as u32).as_ptr(),
                rt.alloc_char('é' as u32).as_ptr()
            );
        }
        unsafe { drop_ctx(ctx) };
    }

    /// `praxis_text_get` is the interning's largest site: it is `t[i]` *and*
    /// every step of `for c in t`. It reaches [`char_ref`] directly rather than
    /// through [`checked_alloc_char`] — a Rust `char` needs no validity check —
    /// so it is its own door and must be pinned as one.
    ///
    /// `text_get_answers_a_char_object` covers the uninterned half (`é`) and the
    /// descriptor; this covers the identity.
    #[test]
    fn text_get_answers_the_interned_char() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let s = "abca";
            let text = praxis_alloc_text(ctx, s.as_ptr(), s.len());
            let zero = praxis_alloc_int(ctx, 0);
            let three = praxis_alloc_int(ctx, 3);
            let first = praxis_text_get(ctx, text, zero);
            let last = praxis_text_get(ctx, text, three);
            assert!(!rt.has_pending_fault());

            // Two reads of the same character are one object, and it is the same
            // object every other door answers.
            assert_eq!(first.as_ptr(), last.as_ptr());
            assert_eq!(
                first.as_ptr(),
                praxis_alloc_char(ctx, i64::from('a' as u32)).as_ptr()
            );
            assert_eq!(
                first.as_ptr(),
                rt.immortals().small_char('a' as u32).unwrap().as_ptr()
            );
            assert_eq!(first.as_char(), 'a');

            // The non-ASCII half still allocates, and still answers the right
            // scalar — the branch a regression would delete.
            let u = "éé";
            let utext = praxis_alloc_text(ctx, u.as_ptr(), u.len());
            let one = praxis_alloc_int(ctx, 1);
            let x = praxis_text_get(ctx, utext, zero);
            let y = praxis_text_get(ctx, utext, one);
            assert_ne!(x.as_ptr(), y.as_ptr(), "`é` is outside the interned range");
            assert_eq!(x.as_char(), 'é');
            assert_eq!(y.as_char(), 'é');
        }
        unsafe { drop_ctx(ctx) };
    }

    /// The two doors into [`checked_alloc_char`] answer the same object, which
    /// is that helper's whole reason for existing — a rule stated at both goes
    /// stale at one, and now the rule includes which object.
    #[test]
    fn int_to_char_answers_the_same_object_as_alloc_char() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let code = praxis_alloc_int(ctx, i64::from('Z' as u32));
            let via_to_char = praxis_int_to_char(ctx, code);
            let via_alloc = praxis_alloc_char(ctx, i64::from('Z' as u32));
            assert!(!rt.has_pending_fault());
            assert_eq!(via_to_char.as_ptr(), via_alloc.as_ptr());
            assert_eq!(via_to_char.as_char(), 'Z');

            // Outside the range both still allocate, and still not each other.
            let big = praxis_alloc_int(ctx, i64::from('é' as u32));
            assert_ne!(
                praxis_int_to_char(ctx, big).as_ptr(),
                praxis_alloc_char(ctx, i64::from('é' as u32)).as_ptr()
            );
        }
        unsafe { drop_ctx(ctx) };
    }

    /// **ADR-143.** `Int.to_text()` answers exactly what `out` writes, and the
    /// assertion is against `out`'s own path rather than a literal.
    ///
    /// Comparing to `"1660"` would pass while the two renderers disagreed about
    /// everything else; comparing to `GcRef::format`'s output cannot, because
    /// that is the function `praxis_write_stdout` calls. `i64::MIN` is in the
    /// list because it is the one value whose negation does not fit, and
    /// therefore the first thing a hand-rolled renderer gets wrong.
    #[test]
    fn int_to_text_renders_exactly_what_out_renders() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; every receiver is an Int.
        unsafe {
            for v in [0_i64, 1, -1, 1660, i64::MAX, i64::MIN, UNINTERNED] {
                let receiver = praxis_alloc_int(ctx, v);
                let answer = praxis_int_to_text(ctx, receiver);
                assert!(!rt.has_pending_fault(), "{v} faulted");
                let mut printed = String::new();
                receiver.format(&mut printed);
                assert_eq!(answer.as_text(), printed, "to_text and out disagree on {v}");
            }
        }
        unsafe { drop_ctx(ctx) };
    }

    /// **ADR-143.** The same claim for `Char.to_text()`, at an interned ASCII
    /// character and an uninterned multi-byte one.
    ///
    /// The multi-byte case is the one that would catch reading the four-byte
    /// payload as an `i64`: `'é'` is `0xE9`, and eight bytes from a four-byte
    /// payload picks up whatever follows it.
    #[test]
    fn char_to_text_renders_exactly_what_out_renders() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; every receiver is a Char.
        unsafe {
            for c in ['#', 'a', 'é', '☃', '\u{10FFFF}'] {
                let receiver = praxis_alloc_char(ctx, i64::from(u32::from(c)));
                let answer = praxis_char_to_text(ctx, receiver);
                assert!(!rt.has_pending_fault(), "{c} faulted");
                let mut printed = String::new();
                receiver.format(&mut printed);
                assert_eq!(answer.as_text(), printed, "to_text and out disagree on {c}");
                assert_eq!(answer.as_text(), c.to_string());
            }
        }
        unsafe { drop_ctx(ctx) };
    }

    /// **ADR-147.** An interpolation hole renders exactly what `out` writes, for
    /// **every** type — including the ones with no `to_text()` row.
    ///
    /// This is the wrapper-level half of ADR-147 decision 2, and it is asserted
    /// against `GcRef::format` — `praxis_write_stdout`'s own call — rather than
    /// against a literal, for `int_to_text_renders_exactly_what_out_renders`'s
    /// reason: a literal comparison passes while the two agree by coincidence.
    ///
    /// The receivers deliberately span a scalar, a `Text` (whose rendering is
    /// its own characters and not a quoted form), a collection and a tuple, so a
    /// wrapper that reached for a scalar payload instead of the descriptor fails
    /// on the last two rather than on none of them.
    #[test]
    fn value_to_text_renders_exactly_what_out_renders() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; every receiver below is freshly allocated here.
        unsafe {
            let empty = praxis_alloc_text(ctx, std::ptr::null(), 0);
            let hello = "hello";
            let text = praxis_alloc_text(ctx, hello.as_ptr(), hello.len());
            let vec = praxis_vec_new(ctx, &scalars::INT as *const _);
            for n in [1_i64, 2, 3] {
                let _ = praxis_vec_push(ctx, vec, praxis_alloc_int(ctx, n));
            }
            let receivers = [
                praxis_alloc_int(ctx, UNINTERNED),
                praxis_alloc_int(ctx, 0),
                praxis_alloc_bool(ctx, 1),
                praxis_alloc_char(ctx, i64::from(u32::from('☃'))),
                empty,
                text,
                vec,
            ];
            for receiver in receivers {
                let answer = praxis_value_to_text(ctx, receiver);
                assert!(!rt.has_pending_fault(), "value_to_text faulted");
                let mut printed = String::new();
                receiver.format(&mut printed);
                assert_eq!(
                    answer.as_text(),
                    printed,
                    "a hole and `out` must write the same characters"
                );
            }
            // The `Text` rows pin the shape a caller is most likely to assume
            // wrong: `out("hello")` writes `hello`, not `"hello"`, so `"{s}"`
            // must not add quotes either.
            assert_eq!(praxis_value_to_text(ctx, text).as_text(), "hello");
            assert_eq!(praxis_value_to_text(ctx, empty).as_text(), "");
        }
        unsafe { drop_ctx(ctx) };
    }

    /// **ADR-144.** `join` puts the separator *between* elements and nowhere
    /// else, which is the whole of the specification and the whole of what an
    /// off-by-one gets wrong.
    #[test]
    fn vec_join_puts_the_separator_between_and_nowhere_else() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; every element and separator is a Text.
        unsafe {
            let cases: [(&[&str], &str, &str); 5] = [
                (&[], ", ", ""),
                (&["only"], ", ", "only"),
                (&["a", "b", "c"], ", ", "a, b, c"),
                (&["a", "b", "c"], "", "abc"),
                (&["é", "☃"], " — ", "é — ☃"),
            ];
            for (items, sep, want) in cases {
                let members: Vec<GcRef> = items.iter().map(|s| rt.alloc_text(s)).collect();
                let vec = rt.alloc_vec(&crate::text::TEXT, members);
                let separator = rt.alloc_text(sep);
                let answer = praxis_vec_join(ctx, vec, separator);
                assert!(!rt.has_pending_fault(), "{items:?} faulted");
                assert_eq!(answer.as_text(), want);
            }
        }
        unsafe { drop_ctx(ctx) };
    }

    /// **ADR-144.** A non-`Text` element is `TypeMismatch` and the Unit
    /// sentinel, not a `Text` payload read out of an `Int`.
    ///
    /// The catalog row's `Text` bound means only a compiler bug gets here, and
    /// this is what that bug looks like when it does: a fault the program can
    /// see, rather than a pointer-and-length pair read from eight bytes of
    /// integer.
    #[test]
    fn vec_join_refuses_a_non_text_element() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let mixed = rt.alloc_vec(&scalars::INT, vec![rt.alloc_text("a"), rt.alloc_int(1)]);
            let sep = rt.alloc_text(",");
            let answer = praxis_vec_join(ctx, mixed, sep);
            assert!(rt.has_pending_fault());
            assert!(std::ptr::eq(answer.descriptor(), &scalars::UNIT));
        }
        unsafe { drop_ctx(ctx) };
    }

    /// **ADR-144.** `Vec[Char].to_text()` is the characters with nothing between
    /// them, and it agrees with `out` on each of them for ADR-143's reason: it
    /// goes through `scalars::write_char` too.
    #[test]
    fn vec_to_text_renders_every_char() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; every element is a Char.
        unsafe {
            for want in ["", ".", "..|", "héllo", "☃☃"] {
                let members: Vec<GcRef> = want
                    .chars()
                    .map(|c| praxis_alloc_char(ctx, i64::from(u32::from(c))))
                    .collect();
                let vec = rt.alloc_vec(&scalars::CHAR, members);
                let answer = praxis_vec_to_text(ctx, vec);
                assert!(!rt.has_pending_fault(), "{want:?} faulted");
                assert_eq!(answer.as_text(), want);
            }
        }
        unsafe { drop_ctx(ctx) };
    }

    /// **ADR-144.** A non-`Char` element faults rather than being read as four
    /// bytes of something else.
    #[test]
    fn vec_to_text_refuses_a_non_char_element() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let mixed = rt.alloc_vec(&scalars::CHAR, vec![rt.alloc_int(65)]);
            let answer = praxis_vec_to_text(ctx, mixed);
            assert!(rt.has_pending_fault());
            assert!(std::ptr::eq(answer.descriptor(), &scalars::UNIT));
        }
        unsafe { drop_ctx(ctx) };
    }

    /// **ADR-145.** `reversed` answers a **new** `Vec` and leaves the receiver
    /// alone — the rule every barrier in this block states, and the one a
    /// wrapper that reversed in place would break invisibly for a caller still
    /// holding `v`.
    ///
    /// The empty case is here because `praxis_vec_sorted` needs a `len() > 1`
    /// guard for the analogous one and this needs none: there is no callback to
    /// avoid calling.
    #[test]
    fn vec_reversed_answers_a_new_vec_and_leaves_the_receiver_alone() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let source = rt.alloc_vec(
                &scalars::INT,
                vec![rt.alloc_int(3), rt.alloc_int(1), rt.alloc_int(2)],
            );
            let answer = praxis_vec_reversed(ctx, source);
            assert!(!rt.has_pending_fault());
            let got: Vec<i64> = answer.as_vec().iter().map(|r| r.as_int()).collect();
            assert_eq!(got, vec![2, 1, 3]);
            let still: Vec<i64> = source.as_vec().iter().map(|r| r.as_int()).collect();
            assert_eq!(still, vec![3, 1, 2], "the receiver is not touched");
            assert_ne!(answer.as_ptr(), source.as_ptr());

            let empty = rt.alloc_vec(&scalars::INT, vec![]);
            assert!(praxis_vec_reversed(ctx, empty).as_vec().is_empty());
            assert!(!rt.has_pending_fault());
        }
        unsafe { drop_ctx(ctx) };
    }

    /// **ADR-145.** Reversal reads no descriptor callback, so a `Vec` of a type
    /// with no `compare` reverses where `sorted` faults.
    ///
    /// This is the runtime half of the catalog row carrying no capability bound.
    /// A `Unit` has no `compare` — `praxis_vec_sorted` raises `TypeMismatch` on
    /// one — and it reverses without a word.
    #[test]
    fn vec_reversed_needs_no_callback_where_sorted_needs_compare() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let closures = rt.alloc_vec(
                &crate::closures::CLOSURE,
                vec![
                    praxis_alloc_closure(ctx, std::ptr::null(), 0),
                    praxis_alloc_closure(ctx, std::ptr::null(), 0),
                ],
            );
            assert_eq!(praxis_vec_reversed(ctx, closures).as_vec().len(), 2);
            assert!(!rt.has_pending_fault(), "reversal asks for no callback");

            praxis_vec_sorted(ctx, closures);
            assert!(rt.has_pending_fault(), "ordering still asks for `compare`");
        }
        unsafe { drop_ctx(ctx) };
    }

    /// The shape of both groupings, read back as nested `Int`s.
    ///
    /// # Safety
    /// `answer` must be a valid `Vec[Vec[Int]]` `GcRef`.
    unsafe fn groups_of_int(answer: GcRef) -> Vec<Vec<i64>> {
        answer
            .as_vec()
            .iter()
            .map(|inner| inner.as_vec().iter().map(|r| r.as_int()).collect())
            .collect()
    }

    /// **ADR-149.** `chunks` partitions: every element appears once, in order,
    /// and a length the size does not divide leaves a *short last chunk* rather
    /// than dropping the tail or padding it.
    #[test]
    fn vec_chunks_partitions_and_keeps_a_short_tail() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let ints: Vec<GcRef> = (1..=5).map(|n| rt.alloc_int(n)).collect();
            let source = rt.alloc_vec(&scalars::INT, ints);

            let two = rt.alloc_int(2);
            let answer = praxis_vec_chunks(ctx, source, two);
            assert!(!rt.has_pending_fault());
            assert_eq!(groups_of_int(answer), vec![vec![1, 2], vec![3, 4], vec![5]]);

            // A size that divides leaves no short chunk, which is the same rule
            // and is worth pinning beside the one that does.
            let five = rt.alloc_int(5);
            assert_eq!(
                groups_of_int(praxis_vec_chunks(ctx, source, five)),
                vec![vec![1, 2, 3, 4, 5]],
            );

            // Wider than the receiver is not a fault: it is one short chunk.
            let nine = rt.alloc_int(9);
            assert_eq!(
                groups_of_int(praxis_vec_chunks(ctx, source, nine)),
                vec![vec![1, 2, 3, 4, 5]],
            );

            let still: Vec<i64> = source.as_vec().iter().map(|r| r.as_int()).collect();
            assert_eq!(still, vec![1, 2, 3, 4, 5], "the receiver is not touched");
        }
        unsafe { drop_ctx(ctx) };
    }

    /// **ADR-149.** `windows` slides by one and keeps only the runs that fit, so
    /// a receiver shorter than the size answers `[]` rather than one short run —
    /// the one place the two groupings differ.
    #[test]
    fn vec_windows_slide_by_one_and_drop_a_run_that_does_not_fit() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let ints: Vec<GcRef> = (1..=4).map(|n| rt.alloc_int(n)).collect();
            let source = rt.alloc_vec(&scalars::INT, ints);

            let two = rt.alloc_int(2);
            assert_eq!(
                groups_of_int(praxis_vec_windows(ctx, source, two)),
                vec![vec![1, 2], vec![2, 3], vec![3, 4]],
            );
            assert!(!rt.has_pending_fault());

            // Exactly the length is one window; one past it is none. Off by one
            // here is the whole difference between `[]` and a wrong answer.
            let four = rt.alloc_int(4);
            assert_eq!(
                groups_of_int(praxis_vec_windows(ctx, source, four)),
                vec![vec![1, 2, 3, 4]],
            );
            let five = rt.alloc_int(5);
            let none = praxis_vec_windows(ctx, source, five);
            assert!(
                none.as_vec().is_empty(),
                "a run of five does not fit in four"
            );
            assert!(
                !rt.has_pending_fault(),
                "not fitting is an answer, not a fault"
            );

            // Windows share their elements rather than copying them, which is
            // the language's reference semantics and not a rule of this wrapper.
            let answer = praxis_vec_windows(ctx, source, two);
            let first = answer.as_vec()[0].as_vec()[1].as_ptr();
            let second = answer.as_vec()[1].as_vec()[0].as_ptr();
            assert_eq!(first, second, "the overlapping element is one object");
        }
        unsafe { drop_ctx(ctx) };
    }

    /// **ADR-149.** The only thing either grouping refuses: a run of `n <= 0` is
    /// not a short run, it is not a run, so there is no sequence of them to
    /// answer with.
    ///
    /// The empty receiver is here beside it because it is the case that looks
    /// like a fault and is not — `[].chunks(2)` is `[]`, the same way `[]`
    /// reverses to `[]`.
    #[test]
    fn a_group_size_of_zero_or_less_is_an_invalid_size_fault() {
        for size in [0i64, -1, i64::MIN] {
            for (name, wrapper) in [
                (
                    "chunks",
                    praxis_vec_chunks as unsafe extern "C" fn(_, _, _) -> _,
                ),
                ("windows", praxis_vec_windows),
            ] {
                let mut rt = Runtime::new();
                let ctx = wired_ctx(&mut rt);
                // SAFETY: ctx wired.
                unsafe {
                    let source = rt.alloc_vec(&scalars::INT, vec![rt.alloc_int(1)]);
                    let n = rt.alloc_int(size);
                    let answer = wrapper(ctx, source, n);
                    assert!(rt.has_pending_fault(), "{name}({size}) must fault");
                    assert_eq!(rt.fault(), crate::FaultKind::InvalidSize, "{name}({size})");
                    assert!(
                        std::ptr::eq(answer.descriptor(), &scalars::UNIT),
                        "{name}({size}) answers the Unit sentinel"
                    );
                }
                unsafe { drop_ctx(ctx) };
            }
        }

        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let empty = rt.alloc_vec(&scalars::INT, vec![]);
            let two = rt.alloc_int(2);
            assert!(praxis_vec_chunks(ctx, empty, two).as_vec().is_empty());
            assert!(praxis_vec_windows(ctx, empty, two).as_vec().is_empty());
            assert!(
                !rt.has_pending_fault(),
                "an empty receiver is an empty answer"
            );
        }
        unsafe { drop_ctx(ctx) };
    }

    /// **ADR-149 decision 1.** The outer `Vec` is labelled `VEC` at every length
    /// and the inner ones carry the receiver's element descriptor.
    ///
    /// The **empty** answer is the whole reason this test exists, and it is the
    /// only part that is a choice: `VEC` is what `outer.push(inner)` already
    /// produces, so a non-empty grouping could hardly answer anything else and
    /// asserting it proves little. With the label inferred from the first group
    /// there would be none to read, and `[1, 2].windows(5)` would carry a null
    /// where `[1, 2].windows(2)` carries `VEC` — one type with two labels, and
    /// the null is the one `vec_format` renders as `[]` and `push` treats as
    /// "adopt whatever arrives".
    #[test]
    fn a_grouping_labels_the_outer_vec_even_when_it_is_empty() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let source = rt.alloc_vec(&scalars::INT, vec![rt.alloc_int(1), rt.alloc_int(2)]);
            let two = rt.alloc_int(2);
            let five = rt.alloc_int(5);

            for answer in [
                praxis_vec_chunks(ctx, source, two),
                praxis_vec_windows(ctx, source, two),
                // The two that come out empty, and the reason this test exists.
                praxis_vec_windows(ctx, source, five),
                praxis_vec_chunks(ctx, rt.alloc_vec(&scalars::INT, vec![]), two),
            ] {
                let p = vec_payload(answer);
                assert!(
                    std::ptr::eq(p.element_descriptor, &crate::collections::VEC),
                    "the outer Vec holds Vecs whether or not it holds any"
                );
                for inner in p.items.iter() {
                    assert!(std::ptr::eq(
                        vec_payload(*inner).element_descriptor,
                        &scalars::INT
                    ));
                }
            }
        }
        unsafe { drop_ctx(ctx) };
    }

    /// **ADR-149.** A grouping reads no descriptor callback, so a `Vec` of a
    /// type with no `compare` groups where `sorted` faults — `reversed`'s claim,
    /// and the runtime half of these two rows carrying no capability bound.
    #[test]
    fn a_grouping_needs_no_callback_where_sorted_needs_compare() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let closures = rt.alloc_vec(
                &crate::closures::CLOSURE,
                vec![
                    praxis_alloc_closure(ctx, std::ptr::null(), 0),
                    praxis_alloc_closure(ctx, std::ptr::null(), 0),
                    praxis_alloc_closure(ctx, std::ptr::null(), 0),
                ],
            );
            let two = rt.alloc_int(2);
            assert_eq!(praxis_vec_chunks(ctx, closures, two).as_vec().len(), 2);
            assert_eq!(praxis_vec_windows(ctx, closures, two).as_vec().len(), 2);
            assert!(!rt.has_pending_fault(), "grouping asks for no callback");

            praxis_vec_sorted(ctx, closures);
            assert!(rt.has_pending_fault(), "ordering still asks for `compare`");
        }
        unsafe { drop_ctx(ctx) };
    }

    /// **ADR-107's pacing half, and ADR-100 §3's analogue.** [`char_ref`] must
    /// give the collector its turn on the path where it allocates *nothing*.
    ///
    /// `TextGet` is `AllocatesAndFaults` in the manifest, which is generated
    /// code's contract that the call site is a GC safepoint. A `for c in line`
    /// loop over ASCII touches nothing else that bumps the pacing counter, and
    /// the counter is the collector's only trigger — so an early return here
    /// would make such a loop run arbitrarily long with no collection at all.
    ///
    /// **The interleaved allocation is load-bearing and must stay unpaced.** The
    /// observable is the live registry *shrinking*, and an interned `Char` never
    /// enters it, so a loop of nothing but `praxis_text_get` could not shrink
    /// anything however well it paced — a guaranteed false pass (see
    /// `UNINTERNED`). `Runtime::alloc_int` is the one helper that grows the heap
    /// **without** pacing, so it supplies the pressure and the population while
    /// leaving `praxis_text_get` as the only safepoint in the loop. Swapping it
    /// for `praxis_alloc_int` would make the test pass with this function's
    /// safepoint deleted.
    #[test]
    fn char_ref_paces_the_collector_even_when_it_answers_from_the_table() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired throughout; `text` and `index` are rooted below.
        unsafe {
            let s = "abcdefgh";
            let text = praxis_alloc_text(ctx, s.as_ptr(), s.len());
            let index = praxis_alloc_int(ctx, 3);
            let mut frame = push_frame(ctx, SlotCount::new(2).unwrap());
            frame.set(0, text);
            frame.set(1, index);

            let mut before = rt.heap().stats().live_count;
            let mut paced = false;
            for i in 0..100_000_i64 {
                // Registered, unrooted, and *unpaced*: pressure the collector can
                // see and reclaim, contributed by something that never offers a
                // turn itself.
                let _ = rt.alloc_int(UNINTERNED + i);
                // The wrapper under test. Every character of `s` is ASCII, so
                // this answers from the table and allocates nothing.
                let c = praxis_text_get(ctx, text, index);
                assert_eq!(c.as_char(), 'd');
                let after = rt.heap().stats().live_count;
                if after < before.saturating_add(1) {
                    paced = true;
                    break;
                }
                before = after;
            }
            drop(frame);
            assert!(
                paced,
                "praxis_text_get never gave the collector a turn on the interned path"
            );
        }
        unsafe { drop_ctx(ctx) };
    }

    /// `default_cell`'s `Char` arm, the fourth boxing site. A `Grid[Char]`'s
    /// fill is NUL, which is inside the range.
    #[test]
    fn a_grid_of_char_fills_with_the_interned_nul() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            let grid = praxis_grid_new(ctx, &crate::scalars::CHAR, 3, 2);
            assert!(!rt.has_pending_fault());
            let nul = rt.immortals().small_char(0).expect("NUL is interned");
            for y in 0..2 {
                for x in 0..3 {
                    let xi = praxis_alloc_int(ctx, x);
                    let yi = praxis_alloc_int(ctx, y);
                    let cell = praxis_grid_get(ctx, grid, xi, yi);
                    assert_eq!(
                        cell.as_ptr(),
                        nul.as_ptr(),
                        "every cell of a fresh Grid[Char] is the one interned NUL"
                    );
                }
            }
        }
        unsafe { drop_ctx(ctx) };
    }

    /// The executable form of "nothing in the language can observe `Char`
    /// identity", and the [`crate::dynamic_key::DynamicKey`] leg of ADR-107's
    /// argument.
    ///
    /// `DynamicKey::eq` opens with a pointer comparison, and that is the line
    /// interning could in principle have moved — but it is a fast path *for*
    /// structural equality and `char_equals` is a reflexive `u32 ==`, so sharing
    /// can only make it fire more often. This asserts the consequence rather than
    /// the argument: the same shape is run twice, once with interned keys and
    /// once with keys the runtime does not intern, and the two must agree.
    #[test]
    fn interning_a_char_does_not_change_keyed_collection_behaviour() {
        // (key, a different key, label): once inside the ASCII range and once
        // outside it. `é` and `ü` are two scalars the table does not hold.
        for (a_ch, b_ch, label) in [('a', 'b', "interned"), ('é', 'ü', "allocated")] {
            let mut rt = Runtime::new();
            let ctx = wired_ctx(&mut rt);
            // SAFETY: ctx wired; every ref below comes from the ABI.
            unsafe {
                let a = praxis_alloc_char(ctx, i64::from(a_ch as u32));
                // A *second* reference to the same value, built separately.
                // Interned it is `a`; uninterned it is a different object with
                // the same payload. Both must key the same slot.
                let a_again = praxis_alloc_char(ctx, i64::from(a_ch as u32));
                let b = praxis_alloc_char(ctx, i64::from(b_ch as u32));
                assert_eq!(
                    std::ptr::eq(a.as_ptr(), a_again.as_ptr()),
                    label == "interned",
                    "the fixture must actually be {label}"
                );

                let set = praxis_set_new(ctx, &crate::scalars::CHAR);
                let _ = praxis_set_insert(ctx, set, a);
                assert_eq!(
                    praxis_bool_load(ctx, praxis_set_contains(ctx, set, a_again)),
                    1,
                    "{label}: an equal Char is the same set member"
                );
                assert_eq!(
                    praxis_bool_load(ctx, praxis_set_contains(ctx, set, b)),
                    0,
                    "{label}: a different Char is not"
                );
                // Inserting the equal-but-possibly-distinct object must not add
                // a second member — the property that would break if the pointer
                // fast path and `char_equals` ever disagreed.
                let _ = praxis_set_insert(ctx, set, a_again);
                assert_eq!(praxis_int_load(ctx, praxis_set_len(ctx, set)), 1, "{label}");

                let counter = praxis_counter_new(ctx, &crate::scalars::CHAR);
                let _ = praxis_counter_inc(ctx, counter, a);
                let _ = praxis_counter_inc(ctx, counter, a_again);
                assert_eq!(
                    praxis_int_load(ctx, praxis_counter_get(ctx, counter, a)),
                    2,
                    "{label}: two bumps of an equal key are one key"
                );
                assert_eq!(
                    praxis_int_load(ctx, praxis_counter_len(ctx, counter)),
                    1,
                    "{label}"
                );
            }
            unsafe { drop_ctx(ctx) };
        }
    }

    /// An interned `Char` is never registered, so a collection cannot reclaim it
    /// however unrooted it is — the `Char` half of
    /// [`an_interned_int_survives_collection_unrooted`].
    #[test]
    fn an_interned_char_survives_collection_unrooted() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx is wired to rt throughout.
        unsafe {
            let _ = praxis_alloc_char(ctx, i64::from('q' as u32));
        }
        assert_eq!(
            rt.heap().stats().live_count,
            0,
            "an interned Char must not enter the live registry"
        );
        // Nothing roots `'q'`: no shadow frame, no native scope, no Rust local
        // the collector can see. A registered object here would be swept.
        rt.collect_now();
        // SAFETY: ctx is still wired; the reference must still be readable.
        unsafe {
            let q = praxis_alloc_char(ctx, i64::from('q' as u32));
            assert!(!q.header().is_poisoned(), "an immortal is never swept");
            assert_eq!(q.as_char(), 'q');
        }
        unsafe { drop_ctx(ctx) };
    }

    /// An interned `Int` is never registered, so a collection cannot reclaim it
    /// however unrooted it is. The `Int` analogue of
    /// `runtime_collect_keeps_immortals_alive_unrooted`.
    #[test]
    fn an_interned_int_survives_collection_unrooted() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx is wired to rt throughout.
        unsafe {
            let _ = praxis_alloc_int(ctx, 5);
        }
        assert_eq!(
            rt.heap().stats().live_count,
            0,
            "an interned Int must not enter the live registry"
        );
        // Nothing roots `5`: no shadow frame, no native scope, no Rust local the
        // collector can see. A registered object here would be swept.
        rt.collect_now();
        // SAFETY: ctx is still wired; the reference must still be readable.
        unsafe {
            let five = praxis_alloc_int(ctx, 5);
            assert!(!five.header().is_poisoned(), "an immortal is never swept");
            assert_eq!(praxis_int_load(ctx, five), 5);
        }
        unsafe { drop_ctx(ctx) };
    }

    /// The executable form of "nothing in the language can observe `Int`
    /// identity": the three keyed collections must behave identically whether
    /// their keys are shared objects or distinct ones.
    ///
    /// [`crate::dynamic_key::DynamicKey`]'s `eq` opens with a pointer
    /// comparison, and that is the line interning could in principle have moved
    /// — but it is a fast path *for* structural equality and `int_equals` is
    /// reflexive, so sharing can only make it fire more often. This asserts the
    /// consequence rather than the argument: every operation below is run twice
    /// at the same shape, once with interned keys and once with uninterned ones,
    /// and the two must agree.
    #[test]
    fn interning_does_not_change_keyed_collection_behaviour() {
        // (key_a, key_b) pairs: two distinct keys, once inside the interned
        // range and once outside it.
        for (a_val, b_val, label) in [
            (5_i64, 6_i64, "interned"),
            (UNINTERNED, UNINTERNED + 1, "allocated"),
        ] {
            let mut rt = Runtime::new();
            let ctx = wired_ctx(&mut rt);
            // SAFETY: ctx wired; every ref below comes from the ABI.
            unsafe {
                let a = praxis_alloc_int(ctx, a_val);
                // A *second* reference to the same value, allocated separately.
                // Interned it is `a`; uninterned it is a different object with
                // the same payload. Both must key the same slot.
                let a_again = praxis_alloc_int(ctx, a_val);
                let b = praxis_alloc_int(ctx, b_val);

                let map = praxis_map_new(ctx, &scalars::INT);
                let one = praxis_alloc_int(ctx, 1);
                let _ = praxis_map_insert(ctx, map, a, one);
                // `map_index` rather than `map_get`: the subscript answers the
                // value where `.get` answers an `Option` (ADR-076), and the
                // value is what has to match.
                assert_eq!(
                    praxis_int_load(ctx, praxis_map_index(ctx, map, a_again)),
                    1,
                    "{label}: an equal key must find the entry"
                );
                assert_eq!(
                    rt.fault(),
                    FaultKind::None,
                    "{label}: an equal key is a present key"
                );
                assert_eq!(
                    praxis_bool_load(ctx, praxis_map_contains(ctx, map, b)),
                    0,
                    "{label}: a different key must not"
                );
                assert_eq!(praxis_int_load(ctx, praxis_map_len(ctx, map)), 1);

                let set = praxis_set_new(ctx, &scalars::INT);
                let _ = praxis_set_insert(ctx, set, a);
                let _ = praxis_set_insert(ctx, set, a_again);
                assert_eq!(
                    praxis_int_load(ctx, praxis_set_len(ctx, set)),
                    1,
                    "{label}: re-inserting an equal value must not grow the set"
                );
                assert_eq!(praxis_bool_load(ctx, praxis_set_contains(ctx, set, b)), 0);

                let counter = praxis_counter_new(ctx, &scalars::INT);
                let _ = praxis_counter_inc(ctx, counter, a);
                let _ = praxis_counter_inc(ctx, counter, a_again);
                assert_eq!(
                    praxis_int_load(ctx, praxis_counter_get(ctx, counter, a)),
                    2,
                    "{label}: two bumps of an equal key are two bumps of one key"
                );
                assert_eq!(praxis_int_load(ctx, praxis_counter_len(ctx, counter)), 1);
            }
            unsafe { drop_ctx(ctx) };
        }
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
        // A fresh *immortal* per call would be unregistered storage no
        // collection can reclaim, leaking one Bool per loop iteration. There
        // are two Bools; a hundred calls must name two objects.
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
    /// singletons — the comparisons and the `is_empty`/`contains` family, which
    /// are what a real program calls in a loop. It is also what makes their
    /// `Effect::Pure` rows honest: nothing here can collect, so the call site is
    /// not a safepoint.
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

    /// Every wrapper that boxes a *derived* scalar — `Text` construction, the
    /// `.len()` family, `Grid` extents, checked arithmetic — paces the
    /// collector. Without that, a program whose pressure comes from those (a
    /// text-processing loop, say) could run arbitrarily long with the collector
    /// never offered a turn. Each is driven here until its own pacing collects.
    ///
    /// **Every receiver is sized past the interned range on purpose.** The first
    /// four wrappers answer a *length*, and a length inside
    /// [`crate::small_int`]'s range is an immortal that never enters the live
    /// registry — so with a five-byte `Text` or a 2×2 `Grid` the shrink test
    /// below is true on the first iteration and the test passes without a
    /// collection ever running (see `UNINTERNED`). Interning removes the
    /// allocation, not the pacing, and it is the pacing this test is about; the
    /// oversized receivers are what keep the observable in place.
    #[test]
    fn every_scalar_boxing_wrapper_paces_the_collector() {
        // (name, a closure that performs one allocating call)
        type Call = unsafe extern "C" fn(*mut RuntimeContext, GcRef) -> GcRef;
        let cases: [(&str, Call); 7] = [
            ("praxis_text_len", praxis_text_len),
            ("praxis_vec_len", praxis_vec_len),
            ("praxis_grid_width", praxis_grid_width),
            ("praxis_grid_height", praxis_grid_height),
            ("praxis_float_to_text", praxis_float_to_text),
            ("praxis_int_to_text", praxis_int_to_text),
            ("praxis_char_to_text", praxis_char_to_text),
        ];
        // One past the interned range, in whatever unit the receiver measures.
        let big = UNINTERNED as usize;
        for (name, call) in cases {
            let mut rt = Runtime::new();
            let ctx = wired_ctx(&mut rt);
            // SAFETY: ctx wired; each receiver matches its wrapper.
            unsafe {
                let text = "x".repeat(big);
                let receiver = match name {
                    "praxis_text_len" => praxis_alloc_text(ctx, text.as_ptr(), big),
                    "praxis_vec_len" => {
                        // The elements are all the same interned `0`, so the Vec
                        // costs one allocation regardless of its length — only
                        // its `len()` matters here.
                        rt.alloc_vec(&scalars::INT, vec![rt.alloc_int(0); big])
                    }
                    "praxis_float_to_text" => praxis_alloc_float(ctx, 1.5_f64.to_bits() as i64),
                    // The two `to_text` rows answer a fresh owned `Text` every
                    // call whatever the receiver is, so an interned receiver is
                    // the honest case: the allocation is the *answer*, not the
                    // argument.
                    "praxis_int_to_text" => praxis_alloc_int(ctx, big as i64),
                    "praxis_char_to_text" => praxis_alloc_char(ctx, i64::from(u32::from('e'))),
                    // `width` reads the first dimension and `height` the second,
                    // so each case makes *its own* answer uninterned and leaves
                    // the other dimension at one cell.
                    "praxis_grid_width" => praxis_grid_new(ctx, &scalars::INT, big as i64, 1),
                    _ => praxis_grid_new(ctx, &scalars::INT, 1, big as i64),
                };
                let mut frame = push_frame(ctx, SlotCount::new(1).unwrap());
                frame.set(0, receiver);

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
                drop(frame);
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
    /// The kind is `IntOverflow` and not `EmptyRange` (ADR-059, ADR-075):
    /// `Int::MIN..Int::MAX` is the *widest* range expressible, so "empty range"
    /// would be a fault message that contradicts the input. `gcd`, `lcm` and
    /// A\*'s path cost answer `IntOverflow` for a result with no `Int` too.
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
        // The add/sub/mul overflow paths are symmetric. Sub: `Int::MIN - 1`
        // overflows.
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
        // This pins the *value*; `bool_and_unit_abi_allocations_reuse_runtime_singletons`
        // pins the identity. Bool equality is structural (§5.5).
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

    // --- Vec[T] collection wrappers ----------------------------------------

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

    /// `praxis_vec_set` and `praxis_deque_set` **replace** — the property that
    /// separates them from the push beside them, and the one a store row pointed
    /// at the wrong wrapper would break silently.
    ///
    /// So each assertion is about what appending would get wrong: the length is
    /// unchanged, the neighbours are unchanged, an index one past the end faults
    /// instead of growing the collection, and a value of the wrong type is
    /// refused rather than retagging an explicitly typed collection.
    #[test]
    fn a_sequence_store_replaces_and_never_appends() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; the stores mutate in place, so the same `GcRef`s
        // stay valid throughout.
        unsafe {
            let v = praxis_vec_new(ctx, &crate::scalars::INT as *const _);
            for n in [10, 20, 30] {
                let _ = praxis_vec_push(ctx, v, praxis_alloc_int(ctx, n));
            }
            let d = praxis_deque_new(ctx, &crate::scalars::INT as *const _);
            for n in [10, 20] {
                let _ = praxis_deque_push_back(ctx, d, praxis_alloc_int(ctx, n));
            }

            let one = praxis_alloc_int(ctx, 1);
            let ninety_nine = praxis_alloc_int(ctx, 99);
            let _ = praxis_vec_set(ctx, v, one, ninety_nine);
            let _ = praxis_deque_set(ctx, d, one, ninety_nine);
            assert!(!rt.has_pending_fault());

            assert_eq!(praxis_int_load(ctx, praxis_vec_len(ctx, v)), 3);
            assert_eq!(praxis_int_load(ctx, praxis_deque_len(ctx, d)), 2);
            let zero = praxis_alloc_int(ctx, 0);
            let two = praxis_alloc_int(ctx, 2);
            assert_eq!(praxis_int_load(ctx, praxis_vec_get(ctx, v, zero)), 10);
            assert_eq!(praxis_int_load(ctx, praxis_vec_get(ctx, v, one)), 99);
            assert_eq!(praxis_int_load(ctx, praxis_vec_get(ctx, v, two)), 30);
            assert_eq!(praxis_int_load(ctx, praxis_deque_get(ctx, d, zero)), 10);
            assert_eq!(praxis_int_load(ctx, praxis_deque_get(ctx, d, one)), 99);

            // One past the end, and a negative index, are both out of range —
            // and neither grows the collection, which is what a store that fell
            // through to the appending wrapper would do.
            let three = praxis_alloc_int(ctx, 3);
            let neg = praxis_alloc_int(ctx, -1);
            type Store = unsafe extern "C" fn(*mut RuntimeContext, GcRef, GcRef, GcRef) -> GcRef;
            type Len = unsafe extern "C" fn(*mut RuntimeContext, GcRef) -> GcRef;
            for (recv, idx, store, len_of) in [
                (v, three, praxis_vec_set as Store, praxis_vec_len as Len),
                (v, neg, praxis_vec_set as Store, praxis_vec_len as Len),
                (d, two, praxis_deque_set as Store, praxis_deque_len as Len),
                (d, neg, praxis_deque_set as Store, praxis_deque_len as Len),
            ] {
                let before = praxis_int_load(ctx, len_of(ctx, recv));
                let _ = store(ctx, recv, idx, ninety_nine);
                assert!(rt.has_pending_fault(), "an out-of-range store must fault");
                assert_eq!(rt.take_fault(), Some(FaultKind::IndexOutOfBounds));
                assert_eq!(
                    praxis_int_load(ctx, len_of(ctx, recv)),
                    before,
                    "a faulting store must not have grown the collection"
                );
            }

            // A `Vec[Int]` refuses a `Float` rather than retagging itself.
            let float = praxis_alloc_float(ctx, 1.5_f64.to_bits() as i64);
            let _ = praxis_vec_set(ctx, v, zero, float);
            assert_eq!(rt.take_fault(), Some(FaultKind::TypeMismatch));
            assert_eq!(praxis_int_load(ctx, praxis_vec_get(ctx, v, zero)), 10);
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn vec_push_many_survive_collection() {
        // Stress: root the receiver/current element exactly as generated code
        // does, push enough elements to force multiple automatic collections,
        // and leave one unrooted allocation per iteration so collection is
        // observable as a live-registry shrink.
        //
        // Both the pushed element and the deliberately-unrooted allocation are
        // offset past the interned range: an interned `Int` is never registered,
        // so an in-range element would trip the shrink test on iteration zero
        // and neither the collection nor the rooting would be exercised (see
        // `UNINTERNED`). The offset is carried through the spot checks below so
        // the values read back are still the values pushed.
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; push mutates in place so `v` stays valid throughout.
        unsafe {
            let v = praxis_vec_new(ctx, &crate::scalars::INT as *const _);
            let mut frame = push_frame(ctx, SlotCount::new(2).unwrap());
            frame.set(0, v);
            let mut observed_reclamation = false;
            for i in 0..5000_i64 {
                let before_alloc = rt.heap().stats().live_count;
                let elem = praxis_alloc_int(ctx, UNINTERNED + i);
                if rt.heap().stats().live_count < before_alloc.saturating_add(1) {
                    observed_reclamation = true;
                }
                frame.set(1, elem);
                let before_push = rt.heap().stats().live_count;
                let _ = praxis_vec_push(ctx, v, elem);
                if rt.heap().stats().live_count < before_push {
                    observed_reclamation = true;
                }
                frame.clear(1);
                let _ = rt.alloc_int(-UNINTERNED - i - 1);
            }
            assert!(
                observed_reclamation,
                "the test must observe an automatic collection, not merely allocation pressure"
            );
            assert_eq!(praxis_int_load(ctx, praxis_vec_len(ctx, v)), 5000);
            // Spot-check first/middle/last. The *indices* stay small (they are
            // interned, which is fine — nothing here watches them); the values
            // carry the offset the elements were pushed with.
            let zero = praxis_alloc_int(ctx, 0);
            assert_eq!(
                praxis_int_load(ctx, praxis_vec_get(ctx, v, zero)),
                UNINTERNED
            );
            let middle = praxis_alloc_int(ctx, 2500);
            assert_eq!(
                praxis_int_load(ctx, praxis_vec_get(ctx, v, middle)),
                UNINTERNED + 2500
            );
            let last = praxis_alloc_int(ctx, 4999);
            assert_eq!(
                praxis_int_load(ctx, praxis_vec_get(ctx, v, last)),
                UNINTERNED + 4999
            );
            drop(frame);
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn vec_get_negative_index_faults() {
        // The `idx < 0` guard in `praxis_vec_get`: a negative index is out of
        // bounds, not a wrapped-around large one.
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
        // Companion to `vec_get_negative_index_faults`, for `praxis_text_get`'s
        // own `idx < 0` guard.
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

    /// **ADR-086, the runtime half.** `praxis_text_get` allocates a `Char`.
    ///
    /// The catalog's twin (`the_two_text_reads_answer_a_char`) is pure data and
    /// cannot see this; this is pure runtime and cannot see that. Both halves
    /// are needed, and they must hold together: with only one, a `Char`-typed
    /// value routes into `praxis_char_load`, whose `read_scalar` answers `None`
    /// against the `INT` descriptor and panics.
    #[test]
    fn text_get_answers_a_char_object() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            // `"sddddd"[4]` must be `'d'` and not `100`: an object carrying the
            // `INT` descriptor would have the right value and the wrong type.
            let s = "sddddd";
            let text = praxis_alloc_text(ctx, s.as_ptr(), s.len());
            let four = praxis_alloc_int(ctx, 4);
            let got = praxis_text_get(ctx, text, four);
            assert!(!rt.has_pending_fault());
            assert!(
                std::ptr::eq(got.descriptor(), &crate::scalars::CHAR),
                "ADR-086: the read answers a Char, not the char's scalar value"
            );
            assert_eq!(got.as_char(), 'd');

            // Scalar-not-byte indexing, pinned at the runtime level too: `é` is
            // one scalar and two UTF-8 bytes, so a byte index would answer 0xC3.
            let u = "héllo";
            let utext = praxis_alloc_text(ctx, u.as_ptr(), u.len());
            let one = praxis_alloc_int(ctx, 1);
            let got = praxis_text_get(ctx, utext, one);
            assert!(!rt.has_pending_fault());
            assert_eq!(got.as_char(), 'é');
        }
        unsafe { drop_ctx(ctx) };
    }

    /// **ADR-115 at the ABI, on the shape that decides it.** `t.len()` and
    /// `t[i]` are defined on scalars (§4.3, ADR-086); indexing bytes is an
    /// optimization licensed by the count, and the licence must be refused on
    /// every text where it would be wrong.
    ///
    /// The cases are chosen to break a byte-indexing implementation that only
    /// looked at the text's own leading byte or only at its first scalar: a
    /// multi-byte scalar at the start, in the middle, at the end, a four-byte
    /// one, and a slice whose own bytes are all one-byte but whose owner's are
    /// not.
    #[test]
    fn a_text_reads_by_scalar_wherever_the_multi_byte_scalar_sits() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired for every call in this block.
        unsafe {
            for src in [
                "",
                "abc",
                "\u{0}\u{7f}",
                "éabc",
                "abéc",
                "abcé",
                "a\u{1F600}b",
                "\u{20AC}\u{20AC}",
                "héllo wörld",
            ] {
                let text = praxis_alloc_text(ctx, src.as_ptr(), src.len());
                let expected: Vec<char> = src.chars().collect();

                let len = praxis_text_len(ctx, text);
                assert!(!rt.has_pending_fault(), "{src:?}");
                assert_eq!(len.as_int(), expected.len() as i64, "{src:?}");

                let empty = praxis_text_is_empty(ctx, text);
                assert_eq!(empty.as_bool(), expected.is_empty(), "{src:?}");

                for (i, want) in expected.iter().enumerate() {
                    let idx = praxis_alloc_int(ctx, i as i64);
                    let got = praxis_text_get(ctx, text, idx);
                    assert!(!rt.has_pending_fault(), "{src:?}[{i}]");
                    assert!(
                        std::ptr::eq(got.descriptor(), &crate::scalars::CHAR),
                        "{src:?}[{i}] answers a Char (ADR-086)"
                    );
                    assert_eq!(got.as_char(), *want, "{src:?}[{i}]");
                }

                // One past the end faults, whichever path answered above.
                let past = praxis_alloc_int(ctx, expected.len() as i64);
                let _ = praxis_text_get(ctx, text, past);
                assert!(rt.has_pending_fault(), "{src:?}[{}]", expected.len());
                assert_eq!(rt.fault(), FaultKind::IndexOutOfBounds);
                let _ = rt.take_fault();
            }

            // A view whose own bytes are all one-byte, inside an owner whose
            // are not. The answers are the same as the owner's corresponding
            // scalars; the byte-index path is refused because the licence is
            // the owner's to give.
            let owner_src = "héllo wörld";
            let owner = praxis_alloc_text(ctx, owner_src.as_ptr(), owner_src.len());
            // "llo " — bytes [3, 7) of the owner, all below 0x80.
            let view = rt
                .alloc_text_slice(owner, 3, 4)
                .expect("[3, 7) is on scalar boundaries");
            let len = praxis_text_len(ctx, view);
            assert_eq!(len.as_int(), 4);
            for (i, want) in "llo ".chars().enumerate() {
                let idx = praxis_alloc_int(ctx, i as i64);
                let got = praxis_text_get(ctx, view, idx);
                assert!(!rt.has_pending_fault());
                assert_eq!(got.as_char(), want);
            }
        }
        unsafe { drop_ctx(ctx) };
    }

    /// **ADR-086's narrowing half.** `Int.to_char()` reaches the same
    /// range check `praxis_alloc_char` does, because they share one helper.
    ///
    /// A wrapper that forwarded `value as u32` without the check would answer
    /// `'A'` for `0x1_0000_0041` instead of faulting, which is the case that
    /// proves this door reaches the shared guard rather than restating it.
    #[test]
    fn int_to_char_rejects_what_is_not_a_scalar_value() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            for bad in [-1_i64, 0xD800, 0x11_0000, 0x1_0000_0041] {
                let n = praxis_alloc_int(ctx, bad);
                let got = praxis_int_to_char(ctx, n);
                assert!(rt.has_pending_fault(), "{bad} must not answer a Char");
                assert_eq!(rt.fault(), FaultKind::InvalidChar, "{bad}");
                assert!(std::ptr::eq(got.descriptor(), &crate::scalars::UNIT));
                let _ = rt.take_fault();
            }

            // …and the round trip holds for one that is.
            let n = praxis_alloc_int(ctx, 233);
            let got = praxis_int_to_char(ctx, n);
            assert!(!rt.has_pending_fault());
            assert_eq!(got.as_char(), 'é');
            let back = praxis_char_to_int(ctx, got);
            assert!(!rt.has_pending_fault());
            assert_eq!(back.as_int(), 233);
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn alloc_text_empty_string_round_trips() {
        // The `len == 0` branch in `praxis_alloc_text` treats an empty buffer as
        // the empty slice. An empty Text must format as "".
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
        // A null element descriptor is kept null — "the caller has no static
        // element type" — and the vec must still be usable.
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
        // And the fault it raises must name itself: a `FaultKind::None` here
        // would have the host report "no fault" while generated code took its
        // fault path.
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

    /// **ADR-111.** Input that is not UTF-8 faults at the `read`, because that
    /// is where the bytes stop being the compiler's and start being the host's.
    ///
    /// Asserted through `praxis_get_input` and never by feeding
    /// `praxis_alloc_text` bad bytes directly: that is a violated precondition,
    /// so it panics through `abi_guard!`, and `praxis_alloc_text`'s
    /// `Allocates` row makes the resulting `Panic` fault unobservable — the
    /// process aborts instead of failing. The property is that a program
    /// reading non-UTF-8 input gets `InvalidText` at its `read`.
    ///
    /// `praxis run` cannot reach this: `lazy_stdin::read` goes through
    /// `std::io::read_to_string` and exits 2 on non-UTF-8 stdin before the
    /// runtime sees a byte (`praxis-cli/src/run.rs`). The reachable caller is an
    /// embedder that installs its own `InputReader`, which is exactly what this
    /// test is.
    #[test]
    fn input_that_is_not_utf8_faults_at_the_read() {
        fn not_utf8() -> Vec<u8> {
            vec![0xF0, 0x28, 0x8C, 0x28]
        }
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        crate::input::install_input_reader(not_utf8);
        let result = unsafe { praxis_get_input(ctx) };
        let unit = rt.immortals().unit();
        crate::input::clear_input_reader();
        unsafe { drop_ctx(ctx) };

        assert_eq!(rt.fault(), FaultKind::InvalidText);
        assert!(rt.has_pending_fault());
        assert_eq!(
            result.as_ptr(),
            unit.as_ptr(),
            "the fault path answers §10.4's defined dummy, not a half-built Text"
        );
    }

    /// The mutation companion, and it is required: a `praxis_get_input` that
    /// faulted on *every* input would pass the gate above.
    ///
    /// Multi-byte on purpose — a validation that accepted only ASCII would also
    /// pass a test written with `"hi"`.
    #[test]
    fn input_that_is_utf8_still_becomes_the_buffer() {
        fn multibyte() -> Vec<u8> {
            "héllo wörld".as_bytes().to_vec()
        }
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        crate::input::install_input_reader(multibyte);
        let result = unsafe { praxis_get_input(ctx) };
        let contents = result.as_text().to_string();
        let descriptor = result.descriptor().name;
        crate::input::clear_input_reader();
        unsafe { drop_ctx(ctx) };

        assert!(!rt.has_pending_fault(), "fault: {:?}", rt.fault());
        assert_eq!(descriptor, "Text");
        assert_eq!(contents, "héllo wörld");
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

    /// An extent must be validated before it becomes a `usize`. Unchecked,
    /// `vec![unit; (w as usize) * (h as usize)]` turns `-1` into `usize::MAX`,
    /// and the products either overflow (a capacity panic across `extern "C"`)
    /// or ask the host for terabytes (an OOM abort). The wrapper must answer
    /// with a fault, and the heap must be untouched — a partly-built grid is as
    /// bad as a crash.
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

    /// **ADR-146.** `Vec(n, fill)` builds `n` slots, all of them the fill, and
    /// the empty case is a `Vec` and not a fault.
    #[test]
    fn vec_filled_builds_n_copies_of_one_value() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let observed: Vec<(usize, bool)> = [0_i64, 1, 7]
            .into_iter()
            .map(|n| unsafe {
                let count = praxis_alloc_int(ctx, n);
                let fill = praxis_alloc_int(ctx, 42);
                let v = praxis_vec_filled(ctx, &crate::scalars::INT as *const _, count, fill);
                let p = vec_payload(v);
                // Every slot is the *same* reference, which is the aliasing
                // ADR-146 decision 4 states rather than n copies of a value.
                let all_same = p.items.iter().all(|item| item.as_ptr() == fill.as_ptr());
                (p.items.len(), all_same)
            })
            .collect();
        unsafe { drop_ctx(ctx) };

        assert_eq!(rt.fault(), FaultKind::None, "no in-range count faults");
        assert_eq!(observed, vec![(0, true), (1, true), (7, true)]);
    }

    /// The other half of ADR-041 decision 1, for the newtype it added: a count
    /// the runtime cannot serve is a fault and not an allocation, and the heap
    /// is untouched — a half-built `Vec` is as bad as a crash.
    #[test]
    fn vec_filled_refuses_a_negative_or_absurd_count() {
        let absurd = crate::collections::VecExtent::MAX_ITEMS as i64 + 1;
        for n in [-1_i64, i64::MIN, absurd, i64::MAX] {
            let mut rt = Runtime::new();
            let ctx = wired_ctx(&mut rt);
            let (result, live_before, live_after, unit) = unsafe {
                let count = praxis_alloc_int(ctx, n);
                let fill = praxis_alloc_int(ctx, 0);
                let before = rt.heap().stats().live_count;
                let r = praxis_vec_filled(ctx, &crate::scalars::INT as *const _, count, fill);
                (
                    r,
                    before,
                    rt.heap().stats().live_count,
                    rt.immortals().unit(),
                )
            };
            unsafe { drop_ctx(ctx) };

            assert_eq!(rt.fault(), FaultKind::InvalidSize, "Vec({n}, 0) must fault");
            assert_eq!(
                result.as_ptr(),
                unit.as_ptr(),
                "a faulted Vec({n}, 0) returns the Unit sentinel"
            );
            assert_eq!(
                live_after, live_before,
                "a rejected Vec({n}, 0) allocates nothing"
            );
        }
    }

    /// A declared element type the fill is not is a `TypeMismatch`, through the
    /// same `adopt_or_reject` a `push` goes through — not a silent retag of the
    /// collection to the fill's type, which is the mislabelling defect one
    /// level down.
    /// A *null* static descriptor adopts instead, which is what "the caller has
    /// no static element type" already means for `praxis_vec_new`.
    #[test]
    fn vec_filled_reconciles_its_element_descriptor() {
        let mut rejecting = Runtime::new();
        let ctx = wired_ctx(&mut rejecting);
        unsafe {
            let count = praxis_alloc_int(ctx, 3);
            let text = praxis_alloc_text(ctx, b"x".as_ptr(), 1);
            praxis_vec_filled(ctx, &crate::scalars::INT as *const _, count, text);
            drop_ctx(ctx);
        }
        assert_eq!(
            rejecting.fault(),
            FaultKind::TypeMismatch,
            "a `Vec[Int]` filled with a `Text` is a mislabelled element descriptor"
        );

        let mut adopting = Runtime::new();
        let ctx = wired_ctx(&mut adopting);
        let adopted = unsafe {
            let count = praxis_alloc_int(ctx, 3);
            let text = praxis_alloc_text(ctx, b"x".as_ptr(), 1);
            let v = praxis_vec_filled(ctx, std::ptr::null(), count, text);
            let matches = std::ptr::eq(vec_payload(v).element_descriptor, text.descriptor());
            drop_ctx(ctx);
            matches
        };
        assert_eq!(adopting.fault(), FaultKind::None);
        assert!(
            adopted,
            "a null static descriptor adopts the fill's, as `praxis_vec_new` already does"
        );
    }

    /// **ADR-146 decision 6.** `Grid(w, h, fill)` accepts a fill
    /// `praxis_grid_new` cannot invent: `default_cell` has no zero value for a
    /// composite and answers `TypeMismatch`, and the explicit fill is exactly
    /// what removes the question. The contrast is the assertion — both calls
    /// are in one test so a later change that reintroduced `default_cell` here
    /// fails rather than passes quietly.
    #[test]
    fn grid_filled_accepts_a_composite_fill_where_grid_new_cannot() {
        let mut inventing = Runtime::new();
        let ctx = wired_ctx(&mut inventing);
        unsafe {
            praxis_grid_new(ctx, &crate::collections::VEC as *const _, 2, 2);
            drop_ctx(ctx);
        }
        assert_eq!(
            inventing.fault(),
            FaultKind::TypeMismatch,
            "`praxis_grid_new` still has no zero value for a `Vec` cell"
        );

        let mut supplied = Runtime::new();
        let ctx = wired_ctx(&mut supplied);
        let (cells, all_same) = unsafe {
            let inner = praxis_vec_new(ctx, &crate::scalars::INT as *const _);
            let (w, h) = (praxis_alloc_int(ctx, 2), praxis_alloc_int(ctx, 2));
            let g = praxis_grid_filled(ctx, &crate::collections::VEC as *const _, w, h, inner);
            let p = grid_payload(g);
            let same = p.items.iter().all(|c| c.as_ptr() == inner.as_ptr());
            let len = p.items.len();
            drop_ctx(ctx);
            (len, same)
        };
        assert_eq!(supplied.fault(), FaultKind::None);
        assert_eq!(cells, 4, "an explicit fill builds all four cells");
        assert!(
            all_same,
            "the four cells are one `Vec`, not four (ADR-146 decision 4)"
        );
    }

    /// `Grid(w, h, fill)` takes the extents `praxis_grid_new` refuses, through
    /// the same `GridExtent::new` — a fill changes nothing about the
    /// arithmetic.
    #[test]
    fn grid_filled_refuses_the_extents_grid_new_refuses() {
        let absurd = GridExtent::MAX_CELLS as i64 + 1;
        for (width, height) in [(-1_i64, 4_i64), (4, -1), (i64::MAX, 2), (absurd, 1)] {
            let mut rt = Runtime::new();
            let ctx = wired_ctx(&mut rt);
            let (result, live_before, live_after, unit) = unsafe {
                let (w, h) = (praxis_alloc_int(ctx, width), praxis_alloc_int(ctx, height));
                let fill = praxis_alloc_int(ctx, 0);
                let before = rt.heap().stats().live_count;
                let r = praxis_grid_filled(ctx, &crate::scalars::INT as *const _, w, h, fill);
                (
                    r,
                    before,
                    rt.heap().stats().live_count,
                    rt.immortals().unit(),
                )
            };
            unsafe { drop_ctx(ctx) };

            assert_eq!(
                rt.fault(),
                FaultKind::InvalidSize,
                "Grid({width}, {height}, 0) must fault"
            );
            assert_eq!(
                result.as_ptr(),
                unit.as_ptr(),
                "a faulted Grid({width}, {height}, 0) returns the Unit sentinel"
            );
            assert_eq!(
                live_after, live_before,
                "a rejected Grid({width}, {height}, 0) allocates nothing"
            );
        }
    }

    /// A member the set cannot hold is a fault, and a negative one does not
    /// vanish silently. Unchecked, `bs.insert(10^18)` would ask `Vec::resize`
    /// for 10^16 words — an OOM abort from inside `extern "C"`.
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

    /// The words of a **live, heap-allocated** `BitSet`, reached the way
    /// generated code reaches them: through
    /// [`INLINE_BITSET_SITE`](crate::bitset::INLINE_BITSET_SITE), from the
    /// object base, with no knowledge of the payload beyond what the site
    /// carries (ADR-118 part 2).
    ///
    /// `a_backend_can_read_the_length_and_the_elements_out_of_a_live_payload`
    /// in `collections.rs` is this test for `Vec`; this is the second payload's
    /// copy of the same agreement between the emitted load and the layout.
    ///
    /// Compiled out under `std-vec-payload`: that arm has no site to name, so
    /// naming one fails the *build* rather than miscompiling a load.
    #[cfg(not(feature = "std-vec-payload"))]
    #[test]
    fn the_inline_bitset_site_addresses_a_live_bitsets_words() {
        use crate::bitset::INLINE_BITSET_SITE;

        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let (words, len) = unsafe {
            let bs = praxis_bitset_new(ctx);
            assert!(
                std::ptr::eq(INLINE_BITSET_SITE.type_id().descriptor(), bs.descriptor()),
                "the site names the descriptor the inline proof compares against"
            );
            for member in [0_i64, 63, 64, 200] {
                let value = praxis_alloc_int(ctx, member);
                let _ = praxis_bitset_insert(ctx, bs, value);
            }
            let base = bs.as_ptr().cast::<u8>().cast_const();
            (
                base.add(INLINE_BITSET_SITE.elements_offset())
                    .cast::<*const u64>()
                    .read(),
                base.add(INLINE_BITSET_SITE.len_offset())
                    .cast::<usize>()
                    .read(),
            )
        };

        assert_eq!(len, 4, "bit 200 lives in the fourth word");
        assert_eq!(
            INLINE_BITSET_SITE.element_shift(),
            3,
            "a word is eight bytes"
        );
        for member in [0_u64, 63, 64, 200] {
            // SAFETY: `member >> 6 < len`, and `words` is the live buffer the
            // site's displacement just answered.
            let w = unsafe { *words.add((member >> 6) as usize) };
            assert!(
                (w >> (member & 63)) & 1 == 1,
                "bit {member} read back through the site's displacements"
            );
        }
        unsafe { drop_ctx(ctx) };
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
            // The answer is the scalar channel's `0`/`1` (ADR-118 decision 6),
            // so there is no box to load it back out of.
            let answer = praxis_bitset_contains(ctx, bs, huge);
            (answer != 0, bitset_payload(bs).words.len())
        };
        unsafe { drop_ctx(ctx) };

        assert!(!present, "an unrepresentable member is absent");
        assert_eq!(words, 0, "a query allocates no words");
        assert_eq!(rt.fault(), FaultKind::None, "a query does not fault");
    }

    /// `(i64::MAX, i64::MAX).neighbors4()` must not overflow the offset addition
    /// and panic across `extern "C"`. Every such neighbour is outside every
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

    /// `map[key]` faults on an absent key where `.get` answers, and
    /// `praxis_counter_set` replaces a count where `praxis_counter_inc` only
    /// adds one.
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

    /// `Map.get` is statically value-typed, so an absent key cannot answer the
    /// Unit sentinel: a value whose static type is `V` and whose runtime
    /// descriptor is `Unit` is a type confusion the program cannot detect.
    ///
    /// The assertion names the variant, not merely `!= UNIT`: the answer is the
    /// `None` of the runtime's own `option_schema`, which is what makes it match
    /// a program's `None` arm.
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

    /// A `Bool` object laid out exactly the way [`Heap::alloc_raw`] lays one
    /// out — a `GcHeader` followed by its payload at
    /// `GcHeader::payload_offset_for(1)` — but in memory *this module* owns,
    /// and with the seven bytes after the one-byte payload set to `0xFF`.
    ///
    /// The heap cannot be asked for this shape. Under the page allocator
    /// (ADR-103) a `Bool` lands on the ladder's bottom rung, so its block is
    /// rounded up to an 8-byte boundary and the seven bytes after the one-byte
    /// payload are slack no object ever writes — and a fresh page is `mmap`ped
    /// zero, so an eight-byte read of a heap `Bool` answers *correctly* by
    /// accident. Owning the storage turns the padding from an accident into a
    /// fixture, which is the only way the read can be measured rather than
    /// sampled.
    ///
    /// The header carries a **freshly minted** `HeapId`, so the collector's
    /// provenance check (`Heap::mark`) skips this object instead of colouring
    /// it: the oracle roots every closure result, and a root the heap did not
    /// allocate is not the heap's to touch.
    #[repr(C)]
    struct DirtyPaddedBool {
        header: crate::gc::GcHeader,
        /// Byte 0 is the `BoolPayload`; bytes 1..8 are the neighbours a
        /// wrong-width or wrong-offset read would consume.
        payload: [u8; 8],
    }

    /// A `false` whose seven following bytes are `0xFF`, leaked once per thread
    /// so a closure entry point can answer it. See [`DirtyPaddedBool`].
    fn dirty_padded_false() -> GcRef {
        thread_local! {
            static CELL: std::cell::Cell<*mut crate::gc::GcHeader> =
                const { std::cell::Cell::new(std::ptr::null_mut()) };
        }
        CELL.with(|cell| {
            if cell.get().is_null() {
                let object = Box::leak(Box::new(DirtyPaddedBool {
                    header: crate::gc::GcHeader::new(
                        &scalars::BOOL,
                        crate::gc::GcHeader::payload_offset_for(scalars::BOOL.align()) as u16,
                        crate::gc::HeapId::mint(),
                    ),
                    payload: [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
                }));
                cell.set(&mut object.header as *mut crate::gc::GcHeader);
            }
            // SAFETY: the pointer heads a leaked, correctly-laid-out `Bool`
            // object that lives for the rest of the process.
            unsafe { GcRef::from_raw(cell.get()) }
        })
    }

    /// As [`always_false`], but answering the [`dirty_padded_false`] fixture.
    ///
    /// # Safety
    /// As [`always_false`].
    unsafe extern "C" fn always_dirty_false(
        _ctx: *mut RuntimeContext,
        _closure: GcRef,
        _state: GcRef,
    ) -> GcRef {
        dirty_padded_false()
    }

    /// `ClosureOracle::is_goal` must read the closure's `Bool` answer at a
    /// `Bool`'s width: the payload is **one** byte (`BoolPayload = u8`, and
    /// `BOOL` is built from it), so an eight-byte read would take seven further
    /// bytes from past the object.
    ///
    /// Every `bfs_distance` / `a_star` / `flood_fill` goal predicate goes
    /// through that read, so this walk is the whole class: the goal answers
    /// `false` at the only reachable state, and the answer must be `None`.
    ///
    /// The `false` it answers is [`dirty_padded_false`], whose payload byte is
    /// `0x00` and whose next seven bytes are `0xFF`. That is what makes this a
    /// gate rather than a walk: an eight-byte read answers
    /// `0xFFFF_FFFF_FFFF_FF00`, which is "goal reached at the start state" and
    /// therefore `Some(0)`; a read at *any* offset past byte zero answers
    /// `0xFF`, likewise `Some(0)`. Only one byte at offset zero answers `None`,
    /// so the test fails if either the width or the offset is wrong. Asking the
    /// heap for this shape does not work — see [`DirtyPaddedBool`].
    ///
    /// [`read_scalar`] is what makes both mistakes unspellable at the call
    /// site, and `int_payload`'s width check — an ordinary branch, so it holds
    /// in every profile — is what stops the next site from making them;
    /// `read_scalar_answers_none_for_a_foreign_descriptor` pins the reader
    /// itself.
    #[test]
    fn a_graph_goal_predicate_reads_a_bool_at_a_bool_s_width() {
        // The fixture is only a gate while its padding is dirty: state that
        // here, so a later edit that zeroes it fails loudly rather than
        // silently turning this back into the walk it replaced.
        let fixture = dirty_padded_false();
        assert!(std::ptr::eq(fixture.descriptor(), &scalars::BOOL));
        assert_eq!(
            unsafe { read_scalar(fixture, scalars::BOOL_PAYLOAD) },
            Some(0u8),
            "the fixture is `false` at a Bool's width"
        );
        assert_ne!(
            unsafe { *fixture.payload::<i64>() },
            0,
            "…and non-zero at an Int's, which is what the wrong read consumed"
        );

        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let answer = unsafe {
            let goal = praxis_alloc_closure(ctx, always_dirty_false as *const u8, 0);
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

    /// The same walk against the immortal `false` every real program's closure
    /// answers — a companion, not a gate: a wrong-width read passes it, because
    /// the allocator leaves a `Bool`'s slack bytes zero. It rules out a reader
    /// that only handles the fixture correctly.
    #[test]
    fn a_graph_goal_predicate_that_is_false_everywhere_finds_nothing() {
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
        assert_eq!(enum_tag_of(answer), crate::enums::OPTION_NONE_TAG as u32);
        unsafe { drop_ctx(ctx) };
    }

    /// The reader itself, both directions. `read_scalar` is what makes a
    /// wrong-type or wrong-width read unspellable at a call site, so its own
    /// contract is pinned here: the right type reads at the right width, and a
    /// foreign type answers `None` instead of reinterpreting the bytes.
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

    /// The same rule under a *tuple* static type:
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
    // `maybe_collect` is the load-bearing mechanism for the shadow-stack
    // spill: the alloc wrappers call it so collection happens automatically
    // inside JIT'd code.

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
        // Allocating past the 64 KiB threshold collects on its own, with no
        // generated frame on the stack and no hand-written `maybe_collect`
        // call. The helper asserts it happens within 10,000 allocations.
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
            // The *sum* has to be uninterned, not just the operands: what is
            // watched below is whether `praxis_int_add`'s result enters the live
            // registry, and an interned sum never does (see `UNINTERNED`).
            let lhs = praxis_alloc_int(ctx, UNINTERNED);
            let rhs = praxis_alloc_int(ctx, 22);
            let mut frame = push_frame(ctx, SlotCount::new(2).unwrap());
            frame.set(0, lhs);
            frame.set(1, rhs);

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
            drop(frame);
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
            let frame = push_frame(ctx, SlotCount::new(0).unwrap());
            live_after_collection = allocate_until_automatic_collection(&rt, ctx);
            drop(frame);
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
        // Uninterned: what this test watches is a *registered* object surviving
        // a collection that roots only through `ParseDetail.partial`, and an
        // interned `Int` is never registered, so it would survive whether the
        // root set included the slot or not (see `UNINTERNED`).
        let partial = rt.alloc_int(UNINTERNED);
        rt.parse_detail_mut()
            .consider(ParseFail::here(0, "test").with_partial(Some(partial)), b"");
        let live_after_collection;
        unsafe {
            let frame = push_frame(ctx, SlotCount::new(0).unwrap());
            live_after_collection = allocate_until_automatic_collection(&rt, ctx);
            drop(frame);
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
        // Uninterned, for `automatic_gc_roots_parse_failure_partial_values`'s
        // reason: the observable is a registered object surviving because the
        // snapshot rooted it.
        let captured = rt.alloc_int(UNINTERNED);
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
            slot_kind: crate::debug::DebugSlotKind::Reference,
        };
        let metas = [meta];
        let func_name = b"main";
        let func_meta = crate::debug::FunctionDebugMeta {
            func_name: func_name.as_ptr(),
            func_name_len: func_name.len() as u32,
            local_count: 1,
            locals: metas.as_ptr(),
            span_start: 0,
            span_end: 0,
        };
        let live_after_collection;
        // SAFETY: `ctx` is wired to `rt`; `func_meta`/`metas` outlive the guard,
        // and the snapshot is taken while the frame is still claimed — the
        // ordering a generated fault epilogue has (ADR-033 decision 1).
        unsafe {
            let mut debug_frame = crate::debug::push_frame(ctx, &func_meta);
            debug_frame.set(0, captured);
            crate::crash_snapshot::praxis_snapshot_debug_chain(ctx);
            drop(debug_frame);
            assert!(rt.crash_snapshot().is_some());

            let shadow_frame = push_frame(ctx, SlotCount::new(0).unwrap());
            live_after_collection = allocate_until_automatic_collection(&rt, ctx);
            drop(shadow_frame);
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
        // one of those is a safepoint, and the shadow stack only sees what
        // generated code spilled — this is all native code, so the result Vec,
        // the points already in it and the tuple `alloc_point` is midway
        // through filling are rooted by the helper's own `NativeScope`.
        //
        // This calls the real helper rather than inlining a sketch of it, and
        // reading the points back afterwards is what proves nothing was
        // reclaimed.
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
            let mut frame = push_frame(ctx, SlotCount::new(1).unwrap());
            frame.set(0, grid);

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

            drop(frame);
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

    // The inline prologue deliberately does not null-check the context
    // (ADR-101): the check would cost every call in the language, and
    // `Runtime::context` is the only producer of a context generated code is
    // handed. `RuntimeContext::placeholder` carries the obligation in its doc.

    // --- a null element type stays unknown ---------------------------------

    /// A collection built with no static element type must not claim to hold
    /// `Int`s, and what it holds must render as what it is.
    ///
    /// The codegen passes a **null** descriptor for `var c = Counter()` — its
    /// contract above `collection_element_descriptor_for` says so, and says
    /// every `praxis_*_new` wrapper reads it that way. Replacing the null with
    /// `&INT` is not a default but a false claim, and the label is dispatched
    /// through: a `Text` key would hash and print as an `i64`, a `Float`
    /// element as the integer its bits spell.
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
            // and a guessed `Int` label would print both as one.
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

    /// A `Map` does not claim its values are `Int`s.
    ///
    /// `praxis_map_new` takes one descriptor — the key's — because the `MapNew`
    /// row carries one type argument, so the value slot starts **null**.
    /// Writing `INT` there would be a claim rather than a default, and it would
    /// be the same word as "unknown", so the adoption that follows could not
    /// tell a `Map` that really holds `Int`s from one that had never been told
    /// anything — and a `Map[Text, Text]`'s value would be read as an `i64`.
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
            // …so the `Vec` its `values()` answers is not labelled `Int`
            // either. A guessed `Int` there would make an empty
            // `Map[Text, Text]`'s values unequal to an empty `Vec[Text]`,
            // because `vec_equals` compares element labels.
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

            // A `Map` that really does hold `Int`s says so — an assertion only
            // a null "unknown" makes possible, since a hardcoded `INT` would be
            // the same word as "never been told".
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

    /// An unlearned label is not a label, so it cannot make two empty
    /// collections unequal.
    ///
    /// `same_element` is not pointer identity, because a never-inserted `Map`'s
    /// `values()` carries no label and an equally-typed empty `Vec[Int]`
    /// carries `Int`. ADR-066 decision 5 is the rule: a null slot means the
    /// *value's own* descriptor answers, and a collection with no label has no
    /// values, so nothing is left to disagree. Reinstating a guessed descriptor
    /// is the fix this rejects.
    ///
    /// The last case is the limit: two collections that have each been told
    /// their element type must still disagree when the types differ.
    #[test]
    fn an_unlearned_element_label_does_not_make_two_empty_collections_unequal() {
        use crate::collections::same_element;
        let int: *const crate::descriptor::TypeDescriptor = &scalars::INT;
        let text: *const crate::descriptor::TypeDescriptor = &crate::text::TEXT;
        let unlearned: *const crate::descriptor::TypeDescriptor = std::ptr::null();

        assert!(same_element(unlearned, int), "no label agrees with `Int`");
        assert!(same_element(int, unlearned), "and in the other order");
        assert!(same_element(unlearned, unlearned));
        assert!(same_element(int, int));
        // The rule this must not weaken: two *learned* labels that differ are
        // two different collections, empty or not.
        assert!(!same_element(int, text));

        // End to end: a `Map` never inserted into has no value label, and its
        // `values()` is an empty `Vec` that is equal to an empty `Vec` however
        // that one was labelled.
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: `ctx` is wired; every argument below is a wrapper's own.
        unsafe {
            let never_inserted = praxis_map_new(ctx, &crate::text::TEXT);
            let unlabelled = praxis_map_values(ctx, never_inserted);
            assert!(vec_payload(unlabelled).element().is_none());

            let labelled_ints = praxis_vec_new(ctx, &scalars::INT as *const _);
            assert!(
                praxis_struct_eq(ctx, unlabelled, labelled_ints) != 0,
                "an empty Map's values are an empty Vec[Int]"
            );
            assert!(
                praxis_struct_eq(ctx, labelled_ints, unlabelled) != 0,
                "and equality is symmetric"
            );

            // A non-empty collection is still not an empty one: the length
            // check behind `same_element` is what answers, and it must.
            praxis_vec_push(ctx, labelled_ints, praxis_alloc_int(ctx, 1));
            assert!(praxis_struct_eq(ctx, unlabelled, labelled_ints) == 0);
        }
        // SAFETY: the context was leaked by `wired_ctx` and is unused after this.
        unsafe { drop_ctx(ctx) };
    }

    // --- the manifest's fault column is checked against the code -----------

    /// Every function defined in this file, as `(name, body)`.
    ///
    /// Line-based on purpose: a definition is a line whose first tokens are one
    /// of Rust's `fn` spellings, and its body runs to the line where the brace
    /// depth opened by that definition returns to zero. Anything cleverer would
    /// be a Rust parser, and anything looser — matching `fn` anywhere — reads
    /// the word out of doc comments and glues unrelated bodies together.
    fn functions_in_this_file() -> Vec<(String, String)> {
        functions_in(include_str!("abi.rs"))
    }

    /// The code of one line, with any `//` comment removed.
    ///
    /// The sweep reads what the **compiler** sees, not what a reader wrote
    /// beside it. The fixed point matches `set_fault(` as a plain substring, so
    /// a comment inside a wrapper naming the helper would otherwise classify
    /// that wrapper as faulting. A sweep a comment can fool is a sweep that gets
    /// edited around rather than satisfied, which is the failure mode the whole
    /// invariant exists to prevent.
    ///
    /// A `//` inside a string or `char` literal is **not** a comment — `"//"`
    /// and `'/'` both occur in this file — so the scan tracks which it is in.
    /// It is not a Rust lexer: a raw string's hashes and a block comment are
    /// not modelled, because neither appears in a function body here and a
    /// half-lexer that claimed to be one would be worse than a stated
    /// limitation. It errs toward keeping code, never toward dropping it.
    fn code_only(line: &str) -> &str {
        let bytes = line.as_bytes();
        let (mut in_str, mut in_char, mut escaped) = (false, false, false);
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if escaped {
                escaped = false;
            } else if c == b'\\' && (in_str || in_char) {
                escaped = true;
            } else if in_str {
                in_str = c != b'"';
            } else if in_char {
                in_char = c != b'\'';
            } else if c == b'"' {
                in_str = true;
            } else if c == b'\'' {
                // A lifetime (`'a`) is not a `char` literal; a `char` literal's
                // closing quote is at most three bytes away (`'\\n'`, `'\\''`).
                in_char = bytes[i + 1..].iter().take(4).any(|b| *b == b'\'');
            } else if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
                return &line[..i];
            }
            i += 1;
        }
        line
    }

    /// Every function defined in `src`, as `(name, code)` — the code only, with
    /// comments stripped by [`code_only`]. Braces are counted on the code too,
    /// so a comment holding an unbalanced brace cannot end a body early or run
    /// two bodies together.
    fn functions_in(src: &str) -> Vec<(String, String)> {
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
        for raw in src.lines() {
            let line = code_only(raw);
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

    /// A wrapper that can raise a fault says so in the manifest.
    ///
    /// A row declared `Allocates` makes `RuntimeSymbol::faults()` answer
    /// `false`, so no `CheckFault` follows the call. If such a wrapper can reach
    /// `set_fault` the consequence is not cosmetic: the operation is silently
    /// abandoned, the wrapper answers the Unit sentinel, and the fault is
    /// observed by some later unrelated check — at the wrong source location,
    /// after the program has computed and possibly printed an answer.
    ///
    /// A hand-corrected row drifts again, so this is the invariant instead: the
    /// file is read at compile time, each `praxis_*` wrapper's body is walked,
    /// and any body that can reach `set_fault` — directly or through a helper
    /// defined here, transitively — must belong to a symbol whose row says
    /// `faults()`.
    ///
    /// One direction only. A row may declare a fault the reader cannot see: the
    /// arithmetic wrappers are generated by `checked_int_binop!` and have no
    /// textual definition at all, and a future wrapper may fault through a
    /// helper in another module. Those are false negatives — this test is weaker
    /// than the truth, never stricter — and the direction it does check is the
    /// one that produces wrong answers.
    /// The fixed point of "can reach `set_fault`" over `defs`: a function
    /// faults if it calls `set_fault`, or calls something that does.
    fn faulting_functions(defs: &[(String, String)]) -> std::collections::HashSet<String> {
        let mut faulting: std::collections::HashSet<String> =
            ["set_fault".to_string()].into_iter().collect();
        loop {
            let mut grew = false;
            for (name, body) in defs {
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
        faulting
    }

    #[test]
    fn a_wrapper_that_can_raise_a_fault_declares_that_it_faults() {
        let defs = functions_in_this_file();
        let faulting = faulting_functions(&defs);

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
        // These three reach `set_fault` only through a helper, so they are the
        // shape the transitive scan has to be able to see.
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
        // **`InvalidText` lives at exactly one site (ADR-111).**
        // `praxis_alloc_text` trusts its caller; the one caller holding raw host
        // bytes raises the fault instead. Asserting the destination is visible
        // to the scan is what distinguishes a relocated fault from a deleted
        // one — without it, deleting the validation outright would leave this
        // test just as green.
        assert!(
            faulting.contains("praxis_get_input"),
            "`praxis_get_input` validates the host's input and raises \
             `InvalidText` itself (ADR-111); a scan that cannot see that cannot \
             tell a relocated fault from a deleted one"
        );
        assert!(
            !faulting.contains("praxis_alloc_text"),
            "`praxis_alloc_text` reaches `set_fault` again. Its row is \
             `Effect::Allocates`, so nothing observes the fault — a violated \
             UTF-8 precondition aborts through `abi_guard!` instead (ADR-111)"
        );
    }
    /// **The sweep above reads code, not prose.** Its classification is a
    /// substring match, so without [`code_only`] a *comment* inside a wrapper
    /// naming the helper would classify that wrapper as faulting and fail the
    /// invariant for a wrapper that cannot fault. A sweep a comment can fool is
    /// a sweep that gets edited around rather than satisfied.
    ///
    /// Synthetic source, because the real file must not contain the shape: the
    /// point is that it may, safely.
    #[test]
    fn the_manifest_sweep_reads_code_and_not_comments() {
        let src = r#"
pub unsafe extern "C" fn praxis_pretend_pure(ctx: *mut RuntimeContext) -> GcRef {
    // It used to call set_fault(ctx, RaisedFault::TYPE_MISMATCH) here, and a
    // later edit removed the only path that could. Prose, not code. }
    let sep = "//";
    let slash = '/';
    let _ = (sep, slash);
    unit_sentinel(ctx)
}

pub unsafe extern "C" fn praxis_pretend_faulting(ctx: *mut RuntimeContext) -> GcRef {
    set_fault(ctx, RaisedFault::TYPE_MISMATCH);
    unit_sentinel(ctx)
}
"#;
        let defs = functions_in(src);
        let names: Vec<&str> = defs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["praxis_pretend_pure", "praxis_pretend_faulting"]);
        // The unbalanced `}` in that comment must not close the body early:
        // braces are counted on the code too, so the whole function is read.
        assert!(
            defs[0].1.contains("unit_sentinel(ctx)"),
            "a brace inside a comment ended the body early: {:?}",
            defs[0].1
        );

        let faulting = faulting_functions(&defs);
        assert!(
            !faulting.contains("praxis_pretend_pure"),
            "a comment naming `set_fault` is not a call to it"
        );
        assert!(
            faulting.contains("praxis_pretend_faulting"),
            "and a real call still is — stripping comments must not blind the sweep"
        );
    }

    /// [`code_only`]'s own contract, both directions.
    #[test]
    fn code_only_keeps_a_slash_inside_a_literal() {
        assert_eq!(code_only("let x = 1; // two"), "let x = 1; ");
        assert_eq!(code_only(r#"let s = "a//b";"#), r#"let s = "a//b";"#);
        assert_eq!(code_only(r"let c = '/'; // gone"), r"let c = '/'; ");
        assert_eq!(
            code_only(r#"let e = "\"//"; // gone"#),
            r#"let e = "\"//"; "#
        );
        assert_eq!(code_only("    /// a doc comment"), "    ");
        assert_eq!(code_only("no comment here"), "no comment here");
        // A lifetime is not a `char` literal, so the comment after it is still
        // a comment.
        assert_eq!(
            code_only("fn f<'a>(x: &'a str) {} // gone"),
            "fn f<'a>(x: &'a str) {} "
        );
    }

    // --- the panic backstop ------------------------------------------------

    /// Every `#[no_mangle] extern "C"` function in this crate has its body
    /// inside `abi_guard!`.
    ///
    /// Per-wrapper totality is the contract: a wrapper validates its arguments
    /// and reports a bad one as a fault, so the guard never fires. This is the
    /// proof that the contract cannot be violated *silently* — a panic
    /// unwinding out of `extern "C"` into Cranelift frames is undefined
    /// behaviour, and the failure mode of forgetting is a corrupted process at
    /// some unrelated later point rather than a message.
    ///
    /// Read as source text on purpose. The property is "every entry point is
    /// wrapped", which is a property of the *set* of entry points; a test that
    /// called them one by one would be a test of the ones somebody remembered.
    ///
    /// **The file set is discovered, not declared.** A hand-written list of
    /// files would make the guarantee "every entry point in a file somebody
    /// remembered to list", and the `wrappers > 100` floor would still pass on
    /// the files that were listed. So the walk covers **every crate's `src/`**,
    /// not only this one: nothing says a future `#[no_mangle]` has to live
    /// here.
    #[test]
    fn every_no_mangle_wrapper_is_behind_the_panic_guard() {
        /// Every `.rs` file under `dir`, recursively, in a stable order.
        fn rust_sources(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            let entries =
                std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
            let mut entries: Vec<_> = entries.map(|e| e.expect("dir entry").path()).collect();
            entries.sort();
            for path in entries {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if name == "target" || name.starts_with('.') {
                    continue;
                }
                if path.is_dir() {
                    rust_sources(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&path)
                        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
                    out.push((path.display().to_string(), text));
                }
            }
        }

        // `crates/`, from this crate's manifest directory.
        let mut crates_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        crates_dir.pop();
        let mut sources: Vec<(String, String)> = Vec::new();
        rust_sources(&crates_dir, &mut sources);
        // A guard against the walk silently covering nothing — a wrong root
        // would otherwise pass by finding no `#[no_mangle]` at all.
        assert!(
            sources.len() > 50,
            "the walk of {} found only {} Rust files, so it is not reading the workspace",
            crates_dir.display(),
            sources.len()
        );

        let mut wrappers = 0usize;
        let mut unguarded: Vec<String> = Vec::new();
        for (file, source) in &sources {
            let lines: Vec<&str> = source.lines().collect();
            for (n, line) in lines.iter().enumerate() {
                if line.trim() != "#[no_mangle]" {
                    continue;
                }
                wrappers += 1;
                // Walk to the line that opens the body, then require the very
                // next non-blank line to be the guard.
                let mut k = n + 1;
                while k < lines.len() && !lines[k].trim_end().ends_with('{') {
                    k += 1;
                }
                // Inclusive of `k`: a one-line signature puts `fn name(` on the
                // very line that opens the body, so an exclusive range would
                // report `<unnamed>` in the message that tells someone which
                // wrapper they forgot.
                let name = lines[n..=k.min(lines.len() - 1)]
                    .iter()
                    .find_map(|l| l.split("fn ").nth(1))
                    .and_then(|l| l.split('(').next())
                    .unwrap_or("<unnamed>")
                    .trim()
                    .to_string();
                let opens_guard = lines
                    .get(k + 1)
                    .map(|l| l.trim_start().starts_with("abi_guard!("))
                    .unwrap_or(false);
                if !opens_guard {
                    unguarded.push(format!("{file}:{} {name}", n + 1));
                }
            }
        }

        assert!(
            wrappers > 100,
            "the scan found only {wrappers} wrappers, so it is not reading the ABI surface"
        );
        assert!(
            unguarded.is_empty(),
            "these `extern \"C\"` entry points can let a panic unwind into generated frames: {unguarded:#?}"
        );
    }

    /// The guard's own behaviour: a panic inside a wrapper becomes a fault with
    /// a message naming the wrapper, and the wrapper returns its defined dummy.
    ///
    /// `praxis_dbg` is the one wrapper that can be made to panic on demand
    /// without an invalid argument — it formats its value, and a `Text` whose
    /// payload is a live `Unit` is a descriptor/payload pairing no validation
    /// catches. Every *reachable* panic is a bug to fix in the wrapper; this
    /// test is about what happens when one is missed.
    #[test]
    fn a_panic_inside_a_wrapper_becomes_a_fault_and_a_defined_dummy() {
        let value = {
            abi_guard!(
                "praxis_test_panics",
                std::ptr::null_mut::<RuntimeContext>(),
                {
                    #[allow(unreachable_code)]
                    {
                        if std::hint::black_box(false) {
                            panic!("this is the guard under test");
                        }
                        7i64
                    }
                }
            )
        };
        assert_eq!(value, 7, "the guard is transparent when nothing panics");

        // A **faulting** wrapper: its call sites can carry a `CheckFault`, so
        // generated code observes the fault before it looks at the value, and
        // the defined dummy is the right answer. The name has to be a real
        // manifest symbol — see `panic_fault_is_observable`, which is what
        // decides whether the dummy is returned at all.
        let mut runtime = crate::Runtime::new();
        let mut ctx = runtime.context();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let dummy: GcRef = abi_guard!("praxis_run_parser", &mut ctx as *mut RuntimeContext, {
            panic!("a wrapper that forgot to be total");
        });
        std::panic::set_hook(previous);

        assert_eq!(
            runtime.fault(),
            crate::FaultKind::Panic,
            "an escaped panic is a fault, not an unwind into generated code"
        );
        assert!(
            runtime
                .fault_message()
                .is_some_and(|m| m.contains("praxis_run_parser")),
            "the fault names the wrapper it escaped, which a bare kind could not"
        );
        assert_eq!(
            dummy.descriptor().id(),
            crate::scalars::UNIT.id(),
            "the dummy is the Unit sentinel §10.4 already specifies"
        );
    }

    /// **The dummy is only returned where the fault will be seen.**
    ///
    /// Generated code tests the fault slot only where MIR emitted a
    /// `CheckFault`, and **`praxis_mir::verify` is what makes that true of a
    /// non-faulting wrapper** — its `RedundantFaultCheck` rule rejects a check
    /// after an instruction that cannot fault (ADR-088). So for the wrappers the
    /// manifest declares non-faulting there is no check, and returning
    /// `unit_sentinel` there would hand a `Unit` into a slot generated code
    /// believes holds a Record, a Tuple or a closure — a descriptor/payload
    /// confusion introduced by the backstop meant to prevent worse. Those abort
    /// instead.
    ///
    /// This test states the classification. The abort itself cannot be asserted
    /// in-process, which is exactly why the rule has to be a total function of
    /// the manifest rather than a case-by-case judgement.
    #[test]
    fn a_panic_dummy_is_only_returned_where_a_fault_check_can_follow() {
        use praxis_stdlib::abi::RuntimeSymbol;

        let mut pure = 0usize;
        let mut faulting = 0usize;
        for symbol in RuntimeSymbol::ALL.iter().copied() {
            let observable = panic_fault_is_observable(symbol.name());
            assert_eq!(
                observable,
                symbol.faults(),
                "`{}` is declared {:?}; the panic dummy must be returned iff a \
                 fault check can follow it",
                symbol.name(),
                symbol.sig().effect
            );
            if symbol.faults() {
                faulting += 1;
            } else {
                pure += 1;
            }
        }
        assert!(
            pure > 0 && faulting > 0,
            "the manifest must contain both classes for this rule to mean anything \
             ({pure} non-faulting, {faulting} faulting)"
        );

        // Every `#[no_mangle]` wrapper in this crate is manifested, so the only
        // unobservable case left is a name that is not a wrapper at all.
        assert!(
            !panic_fault_is_observable("praxis_not_a_wrapper_at_all"),
            "an unknown name is never treated as observable"
        );
    }

    // ---- Process input (§7.10) ----

    /// A reader that answers nothing. A `fn` and not a closure because
    /// [`crate::input::InputReader`] is a plain `fn` pointer.
    fn no_bytes() -> Vec<u8> {
        Vec::new()
    }

    /// Read the bytes behind a `Text` `GcRef`.
    ///
    /// # Safety
    /// `r` must be a live `Text`.
    unsafe fn text_bytes_of(r: GcRef) -> &'static [u8] {
        // SAFETY: the caller guarantees `r` is a live Text, so its payload is a
        // validly-linked `TextPayload`.
        unsafe { crate::text::text_bytes(r.payload::<crate::text::TextPayload>() as *const _) }
    }

    /// A reader that answers zero bytes has given *empty input*, not no input,
    /// so its answer is installed as `input_source` whatever its length.
    ///
    /// Allocating the buffer only `if !bytes.is_empty()` would leave empty
    /// standard input at the immortal Unit, so `praxis_run_parser`'s §6.3
    /// descriptor guard would fault *before* the parser ran — a `ParseFailed`
    /// with no input span, no `expected` and no `actual`, which is none of the
    /// six fields §7.11 says a mismatch carries. A fault raised before any
    /// buffer exists cannot carry them; the buffer is what makes the diagnostic
    /// possible at all (ADR-087).
    #[test]
    fn a_reader_that_answers_zero_bytes_installs_an_empty_text() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        crate::input::install_input_reader(no_bytes);
        // SAFETY: ctx is wired to rt and live for this call.
        let source = unsafe { praxis_get_input(ctx) };
        assert_eq!(
            source.descriptor().id(),
            crate::text::TEXT.id(),
            "a zero-byte answer is still an input buffer"
        );
        // SAFETY: the assertion above proves `source` is a Text.
        assert!(
            unsafe { text_bytes_of(source) }.is_empty(),
            "and the buffer holds exactly what the reader answered"
        );
        // SAFETY: ctx is wired to rt and live for this call.
        assert_eq!(
            unsafe { (*ctx).input_source }.as_ptr(),
            source.as_ptr(),
            "the buffer is installed, not merely returned — §7.10's later \
             `read`s reuse it"
        );
        // SAFETY: ctx came from `wired_ctx` and is not used again.
        unsafe { drop_ctx(ctx) };
    }

    /// **A mutation companion, not a gate.**
    ///
    /// The cheapest wrong repair is to allocate a `Text` unconditionally in
    /// `praxis_get_input`, which passes the gate above and quietly deletes the
    /// one state the §6.3 descriptor guard exists for. A host that installs
    /// **neither** a buffer nor a reader — every JIT test, every embedder — must
    /// still reach `praxis_run_parser` with the Unit source, because
    /// `adv_read_against_non_text_input_faults_cleanly` in the codegen crate's
    /// `jit.rs` is the probe that a `read` there faults instead of
    /// reinterpreting Unit's payload as a `TextPayload` and segfaulting.
    ///
    /// That is the boundary ADR-087 draws: a reader answering zero bytes is a
    /// program state (empty input); no reader at all is a host state (no input),
    /// and no `praxis run` reaches it.
    #[test]
    fn a_host_that_installs_no_reader_keeps_the_unit_source() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        crate::input::clear_input_reader();
        // SAFETY: ctx is wired to rt and live for these calls.
        let before = unsafe { (*ctx).input_source };
        // SAFETY: as above.
        let source = unsafe { praxis_get_input(ctx) };
        assert_eq!(
            source.as_ptr(),
            before.as_ptr(),
            "with no reader installed there is nothing to call and nothing to \
             install; `input_source` is answered untouched"
        );
        assert_ne!(
            source.descriptor().id(),
            crate::text::TEXT.id(),
            "and it is still the Unit the §6.3 guard is the net under"
        );
        // SAFETY: ctx came from `wired_ctx` and is not used again.
        unsafe { drop_ctx(ctx) };
    }

    /// **The guard must not report a parse that never ran.**
    ///
    /// `praxis_run_parser` returns early for a non-Text `input` (§6.3) —
    /// `run_plan` would otherwise reinterpret the payload as a `TextPayload` —
    /// and that early return must still perform the `clear_parse_detail` every
    /// other entry into the parser performs. Without it, a host reaching the
    /// guard after an earlier mismatch reports *that* mismatch's offset and
    /// expectation for a parse that never started.
    ///
    /// Not reachable end to end: a fault is terminal within one `praxis run`,
    /// so the shape is an embedder calling `main` twice (or the crash debugger's
    /// `restart`). This test pins it at the level where the hazard exists.
    ///
    /// Fabricating a `ParseFail` here instead would be worse than clearing: with
    /// no buffer there is no input span, and an invented `expected` would make
    /// an embedder's host bug read as a parse failure at an offset that does not
    /// exist.
    #[test]
    fn the_non_text_guard_does_not_report_a_previous_parses_failure() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        rt.parse_detail_mut()
            .consider(ParseFail::here(7, "int"), b"0123456789");
        assert!(rt.parse_detail().is_set(), "the seed is in place");
        // SAFETY: ctx is wired to rt; the plan index is never read, because the
        // descriptor guard returns before it.
        unsafe {
            let plan = praxis_alloc_int(ctx, 1);
            let unit = (*ctx).unit_ref;
            let result = praxis_run_parser(ctx, plan, unit);
            assert_eq!(
                result.descriptor().id(),
                crate::scalars::UNIT.id(),
                "the guard answers the sentinel"
            );
        }
        assert!(rt.has_pending_fault());
        assert_eq!(rt.fault(), FaultKind::ParseFailed);
        assert!(
            !rt.parse_detail().is_set(),
            "the §6.3 guard runs no parse, so it has no detail to report — and \
             it must not report the previous parse's"
        );
        // SAFETY: ctx came from `wired_ctx` and is not used again.
        unsafe { drop_ctx(ctx) };
    }
}

#[cfg(test)]
mod growth_charging_tests {
    //! **Every wrapper that can grow a collection's buffer charges the pacer**
    //! (ADR-121). See [`super::charge_growth`] for why.
    //!
    //! The values pushed are all inside `small_int`'s interned range, and that
    //! is the whole design of these tests rather than a convenience: an interned
    //! `Int` is an immortal the allocator never charges for, so the *only* thing
    //! that can move `bytes_since_collect` here is the spine. Push
    //! `UNINTERNED + i` instead and every one of these passes whether or not the
    //! growth is charged, because the elements would be paying for it.

    use super::tests::{drop_ctx, wired_ctx};
    use super::*;
    use crate::Runtime;

    /// Reset the counter, run `body`, and answer what it charged.
    fn charged_by(rt: &Runtime, body: impl FnOnce()) -> usize {
        // A collection zeroes the counter, so take a reading either side and
        // require the run not to have collected; the pushes below are far too
        // few to reach any threshold.
        let before = rt.heap().bytes_since_collect();
        body();
        rt.heap().bytes_since_collect().saturating_sub(before)
    }

    /// Enough pushes that amortized doubling must have reallocated at least
    /// once, whatever the initial capacity is.
    const PUSHES: i64 = 256;

    macro_rules! charges_its_spine {
        ($name:ident, $make:expr, $push:expr) => {
            #[test]
            fn $name() {
                let mut rt = Runtime::new();
                let ctx = wired_ctx(&mut rt);
                // SAFETY: `ctx` is wired to `rt` for the whole test.
                unsafe {
                    let subject = $make(ctx);
                    let charged = charged_by(&rt, || {
                        for i in 0..PUSHES {
                            $push(ctx, subject, i);
                        }
                    });
                    assert!(
                        charged > 0,
                        "growing this collection charged the pacer nothing, so a \
                         program whose memory is this buffer would never collect \
                         (ADR-121); every value pushed is an interned immortal, so \
                         the spine is the only thing that could have charged"
                    );
                    drop_ctx(ctx);
                }
            }
        };
    }

    charges_its_spine!(
        vec_push_charges_its_spine,
        |c| praxis_vec_new(c, &crate::scalars::INT),
        |c, s, i| { praxis_vec_push(c, s, praxis_alloc_int(c, i)) }
    );
    charges_its_spine!(
        deque_push_back_charges_its_spine,
        |c| praxis_deque_new(c, &crate::scalars::INT),
        |c, s, i| praxis_deque_push_back(c, s, praxis_alloc_int(c, i))
    );
    charges_its_spine!(
        deque_push_front_charges_its_spine,
        |c| praxis_deque_new(c, &crate::scalars::INT),
        |c, s, i| praxis_deque_push_front(c, s, praxis_alloc_int(c, i))
    );
    charges_its_spine!(
        map_insert_charges_its_spine,
        |c| praxis_map_new(c, &crate::scalars::INT),
        |c, s, i| praxis_map_insert(c, s, praxis_alloc_int(c, i), praxis_alloc_int(c, i))
    );
    charges_its_spine!(
        set_insert_charges_its_spine,
        |c| praxis_set_new(c, &crate::scalars::INT),
        |c, s, i| praxis_set_insert(c, s, praxis_alloc_int(c, i))
    );
    charges_its_spine!(
        counter_set_charges_its_spine,
        |c| praxis_counter_new(c, &crate::scalars::INT),
        |c, s, i| praxis_counter_set(c, s, praxis_alloc_int(c, i), praxis_alloc_int(c, i))
    );
    charges_its_spine!(
        bitset_insert_charges_its_spine,
        |c| praxis_bitset_new(c),
        |c, s, i| praxis_bitset_insert(c, s, praxis_alloc_int(c, i))
    );
    charges_its_spine!(
        max_heap_push_charges_its_spine,
        |c| praxis_max_heap_new(c, &crate::scalars::INT),
        |c, s, i| praxis_max_heap_push(c, s, praxis_alloc_int(c, i))
    );
    charges_its_spine!(
        min_heap_push_charges_its_spine,
        |c| praxis_min_heap_new(c, &crate::scalars::INT),
        |c, s, i| praxis_min_heap_push(c, s, praxis_alloc_int(c, i))
    );
}
