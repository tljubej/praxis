//! `MinHeap[T]` and `MaxHeap[T]` (M8-WS4, §6.1, §11.2).
//!
//! Both reuse Rust's `BinaryHeap` behind an opaque GC object. Rust's
//! `BinaryHeap` is a max-heap, so `MaxHeap[T]` maps directly
//! (`BinaryHeap<HeapEntry>`) and `MinHeap[T]` wraps entries in `Reverse`
//! (`BinaryHeap<Reverse<HeapEntry>>`) so the smallest element surfaces first.
//!
//! The element type must be orderable (§5.4 `SupportsOrd`); the capability
//! check rejects non-orderable types at compile time. Ordering compares the
//! element's `i64` payload (sound for the numeric case, the heap's primary use);
//! a general structural `ord` descriptor callback is a follow-up.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fmt;

use crate::descriptor::{Tracer, TypeDescriptor};
use crate::GcRef;
use crate::TypeId;

/// A max-heap entry: the element `GcRef` plus its descriptor. `Ord` compares
/// the element payloads as `i64` (the numeric case). `MaxHeap` uses this
/// directly; `MinHeap` wraps it in `Reverse`.
#[derive(Clone, Copy)]
pub struct HeapEntry {
    /// The element value.
    pub value: GcRef,
    /// The element's descriptor (for trace/equals/hash/format).
    pub descriptor: &'static TypeDescriptor,
}

impl HeapEntry {
    /// The `i64` payload of the element, for numeric ordering.
    fn int_key(&self) -> i64 {
        // SAFETY: heap elements are orderable; the primary supported case is
        // numeric (Int/UInt). For non-numeric orderable types this reads the
        // leading bytes as i64 — a documented limitation until a structural
        // `ord` descriptor callback lands.
        unsafe { *self.value.payload::<i64>() }
    }
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        if self.value == other.value {
            return true;
        }
        match self.descriptor.equals {
            Some(equals) => {
                // SAFETY: both values match the descriptor (homogeneous heap).
                let a = self.value.payload::<u8>() as *const u8;
                let b = other.value.payload::<u8>() as *const u8;
                unsafe { equals(a, b) }
            }
            None => false,
        }
    }
}

impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.int_key().cmp(&other.int_key())
    }
}

impl std::fmt::Debug for HeapEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HeapEntry({})", self.int_key())
    }
}

// --- MaxHeap payload -------------------------------------------------------

/// The `MaxHeap[T]` payload (§11.2): a max-heap of `HeapEntry`.
#[repr(C)]
pub struct MaxHeapPayload {
    /// The descriptor for every element (for trace/equals/hash/format).
    pub element_descriptor: &'static TypeDescriptor,
    /// The elements, in max-heap order (largest surfaces first).
    pub items: BinaryHeap<HeapEntry>,
}

unsafe fn max_heap_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    let p = unsafe { &*(payload as *const MaxHeapPayload) };
    for entry in p.items.iter() {
        tracer.trace(entry.value);
    }
}

unsafe fn max_heap_drop(payload: *mut u8) {
    unsafe { std::ptr::drop_in_place(payload as *mut MaxHeapPayload) };
}

unsafe fn max_heap_format(payload: *const u8, out: &mut dyn fmt::Write) {
    let p = unsafe { &*(payload as *const MaxHeapPayload) };
    let elem_desc = p.element_descriptor;
    let _ = out.write_str("[");
    for (i, entry) in p.items.iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        let ep = entry.value.payload::<u8>() as *const u8;
        (elem_desc.format)(ep, out);
    }
    let _ = out.write_str("]");
}

/// Descriptor for `MaxHeap[T]` (§11.2, TypeId 17).
pub const MAX_HEAP: &TypeDescriptor = &TypeDescriptor {
    id: TypeId(17),
    name: "MaxHeap",
    size: std::mem::size_of::<MaxHeapPayload>(),
    align: std::mem::align_of::<MaxHeapPayload>(),
    trace: max_heap_trace,
    drop_value: max_heap_drop,
    format: max_heap_format,
    // Heaps are not equatable/hashable (contents + order define identity).
    equals: None,
    hash: None,
};

// --- MinHeap payload -------------------------------------------------------

/// The `MinHeap[T]` payload (§11.2): a min-heap via `Reverse<HeapEntry>`.
#[repr(C)]
pub struct MinHeapPayload {
    /// The descriptor for every element (for trace/equals/hash/format).
    pub element_descriptor: &'static TypeDescriptor,
    /// The elements, wrapped in `Reverse` so the smallest surfaces first.
    pub items: BinaryHeap<Reverse<HeapEntry>>,
}

unsafe fn min_heap_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    let p = unsafe { &*(payload as *const MinHeapPayload) };
    for entry in p.items.iter() {
        tracer.trace(entry.0.value);
    }
}

unsafe fn min_heap_drop(payload: *mut u8) {
    unsafe { std::ptr::drop_in_place(payload as *mut MinHeapPayload) };
}

unsafe fn min_heap_format(payload: *const u8, out: &mut dyn fmt::Write) {
    let p = unsafe { &*(payload as *const MinHeapPayload) };
    let elem_desc = p.element_descriptor;
    let _ = out.write_str("[");
    for (i, entry) in p.items.iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        let ep = entry.0.value.payload::<u8>() as *const u8;
        (elem_desc.format)(ep, out);
    }
    let _ = out.write_str("]");
}

/// Descriptor for `MinHeap[T]` (§11.2, TypeId 18).
pub const MIN_HEAP: &TypeDescriptor = &TypeDescriptor {
    id: TypeId(18),
    name: "MinHeap",
    size: std::mem::size_of::<MinHeapPayload>(),
    align: std::mem::align_of::<MinHeapPayload>(),
    trace: min_heap_trace,
    drop_value: min_heap_drop,
    format: min_heap_format,
    equals: None,
    hash: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heap_descriptors_are_non_equatable() {
        assert!(!MAX_HEAP.is_equatable());
        assert!(!MAX_HEAP.is_hashable());
        assert_eq!(MAX_HEAP.name, "MaxHeap");
        assert!(!MIN_HEAP.is_equatable());
        assert!(!MIN_HEAP.is_hashable());
        assert_eq!(MIN_HEAP.name, "MinHeap");
    }
}
