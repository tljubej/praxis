//! The `Closure` value descriptor (§4.10, M7-WS7).
//!
//! A closure value carries a function pointer (the JIT'd entry point of the
//! closure's synthetic function) plus a captured environment (the values it
//! closes over, each a `GcRef`). A single `CLOSURE` descriptor serves every
//! closure because the per-closure knowledge (arity, env size) lives in the
//! payload.
//!
//! Per §5.5, function and closure values are **never equatable or hashable** —
//! they have no structural identity. The `equals`/`hash` callbacks are `None`.
//!
//! ## Calling convention (Approach B)
//!
//! The closure's synthetic function takes the closure value itself as a hidden
//! first explicit parameter (after the implicit `ctx`): its MIR signature is
//! `fn(ctx, closure_self, params...)`. At entry, a prologue loads each captured
//! value via [`praxis_closure_capture`](crate::abi::praxis_closure_capture) and
//! binds it to a local. Calling a closure value is an indirect call: the call
//! site reads `fn_ptr` via
//! [`praxis_closure_fn_ptr`](crate::abi::praxis_closure_fn_ptr), then emits a
//! native `call_indirect` passing `[ctx, closure, args...]`. Keeping the
//! closure value intact at the call site (rather than spreading the env into
//! trailing params) makes the indirect call uniform per-arity and keeps the
//! closure self-contained for fault snapshots and future borrow/move semantics.

use std::fmt;

use crate::descriptor::{BuiltinTypeId, Tracer, TypeDescriptor};
use crate::GcRef;

/// The runtime payload of a closure value: the function pointer plus the
/// captured environment values (one `GcRef` per captured variable, in capture
/// order established by the HIR capture analysis).
#[repr(C)]
pub struct ClosurePayload {
    /// The JIT'd entry-point function pointer. The calling convention matches
    /// every other Praxis function: `fn(ctx: i64, params..., env...) -> i64`.
    pub fn_ptr: *const u8,
    /// The captured values, in the order the capture analysis recorded them.
    /// Each is a `GcRef` into the GC heap.
    pub env: Vec<GcRef>,
}

unsafe fn closure_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized ClosurePayload.
    let p = unsafe { &*(payload as *const ClosurePayload) };
    for captured in p.env.iter() {
        tracer.trace(*captured);
    }
}

unsafe fn closure_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized ClosurePayload.
    // `drop_in_place` frees the env Vec; the fn_ptr is not owned (it's JIT code).
    unsafe { std::ptr::drop_in_place(payload as *mut ClosurePayload) };
}

unsafe fn closure_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized ClosurePayload.
    let p = unsafe { &*(payload as *const ClosurePayload) };
    // Closures have no source-level printable form; render as `<closure>` with
    // the capture count for debugging.
    let _ = write!(out, "<closure:{}>", p.env.len());
}

/// Descriptor for the `Closure` value type (M7, §4.10). Closures are never
/// equatable or hashable (§5.5: function values have no structural identity).
pub static CLOSURE: TypeDescriptor = TypeDescriptor::builtin::<ClosurePayload>(
    BuiltinTypeId::Closure,
    "Closure",
    closure_trace,
    closure_drop,
    closure_format,
    None,
    None,
    // Ordering: see the ordering ADR; no built-in declares `compare` yet.
    None,
)
.with_owned_bytes(closure_owned_bytes);

/// The heap bytes a closure owns beyond its payload, for GC pacing (RT-04).
/// `capacity`, not `len`: the buffer's real footprint is what the collector is
/// paced against.
///
/// # Safety
/// `payload` must point at an initialized `ClosurePayload`.
unsafe fn closure_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized ClosurePayload.
    let p = unsafe { &*(payload as *const ClosurePayload) };
    p.env.capacity() * std::mem::size_of::<GcRef>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closure_descriptor_reports_capabilities() {
        // Closures are never equatable/hashable (§5.5).
        assert!(!CLOSURE.is_equatable());
        assert!(!CLOSURE.is_hashable());
        assert_eq!(CLOSURE.name, "Closure");
        assert_eq!(CLOSURE.as_builtin(), Some(BuiltinTypeId::Closure));
    }
}
