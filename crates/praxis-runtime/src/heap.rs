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
use crate::gc::{GcHeader, GcRef, HeapId, BLACK, GREY, WHITE};
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
    /// This heap's identity, stamped into every header it allocates. The mark
    /// phase compares it against a root's `heap_id` before touching anything
    /// the header points at, so a root from another heap — or one whose storage
    /// this heap has already swept — is rejected rather than traced.
    id: HeapId,
    /// Every live allocation's header pointer. Precise sweep iterates this.
    /// Wrapped in a `RefCell` because `collect` mutates it while a `&Heap` is
    /// reborrowed by the tracer callbacks.
    live: RefCell<Vec<NonNull<GcHeader>>>,
    /// Bytes allocated since the last collection. Used by [`Heap::maybe_collect`]
    /// to trigger automatic collection on allocation pressure (§12.4, M5). This
    /// is the mechanism that makes "survives collection" observable from JIT'd
    /// code: the alloc wrappers call `maybe_collect` with the current roots.
    bytes_since_collect: RefCell<usize>,
    /// The threshold at/above which [`Heap::maybe_collect`] runs a collection.
    /// Doubled after each collection so the heap grows geometrically (amortized
    /// O(1) allocations per collection), a standard GC pacing heuristic.
    collect_threshold: RefCell<usize>,
}

/// The initial collection threshold (bytes). Small enough that the first
/// collection runs early in a program's life (catching rooting bugs fast in
/// tests), then grows via the doubling rule.
pub const INITIAL_COLLECT_THRESHOLD: usize = 1 << 16; // 64 KiB

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
            id: HeapId::mint(),
            live: RefCell::new(Vec::new()),
            bytes_since_collect: RefCell::new(0),
            collect_threshold: RefCell::new(INITIAL_COLLECT_THRESHOLD),
        }
    }

    /// This heap's identity. Every header it allocates carries it.
    pub fn id(&self) -> HeapId {
        self.id
    }

    /// Whether `value` was allocated by this heap and has not been swept.
    ///
    /// O(1): it reads the owning id out of the header rather than searching the
    /// live registry. This is the guard the collector applies to every root.
    #[inline]
    pub fn owns(&self, value: GcRef) -> bool {
        value.header().heap_id() == Some(self.id)
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
            descriptor.size(),
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
            descriptor.size(),
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
            size,
            descriptor.size(),
            "payload size mismatch for descriptor {}",
            descriptor.name
        );
        assert_eq!(
            align,
            descriptor.align(),
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
        let header_align = std::mem::align_of::<GcHeader>();
        let payload_size = descriptor.size();
        let payload_align = descriptor.align();

        // The allocation must satisfy both header and payload alignment. Where
        // the payload starts is `GcHeader::payload_offset_for`'s decision and
        // nobody else's — the same call the header records and `payload()`
        // reads back.
        let payload_offset = GcHeader::payload_offset_for(payload_align);
        let recorded_offset = u16::try_from(payload_offset).unwrap_or_else(|_| {
            panic!(
                "payload alignment {payload_align} of descriptor {} exceeds the \
                 largest offset a GcHeader can record",
                descriptor.name
            )
        });
        let total = payload_offset
            .checked_add(payload_size)
            .expect("allocation size overflow");
        let align = header_align.max(payload_align);

        let layout = Layout::from_size_align(total, align).expect("invalid layout");

        let base = self.arena.alloc_layout(layout);
        let base_ptr = base.as_ptr();
        let header_ptr = base_ptr as *mut GcHeader;
        let payload_ptr = base_ptr.add(payload_offset);

        // Write the header. Mark starts white (unscanned).
        std::ptr::write(
            header_ptr,
            GcHeader::new(
                descriptor,
                descriptor.size() as u32,
                recorded_offset,
                self.id,
            ),
        );
        // Initialize the payload.
        init(payload_ptr);

        let nn = NonNull::new(header_ptr).expect("bumpalo never returns null");
        self.live.borrow_mut().push(nn);
        // Account for the allocation against the collection pacing counter.
        *self.bytes_since_collect.borrow_mut() += total;
        // SAFETY: `nn` points at the just-allocated, initialized header.
        unsafe { GcRef::from_non_null(nn) }
    }

    /// Run a mark-and-sweep collection (§12.1, ADR-011).
    ///
    /// Every `GcRef` reachable from `roots` (plus everything transitively
    /// reachable through descriptor `trace` callbacks) is marked black and
    /// survives; everything else is finalized via `drop_value` and reclaimed.
    pub fn collect(&self, roots: &crate::roots::RuntimeRoots<'_>) {
        self.collect_inner(roots);
    }

    /// [`Heap::collect`] against an arbitrary root set.
    ///
    /// Test-only: production collection roots from a
    /// [`RuntimeRoots`](crate::roots::RuntimeRoots), which is constructible
    /// only from a live `RuntimeContext` and is exhaustive over the runtime's
    /// owners. Accepting a `&dyn RootSet` on the production path is what let
    /// the automatic collector root from the shadow chain alone (P0-06).
    #[cfg(test)]
    pub fn collect_with(&self, roots: &dyn RootSet) {
        self.collect_inner(roots);
    }

    fn collect_inner(&self, roots: &dyn RootSet) {
        self.mark(roots);
        self.sweep();
        // Reset the pacing counter and grow the threshold geometrically.
        *self.bytes_since_collect.borrow_mut() = 0;
        let mut threshold = self.collect_threshold.borrow_mut();
        *threshold = (*threshold)
            .saturating_mul(2)
            .max(INITIAL_COLLECT_THRESHOLD);
    }

    /// Run a collection if allocation pressure has reached the threshold,
    /// rooting from `roots`. Called by the `praxis_alloc_*` wrappers (§12.4)
    /// so collection happens automatically inside JIT'd code — this is what
    /// makes "nested vectors survive collection" (§19 M5 acceptance) testable
    /// without the host forcing it.
    ///
    /// Returns `true` if a collection ran.
    pub fn maybe_collect(&self, roots: &crate::roots::RuntimeRoots<'_>) -> bool {
        let should = *self.bytes_since_collect.borrow() >= *self.collect_threshold.borrow();
        if should {
            self.collect(roots);
            true
        } else {
            false
        }
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
            // Provenance check, before anything the header points at is read.
            // A reference this heap did not allocate is not this heap's to
            // color: marking a foreign object black delays *its* heap's
            // reclamation of it, and a swept object's descriptor is a null
            // pointer into finalized storage. Both are rejected here.
            if header.heap_id() != Some(self.id) {
                continue;
            }
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
                // Poison before unregistering, so a stale `GcRef` that still
                // names this storage is rejected by the mark phase's provenance
                // check instead of being traced through a finalized payload.
                header.poison();
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
            header.poison();
        }
        self.arena.reset();
        // Pacing is part of the heap's state, so a reset heap paces like a fresh
        // one. Leaving the counter and the geometrically-grown threshold in
        // place meant a reset heap could run for megabytes before its first
        // collection, or collect on its very first allocation (RT-04).
        *self.bytes_since_collect.borrow_mut() = 0;
        *self.collect_threshold.borrow_mut() = INITIAL_COLLECT_THRESHOLD;
        // A reset heap is a different heap: the immortals it handed out are
        // gone, and every `GcRef` minted before this point names storage the
        // arena is free to hand out again. A fresh identity makes those refs
        // fail the mark phase's provenance check rather than be traced.
        self.id = HeapId::mint();
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collections::{VecPayload, VEC};
    use crate::descriptor::TypeDescriptor;
    use crate::roots::RootScope;
    use crate::scalars::{INT, UNIT};
    use crate::{GcRef, Tracer};
    use std::cell::Cell;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[repr(C)]
    struct DropProbe(Arc<AtomicUsize>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    unsafe fn probe_trace(_: *mut u8, _: &mut dyn Tracer) {}
    unsafe fn probe_drop(payload: *mut u8) {
        unsafe { std::ptr::drop_in_place(payload as *mut DropProbe) };
    }
    unsafe fn probe_format(_: *const u8, _: &mut dyn std::fmt::Write) {}

    static DROP_PROBE: TypeDescriptor = TypeDescriptor::for_test::<DropProbe>(
        1,
        "DropProbe",
        probe_trace,
        probe_drop,
        probe_format,
        None,
        None,
        None,
    );

    #[repr(C, align(64))]
    struct Overaligned(u8);

    unsafe fn overaligned_drop(_: *mut u8) {}
    static OVERALIGNED: TypeDescriptor = TypeDescriptor::for_test::<Overaligned>(
        0,
        "Overaligned",
        probe_trace,
        overaligned_drop,
        probe_format,
        None,
        None,
        None,
    );

    #[test]
    fn alloc_int_round_trips_payload() {
        let heap = Heap::new();
        let r = heap.alloc(&INT, 42_i64);
        assert_eq!(r.descriptor().name, "Int");
        // SAFETY: `r` was allocated with INT, payload is i64.
        let v = unsafe { *r.payload::<i64>() };
        assert_eq!(v, 42);
        assert_eq!(heap.stats().live_count, 1);
    }

    #[test]
    fn collect_reclaims_unrooted_allocation() {
        let heap = Heap::new();
        let _ = heap.alloc(&INT, 1_i64);
        assert_eq!(heap.stats().live_count, 1);

        let roots = RootScope::new(); // nothing rooted
        heap.collect_with(&roots);
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
        let r = heap.alloc(&INT, 7_i64);
        scope.root(r);
        assert_eq!(heap.stats().live_count, 1);

        heap.collect_with(&scope);
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
            .map(|&v| heap.alloc(&INT, v))
            .collect();

        // Wrap in a Vec[T] payload. Element type is recorded in the payload
        // (ADR-013).
        let vec_ref = unsafe {
            heap.alloc_with(
                &VEC,
                std::mem::size_of::<VecPayload>(),
                std::mem::align_of::<VecPayload>(),
                |payload| {
                    let vp = VecPayload {
                        element_descriptor: &INT,
                        items: elems.clone(),
                    };
                    (payload as *mut VecPayload).write(vp);
                },
            )
        };
        scope.root(vec_ref);

        // Allocate garbage that should be reclaimed.
        for i in 0..5_i64 {
            let _ = heap.alloc(&INT, 1000 + i);
        }
        assert_eq!(heap.stats().live_count, 9); // vec + 3 ints + 5 garbage

        heap.collect_with(&scope);

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
            let elems: Vec<GcRef> = ints.iter().map(|&v| heap.alloc(&INT, v)).collect();
            unsafe {
                heap.alloc_with(
                    &VEC,
                    std::mem::size_of::<VecPayload>(),
                    std::mem::align_of::<VecPayload>(),
                    |payload| {
                        (payload as *mut VecPayload).write(VecPayload {
                            element_descriptor: &INT,
                            items: elems,
                        });
                    },
                )
            }
        };

        let inner0 = inner_alloc(&[1, 2]);
        let inner1 = inner_alloc(&[3]);
        let outer = unsafe {
            heap.alloc_with(
                &VEC,
                std::mem::size_of::<VecPayload>(),
                std::mem::align_of::<VecPayload>(),
                |payload| {
                    (payload as *mut VecPayload).write(VecPayload {
                        // The element descriptor of a Vec-of-X is VEC itself.
                        element_descriptor: &VEC,
                        items: vec![inner0, inner1],
                    });
                },
            )
        };
        scope.root(outer);

        // Garbage.
        let _ = heap.alloc(&UNIT, ());

        heap.collect_with(&scope);

        // outer + 2 inner vecs + 3 ints = 6 survivors; the Unit garbage dies.
        assert_eq!(heap.stats().live_count, 6);

        let mut out = String::new();
        unsafe { (outer.descriptor().format)(outer.payload::<u8>() as *const u8, &mut out) };
        assert_eq!(out, "[[1, 2], [3]]");
    }

    #[test]
    fn collect_finalizes_unreachable_owned_payload_exactly_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let heap = Heap::new();
        unsafe {
            heap.alloc_with(
                &DROP_PROBE,
                std::mem::size_of::<DropProbe>(),
                std::mem::align_of::<DropProbe>(),
                |payload| (payload as *mut DropProbe).write(DropProbe(Arc::clone(&drops))),
            );
        }

        let roots = RootScope::new();
        heap.collect_with(&roots);
        assert_eq!(drops.load(Ordering::SeqCst), 1);

        heap.collect_with(&roots);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "a swept payload must never be finalized twice"
        );
    }

    #[test]
    #[ignore = "known bug: Heap has no Drop implementation for live payloads"]
    fn dropping_heap_finalizes_live_owned_payloads() {
        let drops = Arc::new(AtomicUsize::new(0));
        {
            let heap = Heap::new();
            unsafe {
                heap.alloc_with(
                    &DROP_PROBE,
                    std::mem::size_of::<DropProbe>(),
                    std::mem::align_of::<DropProbe>(),
                    |payload| (payload as *mut DropProbe).write(DropProbe(Arc::clone(&drops))),
                );
            }
            assert_eq!(drops.load(Ordering::SeqCst), 0);
        }

        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "tearing down a heap must run descriptor finalizers for live payloads"
        );
    }

    #[test]
    fn overaligned_payload_accessor_matches_initialized_address() {
        let initialized_at = Cell::new(std::ptr::null_mut());
        let heap = Heap::new();
        let value = unsafe {
            heap.alloc_with(
                &OVERALIGNED,
                std::mem::size_of::<Overaligned>(),
                std::mem::align_of::<Overaligned>(),
                |payload| {
                    initialized_at.set(payload);
                    (payload as *mut Overaligned).write(Overaligned(7));
                },
            )
        };

        assert_eq!(
            value.payload::<Overaligned>() as *mut u8,
            initialized_at.get(),
            "GcHeader::payload must account for alignment padding inserted by Heap::alloc_raw"
        );
    }

    #[test]
    fn foreign_heap_root_cannot_delay_reclamation() {
        let first = Heap::new();
        let second = Heap::new();
        let value = first.alloc(&INT, 1_i64);
        let mut foreign_roots = RootScope::new();
        foreign_roots.root(value);

        second.collect_with(&foreign_roots);
        first.collect_with(&RootScope::new());

        assert_eq!(
            first.stats().live_count,
            0,
            "a collection on another heap must not mutate this heap's mark colors"
        );
    }

    #[test]
    fn reset_restores_collection_pacing() {
        let mut heap = Heap::new();
        heap.collect_with(&RootScope::new());
        let _ = heap.alloc(&INT, 1_i64);
        assert_ne!(*heap.bytes_since_collect.borrow(), 0);
        assert_ne!(*heap.collect_threshold.borrow(), INITIAL_COLLECT_THRESHOLD);

        heap.reset();

        assert_eq!(*heap.bytes_since_collect.borrow(), 0);
        assert_eq!(*heap.collect_threshold.borrow(), INITIAL_COLLECT_THRESHOLD);
    }

    #[test]
    #[ignore = "known bug: sweeping finalizes payloads but cannot reuse bump-arena storage"]
    fn repeated_collection_reuses_dead_object_storage() {
        let heap = Heap::new();
        const OBJECTS_PER_CYCLE: usize = 4_096;

        for i in 0..OBJECTS_PER_CYCLE {
            let _ = heap.alloc(&INT, i as i64);
        }
        heap.collect_with(&RootScope::new());
        let first_cycle_bytes = heap.arena.allocated_bytes();

        for cycle in 1..=8 {
            for i in 0..OBJECTS_PER_CYCLE {
                let _ = heap.alloc(&INT, (cycle * OBJECTS_PER_CYCLE + i) as i64);
            }
            heap.collect_with(&RootScope::new());
        }
        let final_bytes = heap.arena.allocated_bytes();

        assert!(
            final_bytes <= first_cycle_bytes.saturating_mul(2),
            "reclaiming the same bounded working set repeatedly grew the arena \
             from {first_cycle_bytes} to {final_bytes} bytes"
        );
    }

    /// The allocator must record the offset it actually used, for every
    /// alignment — this is the invariant that makes `GcHeader::payload` and
    /// `alloc_raw` two readings of one calculation rather than two
    /// calculations that happen to agree.
    #[test]
    fn every_allocation_records_the_offset_it_was_laid_out_with() {
        let heap = Heap::new();

        let int = heap.alloc(&INT, 1_i64);
        assert_eq!(
            int.payload::<i64>() as usize - int.as_ptr() as usize,
            GcHeader::payload_offset_for(INT.align())
        );

        let over = unsafe {
            heap.alloc_with(
                &OVERALIGNED,
                std::mem::size_of::<Overaligned>(),
                std::mem::align_of::<Overaligned>(),
                |payload| (payload as *mut Overaligned).write(Overaligned(1)),
            )
        };
        assert_eq!(
            over.payload::<Overaligned>() as usize - over.as_ptr() as usize,
            GcHeader::payload_offset_for(OVERALIGNED.align())
        );
        assert_eq!(over.payload::<Overaligned>() as usize % 64, 0);
    }

    /// Every allocation carries its heap's identity, and only that heap's.
    #[test]
    fn allocations_carry_their_owning_heap() {
        let first = Heap::new();
        let second = Heap::new();
        let mine = first.alloc(&INT, 1_i64);

        assert_eq!(mine.header().heap_id(), Some(first.id()));
        assert!(first.owns(mine));
        assert!(!second.owns(mine));
    }

    /// Sweep poisons before it unregisters, so the storage stops claiming to be
    /// a typed object the moment it stops being one. This is the precondition
    /// for reusing swept arena storage (RT-01): without it, a stale `GcRef`
    /// would be traced into whatever the allocator put there next.
    #[test]
    fn sweeping_poisons_the_reclaimed_header() {
        let heap = Heap::new();
        let doomed = heap.alloc(&INT, 1_i64);
        assert!(!doomed.header().is_poisoned());

        heap.collect_with(&RootScope::new());

        assert_eq!(heap.stats().live_count, 0);
        assert!(doomed.header().is_poisoned());
        assert_eq!(doomed.header().heap_id(), None);
    }

    /// A stale root — one naming storage this heap has already swept — must be
    /// rejected by the same provenance check that rejects a foreign root,
    /// rather than dereferencing the finalized payload's descriptor.
    #[test]
    fn a_swept_reference_is_not_traced_again() {
        let heap = Heap::new();
        let stale = heap.alloc(&INT, 1_i64);
        heap.collect_with(&RootScope::new());
        assert!(stale.header().is_poisoned());

        let mut stale_roots = RootScope::new();
        stale_roots.root(stale);
        heap.collect_with(&stale_roots);

        assert_eq!(
            heap.stats().live_count,
            0,
            "a poisoned header must not be resurrected by rooting it"
        );
    }

    /// A reset heap is a different heap, so the refs it minted before the reset
    /// no longer pass the provenance check even though the arena may hand their
    /// addresses out again.
    #[test]
    fn reset_mints_a_new_heap_identity() {
        let mut heap = Heap::new();
        let before = heap.id();
        let _ = heap.alloc(&INT, 1_i64);

        heap.reset();

        assert_ne!(heap.id(), before);
        assert_eq!(heap.alloc(&INT, 2_i64).header().heap_id(), Some(heap.id()));
    }
}
