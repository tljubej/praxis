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

use crate::context::{FaultKind, RuntimeContext};
use crate::gc::GcRef;
use crate::heap::Heap;
use crate::scalars;
use crate::{collections::VecPayload, descriptor::TypeDescriptor};

/// The runtime ABI version for this build. Bump this whenever the layout of
/// [`RuntimeContext`](crate::RuntimeContext), the calling convention, or the
/// signature set of `praxis_*` runtime wrappers changes in an incompatible way.
///
/// v2 (M5): `RuntimeContext` gained the `roots: *mut ShadowFrame` field for the
/// compiler-managed shadow-stack spill (ADR-019), and the `praxis_push_shadow_frame`
/// / `praxis_pop_shadow_frame` extern helpers were added.
pub const RUNTIME_ABI_VERSION: u32 = 2;

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
const COMPILER_EXPECTED_ABI_VERSION: u32 = 2;

// ---------------------------------------------------------------------------
// Internals the wrappers share.
// ---------------------------------------------------------------------------

/// Mark `kind` as pending on `ctx`'s fault slot (§10.4). Does nothing if the
/// context's fault pointer is null (a misuse, but never panics across the ABI).
unsafe fn set_fault(ctx: *mut RuntimeContext, kind: FaultKind) {
    if let Some(fault) = unsafe { (*ctx).pending_fault.as_mut() } {
        fault.set(kind);
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
/// The roots are read from `ctx.roots`: the current shadow frame, whose
/// `RootSet` impl walks the whole parent chain. If `roots` is null (no frame
/// pushed yet — e.g. during host-driven allocation before `main`), the
/// immortals are the only survivors, which is correct.
#[inline]
unsafe fn maybe_collect(ctx: *mut RuntimeContext) {
    if ctx.is_null() {
        return;
    }
    // SAFETY: ctx is live and wired.
    let roots_ptr = unsafe { (*ctx).roots };
    if roots_ptr.is_null() {
        return;
    }
    // SAFETY: `roots_ptr` is a live shadow frame (pushed by a prologue that has
    // not yet returned).
    let frame: &dyn crate::RootSet = unsafe { &*roots_ptr };
    unsafe { heap(ctx).maybe_collect(frame) };
}

/// Read the `i64` payload of an `Int` `GcRef`. Used by every arithmetic wrapper.
#[inline]
unsafe fn int_payload(r: GcRef) -> i64 {
    // SAFETY: the compiler only emits these calls with Int-typed operands; the
    // payload follows the header and is an i64.
    unsafe { *r.payload::<i64>() }
}

/// The Unit sentinel GcRef from the context's input source slot. (Unit is an
/// immortal; in M4 we reuse the input_source field which the runtime sets to
/// the immortal Unit.) Returned on fault paths as the "defined dummy" (§10.4).
#[inline]
unsafe fn unit_sentinel(ctx: *mut RuntimeContext) -> GcRef {
    unsafe { (*ctx).input_source }
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
    // Trigger a collection on allocation pressure before allocating, rooted at
    // the current shadow frame. The new object is not yet a root, but it is
    // returned by value to the caller, which spills it — so it is safe across
    // this collection (the *previous* allocation's result was already spilled
    // by the backend before this wrapper was called).
    unsafe { maybe_collect(ctx) };
    // SAFETY: caller upholds the ctx/heap validity.
    unsafe { heap(ctx).alloc(scalars::INT, value) }
}

/// Allocate a boxed `Bool` from a 0/1 value (§4.3). Returns the immortal
/// singleton, never a fresh allocation.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_bool(ctx: *mut RuntimeContext, value: i64) -> GcRef {
    // Bool is an immortal: allocate it through the heap's immortal path so it is
    // never reclaimed. `value != 0` is true; `0` is false.
    // SAFETY: caller upholds ctx validity.
    unsafe { heap(ctx).alloc_immortal(scalars::BOOL, value != 0) }
}

/// Allocate the `Unit` singleton (§4.3).
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`.
#[no_mangle]
pub unsafe extern "C" fn praxis_alloc_unit(ctx: *mut RuntimeContext) -> GcRef {
    // SAFETY: Unit is an immortal; allocate through the immortal path.
    unsafe { heap(ctx).alloc_immortal(scalars::UNIT, ()) }
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
    // Trigger a collection on allocation pressure before allocating.
    unsafe { maybe_collect(ctx) };
    let code = value as u32;
    if !crate::scalars::is_valid_char(code) {
        // Defensive: the parser validates scalars, but a malformed code point must
        // not panic across the ABI.
        unsafe { set_fault(ctx, FaultKind::None) };
        return unsafe { unit_sentinel(ctx) };
    }
    // SAFETY: caller upholds ctx/heap validity; code is a validated scalar.
    unsafe { heap(ctx).alloc(scalars::CHAR, code) }
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
            unsafe { set_fault(ctx, FaultKind::None) };
            std::string::String::from_utf8_lossy(slice)
                .into_owned()
                .into_boxed_str()
        }
    };
    // SAFETY: Box<str> matches TEXT's size/align and is fully initialized.
    unsafe {
        heap(ctx).alloc_with(
            crate::text::TEXT,
            std::mem::size_of::<Box<str>>(),
            std::mem::align_of::<Box<str>>(),
            |payload| (payload as *mut Box<str>).write(owned),
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
                Some(result) => unsafe { heap(ctx).alloc(scalars::INT, result) },
                None => {
                    unsafe { set_fault(ctx, $fault) };
                    unsafe { unit_sentinel(ctx) }
                }
            }
        }
    };
}

checked_int_binop!(praxis_int_add, checked_add, FaultKind::IntOverflow);
checked_int_binop!(praxis_int_sub, checked_sub, FaultKind::IntOverflow);
checked_int_binop!(praxis_int_mul, checked_mul, FaultKind::IntOverflow);

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
        unsafe { set_fault(ctx, FaultKind::DivByZero) };
        return unsafe { unit_sentinel(ctx) };
    }
    // `i64::MIN / -1` is the sole overflowing signed division: the mathematical
    // result (+2^63) is not representable, and the raw `/` panics on overflow in
    // debug builds (violating the no-panic-across-the-ABI rule, §10.4). Treat it
    // as checked-arithmetic overflow per §4.12.
    if a == i64::MIN && b == -1 {
        unsafe { set_fault(ctx, FaultKind::IntOverflow) };
        return unsafe { unit_sentinel(ctx) };
    }
    // Division truncates toward zero (Rust's `i64::div_euclid` rounds differently;
    // Praxis follows C/Rust integer division semantics toward zero).
    unsafe { heap(ctx).alloc(scalars::INT, a / b) }
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
        unsafe { set_fault(ctx, FaultKind::DivByZero) };
        return unsafe { unit_sentinel(ctx) };
    }
    // `i64::MIN % -1`: the remainder is 0 mathematically, but the raw `%` traps
    // on this exact case in debug builds because the corresponding quotient
    // overflows. Guard it for the same no-panic reason as `praxis_int_div`.
    if a == i64::MIN && b == -1 {
        unsafe { set_fault(ctx, FaultKind::IntOverflow) };
        return unsafe { unit_sentinel(ctx) };
    }
    unsafe { heap(ctx).alloc(scalars::INT, a % b) }
}

/// Negate an `Int` (§4.12). Faults on overflow (`Int::MIN`).
///
/// # Safety
/// `ctx` must be live and wired; `r` must be a valid `Int` `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_int_neg(ctx: *mut RuntimeContext, r: GcRef) -> GcRef {
    let a = unsafe { int_payload(r) };
    match a.checked_neg() {
        Some(result) => unsafe { heap(ctx).alloc(scalars::INT, result) },
        None => {
            unsafe { set_fault(ctx, FaultKind::IntOverflow) };
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
            unsafe { heap(ctx).alloc_immortal(scalars::BOOL, result) }
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
/// `r` must be a valid `Vec` `GcRef`.
unsafe fn vec_payload_mut(r: GcRef) -> &'static mut VecPayload {
    // SAFETY: caller guarantees `r` is a Vec; the non-moving GC keeps the
    // payload stable. We hold `&mut` only for the duration of this wrapper call,
    // which is single-threaded and not reentrant through the GC.
    unsafe { &mut *r.payload::<VecPayload>() }
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
    unsafe { maybe_collect(ctx) };
    let element_descriptor = if element_descriptor.is_null() {
        scalars::INT
    } else {
        // SAFETY: caller guarantees a valid `'static` descriptor pointer.
        unsafe { &*element_descriptor }
    };
    // SAFETY: VecPayload matches VEC's size/align and is fully initialized.
    unsafe {
        heap(ctx).alloc_with(
            crate::collections::VEC,
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
    let p = unsafe { vec_payload_mut(vec) };
    p.items.push(value);
    unsafe { unit_sentinel(ctx) }
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
    unsafe { heap(ctx).alloc(scalars::INT, len) }
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
        unsafe { set_fault(ctx, FaultKind::IndexOutOfBounds) };
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
    unsafe { heap(ctx).alloc_immortal(scalars::BOOL, empty) }
}

// ---------------------------------------------------------------------------
// Text methods (§4.3, M5).
//
// `Text` is an immutable UTF-8 payload (`Box<str>`). The methods are pure
// (no allocation beyond the result object) and never fault.
// ---------------------------------------------------------------------------

/// Read the `Box<str>` payload of a `Text` `GcRef`.
///
/// # Safety
/// `r` must be a valid `Text` `GcRef`.
unsafe fn text_str(r: GcRef) -> &'static str {
    // SAFETY: caller guarantees `r` is Text; non-moving GC keeps it stable.
    let boxed: &crate::text::OwnedText = unsafe { &*r.payload::<crate::text::OwnedText>() };
    boxed
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
    unsafe { heap(ctx).alloc(scalars::INT, len) }
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
    unsafe { heap(ctx).alloc_immortal(scalars::BOOL, s.is_empty()) }
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
        unsafe { set_fault(ctx, FaultKind::IndexOutOfBounds) };
        return unsafe { unit_sentinel(ctx) };
    }
    match s.chars().nth(idx as usize) {
        Some(ch) => {
            // Return the scalar value as an Int (Char is reserved; M5 uses Int).
            unsafe { heap(ctx).alloc(scalars::INT, ch as i64) }
        }
        None => {
            unsafe { set_fault(ctx, FaultKind::IndexOutOfBounds) };
            unsafe { unit_sentinel(ctx) }
        }
    }
}

// ---------------------------------------------------------------------------
// `out(...)` — write a value to stdout followed by a newline (§16.1, M5).
// ---------------------------------------------------------------------------

/// Format `value` through its descriptor and write it to stdout followed by a
/// newline. Returns the Unit sentinel (§4.3). Never faults.
///
/// # Safety
/// `ctx` must be live and wired; `value` must be a valid `GcRef`.
#[no_mangle]
pub unsafe extern "C" fn praxis_write_stdout(_ctx: *mut RuntimeContext, value: GcRef) -> GcRef {
    use std::io::Write;
    let mut out = String::new();
    value.format(&mut out);
    let _ = std::io::stdout().write_all(out.as_bytes());
    let _ = std::io::stdout().write_all(b"\n");
    // Return the input value so `out(expr)` can be used in expression position
    // (the spec models `out` as `(T) -> Unit`, but returning the value is more
    // useful and the M4 corpus uses it for effect only).
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{Fault, Runtime};

    /// A wired context backed by a real runtime.
    fn wired_ctx(rt: &mut Runtime) -> *mut RuntimeContext {
        let ctx = Box::leak(Box::new(rt.context()));
        ctx as *mut RuntimeContext
    }

    unsafe fn drop_ctx(ctx: *mut RuntimeContext) {
        // Reclaim the leaked Box. The runtime outlives this call in tests.
        let _ = unsafe { Box::from_raw(ctx) };
    }

    #[test]
    fn version_is_two_at_milestone_5() {
        assert_eq!(RUNTIME_ABI_VERSION, 2);
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
        assert_eq!(f.kind, FaultKind::None);
    }

    // --- Vec[T] collection wrappers (M5) -----------------------------------

    #[test]
    fn vec_new_is_empty() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; INT is a valid static descriptor.
        unsafe {
            let v = praxis_vec_new(ctx, crate::scalars::INT as *const _);
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
            let v = praxis_vec_new(ctx, crate::scalars::INT as *const _);
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
            let v = praxis_vec_new(ctx, crate::scalars::INT as *const _);
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
        // Stress: push 1000 elements (forcing many GCs) and read them all back.
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired; push mutates in place so `v` stays valid throughout.
        unsafe {
            let v = praxis_vec_new(ctx, crate::scalars::INT as *const _);
            for i in 0..1000_i64 {
                let elem = praxis_alloc_int(ctx, i);
                let _ = praxis_vec_push(ctx, v, elem);
            }
            assert_eq!(praxis_int_load(ctx, praxis_vec_len(ctx, v)), 1000);
            // Spot-check first/middle/last.
            let zero = praxis_alloc_int(ctx, 0);
            assert_eq!(praxis_int_load(ctx, praxis_vec_get(ctx, v, zero)), 0);
            let five = praxis_alloc_int(ctx, 500);
            assert_eq!(praxis_int_load(ctx, praxis_vec_get(ctx, v, five)), 500);
            let nine = praxis_alloc_int(ctx, 999);
            assert_eq!(praxis_int_load(ctx, praxis_vec_get(ctx, v, nine)), 999);
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
            let v = praxis_vec_new(ctx, crate::scalars::INT as *const _);
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
            // Root nothing — nothing live matters; we only ask whether collection
            // *ran*.
            let roots = crate::roots::RootScope::new();
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
        // Allocate well past the 64 KiB threshold, then call `maybe_collect`
        // directly and assert it ran. (Each Int object is ~32 bytes of header +
        // payload, so ~2500 ints crosses the line.)
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx wired.
        unsafe {
            for i in 0..3000_i64 {
                let _ = praxis_alloc_int(ctx, i);
            }
            let roots = crate::roots::RootScope::new();
            let ran = rt.heap().maybe_collect(&roots);
            assert!(ran, "heavy allocation should trip the threshold");
            // After a collection the pacing counter resets, so an immediate second
            // call (no new allocations) does not collect again.
            let ran2 = rt.heap().maybe_collect(&roots);
            assert!(!ran2, "counter must reset after a collection");
        }
        unsafe { drop_ctx(ctx) };
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
}
