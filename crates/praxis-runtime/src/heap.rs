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
use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::ptr::NonNull;

use bumpalo::Bump;

use crate::descriptor::{Payload, TypeDescriptor};
use crate::gc::{GcHeader, GcRef, HeapId, BLACK, GREY, WHITE};
use crate::roots::{RootSet, RuntimeRoots};
use crate::Tracer;

/// Proof that the collector was given a chance to run at this point.
///
/// [`Heap::alloc`] and [`Heap::alloc_with`] demand one, and [`Heap::pace`] —
/// which *performs* the [`Heap::maybe_collect`] — is its only producer. The
/// field is private to this module, so "allocate on the paced path without
/// pacing" has no spelling: obtaining the token is the pacing.
///
/// `pace` in turn takes a [`RuntimeRoots`], which is constructible only from a
/// live `RuntimeContext` and is exhaustive over the runtime's owners, so the
/// collection a token permits can never run against a partial root set.
///
/// Deliberately neither `Copy` nor `Clone`: one token, one allocation. A
/// wrapper that allocates twice paces twice.
#[must_use = "a Safepoint is the permission to allocate; dropping it wasted a pacing check"]
pub struct Safepoint<'a>(PhantomData<&'a Heap>);

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
    ///
    /// A `Cell`, not a `RefCell`: a `usize` is `Copy`, so there is nothing to
    /// borrow, and paying a borrow-flag round trip for it on the hottest path in
    /// the runtime buys only a double-borrow panic that a scalar cannot need.
    bytes_since_collect: Cell<usize>,
    /// The threshold at/above which [`Heap::maybe_collect`] runs a collection.
    /// Doubled after each collection so the heap grows geometrically (amortized
    /// O(1) allocations per collection), a standard GC pacing heuristic.
    ///
    /// A `Cell` for [`Heap::bytes_since_collect`]'s reason.
    collect_threshold: Cell<usize>,
    /// Swept blocks, keyed by the exact `[header|payload]` layout they were laid
    /// out with, ready to be handed back out (RT-01).
    ///
    /// A `bumpalo::Bump` can only reclaim *everything* — it has no route to
    /// return one block — so sweep finalized and unregistered a dead object but
    /// its bytes stayed spent. A program that allocated and collected a bounded
    /// working set in a loop grew the arena forever while `live_count` returned
    /// to zero each cycle.
    ///
    /// Keying on the whole layout rather than on a descriptor is what makes
    /// handing a block back sound: an exact `(size, align)` match means the
    /// reused storage is large enough and correctly aligned for whatever the
    /// next allocation puts there, whoever allocated it first.
    ///
    /// *How* the bucket is found is [`FreeList`]'s business and is orthogonal to
    /// that argument — see its doc comment for why it stopped being a hash map.
    free: RefCell<FreeList>,
}

/// What ran a collection. Only allocation pressure grows the pacing threshold:
/// a host that collects on a schedule is not evidence the program needs a
/// larger budget between collections (RT-04).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Trigger {
    /// [`Heap::maybe_collect`] found the pacing counter at the threshold.
    Paced,
    /// A host called [`Heap::collect`] outright.
    Explicit,
}

/// The size and alignment of one whole `[header|payload]` allocation — the
/// free-list key. Not the payload's own layout: the payload's offset within the
/// block is recomputed and re-recorded on every reuse, so two descriptors that
/// split the same total differently still share a block.
///
/// Deliberately not `Hash`. The free list used to be a `HashMap` keyed on this,
/// and hashing a 16-byte key twice per object — once to find a bucket in
/// [`Heap::alloc_raw`], once to file a swept block in [`Heap::sweep`] — was 34%
/// of runtime on `collatz` and 33% on `primes`, ahead of the generated code
/// (docs/handovers/21-where-the-time-goes.md §3.1). Withholding the derive makes
/// re-introducing a hash lookup a compile error rather than a silent regression.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct BlockLayout {
    size: usize,
    align: usize,
}

impl BlockLayout {
    /// The block `descriptor`'s objects occupy, and where their payload starts
    /// within it. The single calculation both [`Heap::alloc_raw`] and
    /// [`Heap::sweep`] read, so a block can only be filed under the layout it
    /// actually has.
    ///
    /// # Panics
    /// Panics if the payload alignment exceeds what a `GcHeader` can record, or
    /// if the total size overflows.
    fn of(descriptor: &TypeDescriptor) -> (usize, BlockLayout) {
        let payload_align = descriptor.align();
        let payload_offset = GcHeader::payload_offset_for(payload_align);
        let size = payload_offset
            .checked_add(descriptor.size())
            .expect("allocation size overflow");
        let align = std::mem::align_of::<GcHeader>().max(payload_align);
        (payload_offset, BlockLayout { size, align })
    }

    /// This block as a [`Layout`], for the arena.
    fn layout(self) -> Layout {
        Layout::from_size_align(self.size, self.align).expect("invalid layout")
    }
}

/// The alignment every block has, because every block starts with a
/// [`GcHeader`]. [`BlockLayout::of`] takes the max of this and the payload's own
/// alignment, and no built-in payload is over-aligned — so in a real program
/// this is not a lower bound, it is the answer.
const MIN_BLOCK_ALIGN: usize = std::mem::align_of::<GcHeader>();

/// One past the largest block size [`SizeClass`] indexes directly.
///
/// The set of block layouts a Praxis program can produce is *closed*:
/// [`TypeDescriptor::builtin`] is the only non-test constructor and
/// [`crate::descriptor::BUILTINS`] is the whole list of descriptors that call
/// it. A record, enum, tuple or closure does not widen it either — each boxes
/// its schema behind a pointer and its fields behind a `Vec<GcRef>`, so its
/// payload is one fixed struct whatever its arity. The largest block the
/// language can ask for today is well under this bound, and
/// `every_builtin_block_has_a_size_class` is what keeps that honest: a
/// twenty-third built-in with a bigger payload fails that test rather than
/// silently degrading to the linear scan.
const SIZE_CLASSES: usize = 128;

/// The bucket index for a block whose alignment is exactly [`MIN_BLOCK_ALIGN`]
/// and whose size fits [`SIZE_CLASSES`] — which is every block a real program
/// allocates.
///
/// **This newtype is how indexing by size alone stays exact.** RT-01's soundness
/// rests on handing a swept block only to a request of the same `(size, align)`,
/// and a bare `size` index would break that for two layouts that share a size
/// but not an alignment: file a `{48, 8}` block, hand it to a `{48, 16}`
/// request, and the payload lands at `base + 32` where `base` is only 8-aligned.
/// [`SizeClass::of`] is the only constructor and it rejects exactly that case,
/// so "the index is the size" and "the bucket holds one layout" cannot come
/// apart. Everything it rejects is keyed on the whole [`BlockLayout`] instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct SizeClass(usize);

impl SizeClass {
    /// `block`'s class, or `None` if it is over-aligned or too large — in which
    /// case it belongs in [`FreeList::oversize`], not in the array.
    #[inline]
    fn of(block: BlockLayout) -> Option<SizeClass> {
        if block.align == MIN_BLOCK_ALIGN && block.size < SIZE_CLASSES {
            Some(SizeClass(block.size))
        } else {
            None
        }
    }

    /// The one layout this class holds. The inverse of [`SizeClass::of`], and it
    /// exists so the round trip is testable rather than merely asserted — hence
    /// test-only: nothing on the allocation path needs to go this way.
    #[cfg(test)]
    #[inline]
    fn layout(self) -> BlockLayout {
        BlockLayout {
            size: self.0,
            align: MIN_BLOCK_ALIGN,
        }
    }
}

/// Swept blocks awaiting reuse, bucketed by layout.
///
/// This was a `HashMap<BlockLayout, Vec<NonNull<u8>>>` with the default
/// `RandomState`, probed once per allocation and once per swept block. A program
/// uses single digits of distinct block layouts, and SipHash over a 16-byte key
/// — twice per object — was 34% of `collatz`'s runtime, ahead of everything else
/// including the generated code (docs/handovers/21-where-the-time-goes.md §3.1).
/// The hash bought nothing: the key set is closed and small enough to address
/// directly.
struct FreeList {
    /// `classed[c.0]` holds blocks — and only blocks — whose layout is
    /// `SizeClass(c.0).layout()`. [`SizeClass::of`] is the only way in, so the
    /// index *is* the size and the alignment is [`MIN_BLOCK_ALIGN`] by
    /// construction.
    classed: [Vec<NonNull<u8>>; SIZE_CLASSES],
    /// Everything [`SizeClass::of`] rejects: an over-aligned payload, or one
    /// larger than the class array. No built-in descriptor produces either, so
    /// this list is empty in every real program — which is why a linear scan is
    /// the right shape for it. It is still keyed on the whole [`BlockLayout`]
    /// and compared with `==`, so it is exact for the array's reason.
    oversize: Vec<(BlockLayout, Vec<NonNull<u8>>)>,
}

impl FreeList {
    fn new() -> FreeList {
        FreeList {
            classed: std::array::from_fn(|_| Vec::new()),
            oversize: Vec::new(),
        }
    }

    /// A filed block of exactly `block`'s layout, if one is available.
    #[inline]
    fn take(&mut self, block: BlockLayout) -> Option<NonNull<u8>> {
        match SizeClass::of(block) {
            Some(class) => self.classed[class.0].pop(),
            None => self
                .oversize
                .iter_mut()
                .find(|(filed, _)| *filed == block)
                .and_then(|(_, blocks)| blocks.pop()),
        }
    }

    /// File `base`, a swept block of exactly `block`'s layout, for reuse.
    #[inline]
    fn put(&mut self, block: BlockLayout, base: NonNull<u8>) {
        match SizeClass::of(block) {
            Some(class) => self.classed[class.0].push(base),
            None => match self.oversize.iter_mut().find(|(filed, _)| *filed == block) {
                Some((_, blocks)) => blocks.push(base),
                None => self.oversize.push((block, vec![base])),
            },
        }
    }

    /// Forget every filed block. Both halves, always: a block left in `oversize`
    /// after [`Heap::reset`] points into arena storage that no longer exists.
    fn clear(&mut self) {
        for bucket in &mut self.classed {
            bucket.clear();
        }
        self.oversize.clear();
    }

    /// How many blocks are filed, across both halves.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.classed.iter().map(Vec::len).sum::<usize>()
            + self
                .oversize
                .iter()
                .map(|(_, blocks)| blocks.len())
                .sum::<usize>()
    }
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
            bytes_since_collect: Cell::new(0),
            collect_threshold: Cell::new(INITIAL_COLLECT_THRESHOLD),
            free: RefCell::new(FreeList::new()),
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

    /// Bytes charged against the pacing counter since the last collection.
    ///
    /// Test-only, and deliberately not part of [`HeapStats`]: pacing is the
    /// collector's own schedule and nothing outside this crate has any business
    /// reading it, let alone deciding from it. It is here so a sibling module's
    /// test can assert the RT-04 property that a *non*-collectable allocation
    /// leaves the schedule alone (see [`Heap::alloc_immortal`]).
    #[cfg(test)]
    pub(crate) fn bytes_since_collect(&self) -> usize {
        self.bytes_since_collect.get()
    }

    /// Allocate an immortal object: same layout as [`Heap::alloc`], but **not**
    /// registered in the live set, so the collector never reclaims it (§4.3,
    /// M3 deliverable). Used for the `Unit`/`Bool` singletons.
    ///
    /// Restricted to [`Immortals::new`](crate::immortal::Immortals::new) by the
    /// [`ImmortalWitness`](crate::immortal::ImmortalWitness) it takes, which
    /// only that module can construct. The restriction is load-bearing twice
    /// over: an immortal is invisible to sweep *and* to [`Heap`]'s `Drop`, so
    /// every immortal payload must be `Copy` (nothing to finalize) and must be
    /// minted exactly once at startup. Minting one per call — which the `Bool`
    /// wrappers used to do — is unregistered arena storage nothing ever
    /// reclaims (RT-03).
    ///
    /// **This allocation is pacing-neutral, and that is not an optimization.**
    /// [`Heap::alloc_raw`] charges every block against `bytes_since_collect`
    /// because pacing measures the pressure a program is putting on the
    /// collector (RT-04) — and an object no collection can ever reclaim exerts
    /// none: collecting harder does not give one byte of it back. Charging it
    /// anyway made the immortal table a hidden GC-schedule change, because the
    /// interned small-`Int` table ([`crate::small_int`]) is ~40 KiB against a
    /// 64 KiB [`INITIAL_COLLECT_THRESHOLD`]: every program's *first* real
    /// allocation would have arrived with two thirds of its budget already
    /// spent, and widening the interned range would have moved the first
    /// collection of every program in the language. So the counter is
    /// snapshotted and restored around the call rather than the charge being
    /// skipped inside `alloc_raw`, which would need a flag on the one path
    /// every real allocation takes.
    pub(crate) fn alloc_immortal<T: Copy>(
        &self,
        payload: Payload<T>,
        value: T,
        _witness: crate::immortal::ImmortalWitness,
    ) -> GcRef {
        let descriptor = payload.descriptor();
        let charged_before = self.bytes_since_collect.get();
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
        // Un-charge the block: see this function's doc. Restoring the snapshot
        // rather than subtracting the block size keeps this correct whatever
        // `alloc_raw` decides an object costs (it also charges the descriptor's
        // owned bytes, which for a `Copy` immortal is zero today and need not
        // stay so).
        self.bytes_since_collect.set(charged_before);
        r
    }

    /// Give the collector a chance to run, and hand back the [`Safepoint`] that
    /// permits one allocation.
    ///
    /// This is the only producer of a `Safepoint`, and it is what makes
    /// "allocate without pacing" unwritable on the paced path: the token
    /// [`Heap::alloc`] demands cannot be obtained except by performing the
    /// [`Heap::maybe_collect`] that mints it, against the whole
    /// [`RuntimeRoots`] (P0-08b).
    pub fn pace(&self, roots: &RuntimeRoots<'_>) -> Safepoint<'_> {
        self.maybe_collect(roots);
        Safepoint(PhantomData)
    }

    /// Allocate an object with the given descriptor and a `Copy` payload `value`,
    /// returning a reference to it.
    ///
    /// Takes the [`Safepoint`] minted by [`Heap::pace`]: an allocation on this
    /// path has necessarily given the collector its chance. For payloads that
    /// own Rust resources (`Box<str>`, `VecPayload`) use [`Heap::alloc_with`],
    /// which writes the value via `ptr::write` so its `Drop` later runs
    /// correctly.
    ///
    /// The descriptor arrives as a [`Payload<T>`], so "this value is not that
    /// descriptor's payload" is a type error at the call rather than an assert
    /// here (REP-02).
    pub fn alloc<T: Copy>(
        &self,
        _safepoint: Safepoint<'_>,
        payload: Payload<T>,
        value: T,
    ) -> GcRef {
        self.alloc_unpaced(payload, value)
    }

    /// Allocate an object whose payload owns Rust resources, initializing it
    /// with `init`. `init` receives a pointer to the uninitialized payload bytes
    /// and must fully initialize them.
    ///
    /// This path keeps its runtime layout assertions, deliberately: `init` is a
    /// closure writing through a `*mut u8`, so there is no payload type for a
    /// [`Payload<T>`] to carry (REP-02 removed the checks from the `Copy` path,
    /// where the type *is* available). Every caller here writes a specific
    /// non-`Copy` payload — a `Box<str>`, a `VecPayload` — and passes that type's
    /// own `size_of`/`align_of`, which is why the assertions have never fired.
    ///
    /// # Safety
    /// `init` must initialize the payload in place and must not panic after
    /// partial initialization (if it does, the payload's `Drop` will not run,
    /// leaking the partially-initialized resources). The descriptor's `size`/
    /// `align` must match the value `init` writes.
    pub unsafe fn alloc_with(
        &self,
        _safepoint: Safepoint<'_>,
        descriptor: &'static TypeDescriptor,
        size: usize,
        align: usize,
        init: impl FnOnce(*mut u8),
    ) -> GcRef {
        // SAFETY: forwarded from the caller's contract above.
        unsafe { self.alloc_with_unpaced(descriptor, size, align, init) }
    }

    /// [`Heap::alloc`] **without** pacing the collector.
    ///
    /// The heap grows by this allocation and nothing here gives the collector a
    /// chance to reclaim; something else must pace, or the arena grows until it
    /// does. **One** caller legitimately cannot pace, and it is the only one:
    /// the host's own `Runtime::alloc_*` helpers, which hold their results in
    /// Rust locals that no root set can see, so a collection *here* would
    /// reclaim the value being returned.
    ///
    /// The parser interpreter used to be the second. It held seventeen
    /// `Vec<GcRef>` intermediates that no root set could see, so pacing it
    /// would have reclaimed the values it was assembling — which is why the
    /// back door had to exist and why the roots and the move to this path had
    /// to be one commit (IPR-14, ADR-040 Decision 3, hazard H1). It has
    /// `NativeScope`s now and allocates through [`Heap::alloc`]. Do not add a
    /// third caller: the argument that justified this one is "no root set can
    /// see my locals", and the answer to that is a `NativeScope`, not this.
    ///
    /// A `praxis_*` wrapper must never use this: generated code roots what it
    /// holds across a call the manifest declares `Allocates`, which is exactly
    /// what makes the paced path safe there.
    pub(crate) fn alloc_unpaced<T: Copy>(&self, payload: Payload<T>, value: T) -> GcRef {
        // SAFETY: `T: Copy`, so writing the bytes is sufficient initialization
        // (no `Drop` to run later); and `Payload<T>` is the descriptor's own
        // payload type, so the bytes fit the block `alloc_raw` lays out.
        unsafe { self.alloc_raw(payload.descriptor(), |p| (p as *mut T).write(value)) }
    }

    /// [`Heap::alloc_with`] **without** pacing the collector. See
    /// [`Heap::alloc_unpaced`] for who may call this and why.
    ///
    /// # Safety
    /// As [`Heap::alloc_with`].
    pub(crate) unsafe fn alloc_with_unpaced(
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
    /// Storage comes from the free list of swept blocks when one of the exact
    /// layout is available, and from the arena otherwise (RT-01).
    ///
    /// # Safety
    /// `init` must fully initialize `descriptor.size` bytes of the payload and
    /// the bytes must be valid as the descriptor's payload type thereafter.
    unsafe fn alloc_raw(
        &self,
        descriptor: &'static TypeDescriptor,
        init: impl FnOnce(*mut u8),
    ) -> GcRef {
        // Where the payload starts is `GcHeader::payload_offset_for`'s decision
        // and nobody else's — the same call the header records and `payload()`
        // reads back — and the block that holds it is `BlockLayout::of`'s, the
        // same call `sweep` files a reclaimed block under.
        let (payload_offset, block) = BlockLayout::of(descriptor);
        let recorded_offset = u16::try_from(payload_offset).unwrap_or_else(|_| {
            panic!(
                "payload alignment {} of descriptor {} exceeds the \
                 largest offset a GcHeader can record",
                descriptor.align(),
                descriptor.name
            )
        });

        // Reuse a swept block of this exact layout, or take fresh arena bytes.
        // The block was poisoned and its payload finalized before it was filed,
        // so nothing outstanding claims it is still a typed object.
        let reused = self.free.borrow_mut().take(block);
        let base = match reused {
            Some(base) => base,
            None => self.arena.alloc_layout(block.layout()),
        };
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
        // Reused storage counts too: pacing measures the pressure a program is
        // putting on the collector, not the arena's high-water mark.
        //
        // The block is only part of what the object costs. A `Text` is 40 bytes
        // of block and a `Box<str>` of whatever length the program read; a
        // freshly built `Vec` is 40 bytes and a buffer of `capacity` refs. The
        // descriptor measures the rest, so a text-heavy program no longer
        // under-reports its pressure by essentially its whole footprint
        // (RT-04). Growth *after* this point — a `push` that reallocates — is
        // still uncharged; its elements are themselves paced allocations, so
        // the residual under-count is the spine, not the contents.
        // SAFETY: `init` has run, so the payload is a valid value of `descriptor`.
        let owned = unsafe { descriptor.owned_bytes_of(payload_ptr) };
        self.bytes_since_collect
            .set(self.bytes_since_collect.get() + block.size.saturating_add(owned));
        // SAFETY: `nn` points at the just-allocated, initialized header.
        unsafe { GcRef::from_non_null(nn) }
    }

    /// Run a mark-and-sweep collection (§12.1, ADR-011).
    ///
    /// Every `GcRef` reachable from `roots` (plus everything transitively
    /// reachable through descriptor `trace` callbacks) is marked black and
    /// survives; everything else is finalized via `drop_value` and reclaimed.
    pub fn collect(&self, roots: &RuntimeRoots<'_>) {
        self.collect_inner(roots, Trigger::Explicit);
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
        self.collect_inner(roots, Trigger::Explicit);
    }

    /// [`Heap::maybe_collect`] against an arbitrary root set. Test-only, and
    /// the counterpart of [`Heap::collect_with`]: it is the only way an
    /// in-crate test can exercise the *paced* path, which is the one that grows
    /// the threshold.
    #[cfg(test)]
    pub fn maybe_collect_with(&self, roots: &dyn RootSet) -> bool {
        let should = self.bytes_since_collect.get() >= self.collect_threshold.get();
        if should {
            self.collect_inner(roots, Trigger::Paced);
        }
        should
    }

    fn collect_inner(&self, roots: &dyn RootSet, trigger: Trigger) {
        self.mark(roots);
        self.sweep();
        self.bytes_since_collect.set(0);
        // Grow the threshold geometrically, so allocations per collection are
        // amortized O(1) — but only when *pacing* was what ran this collection.
        //
        // Doubling on an explicit collection too meant a host that collected on
        // a schedule (the debugger between REPL commands, a test between
        // phases) pushed the automatic threshold up without any allocation
        // pressure having caused it, and after a few such calls the program was
        // effectively running without a collector (RT-04).
        if trigger == Trigger::Paced {
            self.collect_threshold.set(
                self.collect_threshold
                    .get()
                    .saturating_mul(2)
                    .max(INITIAL_COLLECT_THRESHOLD),
            );
        }
    }

    /// Run a collection if allocation pressure has reached the threshold,
    /// rooting from `roots`. Reached from every allocation through
    /// [`Heap::pace`] (§12.4), so collection happens automatically inside JIT'd
    /// code — this is what makes "nested vectors survive collection" (§19 M5
    /// acceptance) testable without the host forcing it.
    ///
    /// Returns `true` if a collection ran.
    pub fn maybe_collect(&self, roots: &RuntimeRoots<'_>) -> bool {
        let should = self.bytes_since_collect.get() >= self.collect_threshold.get();
        if should {
            self.collect_inner(roots, Trigger::Paced);
        }
        should
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

    /// Sweep phase: finalize every still-white allocation, unregister it, and
    /// file its storage for reuse.
    fn sweep(&self) {
        let mut live = self.live.borrow_mut();
        let mut free = self.free.borrow_mut();
        let mut i = 0;
        while i < live.len() {
            // SAFETY: every entry in `live` was pushed by `alloc_raw` and points
            // at an initialized header + payload.
            let header = unsafe { live[i].as_ref() };
            if header.mark_color() == WHITE {
                // Finalize: run the descriptor's drop_value on the payload.
                let desc = header.descriptor();
                let payload = header.payload::<u8>();
                // Read the block's layout while the descriptor is still there —
                // `poison` takes it away.
                let (_, block) = BlockLayout::of(desc);
                // SAFETY: payload matches `desc` and is about to become invalid.
                unsafe { (desc.drop_value)(payload) };
                // Poison before unregistering, so a stale `GcRef` that still
                // names this storage is rejected by the mark phase's provenance
                // check instead of being traced through a finalized payload.
                // This is also RT-01's precondition: between filing the block
                // and handing it out again, it must not claim to be a typed
                // object, or a stale reference would be traced through whatever
                // the allocator put there next (hazard H7).
                header.poison();
                free.put(block, live[i].cast::<u8>());
                live.swap_remove(i);
            } else {
                // Reset to white for the next collection and keep it.
                header.set_mark_color(WHITE);
                i += 1;
            }
        }
    }

    /// Finalize and unregister **every** still-live allocation, reachable or
    /// not, and discard the free list.
    ///
    /// Sweep only finalizes what it proved unreachable, so this is the other
    /// half: at teardown, whatever a program left live still owns the
    /// `Box<str>` / `Vec` / `HashMap` backing allocations its payload points
    /// at, and those are not in the arena — dropping the `Bump` reclaims the
    /// `[header|payload]` blocks and leaks everything they own (RT-02).
    ///
    /// After this the heap holds nothing, so [`Heap::reset`] and `Drop` can
    /// both use it and neither can double-finalize.
    fn finalize_all(&self) {
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
        // Every filed block points into storage that is about to go away.
        self.free.borrow_mut().clear();
    }

    /// Reset the heap to empty, dropping everything. Used by tests and, later,
    /// runtime teardown. Immortal singletons must be re-allocated afterwards —
    /// which since the small-`Int` table ([`crate::small_int`]) means the whole
    /// `Immortals` value, not just the three singletons: a `RuntimeContext`
    /// minted before the reset holds `unit_ref`, `true_ref`, `false_ref` **and**
    /// a `small_ints` pointer, and every one of them names storage the arena is
    /// now free to hand out again.
    pub fn reset(&mut self) {
        // Finalize every live allocation before tearing down the arena.
        self.finalize_all();
        self.arena.reset();
        // Pacing is part of the heap's state, so a reset heap paces like a fresh
        // one. Leaving the counter and the geometrically-grown threshold in
        // place meant a reset heap could run for megabytes before its first
        // collection, or collect on its very first allocation (RT-04).
        self.bytes_since_collect.set(0);
        self.collect_threshold.set(INITIAL_COLLECT_THRESHOLD);
        // A reset heap is a different heap: the immortals it handed out are
        // gone, and every `GcRef` minted before this point names storage the
        // arena is free to hand out again. A fresh identity makes those refs
        // fail the mark phase's provenance check rather than be traced.
        self.id = HeapId::mint();
    }
}

impl Drop for Heap {
    /// Finalize whatever the program left live (RT-02).
    ///
    /// `Bump::drop` reclaims the `[header|payload]` blocks, and nothing else:
    /// the `Box<str>` behind a `Text`, the `Vec<GcRef>` behind a `Vec[T]`, the
    /// `HashMap` behind a `Map[K,V]` are ordinary Rust allocations the arena
    /// never owned. Without this, every object still reachable at teardown
    /// leaked its backing store.
    ///
    /// **A `GcRef` does not outlive the heap.** Finalizing here makes that a
    /// visible use-after-free for a host that reads one afterwards, where it
    /// used to be a quiet read of stale-but-intact bytes (hazard H8). The two
    /// consumers that take a value out of the runtime were audited with this
    /// change:
    ///
    /// * `praxis-cli/src/run.rs` takes the crash snapshot, renders it, then
    ///   moves the `Runtime` into the `DebugSession` the `Repl` owns. `Repl`
    ///   declares `snapshot` before `session`, so the snapshot is dropped
    ///   first and nothing reads a `GcRef` after teardown.
    /// * `praxis-debugger/src/repl.rs` replaces its snapshot after a
    ///   `restart`/`reload` while the runtime is still alive.
    ///
    /// `CrashSnapshot` and `ParseDetail` hold `GcRef`s but have no `Drop` that
    /// dereferences one, so field order within `Runtime` — where `heap` is
    /// declared first and therefore dropped first — is safe either way. No
    /// descriptor's `drop_value` dereferences a `GcRef` either, so finalization
    /// order among live objects does not matter.
    fn drop(&mut self) {
        self.finalize_all();
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
    use crate::scalars::{INT, INT_PAYLOAD, UNIT_PAYLOAD};
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
        let r = heap.alloc_unpaced(INT_PAYLOAD, 42_i64);
        assert_eq!(r.descriptor().name, "Int");
        // SAFETY: `r` was allocated with INT, payload is i64.
        let v = unsafe { *r.payload::<i64>() };
        assert_eq!(v, 42);
        assert_eq!(heap.stats().live_count, 1);
    }

    #[test]
    fn collect_reclaims_unrooted_allocation() {
        let heap = Heap::new();
        let _ = heap.alloc_unpaced(INT_PAYLOAD, 1_i64);
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
        let r = heap.alloc_unpaced(INT_PAYLOAD, 7_i64);
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
            .map(|&v| heap.alloc_unpaced(INT_PAYLOAD, v))
            .collect();

        // Wrap in a Vec[T] payload. Element type is recorded in the payload
        // (ADR-013).
        let vec_ref = unsafe {
            heap.alloc_with_unpaced(
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
            let _ = heap.alloc_unpaced(INT_PAYLOAD, 1000 + i);
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
            let elems: Vec<GcRef> = ints
                .iter()
                .map(|&v| heap.alloc_unpaced(INT_PAYLOAD, v))
                .collect();
            unsafe {
                heap.alloc_with_unpaced(
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
            heap.alloc_with_unpaced(
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
        let _ = heap.alloc_unpaced(UNIT_PAYLOAD, ());

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
            heap.alloc_with_unpaced(
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
    fn dropping_heap_finalizes_live_owned_payloads() {
        let drops = Arc::new(AtomicUsize::new(0));
        {
            let heap = Heap::new();
            unsafe {
                heap.alloc_with_unpaced(
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

    /// Reachability is irrelevant at teardown: an object the collector would
    /// have *kept* still owns its backing allocations, and the heap is the last
    /// owner. Rooting it must not exempt it.
    #[test]
    fn dropping_heap_finalizes_reachable_payloads_too() {
        let drops = Arc::new(AtomicUsize::new(0));
        {
            let heap = Heap::new();
            let mut scope = RootScope::new();
            let probe = unsafe {
                heap.alloc_with_unpaced(
                    &DROP_PROBE,
                    std::mem::size_of::<DropProbe>(),
                    std::mem::align_of::<DropProbe>(),
                    |payload| (payload as *mut DropProbe).write(DropProbe(Arc::clone(&drops))),
                )
            };
            scope.root(probe);
            heap.collect_with(&scope);
            assert_eq!(drops.load(Ordering::SeqCst), 0, "a rooted probe survives");
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    /// `reset` and `Drop` share one finalizer loop, and it empties the registry
    /// — so a heap that is reset and then dropped finalizes each payload once,
    /// not twice.
    #[test]
    fn resetting_then_dropping_finalizes_each_payload_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        {
            let mut heap = Heap::new();
            unsafe {
                heap.alloc_with_unpaced(
                    &DROP_PROBE,
                    std::mem::size_of::<DropProbe>(),
                    std::mem::align_of::<DropProbe>(),
                    |payload| (payload as *mut DropProbe).write(DropProbe(Arc::clone(&drops))),
                );
            }
            heap.reset();
            assert_eq!(drops.load(Ordering::SeqCst), 1, "reset finalizes");
        }
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "the drop after a reset must find nothing left to finalize"
        );
    }

    #[test]
    fn overaligned_payload_accessor_matches_initialized_address() {
        let initialized_at = Cell::new(std::ptr::null_mut());
        let heap = Heap::new();
        let value = unsafe {
            heap.alloc_with_unpaced(
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
        let value = first.alloc_unpaced(INT_PAYLOAD, 1_i64);
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

    /// Allocate until the pacing counter runs a collection, and return whether
    /// one happened within `limit` allocations.
    fn allocate_until_paced(heap: &Heap, limit: usize) -> bool {
        for i in 0..limit {
            let _ = heap.alloc_unpaced(INT_PAYLOAD, i as i64);
            if heap.maybe_collect_with(&RootScope::new()) {
                return true;
            }
        }
        false
    }

    #[test]
    fn reset_restores_collection_pacing() {
        let mut heap = Heap::new();
        // Only a *paced* collection grows the threshold, so drive one.
        assert!(allocate_until_paced(&heap, 100_000));
        let _ = heap.alloc_unpaced(INT_PAYLOAD, 1_i64);
        assert_ne!(heap.bytes_since_collect.get(), 0);
        assert_ne!(heap.collect_threshold.get(), INITIAL_COLLECT_THRESHOLD);

        heap.reset();

        assert_eq!(heap.bytes_since_collect.get(), 0);
        assert_eq!(heap.collect_threshold.get(), INITIAL_COLLECT_THRESHOLD);
    }

    /// A host that collects on a schedule — the debugger between REPL commands,
    /// a test between phases — is not evidence that the program needs a bigger
    /// budget between automatic collections. Doubling on every explicit collect
    /// meant a few such calls left the program effectively running without a
    /// collector (RT-04).
    #[test]
    fn an_explicit_collection_does_not_grow_the_pacing_threshold() {
        let heap = Heap::new();
        for _ in 0..8 {
            heap.collect_with(&RootScope::new());
        }
        assert_eq!(
            heap.collect_threshold.get(),
            INITIAL_COLLECT_THRESHOLD,
            "an explicit collection must leave the automatic threshold alone"
        );

        assert!(allocate_until_paced(&heap, 100_000));
        assert_eq!(
            heap.collect_threshold.get(),
            INITIAL_COLLECT_THRESHOLD * 2,
            "a paced collection is what grows it"
        );
    }

    /// Pacing counts what an object *costs*, not the size of its fixed block.
    /// A `Text` is 40 bytes of block plus a `Box<str>` of whatever the program
    /// read; charging only the block made a text-heavy program invisible to the
    /// collector — it under-reported its footprint by essentially all of it
    /// (RT-04).
    #[test]
    fn pacing_charges_the_bytes_a_payload_owns() {
        use crate::text::{TextPayload, TEXT};

        let alloc_text = |heap: &Heap, len: usize| {
            let owned: Box<str> = "x".repeat(len).into_boxed_str();
            // SAFETY: TextPayload matches TEXT's size/align and is initialized.
            unsafe {
                heap.alloc_with_unpaced(
                    &TEXT,
                    std::mem::size_of::<TextPayload>(),
                    std::mem::align_of::<TextPayload>(),
                    |p| (p as *mut TextPayload).write(TextPayload::Owned(owned)),
                )
            }
        };

        let small = Heap::new();
        alloc_text(&small, 8);
        let big = Heap::new();
        alloc_text(&big, 64 * 1024);

        let charged_small = small.bytes_since_collect.get();
        let charged_big = big.bytes_since_collect.get();
        assert_eq!(
            charged_big - charged_small,
            64 * 1024 - 8,
            "the Box<str> must be charged at its real length"
        );

        // And the consequence that matters: one large Text is enough pressure
        // to reach the threshold, where before it took thousands of them.
        assert!(
            big.maybe_collect_with(&RootScope::new()),
            "a 64 KiB Text must reach the 64 KiB threshold on its own"
        );
    }

    /// A source-slice `Text` borrows its owner's buffer. Charging its length
    /// would count the same bytes once per slice — a parser that slices a
    /// megabyte of input into a thousand fields would report a gigabyte.
    #[test]
    fn a_source_slice_text_is_charged_nothing_beyond_its_block() {
        use crate::text::{TextPayload, TEXT};
        let heap = Heap::new();

        let owner: Box<str> = "x".repeat(4096).into_boxed_str();
        // SAFETY: TextPayload matches TEXT's size/align and is initialized.
        let owner_ref = unsafe {
            heap.alloc_with_unpaced(
                &TEXT,
                std::mem::size_of::<TextPayload>(),
                std::mem::align_of::<TextPayload>(),
                |p| (p as *mut TextPayload).write(TextPayload::Owned(owner)),
            )
        };
        let after_owner = heap.bytes_since_collect.get();

        // SAFETY: as above; the range lands inside the owner.
        unsafe {
            heap.alloc_with_unpaced(
                &TEXT,
                std::mem::size_of::<TextPayload>(),
                std::mem::align_of::<TextPayload>(),
                |p| {
                    // SAFETY: `owner_ref` is the live Text allocated just above.
                    let slice = crate::text::SourceSlice::new(owner_ref, 0, 4096)
                        .expect("the whole owner is a valid slice of itself");
                    (p as *mut TextPayload).write(TextPayload::Slice(slice))
                },
            );
        }

        let (_, block) = BlockLayout::of(&TEXT);
        assert_eq!(
            heap.bytes_since_collect.get() - after_owner,
            block.size,
            "a slice owns no bytes of its own"
        );
    }

    #[test]
    fn repeated_collection_reuses_dead_object_storage() {
        let heap = Heap::new();
        const OBJECTS_PER_CYCLE: usize = 4_096;

        for i in 0..OBJECTS_PER_CYCLE {
            let _ = heap.alloc_unpaced(INT_PAYLOAD, i as i64);
        }
        heap.collect_with(&RootScope::new());
        let first_cycle_bytes = heap.arena.allocated_bytes();

        for cycle in 1..=8 {
            for i in 0..OBJECTS_PER_CYCLE {
                let _ = heap.alloc_unpaced(INT_PAYLOAD, (cycle * OBJECTS_PER_CYCLE + i) as i64);
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

        let int = heap.alloc_unpaced(INT_PAYLOAD, 1_i64);
        assert_eq!(
            int.payload::<i64>() as usize - int.as_ptr() as usize,
            GcHeader::payload_offset_for(INT.align())
        );

        let over = unsafe {
            heap.alloc_with_unpaced(
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
        let mine = first.alloc_unpaced(INT_PAYLOAD, 1_i64);

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
        let doomed = heap.alloc_unpaced(INT_PAYLOAD, 1_i64);
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
        let stale = heap.alloc_unpaced(INT_PAYLOAD, 1_i64);
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

    /// The free list is keyed by the whole block, not by the type that happened
    /// to occupy it first, so a reclaimed `Int` block houses the next `Float`.
    /// The reused object must be indistinguishable from a fresh one: re-headed
    /// with this heap's id, unpoisoned, and reading back as its new type.
    #[test]
    fn a_reclaimed_block_is_reused_for_the_next_object_of_its_layout() {
        use crate::scalars::FLOAT_PAYLOAD;
        let heap = Heap::new();

        let doomed = heap.alloc_unpaced(INT_PAYLOAD, 1_i64);
        let address = doomed.as_ptr();
        heap.collect_with(&RootScope::new());
        assert!(doomed.header().is_poisoned());

        // `Float`'s payload has `Int`'s size and alignment, so it files under
        // the same `BlockLayout`.
        let reused = heap.alloc_unpaced(FLOAT_PAYLOAD, 2.5_f64);

        assert_eq!(
            reused.as_ptr(),
            address,
            "a swept block must be handed back out, not left spent"
        );
        assert!(!reused.header().is_poisoned());
        assert_eq!(reused.header().heap_id(), Some(heap.id()));
        assert_eq!(reused.descriptor().name, "Float");
        // SAFETY: `reused` was just allocated with FLOAT.
        assert_eq!(unsafe { *reused.payload::<f64>() }, 2.5);
        assert_eq!(heap.stats().live_count, 1);
    }

    /// Blocks that are still filed when the arena is torn down must not be
    /// handed out afterwards — they point into storage `Bump::reset` reclaimed.
    #[test]
    fn reset_discards_the_free_list() {
        let mut heap = Heap::new();
        let doomed = heap.alloc_unpaced(INT_PAYLOAD, 1_i64);
        let stale = doomed.as_ptr();
        heap.collect_with(&RootScope::new());
        assert_eq!(heap.free.borrow().len(), 1);

        heap.reset();

        assert_eq!(heap.free.borrow().len(), 0);
        let fresh = heap.alloc_unpaced(INT_PAYLOAD, 2_i64);
        assert_eq!(fresh.header().heap_id(), Some(heap.id()));
        // The address may legitimately be reused by the fresh arena; what must
        // not happen is the *stale block* being handed out with the old layout
        // bookkeeping still attached. The fresh heap identity is the check.
        let _ = stale;
    }

    /// The class array must cover the whole language, not just the scalars the
    /// benchmark happened to allocate. `BUILTINS` is the closed set of
    /// descriptors a program can allocate through, so this makes "the fast path
    /// covers everything" a checked invariant: a twenty-third built-in with a
    /// payload past the bound fails here instead of silently degrading to the
    /// linear scan.
    #[test]
    fn every_builtin_block_has_a_size_class() {
        for descriptor in crate::descriptor::BUILTINS {
            let (_, block) = BlockLayout::of(descriptor);
            let class = SizeClass::of(block).unwrap_or_else(|| {
                panic!(
                    "descriptor {} has block {{size: {}, align: {}}}, which no \
                     SizeClass indexes (SIZE_CLASSES = {SIZE_CLASSES}, \
                     MIN_BLOCK_ALIGN = {MIN_BLOCK_ALIGN})",
                    descriptor.name, block.size, block.align
                )
            });
            assert_eq!(
                class.layout(),
                block,
                "SizeClass::of and SizeClass::layout must be inverses for {}",
                descriptor.name
            );
        }
    }

    /// RT-01's exactness, as an executable proposition. Indexing by size alone
    /// would hand a `{48, 8}` block to a `{48, 16}` request, putting a 16-aligned
    /// payload at `base + 32` where `base` is only 8-aligned. `SizeClass::of` is
    /// the single point where that could go wrong, so it is tested directly.
    #[test]
    fn a_size_class_never_conflates_two_alignments() {
        assert_eq!(
            SizeClass::of(BlockLayout { size: 48, align: 8 }),
            Some(SizeClass(48))
        );
        assert_eq!(
            SizeClass::of(BlockLayout {
                size: 48,
                align: 16
            }),
            None,
            "same size, different alignment: not the same class"
        );
        assert_eq!(
            SizeClass::of(BlockLayout {
                size: SIZE_CLASSES,
                align: MIN_BLOCK_ALIGN
            }),
            None,
            "one past the array is out of range, not index 0"
        );
    }

    /// The `oversize` fallback is not decoration: an over-aligned block must be
    /// reclaimed and reissued like any other, and must still land at its
    /// alignment. Nothing tested that before — the free list only ever saw
    /// 8-aligned blocks in the suite.
    #[test]
    fn an_overaligned_block_round_trips_through_the_free_list() {
        let heap = Heap::new();
        let doomed = unsafe {
            heap.alloc_with_unpaced(
                &OVERALIGNED,
                std::mem::size_of::<Overaligned>(),
                std::mem::align_of::<Overaligned>(),
                |payload| (payload as *mut Overaligned).write(Overaligned(1)),
            )
        };
        let address = doomed.as_ptr();

        heap.collect_with(&RootScope::new());
        assert_eq!(heap.free.borrow().len(), 1, "the block must be filed");

        let reused = unsafe {
            heap.alloc_with_unpaced(
                &OVERALIGNED,
                std::mem::size_of::<Overaligned>(),
                std::mem::align_of::<Overaligned>(),
                |payload| (payload as *mut Overaligned).write(Overaligned(2)),
            )
        };
        assert_eq!(
            reused.as_ptr(),
            address,
            "an over-aligned block must be handed back out, not left spent"
        );
        assert_eq!(reused.payload::<Overaligned>() as usize % 64, 0);
    }

    #[repr(C)]
    struct Aligned8([u64; 3]);

    #[repr(C, align(16))]
    struct Aligned16([u64; 2]);

    static ALIGNED_8: TypeDescriptor = TypeDescriptor::for_test::<Aligned8>(
        2,
        "Aligned8",
        probe_trace,
        overaligned_drop,
        probe_format,
        None,
        None,
        None,
    );

    static ALIGNED_16: TypeDescriptor = TypeDescriptor::for_test::<Aligned16>(
        3,
        "Aligned16",
        probe_trace,
        overaligned_drop,
        probe_format,
        None,
        None,
        None,
    );

    /// The adversarial test for the one way size-class indexing can go wrong.
    /// Both blocks are 48 bytes; only the alignment separates them, and a swept
    /// 8-aligned block must never satisfy a 16-aligned request.
    #[test]
    fn a_swept_block_is_never_handed_to_a_request_of_another_alignment() {
        let (_, eight) = BlockLayout::of(&ALIGNED_8);
        let (_, sixteen) = BlockLayout::of(&ALIGNED_16);
        assert_eq!(
            eight.size, sixteen.size,
            "the fixtures must share a size or this test proves nothing"
        );
        assert_ne!(eight.align, sixteen.align);

        let heap = Heap::new();
        let doomed = unsafe {
            heap.alloc_with_unpaced(
                &ALIGNED_8,
                std::mem::size_of::<Aligned8>(),
                std::mem::align_of::<Aligned8>(),
                |payload| (payload as *mut Aligned8).write(Aligned8([1, 2, 3])),
            )
        };
        let address = doomed.as_ptr();
        heap.collect_with(&RootScope::new());

        let other = unsafe {
            heap.alloc_with_unpaced(
                &ALIGNED_16,
                std::mem::size_of::<Aligned16>(),
                std::mem::align_of::<Aligned16>(),
                |payload| (payload as *mut Aligned16).write(Aligned16([4, 5])),
            )
        };
        assert_ne!(
            other.as_ptr(),
            address,
            "a block filed under {{48, 8}} must not satisfy a {{48, 16}} request"
        );
        assert_eq!(other.payload::<Aligned16>() as usize % 16, 0);
    }

    /// A reset heap is a different heap, so the refs it minted before the reset
    /// no longer pass the provenance check even though the arena may hand their
    /// addresses out again.
    #[test]
    fn reset_mints_a_new_heap_identity() {
        let mut heap = Heap::new();
        let before = heap.id();
        let _ = heap.alloc_unpaced(INT_PAYLOAD, 1_i64);

        heap.reset();

        assert_ne!(heap.id(), before);
        assert_eq!(
            heap.alloc_unpaced(INT_PAYLOAD, 2_i64).header().heap_id(),
            Some(heap.id())
        );
    }
}
