//! The `Vec[T]` collection descriptor (§11.2, ADR-013).
//!
//! §11.2 maps `Vec[T]` to a Rust `Vec<GcRef>`. The static element type `T` is
//! enforced by the compiler and **recorded in the collection object's payload**
//! (§11.2: "recorded in the collection object's type descriptor"). The payload
//! therefore carries an element descriptor alongside the items, so `trace`,
//! `format`, and `equals` dispatch element-wise without a type switch.
//!
//! This is the composite type that proves nested `GcRef` tracing (ADR-013):
//! `trace` forwards every element to the tracer. The method surface
//! (`push`/`get`/…) is M5; M3 ships only allocation, tracing, dropping,
//! formatting, and equality.

use std::fmt;

use crate::descriptor::{Tracer, TypeDescriptor};
use crate::GcRef;
use crate::{DynamicHasher, TypeId};

/// The `Vec[T]` payload: the element descriptor plus the boxed slice of items.
///
/// `items` is a `Box<[GcRef]>` (not `Vec<GcRef>`) so the payload has a stable,
/// simple ownership story; it is constructed from a `Vec<GcRef>` at allocation
/// time. Both fields are `Drop`, so [`Vec::drop_value`] releases them on sweep
/// (§12.5).
#[repr(C)]
pub struct VecPayload {
    /// The descriptor for every element in `items`. Set at construction; all
    /// elements must share it. Read by `trace`/`format`/`equals` to dispatch
    /// without a scattered type switch (§11.4).
    pub element_descriptor: *const TypeDescriptor,
    /// The elements, in order.
    pub items: Box<[GcRef]>,
}

unsafe fn vec_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized `VecPayload`.
    let p = unsafe { &*(payload as *const VecPayload) };
    for item in p.items.iter() {
        tracer.trace(*item);
    }
}

unsafe fn vec_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized `VecPayload`.
    // `drop_in_place` frees the boxed slice; the element descriptor is a static
    // reference and is not owned.
    unsafe { std::ptr::drop_in_place(payload as *mut VecPayload) };
}

unsafe fn vec_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized `VecPayload`.
    let p = unsafe { &*(payload as *const VecPayload) };
    let elem_desc = unsafe { &*p.element_descriptor };
    let _ = out.write_str("[");
    for (i, item) in p.items.iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        // Route element formatting through the element descriptor (§11.4).
        let elem_payload = item.payload::<u8>() as *const u8;
        (elem_desc.format)(elem_payload, out);
    }
    let _ = out.write_str("]");
}

unsafe fn vec_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at initialized `VecPayload`s
    // with compatible element descriptors.
    let pa = unsafe { &*(a as *const VecPayload) };
    let pb = unsafe { &*(b as *const VecPayload) };
    if pa.items.len() != pb.items.len() {
        return false;
    }
    // Element-wise equality through the element descriptor (§11.4). If the
    // element type is not equatable, the collection is not equatable (§5.5).
    let Some(eq) = unsafe { &*pa.element_descriptor }.equals else {
        return false;
    };
    for (x, y) in pa.items.iter().zip(pb.items.iter()) {
        let xe = x.payload::<u8>() as *const u8;
        let ye = y.payload::<u8>() as *const u8;
        if !eq(xe, ye) {
            return false;
        }
    }
    true
}

unsafe fn vec_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an initialized `VecPayload`.
    let p = unsafe { &*(payload as *const VecPayload) };
    let Some(hash_elem) = unsafe { &*p.element_descriptor }.hash else {
        return;
    };
    // Length first to distinguish prefixes (standard sequence-hash practice).
    hasher.write_bytes(&(p.items.len() as u64).to_le_bytes());
    for item in p.items.iter() {
        let elem_payload = item.payload::<u8>() as *const u8;
        hash_elem(elem_payload, hasher);
    }
}

/// Descriptor for the `Vec[T]` collection (§11.2). The per-instance element
/// type lives in the payload, so a single descriptor serves all `Vec[T]`.
pub const VEC: &TypeDescriptor = &TypeDescriptor {
    id: TypeId(6),
    name: "Vec",
    size: std::mem::size_of::<VecPayload>(),
    align: std::mem::align_of::<VecPayload>(),
    trace: vec_trace,
    drop_value: vec_drop,
    format: vec_format,
    equals: Some(vec_equals),
    hash: Some(vec_hash),
};

#[cfg(test)]
mod tests {
    // The Vec descriptor is exercised end-to-end through the Heap in heap.rs
    // (allocation, tracing, collection of nested references). Here we only
    // sanity-check the descriptor is well-formed.
    use super::*;

    #[test]
    fn vec_descriptor_reports_capabilities() {
        assert!(VEC.is_equatable());
        assert!(VEC.is_hashable());
        assert_eq!(VEC.name, "Vec");
    }
}
