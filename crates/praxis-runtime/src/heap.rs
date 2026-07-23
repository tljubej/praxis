//! The GC heap and the precise non-moving mark-and-sweep collector (§12, ADR-011).
//!
//! Every allocation is `[GcHeader | payload]` laid out contiguously in a
//! [`bumpalo::Bump`] arena. A side `live` registry (`Vec<NonNull<GcHeader>>`)
//! tracks every outstanding allocation so sweep is precise: there is no need to
//! recover object boundaries by scanning. Objects never move (§12.1), so `GcRef`
//! addresses are stable for the object's lifetime.
//!
//! Collection is tri-color mark-and-sweep with no write barrier (§12.1):
//!   1. **Mark** — start from the root set; for each reachable object, run its
//!      descriptor `trace` callback to enqueue child references.
//!   2. **Sweep** — any allocation still white after marking gets its descriptor
//!      `drop_value` called (§12.5) and is dropped from the registry.

use std::alloc::Layout;
use std::cell::RefCell;
use std::ptr::NonNull;

use bumpalo::Bump;

use crate::descriptor::TypeDescriptor;
use crate::gc::{GcHeader, GcRef, BLACK, GREY, WHITE};
use crate::roots::RootSet;
use crate::Tracer;

/// A precise, non-moving GC heap (§12.1, ADR-011).
///
/// `#[repr(C)]` so the `RuntimeContext.heap` pointer offset is stable
/// (Appendix B). The interior `RefCell` lets the mark phase enqueue child
/// references through a `&Heap` (the collector borrows the heap immutably while
/// the worklist, owned by the mark, grows).
#[repr(C)]
pub struct Heap {
    arena: Bump,
    /// Every live allocation's header pointer. Precise sweep iterates this.
    /// Wrapped in a `RefCell` because `collect` mutates it while a `&Heap` is
    /// reborrowed by the tracer callbacks.
    live: RefCell<Vec<NonNull<GcHeader>>>,
}

// SAFETY: the heap owns raw allocations that are only accessed through `GcRef`s
// the caller keeps rooted. It is `Send` because the collector is
// single-threaded (§12.1) and the heap is never shared across threads in M3.
// It is not `Sync` (no `&Heap` aliasing across threads); the collector is
// single-threaded.
unsafe impl Send for Heap {}

/// Lightweight allocation statistics for tests and debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapStats {
    /// Number of currently-registered live allocations.
    pub live_count: usize,
}

impl Heap {
    /// A fresh, empty heap.
    pub fn new() -> Self {
        Heap {
            arena: Bump::new(),
            live: RefCell::new(Vec::new()),
        }
    }

    /// Current allocation count.
    pub fn stats(&self) -> HeapStats {
        HeapStats {
            live_count: self.live.borrow().len(),
        }
    }

    /// Allocate an immortal object: same layout as [`Heap::alloc`], but **not**
    /// registered in the live set, so the collector never reclaims it (§4.3,
    /// M3 deliverable). Used for the `Unit`/`Bool` singletons.
    pub(crate) fn alloc_immortal<T: Copy>(
        &self,
        descriptor: &'static TypeDescriptor,
        value: T,
    ) -> GcRef {
        assert_eq!(
            std::mem::size_of::<T>(),
            descriptor.size,
            "payload size mismatch for descriptor {}",
            descriptor.name
        );
        // SAFETY: `T: Copy`, bytes are fully initialized.
        let r = unsafe { self.alloc_raw(descriptor, |payload| (payload as *mut T).write(value)) };
        // Remove the registration `alloc_raw` just performed, so sweep skips it.
        let mut live = self.live.borrow_mut();
        if let Some(idx) = live
            .iter()
            .position(|p| p.as_ptr() == r.as_non_null().as_ptr())
        {
            live.swap_remove(idx);
        }
        drop(live);
        r
    }

    /// Allocate an object with the given descriptor and a `Copy` payload `value`,
    /// returning a reference to it.
    ///
    /// For payloads that own Rust resources (`Box<str>`, `VecPayload`) use
    /// [`Heap::alloc_with`], which writes the value via `ptr::write` so its
    /// `Drop` later runs correctly.
    ///
    /// # Panics
    /// Panics if `T`'s size does not match `descriptor.size`.
    pub fn alloc<T: Copy>(&self, descriptor: &'static TypeDescriptor, value: T) -> GcRef {
        assert_eq!(
            std::mem::size_of::<T>(),
            descriptor.size,
            "payload size mismatch for descriptor {}",
            descriptor.name
        );
        // SAFETY: `T: Copy`, so writing the bytes is sufficient initialization
        // (no `Drop` to run later).
        unsafe { self.alloc_raw(descriptor, |payload| (payload as *mut T).write(value)) }
    }

    /// Allocate an object whose payload owns Rust resources, initializing it
    /// with `init`. `init` receives a pointer to the uninitialized payload bytes
    /// and must fully initialize them.
    ///
    /// # Safety
    /// `init` must initialize the payload in place and must not panic after
    /// partial initialization (if it does, the payload's `Drop` will not run,
    /// leaking the partially-initialized resources). The descriptor's `size`/
    /// `align` must match the value `init` writes.
    pub unsafe fn alloc_with(
        &self,
        descriptor: &'static TypeDescriptor,
        size: usize,
        align: usize,
        init: impl FnOnce(*mut u8),
    ) -> GcRef {
        assert_eq!(
            size, descriptor.size,
            "payload size mismatch for descriptor {}",
            descriptor.name
        );
        assert_eq!(
            align, descriptor.align,
            "payload align mismatch for descriptor {}",
            descriptor.name
        );
        // SAFETY: forwarded from the caller's contract above.
        unsafe { self.alloc_raw(descriptor, init) }
    }

    /// The shared low-level allocator: lay out `[GcHeader | payload]`, run `init`
    /// on the payload, register the header in `live`.
    ///
    /// # Safety
    /// `init` must fully initialize `descriptor.size` bytes of the payload and
    /// the bytes must be valid as the descriptor's payload type thereafter.
    unsafe fn alloc_raw(
        &self,
        descriptor: &'static TypeDescriptor,
        init: impl FnOnce(*mut u8),
    ) -> GcRef {
        let header_size = std::mem::size_of::<GcHeader>();
        let header_align = std::mem::align_of::<GcHeader>();
        let payload_size = descriptor.size;
        let payload_align = descriptor.align;

        // The allocation must satisfy both header and payload alignment; the
        // payload starts exactly `header_size` bytes in, padded so the payload
        // is properly aligned.
        let padded_header_size = round_up(header_size, payload_align);
        let total = padded_header_size
            .checked_add(payload_size)
            .expect("allocation size overflow");
        let align = header_align.max(payload_align);

        let layout = Layout::from_size_align(total, align).expect("invalid layout");

        let base = self.arena.alloc_layout(layout);
        let base_ptr = base.as_ptr();
        let header_ptr = base_ptr as *mut GcHeader;
        let payload_ptr = base_ptr.add(padded_header_size);

        // Write the header. Mark starts white (unscanned).
        std::ptr::write(
            header_ptr,
            GcHeader {
                descriptor: descriptor as *const TypeDescriptor,
                mark: std::cell::Cell::new(WHITE),
                size: descriptor.size as u32,
            },
        );
        // Initialize the payload.
        init(payload_ptr);

        let nn = NonNull::new(header_ptr).expect("bumpalo never returns null");
        self.live.borrow_mut().push(nn);
        // SAFETY: `nn` points at the just-allocated, initialized header.
        unsafe { GcRef::from_non_null(nn) }
    }

    /// Run a mark-and-sweep collection (§12.1, ADR-011).
    ///
    /// Every `GcRef` reachable from `roots` (plus everything transitively
    /// reachable through descriptor `trace` callbacks) is marked black and
    /// survives; everything else is finalized via `drop_value` and reclaimed.
    pub fn collect(&self, roots: &dyn RootSet) {
        self.mark(roots);
        self.sweep();
    }

    /// Mark phase: color every reachable object black.
    fn mark(&self, roots: &dyn RootSet) {
        let mut worklist: Vec<GcRef> = Vec::new();
        roots.push_roots(&mut worklist);

        // The tracer enqueues child references onto the worklist. It borrows the
        // worklist (and indirectly `&self` through descriptor callbacks), so it
        // is dropped before we touch `self.live` again.
        struct Enqueuer<'a>(&'a mut Vec<GcRef>);
        impl Tracer for Enqueuer<'_> {
            fn trace(&mut self, reference: GcRef) {
                self.0.push(reference);
            }
        }

        while let Some(r) = worklist.pop() {
            let header = r.header();
            if header.mark_color() == BLACK {
                continue;
            }
            // Color black first, *then* trace, so the descriptor's `trace`
            // callback may enqueue children that point back to this object
            // without re-tracing it infinitely.
            header.set_mark_color(BLACK);
            let desc = header.descriptor();
            let payload = r.payload::<u8>();
            // SAFETY: `r` is a live, reachable object whose payload matches its
            // descriptor.
            let mut enq = Enqueuer(&mut worklist);
            // Mark grey transiently to denote "in progress" is unnecessary; we
            // color black immediately (single-threaded, no concurrency).
            let _ = GREY; // GREY kept for future concurrent-collection use.
            unsafe { (desc.trace)(payload, &mut enq) };
        }
    }

    /// Sweep phase: finalize and unregister every still-white allocation.
    fn sweep(&self) {
        let mut live = self.live.borrow_mut();
        let mut i = 0;
        while i < live.len() {
            // SAFETY: every entry in `live` was pushed by `alloc_raw` and points
            // at an initialized header + payload.
            let header = unsafe { live[i].as_ref() };
            if header.mark_color() == WHITE {
                // Finalize: run the descriptor's drop_value on the payload.
                let desc = header.descriptor();
                let payload = header.payload::<u8>();
                // SAFETY: payload matches `desc` and is about to become invalid.
                unsafe { (desc.drop_value)(payload) };
                live.swap_remove(i);
            } else {
                // Reset to white for the next collection and keep it.
                header.set_mark_color(WHITE);
                i += 1;
            }
        }
    }

    /// Reset the heap to empty, dropping everything. Used by tests and, later,
    /// runtime teardown. Immortal singletons must be re-allocated afterwards.
    pub fn reset(&mut self) {
        // Finalize every live allocation before tearing down the arena.
        let live = std::mem::take(&mut *self.live.borrow_mut());
        for nn in live {
            // SAFETY: each `nn` is a live, initialized allocation.
            let header = unsafe { nn.as_ref() };
            let desc = header.descriptor();
            let payload = header.payload::<u8>();
            // SAFETY: payload matches `desc`, becomes invalid after this.
            unsafe { (desc.drop_value)(payload) };
        }
        self.arena.reset();
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

/// Round `n` up to the next multiple of `align` (which must be a power of two).
fn round_up(n: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (n + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collections::{VecPayload, VEC};
    use crate::roots::RootScope;
    use crate::scalars::{INT, UNIT};
    use crate::GcRef;

    #[test]
    fn alloc_int_round_trips_payload() {
        let heap = Heap::new();
        let r = heap.alloc(INT, 42_i64);
        assert_eq!(r.descriptor().name, "Int");
        // SAFETY: `r` was allocated with INT, payload is i64.
        let v = unsafe { *r.payload::<i64>() };
        assert_eq!(v, 42);
        assert_eq!(heap.stats().live_count, 1);
    }

    #[test]
    fn collect_reclaims_unrooted_allocation() {
        let heap = Heap::new();
        let _ = heap.alloc(INT, 1_i64);
        assert_eq!(heap.stats().live_count, 1);

        let roots = RootScope::new(); // nothing rooted
        heap.collect(&roots);
        assert_eq!(
            heap.stats().live_count,
            0,
            "unrooted Int should be reclaimed"
        );
    }

    #[test]
    fn collect_preserves_rooted_allocation() {
        let heap = Heap::new();
        let mut scope = RootScope::new();
        let r = heap.alloc(INT, 7_i64);
        scope.root(r);
        assert_eq!(heap.stats().live_count, 1);

        heap.collect(&scope);
        assert_eq!(heap.stats().live_count, 1, "rooted Int survives");
        // Payload still readable after collection.
        // SAFETY: `r` survived and was allocated with INT.
        let v = unsafe { *r.payload::<i64>() };
        assert_eq!(v, 7);
    }

    #[test]
    fn collect_preserves_nested_references() {
        // The headline M3 acceptance test: a Vec of Int is rooted, garbage is
        // allocated, and after collection the whole nested graph survives and
        // is readable through the element descriptors.
        let heap = Heap::new();
        let mut scope = RootScope::new();

        // Build [10, 20, 30] as Int GcRefs.
        let elems: Vec<GcRef> = [10_i64, 20, 30]
            .iter()
            .map(|&v| heap.alloc(INT, v))
            .collect();

        // Wrap in a Vec[T] payload. Element type is recorded in the payload
        // (ADR-013).
        let vec_ref = unsafe {
            heap.alloc_with(
                VEC,
                std::mem::size_of::<VecPayload>(),
                std::mem::align_of::<VecPayload>(),
                |payload| {
                    let vp = VecPayload {
                        element_descriptor: INT,
                        items: elems.clone().into_boxed_slice(),
                    };
                    (payload as *mut VecPayload).write(vp);
                },
            )
        };
        scope.root(vec_ref);

        // Allocate garbage that should be reclaimed.
        for i in 0..5_i64 {
            let _ = heap.alloc(INT, 1000 + i);
        }
        assert_eq!(heap.stats().live_count, 9); // vec + 3 ints + 5 garbage

        heap.collect(&scope);

        // The vec and its 3 elements survive; the 5 garbage ints are reclaimed.
        assert_eq!(heap.stats().live_count, 4);

        // Format the vec through its descriptor to prove the nested graph is
        // intact and readable end to end.
        let mut out = String::new();
        let desc = vec_ref.descriptor();
        // SAFETY: vec_ref's payload is a VecPayload.
        unsafe { (desc.format)(vec_ref.payload::<u8>() as *const u8, &mut out) };
        assert_eq!(out, "[10, 20, 30]");
    }

    #[test]
    fn collect_handles_vec_of_vec() {
        // Deeper nesting: [[1, 2], [3]] — only the outer vec is rooted; the
        // inner vecs and their ints must survive via transitive tracing.
        let heap = Heap::new();
        let mut scope = RootScope::new();

        let inner_alloc = |ints: &[i64]| -> GcRef {
            let elems: Vec<GcRef> = ints.iter().map(|&v| heap.alloc(INT, v)).collect();
            unsafe {
                heap.alloc_with(
                    VEC,
                    std::mem::size_of::<VecPayload>(),
                    std::mem::align_of::<VecPayload>(),
                    |payload| {
                        (payload as *mut VecPayload).write(VecPayload {
                            element_descriptor: INT,
                            items: elems.into_boxed_slice(),
                        });
                    },
                )
            }
        };

        let inner0 = inner_alloc(&[1, 2]);
        let inner1 = inner_alloc(&[3]);
        let outer = unsafe {
            heap.alloc_with(
                VEC,
                std::mem::size_of::<VecPayload>(),
                std::mem::align_of::<VecPayload>(),
                |payload| {
                    (payload as *mut VecPayload).write(VecPayload {
                        // The element descriptor of a Vec-of-X is VEC itself.
                        element_descriptor: VEC,
                        items: vec![inner0, inner1].into_boxed_slice(),
                    });
                },
            )
        };
        scope.root(outer);

        // Garbage.
        let _ = heap.alloc(UNIT, ());

        heap.collect(&scope);

        // outer + 2 inner vecs + 3 ints = 6 survivors; the Unit garbage dies.
        assert_eq!(heap.stats().live_count, 6);

        let mut out = String::new();
        unsafe { (outer.descriptor().format)(outer.payload::<u8>() as *const u8, &mut out) };
        assert_eq!(out, "[[1, 2], [3]]");
    }

    #[test]
    fn round_up_is_correct() {
        assert_eq!(round_up(0, 8), 0);
        assert_eq!(round_up(1, 8), 8);
        assert_eq!(round_up(8, 8), 8);
        assert_eq!(round_up(9, 8), 16);
        assert_eq!(round_up(16, 1), 16);
    }
}
