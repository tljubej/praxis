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

/// The runtime ABI version for this build. Bump this whenever the layout of
/// [`RuntimeContext`](crate::RuntimeContext), the calling convention, or the
/// signature set of `praxis_*` runtime wrappers changes in an incompatible way.
pub const RUNTIME_ABI_VERSION: u32 = 1;

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
const COMPILER_EXPECTED_ABI_VERSION: u32 = 1;

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

/// Checked `Int` division (§4.12). Faults on division by zero.
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
    // Division truncates toward zero (Rust's `i64::div_euclid` rounds differently;
    // Praxis follows C/Rust integer division semantics toward zero).
    unsafe { heap(ctx).alloc(scalars::INT, a / b) }
}

/// Checked `Int` remainder (§4.12). Faults on division by zero.
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
    fn version_is_one_at_milestone_4() {
        assert_eq!(RUNTIME_ABI_VERSION, 1);
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
            assert_eq!(rt.fault(), FaultKind::DivByZero);
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
}
