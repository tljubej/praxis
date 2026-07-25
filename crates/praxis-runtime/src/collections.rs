//! The `Vec[T]` collection descriptor (§11.2, ADR-013).
//!
//! §11.2 maps `Vec[T]` to a Rust `Vec<GcRef>`. The static element type `T` is
//! enforced by the compiler and **recorded in the collection object's payload**
//! (§11.2: "recorded in the collection object's type descriptor"). The payload
//! therefore carries an element descriptor alongside the items, so `trace`,
//! `format`, and `equals` dispatch element-wise without a type switch.
//!
//! This is the composite type that proves nested `GcRef` tracing (ADR-013):
//! `trace` forwards every element to the tracer.
//!
//! **M5 change:** `items` is now a `Vec<GcRef>` (was `Box<[GcRef]>` in M3) so
//! `push` mutates the vector *in place* — matching §4.2's "a `let` binding may
//! still point to a mutable object" and §11.1's `push -> Unit` (the receiver is
//! mutated, no new reference returned). The `Vec`'s backing storage may
//! reallocate internally, but the `VecPayload` object itself stays at the same
//! GC address (non-moving collector, ADR-011), so existing `GcRef`s remain
//! valid. Per §11.5, runtime wrappers never expose an interior pointer to the
//! `Vec`'s backing buffer across a capacity-mutating op; they reload from the
//! payload each call.

use std::fmt;

use crate::descriptor::{Tracer, TypeDescriptor};
use crate::GcRef;
use crate::{DynamicHasher, TypeId};

/// The `Vec[T]` payload: the element descriptor plus the growable items.
///
/// `items` is a `Vec<GcRef>` so `push` can grow it in place (§11.1). Both fields
/// are `Drop`, so [`VEC`]`'s `drop_value` releases them on sweep (§12.5).
#[repr(C)]
pub struct VecPayload {
    /// The descriptor for every element in `items`. Set at construction; all
    /// elements must share it. Read by `trace`/`format`/`equals` to dispatch
    /// without a scattered type switch (§11.4).
    pub element_descriptor: *const TypeDescriptor,
    /// The elements, in order. A `Vec` (not `Box<[T]>`) so `push` mutates in
    /// place.
    pub items: Vec<GcRef>,
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
    // SAFETY: caller guarantees both pointers point at initialized VecPayload`s
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

// ===========================================================================
// Deque[T] (M8-WS2, §6.1). A double-ended queue backed by Rust's `VecDeque`.
// Mirrors `VecPayload` exactly (element descriptor + growable items) so trace/
// format/equals/hash dispatch identically; only the backing store and the
// front/back method surface differ.
// ===========================================================================

use std::collections::VecDeque;

/// The `Deque[T]` payload: the element descriptor plus a growable `VecDeque`.
/// `VecDeque` (not `Vec`) so `push_front`/`pop_front` are O(1) amortized.
/// Both fields are `Drop`, so [`DEQUE`]'s `drop_value` releases them on sweep.
#[repr(C)]
pub struct DequePayload {
    /// The descriptor for every element (homogeneous, like `VecPayload`).
    pub element_descriptor: *const TypeDescriptor,
    /// The elements. A `VecDeque` so both ends are cheap to mutate.
    pub items: VecDeque<GcRef>,
}

unsafe fn deque_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized DequePayload.
    let p = unsafe { &*(payload as *const DequePayload) };
    for item in p.items.iter() {
        tracer.trace(*item);
    }
}

unsafe fn deque_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized DequePayload.
    unsafe { std::ptr::drop_in_place(payload as *mut DequePayload) };
}

unsafe fn deque_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized DequePayload.
    let p = unsafe { &*(payload as *const DequePayload) };
    let elem_desc = unsafe { &*p.element_descriptor };
    let _ = out.write_str("[");
    for (i, item) in p.items.iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        let elem_payload = item.payload::<u8>() as *const u8;
        (elem_desc.format)(elem_payload, out);
    }
    let _ = out.write_str("]");
}

unsafe fn deque_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at initialized DequePayloads.
    let pa = unsafe { &*(a as *const DequePayload) };
    let pb = unsafe { &*(b as *const DequePayload) };
    if pa.items.len() != pb.items.len() {
        return false;
    }
    let Some(eq) = (unsafe { &*pa.element_descriptor }).equals else {
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

unsafe fn deque_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an initialized DequePayload.
    let p = unsafe { &*(payload as *const DequePayload) };
    let Some(hash_elem) = (unsafe { &*p.element_descriptor }).hash else {
        return;
    };
    hasher.write_bytes(&(p.items.len() as u64).to_le_bytes());
    for item in p.items.iter() {
        let elem_payload = item.payload::<u8>() as *const u8;
        hash_elem(elem_payload, hasher);
    }
}

/// Descriptor for the `Deque[T]` collection (§6.1). The per-instance element
/// type lives in the payload, so a single descriptor serves all `Deque[T]`.
pub const DEQUE: &TypeDescriptor = &TypeDescriptor {
    id: TypeId(13),
    name: "Deque",
    size: std::mem::size_of::<DequePayload>(),
    align: std::mem::align_of::<DequePayload>(),
    trace: deque_trace,
    drop_value: deque_drop,
    format: deque_format,
    equals: Some(deque_equals),
    hash: Some(deque_hash),
};

// ===========================================================================
// Grid[T] (M6, §7.5 `grid`, §7.8 type derivation). M6 ships a minimal runtime
// type — row-major storage with a known width — so the synthesized type is the
// spec-faithful `Grid[T]`. Grid methods (neighbors, indexing, etc.) are M8.
// ===========================================================================

/// The `Grid[T]` payload: a row-major sequence of `GcRef`s plus the fixed
/// column count (width). `items.len() == width * height`. Mirrors `VecPayload`
/// but carries rectangular shape so M8 methods and indexing are cheap.
#[repr(C)]
pub struct GridPayload {
    /// The descriptor for every cell in `items` (homogeneous, like Vec).
    pub element_descriptor: *const TypeDescriptor,
    /// Row-major cells: `items[row * width + col]`.
    pub items: Vec<GcRef>,
    /// The number of columns (all rows share this width).
    pub width: usize,
}

unsafe fn grid_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized GridPayload.
    let p = unsafe { &*(payload as *const GridPayload) };
    for cell in p.items.iter() {
        tracer.trace(*cell);
    }
}

unsafe fn grid_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized GridPayload.
    unsafe { std::ptr::drop_in_place(payload as *mut GridPayload) };
}

unsafe fn grid_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized GridPayload.
    let p = unsafe { &*(payload as *const GridPayload) };
    let elem_desc = unsafe { &*p.element_descriptor };
    let _ = out.write_str("[");
    for (i, cell) in p.items.iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        let elem_payload = cell.payload::<u8>() as *const u8;
        (elem_desc.format)(elem_payload, out);
    }
    let _ = out.write_str("]");
}

/// Descriptor for the `Grid[T]` collection (M6, §7.8). Element-wise equality and
/// hashing are deferred to M8 (grid-as-map-key is an M8 concern); the descriptor
/// is marked non-equatable / non-hashable for now.
pub const GRID: &TypeDescriptor = &TypeDescriptor {
    id: TypeId(7),
    name: "Grid",
    size: std::mem::size_of::<GridPayload>(),
    align: std::mem::align_of::<GridPayload>(),
    trace: grid_trace,
    drop_value: grid_drop,
    format: grid_format,
    equals: None,
    hash: None,
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
