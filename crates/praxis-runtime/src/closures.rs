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
//! The closure's synthetic function takes the captured environment values as
//! trailing parameters (after the explicit params), so calling a closure is a
//! direct native call through `fn_ptr` with the env spread into the argument
//! list. This avoids a separate env-struct indirection at the call site.

use std::fmt;

use crate::descriptor::{Tracer, TypeDescriptor, TypeId};
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
pub const CLOSURE: &TypeDescriptor = &TypeDescriptor {
    id: TypeId(11),
    name: "Closure",
    size: std::mem::size_of::<ClosurePayload>(),
    align: std::mem::align_of::<ClosurePayload>(),
    trace: closure_trace,
    drop_value: closure_drop,
    format: closure_format,
    equals: None,
    hash: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closure_descriptor_reports_capabilities() {
        // Closures are never equatable/hashable (§5.5).
        assert!(!CLOSURE.is_equatable());
        assert!(!CLOSURE.is_hashable());
        assert_eq!(CLOSURE.name, "Closure");
        assert_eq!(CLOSURE.id, TypeId(11));
    }
}
