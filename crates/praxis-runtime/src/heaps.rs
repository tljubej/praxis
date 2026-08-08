//! `MinHeap[T]` and `MaxHeap[T]` (M8-WS4, §6.1, §11.2).
//!
//! Both reuse Rust's `BinaryHeap` behind an opaque GC object. Rust's
//! `BinaryHeap` is a max-heap, so `MaxHeap[T]` maps directly
//! (`BinaryHeap<HeapEntry>`) and `MinHeap[T]` wraps entries in `Reverse`
//! (`BinaryHeap<Reverse<HeapEntry>>`) so the smallest element surfaces first.
//!
//! The element type must be orderable (§5.4 `SupportsOrd`); the capability
//! check rejects non-orderable types at compile time. Ordering goes through the
//! element descriptor's `compare` callback (ADR-045), so a `MinHeap[Float]`
//! orders numerically and a `MinHeap[Text]` lexicographically.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fmt::Write as _;

use crate::collections::nullable;
use crate::descriptor::{BuiltinTypeId, FormatSink, Tracer, TypeDescriptor};
use crate::GcRef;

/// A heap's elements in **pop order** — the order the program would see them if
/// it drained the heap, without draining it.
///
/// `BinaryHeap::iter` yields the backing array, which is heap-ordered only at
/// the root: `[3, 1, 2]` and `[3, 2, 1]` are both valid layouts for the same
/// contents, and which one exists depends on insertion history. So the same
/// heap could print two ways (RT-16) — and, since `for x in h` iterates a
/// snapshot of this (REP-15, ADR-066), could *answer* two ways. Sorting
/// descending by the heap's own `Ord` is pop order for a `BinaryHeap<T>`
/// whatever `T` is — for `MinHeap`, whose `T` is `Reverse<HeapEntry>`, that
/// comes out ascending by element, which is what `pop` gives.
///
/// This is the one collection whose deterministic order is also *meaningful*:
/// heaps carry an ordering by construction, so nothing here waits on D3.
pub(crate) fn in_pop_order<T: Ord, F: Fn(&T) -> GcRef>(
    items: &BinaryHeap<T>,
    value_of: F,
) -> Vec<GcRef> {
    let mut ordered: Vec<&T> = items.iter().collect();
    ordered.sort_unstable_by(|a, b| b.cmp(a));
    ordered.into_iter().map(value_of).collect()
}

/// Write a heap's elements in [`in_pop_order`].
///
/// Each element formats through the descriptor in its **own** header, not
/// through the heap's element label. The label is what the construction site
/// knew and may be null (REP-41); `HeapEntry::cmp` has always ordered by the
/// element's own descriptor, and this is the same rule for printing — a
/// `MinHeap` built with no static element type used to render its `Float`s as
/// the integers its `INT` label promised.
unsafe fn write_in_pop_order<T: Ord, F: Fn(&T) -> GcRef>(
    out: &mut FormatSink<'_>,
    items: &BinaryHeap<T>,
    value_of: F,
) {
    let _ = out.write_str("[");
    for (i, value) in in_pop_order(items, value_of).into_iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        let ep = value.payload::<u8>() as *const u8;
        // SAFETY: an object's payload always matches the descriptor in its own
        // header.
        unsafe { (value.descriptor().format)(ep, out) };
    }
    let _ = out.write_str("]");
}

/// A max-heap entry: the element `GcRef` plus its descriptor. `Ord` dispatches
/// to the descriptor's `compare` callback (ADR-045). `MaxHeap` uses this
/// directly; `MinHeap` wraps it in `Reverse`.
#[derive(Clone, Copy)]
pub struct HeapEntry {
    /// The element value.
    pub value: GcRef,
    /// The element's descriptor (for trace/equals/hash/format/compare).
    pub descriptor: &'static TypeDescriptor,
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
    /// The element type's own order, through its descriptor (ADR-045).
    ///
    /// Two answers are `Equal` for a reason rather than by accident: entries
    /// whose descriptors differ (which a homogeneous heap never has, so this is
    /// the miscompile case) and an element type with no ordering at all. There
    /// is no fault channel inside `Ord`, and a heap whose comparisons are all
    /// `Equal` is a consistent total order — the heap degrades to a bag instead
    /// of corrupting its sift invariants. What it must never do is what it used
    /// to: read every payload as an `i64`, which put `-2.0` after `-1.0` and
    /// read four bytes past a `Char`.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if !std::ptr::eq(self.descriptor, other.descriptor) {
            return std::cmp::Ordering::Equal;
        }
        match self.descriptor.compare {
            // SAFETY: both values carry this descriptor (checked above), so
            // both payloads are values of its type.
            Some(compare) => unsafe {
                compare(
                    self.value.payload::<u8>() as *const u8,
                    other.value.payload::<u8>() as *const u8,
                )
            },
            None => std::cmp::Ordering::Equal,
        }
    }
}

impl std::fmt::Debug for HeapEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut rendered = String::new();
        // SAFETY: the entry's value is a live object of its descriptor's type.
        // The debug style, for `DynamicKey`'s reason: this is a Rust `Debug`
        // impl, and a quoted string is what one of those shows.
        unsafe {
            (self.descriptor.format)(
                self.value.payload::<u8>() as *const u8,
                &mut FormatSink::debug(&mut rendered),
            );
        };
        write!(f, "HeapEntry({rendered})")
    }
}

// --- MaxHeap payload -------------------------------------------------------

/// The `MaxHeap[T]` payload (§11.2): a max-heap of `HeapEntry`.
#[repr(C)]
pub struct MaxHeapPayload {
    /// The descriptor for every element, or null when the construction site had
    /// no static element type (REP-41). A label: each element carries its own
    /// descriptor, both on its `HeapEntry` and in its object header. Read it
    /// through [`MaxHeapPayload::element`].
    pub element_descriptor: *const TypeDescriptor,
    /// The elements, in max-heap order (largest surfaces first).
    pub items: BinaryHeap<HeapEntry>,
}

impl MaxHeapPayload {
    /// The element label, or `None` when this heap was never told its element
    /// type.
    #[must_use]
    pub fn element(&self) -> Option<&'static TypeDescriptor> {
        nullable(self.element_descriptor)
    }
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

unsafe fn max_heap_format(payload: *const u8, out: &mut FormatSink<'_>) {
    let p = unsafe { &*(payload as *const MaxHeapPayload) };
    // SAFETY: every element matches the heap's element descriptor.
    unsafe { write_in_pop_order(out, &p.items, |e| e.value) };
}

/// Descriptor for `MaxHeap[T]` (§11.2, TypeId 17).
// Heaps are not equatable/hashable (contents + order define identity).
pub static MAX_HEAP: TypeDescriptor = TypeDescriptor::builtin::<MaxHeapPayload>(
    BuiltinTypeId::MaxHeap,
    "MaxHeap",
    max_heap_trace,
    max_heap_drop,
    max_heap_format,
    None,
    None,
    // No container order: a mutable collection can never be a `Map` key or a
    // `Set` member (ADR-057 D4), so nothing ever has to put one in a
    // deterministic sequence (ADR-138).
    None,
)
.with_owned_bytes(max_heap_owned_bytes);

impl MaxHeapPayload {
    /// The sift array this payload owns beyond its GC block, for GC pacing
    /// (RT-04) — `capacity`, not `len`.
    ///
    /// One statement of the size, with two readers (ADR-121):
    /// [`VecPayload::owned_bytes`](crate::collections::VecPayload::owned_bytes)
    /// is that statement.
    #[must_use]
    pub(crate) fn owned_bytes(&self) -> usize {
        self.items.capacity() * std::mem::size_of::<HeapEntry>()
    }
}

unsafe fn max_heap_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized MaxHeapPayload.
    let p = unsafe { &*(payload as *const MaxHeapPayload) };
    p.owned_bytes()
}

// --- MinHeap payload -------------------------------------------------------

/// The `MinHeap[T]` payload (§11.2): a min-heap via `Reverse<HeapEntry>`.
#[repr(C)]
pub struct MinHeapPayload {
    /// The descriptor for every element, or null when the construction site had
    /// no static element type (REP-41). See [`MaxHeapPayload::element_descriptor`].
    pub element_descriptor: *const TypeDescriptor,
    /// The elements, wrapped in `Reverse` so the smallest surfaces first.
    pub items: BinaryHeap<Reverse<HeapEntry>>,
}

impl MinHeapPayload {
    /// The element label, or `None` when this heap was never told its element
    /// type.
    #[must_use]
    pub fn element(&self) -> Option<&'static TypeDescriptor> {
        nullable(self.element_descriptor)
    }
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

unsafe fn min_heap_format(payload: *const u8, out: &mut FormatSink<'_>) {
    let p = unsafe { &*(payload as *const MinHeapPayload) };
    // SAFETY: every element matches the heap's element descriptor. The stored
    // entry is `Reverse<HeapEntry>`, so pop order is ascending by element.
    unsafe { write_in_pop_order(out, &p.items, |e| e.0.value) };
}

/// Descriptor for `MinHeap[T]` (§11.2, TypeId 18).
pub static MIN_HEAP: TypeDescriptor = TypeDescriptor::builtin::<MinHeapPayload>(
    BuiltinTypeId::MinHeap,
    "MinHeap",
    min_heap_trace,
    min_heap_drop,
    min_heap_format,
    None,
    None,
    // No container order: a mutable collection can never be a `Map` key or a
    // `Set` member (ADR-057 D4), so nothing ever has to put one in a
    // deterministic sequence (ADR-138).
    None,
)
.with_owned_bytes(min_heap_owned_bytes);

impl MinHeapPayload {
    /// The sift array this payload owns beyond its GC block, for GC pacing
    /// (RT-04) — `capacity`, not `len`, and of `Reverse<HeapEntry>` because
    /// that is what a min-heap stores.
    ///
    /// One statement of the size, with two readers (ADR-121):
    /// [`VecPayload::owned_bytes`](crate::collections::VecPayload::owned_bytes)
    /// is that statement.
    #[must_use]
    pub(crate) fn owned_bytes(&self) -> usize {
        self.items.capacity() * std::mem::size_of::<Reverse<HeapEntry>>()
    }
}

unsafe fn min_heap_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized MinHeapPayload.
    let p = unsafe { &*(payload as *const MinHeapPayload) };
    p.owned_bytes()
}

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

    #[test]
    fn float_heap_entries_use_numeric_order() {
        let rt = crate::Runtime::new();
        let minus_two = HeapEntry {
            value: rt.alloc_float(-2.0),
            descriptor: &crate::scalars::FLOAT,
        };
        let minus_one = HeapEntry {
            value: rt.alloc_float(-1.0),
            descriptor: &crate::scalars::FLOAT,
        };

        assert_eq!(
            minus_two.cmp(&minus_one),
            std::cmp::Ordering::Less,
            "orderable Float values must use IEEE numeric ordering, not signed bit-pattern order"
        );
    }

    /// P0-12. A `Char` payload is four bytes; the old `int_key` read eight from
    /// a four-byte-aligned address, so the ordering depended on whatever
    /// followed the object.
    #[test]
    fn char_heap_entries_order_by_unicode_scalar_value() {
        let rt = crate::Runtime::new();
        let a = HeapEntry {
            value: rt.alloc_char('a' as u32),
            descriptor: &crate::scalars::CHAR,
        };
        let beta = HeapEntry {
            value: rt.alloc_char('β' as u32),
            descriptor: &crate::scalars::CHAR,
        };
        assert_eq!(a.cmp(&beta), std::cmp::Ordering::Less);
        assert_eq!(beta.cmp(&a), std::cmp::Ordering::Greater);
    }

    /// **REP-15.** `in_pop_order` answers what draining the heap would, without
    /// draining it — and for a `MinHeap`, whose entries are `Reverse`d, that is
    /// ascending.
    ///
    /// The backing array is the thing this is not: `[3, 1, 2]` and `[3, 2, 1]`
    /// are both valid layouts for the same heap, so a `for` that indexed the
    /// array would answer by insertion history. It answered worse than that
    /// before ADR-066 — it read the payload as a `Vec`'s.
    #[test]
    fn a_heaps_snapshot_is_the_order_draining_it_would_give() {
        let rt = crate::Runtime::new();
        let entry = |n: i64| HeapEntry {
            value: rt.alloc_int(n),
            descriptor: &crate::scalars::INT,
        };
        let read_back = |items: Vec<GcRef>| -> Vec<i64> {
            // SAFETY: every element is an `Int`.
            items
                .into_iter()
                .map(|v| unsafe { *v.payload::<i64>() })
                .collect()
        };

        // Built in an order whose array layout is not sorted, so "the array" and
        // "pop order" are different sequences.
        let mut max: BinaryHeap<HeapEntry> = BinaryHeap::new();
        for n in [3, 1, 2] {
            max.push(entry(n));
        }
        assert_eq!(read_back(in_pop_order(&max, |e| e.value)), vec![3, 2, 1]);

        let mut min: BinaryHeap<Reverse<HeapEntry>> = BinaryHeap::new();
        for n in [3, 1, 2] {
            min.push(Reverse(entry(n)));
        }
        assert_eq!(read_back(in_pop_order(&min, |e| e.0.value)), vec![1, 2, 3]);

        // Draining the same heap agrees, which is the property that makes this
        // order the meaningful one rather than merely a fixed one.
        let mut drained = Vec::new();
        while let Some(Reverse(e)) = min.pop() {
            drained.push(unsafe { *e.value.payload::<i64>() });
        }
        assert_eq!(drained, vec![1, 2, 3]);

        // And the snapshot did not consume the heap it was taken from.
        let mut max_again: BinaryHeap<HeapEntry> = BinaryHeap::new();
        for n in [3, 1, 2] {
            max_again.push(entry(n));
        }
        let _ = in_pop_order(&max_again, |e| e.value);
        assert_eq!(max_again.len(), 3, "iterating is not popping");
    }

    /// ADR-045 decision 3: an entry never dispatches a `compare` callback at a
    /// value of another type. A homogeneous heap cannot reach this; a
    /// miscompiled one gets `Equal` rather than a `Text` payload read as an
    /// `Int`.
    #[test]
    fn entries_of_different_types_do_not_dispatch_a_callback() {
        let rt = crate::Runtime::new();
        let int = HeapEntry {
            value: rt.alloc_int(1),
            descriptor: &crate::scalars::INT,
        };
        let text = HeapEntry {
            value: rt.alloc_text("zzz"),
            descriptor: &crate::text::TEXT,
        };
        assert_eq!(int.cmp(&text), std::cmp::Ordering::Equal);
        assert_eq!(text.cmp(&int), std::cmp::Ordering::Equal);
    }

    /// An element type with no ordering leaves the heap a bag: every comparison
    /// is `Equal`, which is still a consistent total order, so `BinaryHeap`
    /// keeps its invariants.
    ///
    /// The fixture is a closure, which is the strongest remaining case: ADR-138
    /// populated `compare` on every type a `Map` key can be, so `Bool` — what
    /// this used to reach for — now has one. What is left without a `compare` is
    /// exactly what can never be a key, and a closure is the type that can never
    /// even be compared.
    #[test]
    fn an_element_type_with_no_container_order_compares_equal_rather_than_reading_bytes() {
        let mut rt = crate::Runtime::new();
        assert!(
            !crate::closures::CLOSURE.is_orderable(),
            "a closure has no ordering of any kind (ADR-138)"
        );
        let mut ctx = rt.context();
        // SAFETY: a live context, and a closure that captures nothing.
        let make = |ctx: &mut crate::RuntimeContext| HeapEntry {
            value: unsafe { crate::abi::praxis_alloc_closure(ctx, std::ptr::null(), 0) },
            descriptor: &crate::closures::CLOSURE,
        };
        let a = make(&mut ctx);
        let b = make(&mut ctx);
        assert_eq!(a.cmp(&b), std::cmp::Ordering::Equal);
    }

    /// A `MinHeap[Text]` pops lexicographically. The end-to-end proof that the
    /// heap's order is the descriptor's order and not the payload's address:
    /// these three texts are allocated in an order that does not match their
    /// lexicographic one.
    #[test]
    fn a_text_heap_pops_in_lexicographic_order() {
        let rt = crate::Runtime::new();
        let mut items = BinaryHeap::new();
        for s in ["pear", "apple", "quince", "banana"] {
            items.push(Reverse(HeapEntry {
                value: rt.alloc_text(s),
                descriptor: &crate::text::TEXT,
            }));
        }
        let payload = MinHeapPayload {
            element_descriptor: &crate::text::TEXT,
            items,
        };
        assert_eq!(
            rendered(min_heap_format, &payload),
            "[apple, banana, pear, quince]"
        );
    }

    /// Render a heap payload through its own `format` callback, without
    /// allocating a GC object to hold it — the callback takes a payload
    /// pointer, which is all a formatting test needs.
    fn rendered<P>(format: crate::FormatFn, payload: &P) -> String {
        let mut s = String::new();
        // The program's own rendering, which is what these tests are about.
        let mut sink = FormatSink::display(&mut s);
        // SAFETY: `payload` is an initialized value of the type `format` reads.
        unsafe { format((payload as *const P).cast::<u8>(), &mut sink) };
        s
    }

    /// RT-16. `BinaryHeap::iter` walks the backing array, which is heap-ordered
    /// only at the root — `[3, 1, 2]` and `[3, 2, 1]` are both valid layouts for
    /// the same contents, and which one exists depends on insertion history. So
    /// two heaps that are equal as values printed differently. Pop order is the
    /// order the program would observe, and it is the same for both.
    #[test]
    fn heap_formatting_does_not_depend_on_insertion_order() {
        let rt = crate::Runtime::new();
        let build = |order: [i64; 5]| {
            let mut items = BinaryHeap::new();
            for n in order {
                items.push(HeapEntry {
                    value: rt.alloc_int(n),
                    descriptor: &crate::scalars::INT,
                });
            }
            MaxHeapPayload {
                element_descriptor: &crate::scalars::INT,
                items,
            }
        };

        let ascending = build([1, 5, 3, 9, 2]);
        let descending = build([2, 9, 3, 5, 1]);
        let backing =
            |p: &MaxHeapPayload| p.items.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>();
        assert_ne!(
            backing(&ascending),
            backing(&descending),
            "the two backing arrays must actually differ, or this proves nothing"
        );

        let a = rendered(max_heap_format, &ascending);
        let d = rendered(max_heap_format, &descending);
        assert_eq!(a, d, "the same contents must render the same way");
        assert_eq!(a, "[9, 5, 3, 2, 1]", "a max-heap renders in pop order");
    }

    /// A `MinHeap` stores `Reverse<HeapEntry>`, so "descending by the stored
    /// `Ord`" comes out ascending by element — which is what `pop` gives.
    #[test]
    fn a_min_heap_renders_smallest_first() {
        let rt = crate::Runtime::new();
        let mut items = BinaryHeap::new();
        for n in [4_i64, 1, 7, 2] {
            items.push(Reverse(HeapEntry {
                value: rt.alloc_int(n),
                descriptor: &crate::scalars::INT,
            }));
        }
        let payload = MinHeapPayload {
            element_descriptor: &crate::scalars::INT,
            items,
        };
        assert_eq!(rendered(min_heap_format, &payload), "[1, 2, 4, 7]");
    }
}
