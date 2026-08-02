//! The GC heap and the precise non-moving mark-and-sweep collector (§12,
//! ADR-011, ADR-103).
//!
//! Every allocation is `[GcHeader | payload]` laid out contiguously in a **block**
//! on a size-class **page** ([`crate::page`]). The page's `allocated` bitmap is
//! the record of every outstanding allocation, so sweep is precise: object
//! boundaries are recovered from the page's stride rather than scanned for, and
//! there is no side registry to push to or walk. Objects never move (§12.1), so
//! `GcRef` addresses are stable for the object's lifetime.
//!
//! Collection is mark-and-sweep with no write barrier (§12.1):
//!   1. **Mark** — start from the root set; for each reachable object, set its
//!      block's bit in its page's `mark` bitmap and run its descriptor `trace`
//!      callback to enqueue child references.
//!   2. **Sweep** — walk every page a word at a time; each block in `allocated &
//!      !mark` gets its descriptor `drop_value` called (§12.5), is poisoned, and
//!      has its `allocated` bit cleared so the block can be reissued (RT-01).

use std::cell::Cell;
use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::descriptor::{Payload, TypeDescriptor};
use crate::gc::{GcHeader, GcRef, HeapId};
use crate::page::{self, PageHeader, SizeClass, NUM_CLASSES};
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
/// (Appendix B). Every mutable field is a [`Cell`]: the collector runs through a
/// `&Heap` that the descriptor `trace` callbacks reborrow, so a `RefCell` would
/// only buy a double-borrow panic that a scalar and a raw pointer cannot need —
/// and it charged a borrow-flag round trip on the hottest path in the runtime.
#[repr(C)]
pub struct Heap {
    /// This heap's identity, stamped into every header it allocates. The mark
    /// phase compares it against a root's `heap_id` before touching anything
    /// the header points at, so a root from another heap — or one whose storage
    /// this heap has already swept — is rejected rather than traced.
    id: HeapId,
    /// How many collectable objects this heap holds — what [`HeapStats`]
    /// reports.
    ///
    /// A running counter rather than a registry's length. It counts exactly what
    /// the registry used to: immortals are not in it, because
    /// [`Heap::alloc_immortal`] does not bump it, and sweep decrements it by the
    /// blocks it actually reclaimed.
    live_count: Cell<usize>,
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
    /// Every page this heap owns, newest first, linked by `PageHeader::next`.
    /// The only list that is exhaustive: sweep, `reset` and `Drop` all walk it,
    /// and a page is on it from the moment it is created until the heap dies.
    pages: Cell<*mut PageHeader>,
    /// Per size class, the head of the list of pages that may still have a free
    /// block, linked by `PageHeader::next_of_class`.
    ///
    /// Allocation takes the head, and drops it off this list the moment it
    /// reports itself full. Sweep rebuilds all three availability lists from the
    /// pages' own `live_count`s, which is what keeps a page from ever being on
    /// two of them at once.
    partial: [Cell<*mut PageHeader>; NUM_CLASSES],
    /// Small pages holding nothing, awaiting a class.
    ///
    /// **This is where the free list went.** A per-layout free list left an
    /// emptied bucket as dead capital for every other layout — a program that
    /// filled a heap with `Int`s and then with `Text`s paid for both. A page
    /// that empties is re-classed on demand, so storage is reusable across
    /// layouts (RT-01, strengthened).
    empty: Cell<*mut PageHeader>,
    /// Large pages holding nothing. Keyed on the whole layout rather than a
    /// class, because that is exactly what the ladder rejected them for; the
    /// list is empty in every real program, which is why a linear scan is the
    /// right shape for it.
    empty_large: Cell<*mut PageHeader>,
    /// Pages flagged immortal, linked by `PageHeader::next_of_class`.
    ///
    /// A separate list rather than a single page because the immortals are not
    /// one size class: `Unit` is a bare header and the interned small-`Int`
    /// table ([`crate::small_int`]) is a thousand blocks of the next rung up.
    immortal_pages: Cell<*mut PageHeader>,
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

/// The size and alignment of one whole `[header|payload]` allocation — what a
/// page must be able to hold. Not the payload's own layout: the payload's offset
/// within the block is recomputed on every reuse, so two descriptors that split
/// the same total differently still share a block.
///
/// Deliberately not `Hash`. The free list used to be a `HashMap` keyed on this,
/// and hashing a 16-byte key twice per object — once to find a bucket in
/// [`Heap::alloc_raw`], once to file a swept block in [`Heap::sweep`] — was 34%
/// of runtime on `collatz` and 33% on `primes`, ahead of the generated code
/// (docs/handovers/21-where-the-time-goes.md §3.1). Withholding the derive makes
/// re-introducing a hash lookup a compile error rather than a silent regression.
/// There is no hash left to re-introduce — a block's page is a mask and its
/// class is a subtraction — and the derive stays withheld so it stays that way.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct BlockLayout {
    pub(crate) size: usize,
    pub(crate) align: usize,
}

impl BlockLayout {
    /// The block `descriptor`'s objects occupy, and where their payload starts
    /// within it. The single calculation both [`Heap::alloc_raw`] and
    /// [`SizeClass::of`] read, so a block can only be placed on a page that
    /// holds the layout it actually has.
    ///
    /// # Panics
    /// Panics if the payload alignment exceeds what a `GcHeader` can record, or
    /// if the total size overflows.
    pub(crate) fn of(descriptor: &TypeDescriptor) -> (usize, BlockLayout) {
        let payload_align = descriptor.align();
        let payload_offset = GcHeader::payload_offset_for(payload_align);
        let size = payload_offset
            .checked_add(descriptor.size())
            .expect("allocation size overflow");
        let align = std::mem::align_of::<GcHeader>().max(payload_align);
        (payload_offset, BlockLayout { size, align })
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
    ///
    /// No page is created here: the first allocation of a class creates that
    /// class's first page. A heap that never allocates costs nothing, which
    /// matters because the debugger mints a second one (ADR-032).
    pub fn new() -> Self {
        Heap {
            id: HeapId::mint(),
            live_count: Cell::new(0),
            bytes_since_collect: Cell::new(0),
            collect_threshold: Cell::new(INITIAL_COLLECT_THRESHOLD),
            pages: Cell::new(std::ptr::null_mut()),
            partial: std::array::from_fn(|_| Cell::new(std::ptr::null_mut())),
            empty: Cell::new(std::ptr::null_mut()),
            empty_large: Cell::new(std::ptr::null_mut()),
            immortal_pages: Cell::new(std::ptr::null_mut()),
        }
    }

    /// Bytes of address space this heap's pages occupy.
    ///
    /// The page allocator's answer to what `bumpalo::Bump::allocated_bytes`
    /// used to report, and the number RT-01 is about: a program that allocates
    /// and collects a bounded working set in a loop must not grow it.
    pub fn committed_bytes(&self) -> usize {
        self.walk_pages().map(|page| page.page_bytes()).sum()
    }

    /// How many pages this heap holds, live or pooled.
    pub fn page_count(&self) -> usize {
        self.walk_pages().count()
    }

    /// Every page this heap owns, in no particular order.
    ///
    /// A borrowing iterator rather than a raw loop at each call site: the pages
    /// outlive any one borrow, so handing out `&PageHeader` bound to `&self` is
    /// exactly the lifetime the callers want, and it keeps the `unsafe` in one
    /// place.
    fn walk_pages(&self) -> impl Iterator<Item = &PageHeader> {
        let mut next = self.pages.get();
        std::iter::from_fn(move || {
            if next.is_null() {
                return None;
            }
            // SAFETY: every page on this list was created by this heap and is
            // released only by `Heap::drop`, which runs after every borrow of
            // `self` has ended.
            let page = unsafe { &*next };
            next = page.next();
            Some(page)
        })
    }

    /// Thread a freshly created page onto the heap's page list.
    fn adopt(&self, page: *mut PageHeader) {
        // SAFETY: `page` was just created and nothing else names it.
        unsafe { (*page).set_next(self.pages.get()) };
        self.pages.set(page);
    }

    /// Take a block of `block`'s layout, and report the stride it was taken at
    /// — which is what the pacing counter is charged, because it is what the
    /// heap actually spent.
    #[inline]
    fn claim_block(
        &self,
        descriptor: &'static TypeDescriptor,
        payload_offset: usize,
        block: BlockLayout,
    ) -> (*mut u8, usize) {
        let Some(class) = SizeClass::of(block) else {
            return (
                self.claim_large_block(descriptor, payload_offset, block),
                block.size,
            );
        };
        // Resolve the class's list head **once**. `class.index()` is below
        // `NUM_CLASSES` by construction but the optimizer cannot see that, so
        // indexing inside the loop would put a bounds check and its panic path
        // on the hottest instruction sequence in the runtime, twice per turn.
        let head_cell = self
            .partial
            .get(class.index())
            .expect("SizeClass::of yields an index below NUM_CLASSES");
        loop {
            let head = head_cell.get();
            if head.is_null() {
                self.grow_class(class);
                continue;
            }
            // SAFETY: a page on an availability list is one of this heap's own,
            // live until `Heap::drop`.
            let page = unsafe { &*head };
            match page.claim_free_block() {
                Some(base) => return (base, class.block_size()),
                // Full. Drop it off the availability list — sweep relinks it if
                // it ever frees anything, and until then re-scanning its bitmap
                // on every allocation would be the cost this design removes.
                None => head_cell.set(page.next_of_class()),
            }
        }
    }

    /// Put a page of `class` at the head of its availability list: a pooled
    /// empty one if there is one, a fresh one otherwise.
    #[cold]
    #[inline(never)]
    fn grow_class(&self, class: SizeClass) {
        let page = match self.pop_empty() {
            Some(page) => {
                // SAFETY: a pooled page is live and holds nothing.
                unsafe { (*page).reclass(class) };
                page
            }
            None => {
                let page = PageHeader::new_small(class, self.id.get());
                self.adopt(page);
                page
            }
        };
        // SAFETY: `page` is live and on no availability list.
        unsafe { (*page).set_next_of_class(self.partial[class.index()].get()) };
        self.partial[class.index()].set(page);
    }

    /// Pop a pooled empty page, if any.
    fn pop_empty(&self) -> Option<*mut PageHeader> {
        let page = self.empty.get();
        if page.is_null() {
            return None;
        }
        // SAFETY: a pooled page is one of this heap's own.
        self.empty.set(unsafe { (*page).next_of_class() });
        Some(page)
    }

    /// The one block of a page laid out for exactly this layout — pooled if one
    /// is available, fresh otherwise.
    ///
    /// No production descriptor comes here; see [`PageHeader::new_large`].
    #[cold]
    #[inline(never)]
    fn claim_large_block(
        &self,
        descriptor: &'static TypeDescriptor,
        payload_offset: usize,
        block: BlockLayout,
    ) -> *mut u8 {
        let mut previous: *mut PageHeader = std::ptr::null_mut();
        let mut current = self.empty_large.get();
        while !current.is_null() {
            // SAFETY: a pooled page is one of this heap's own.
            let page = unsafe { &*current };
            if page.fits_large(payload_offset, block) {
                if previous.is_null() {
                    self.empty_large.set(page.next_of_class());
                } else {
                    // SAFETY: `previous` is the page we visited last.
                    unsafe { (*previous).set_next_of_class(page.next_of_class()) };
                }
                page.set_next_of_class(std::ptr::null_mut());
                page.rewind_cursor();
                return page
                    .claim_free_block()
                    .expect("an empty large page has its block");
            }
            previous = current;
            current = page.next_of_class();
        }
        let page = PageHeader::new_large(descriptor, payload_offset, block, self.id.get());
        self.adopt(page);
        // SAFETY: `page` was just created with one free block.
        unsafe { (*page).claim_free_block() }.expect("a fresh large page has its block")
    }

    /// Rebuild the three availability lists from the pages' own liveness, and
    /// rewind every page's allocation cursor.
    ///
    /// Rebuilding rather than unlinking is what makes "a page is on at most one
    /// availability list" structural instead of a discipline four call sites
    /// have to keep. Membership is a function of `live_count`, and after a sweep
    /// every `live_count` is final.
    ///
    /// Rewinding the cursor is not cosmetic: it is what makes the next
    /// allocation of a class take the *lowest* free block, so the address a
    /// collection just reclaimed is the address the next object of that layout
    /// gets. `a_reclaimed_block_is_reused_for_the_next_object_of_its_layout`
    /// pins it, and a "resume where we left off" cursor would silently break it.
    fn relink_pages(&self) {
        for head in &self.partial {
            head.set(std::ptr::null_mut());
        }
        self.empty.set(std::ptr::null_mut());
        self.empty_large.set(std::ptr::null_mut());
        let mut current = self.pages.get();
        while !current.is_null() {
            // SAFETY: every page on this list is this heap's own.
            let page = unsafe { &*current };
            let next = page.next();
            if !page.is_immortal() {
                page.rewind_cursor();
                // An empty page joins the pool its geometry can be reused from;
                // a small page with room goes back to its class; a full page —
                // and a large page holding its one object — joins nothing, and
                // waits for a later sweep to free something.
                let list = if page.live_count() == 0 {
                    match page.class() {
                        Some(_) => Some(&self.empty),
                        None => Some(&self.empty_large),
                    }
                } else {
                    match page.class() {
                        Some(class) if (page.live_count() as usize) < page.block_count() => {
                            Some(&self.partial[class.index()])
                        }
                        _ => None,
                    }
                };
                match list {
                    Some(head) => {
                        page.set_next_of_class(head.get());
                        head.set(current);
                    }
                    None => page.set_next_of_class(std::ptr::null_mut()),
                }
            }
            current = next;
        }
    }

    /// This heap's identity. Every header it allocates carries it.
    pub fn id(&self) -> HeapId {
        self.id
    }

    /// Whether `value` was allocated by this heap and has not been swept.
    ///
    /// O(1): it reads the owning id out of the header, which is the same test
    /// the collector applies to every root, and the same one that licenses
    /// masking an address to find its page.
    #[inline]
    pub fn owns(&self, value: GcRef) -> bool {
        value.header().heap_id() == Some(self.id)
    }

    /// Current allocation count.
    pub fn stats(&self) -> HeapStats {
        HeapStats {
            live_count: self.live_count.get(),
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

    /// Allocate an immortal object: same layout as [`Heap::alloc`], but on a
    /// page the collector never walks, so it is never reclaimed (§4.3, M3
    /// deliverable). Used for the `Unit`/`Bool` singletons and the interned
    /// small-`Int` table.
    ///
    /// The exemption used to be an omission — `alloc_raw` registered the object
    /// and this function linear-scanned the registry to un-register it. It is
    /// now a page flag, which is a stronger statement of the same thing: sweep
    /// and `finalize_all` do not read an immortal page's `allocated` bitmap at
    /// all, so there is no window in which an immortal is momentarily
    /// collectable and no scan whose cost grows with the heap.
    ///
    /// Restricted to [`Immortals::new`](crate::immortal::Immortals::new) by the
    /// [`ImmortalWitness`](crate::immortal::ImmortalWitness) it takes, which
    /// only that module can construct. The restriction is load-bearing twice
    /// over: an immortal is invisible to sweep *and* to [`Heap`]'s `Drop`, so
    /// every immortal payload must be `Copy` (nothing to finalize) and must be
    /// minted exactly once at startup. Minting one per call — which the `Bool`
    /// wrappers used to do — is storage nothing ever reclaims (RT-03).
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
    /// skipped inside `occupy`, which would need a flag on the one path every
    /// real allocation takes.
    pub(crate) fn alloc_immortal<T: Copy>(
        &self,
        payload: Payload<T>,
        value: T,
        _witness: crate::immortal::ImmortalWitness,
    ) -> GcRef {
        let descriptor = payload.descriptor();
        let (payload_offset, block) = BlockLayout::of(descriptor);
        let class = SizeClass::of(block).expect(
            "an immortal payload is a scalar, and the size-class ladder holds every scalar",
        );
        let charged_before = self.bytes_since_collect.get();
        let base = self.claim_immortal_block(class);
        // SAFETY: `base` is a fresh block of `class`, whose stride is at least
        // `block.size`; `T: Copy`, so writing the bytes fully initializes the
        // payload.
        let r = unsafe {
            self.occupy(
                base,
                class.block_size(),
                descriptor,
                payload_offset,
                |payload| (payload as *mut T).write(value),
            )
        };
        // Un-charge the block: see this function's doc. Restoring the snapshot
        // rather than subtracting the block size keeps this correct whatever
        // `occupy` decides an object costs (it also charges the descriptor's
        // owned bytes, which for a `Copy` immortal is zero today and need not
        // stay so).
        self.bytes_since_collect.set(charged_before);
        r
    }

    /// A block on an immortal page of `class`, creating one if none has room.
    ///
    /// Linear over the immortal pages, which is right: there are three of them
    /// after `Immortals::new` and none is ever added afterwards, because
    /// [`ImmortalWitness`](crate::immortal::ImmortalWitness) confines minting to
    /// startup. They are not one class — `Unit` is a bare header and the
    /// interned `Int` table is a thousand blocks of the next rung up — which is
    /// why this is a list rather than the single page it would otherwise be.
    #[cold]
    #[inline(never)]
    fn claim_immortal_block(&self, class: SizeClass) -> *mut u8 {
        let mut current = self.immortal_pages.get();
        while !current.is_null() {
            // SAFETY: an immortal page is one of this heap's own.
            let page = unsafe { &*current };
            if page.class() == Some(class) {
                if let Some(base) = page.claim_free_block() {
                    return base;
                }
            }
            current = page.next_of_class();
        }
        let page = PageHeader::new_small(class, self.id.get());
        // SAFETY: `page` was just created and nothing else names it.
        unsafe {
            (*page).set_immortal();
            (*page).set_next_of_class(self.immortal_pages.get());
        }
        self.adopt(page);
        self.immortal_pages.set(page);
        // SAFETY: a fresh page has room.
        unsafe { (*page).claim_free_block() }.expect("a fresh page has room")
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
    /// chance to reclaim; something else must pace, or the heap grows until it
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

    /// The shared low-level allocator: take a block from a page, lay out
    /// `[GcHeader | payload]` in it, and run `init` on the payload. Claiming the
    /// block *is* the registration — the page's `allocated` bit is what sweep
    /// enumerates.
    ///
    /// The whole fast path is a load of the class's partial-page pointer, one
    /// bitmap word, an `andnot`, a `trailing_zeros`, a bitmap store, the header
    /// store, the payload `init` and two counter bumps. No hash, no registry
    /// push, no reallocation, no `RefCell` borrow.
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
        // same call `SizeClass::of` chooses a page from.
        let (payload_offset, block) = BlockLayout::of(descriptor);
        let (base, stride) = self.claim_block(descriptor, payload_offset, block);
        self.live_count.set(self.live_count.get() + 1);
        // SAFETY: `base` is a fresh block of at least `block.size` bytes,
        // aligned for a `GcHeader` and for this descriptor's payload; `init`'s
        // contract is forwarded from this function's.
        unsafe { self.occupy(base, stride, descriptor, payload_offset, init) }
    }

    /// Head a claimed block with `descriptor`, initialize its payload, and
    /// charge the allocation against the pacing counter.
    ///
    /// Shared by [`Heap::alloc_raw`] and [`Heap::alloc_immortal`], which differ
    /// only in which page the block came from and in whether the collector will
    /// ever look at it again. Keeping one body is what stops the immortal path
    /// from drifting away from the layout every other object has — the whole
    /// point of an immortal is that its accessors work on it unchanged.
    ///
    /// # Safety
    /// `base` must be an unclaimed block of at least `payload_offset +
    /// descriptor.size()` bytes, aligned for a `GcHeader` and for the payload;
    /// `init` must fully initialize the payload as `descriptor`'s type.
    unsafe fn occupy(
        &self,
        base: *mut u8,
        stride: usize,
        descriptor: &'static TypeDescriptor,
        payload_offset: usize,
        init: impl FnOnce(*mut u8),
    ) -> GcRef {
        // Unreachable in practice — a payload aligned past a page's reach is
        // rejected by `PageHeader::new_large` before it gets here — but it is
        // the header field's own bound, and ADR-039's Consequences say the
        // allocator panics naming the descriptor rather than truncating.
        let recorded_offset = u16::try_from(payload_offset).unwrap_or_else(|_| {
            panic!(
                "payload alignment {} of descriptor {} exceeds the \
                 largest offset a GcHeader can record",
                descriptor.align(),
                descriptor.name
            )
        });
        let header_ptr = base as *mut GcHeader;
        // SAFETY: the block is at least `payload_offset + size` bytes.
        let payload_ptr = unsafe { base.add(payload_offset) };

        // Write the header. Mark starts white (unscanned).
        // SAFETY: `base` is an unclaimed, header-aligned block.
        unsafe {
            std::ptr::write(
                header_ptr,
                GcHeader::new(
                    descriptor,
                    descriptor.size() as u32,
                    recorded_offset,
                    self.id,
                ),
            );
        }
        // Initialize the payload.
        init(payload_ptr);

        // Account for the allocation against the collection pacing counter.
        // Reused storage counts too: pacing measures the pressure a program is
        // putting on the collector, not the heap's high-water mark. The stride
        // is what a block costs — a class rounds up to it, and that rounding is
        // real memory the program spent.
        //
        // The block is only part of what the object costs. A `Text` is 56 bytes
        // of block and a `Box<str>` of whatever length the program read; a
        // freshly built `Vec` is 56 bytes and a buffer of `capacity` refs. The
        // descriptor measures the rest, so a text-heavy program no longer
        // under-reports its pressure by essentially its whole footprint
        // (RT-04). Growth *after* this point — a `push` that reallocates — is
        // still uncharged; its elements are themselves paced allocations, so
        // the residual under-count is the spine, not the contents.
        // SAFETY: `init` has run, so the payload is a valid value of `descriptor`.
        let owned = unsafe { descriptor.owned_bytes_of(payload_ptr) };
        self.bytes_since_collect
            .set(self.bytes_since_collect.get() + stride.saturating_add(owned));
        // SAFETY: `header_ptr` is inside a live page, so it is non-null, and it
        // has just been initialized.
        unsafe { GcRef::from_non_null(NonNull::new_unchecked(header_ptr)) }
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

    /// Mark phase: set the page bit of every reachable object.
    fn mark(&self, roots: &dyn RootSet) {
        let mut worklist: Vec<GcRef> = Vec::new();
        roots.push_roots(&mut worklist);

        // The tracer enqueues child references onto the worklist. The grey set
        // *is* this worklist — a transient grey colour would say nothing extra
        // in a single-threaded collector with no concurrency, which is why the
        // header never had a use for a third colour and no longer has a byte
        // for one.
        struct Enqueuer<'a>(&'a mut Vec<GcRef>);
        impl Tracer for Enqueuer<'_> {
            fn trace(&mut self, reference: GcRef) {
                self.0.push(reference);
            }
        }

        while let Some(r) = worklist.pop() {
            let header = r.header();
            // (a) Provenance check, before anything the header points at is
            // read, and before the address is masked. A reference this heap did
            // not allocate is not this heap's to colour: marking a foreign
            // object delays *its* heap's reclamation of it, and a swept
            // object's descriptor is a null pointer into finalized storage.
            // Both are rejected here (ADR-039 Decision 2).
            //
            // **This check is also what makes the mask below sound.** Only this
            // heap's allocator writes this heap's id into a header, and it only
            // ever writes one into a block on one of this heap's pages — so a
            // header that passes here is inside a page, and `page_of` is
            // arithmetic on an address whose provenance is already established.
            // A `GcHeader` that lives anywhere else (a test fixture, another
            // heap's object) carries an id this test rejects.
            if header.heap_id() != Some(self.id) {
                continue;
            }
            // (b) The mark bit lives in the page, not in the header: sweep's
            // per-survivor "reset to white" store — a random-access write per
            // live object per collection — becomes one store per 64 blocks.
            let address = r.as_ptr() as *const u8;
            // SAFETY: (a) established that this heap allocated this block, so
            // masking its address yields one of this heap's own live pages.
            let page = unsafe { &*page::page_of(address) };
            debug_assert_eq!(page.heap_id(), self.id.get());
            let index = page.block_index(address);
            debug_assert!(page.is_allocated(index), "a live header on a free block");
            // Set the bit first, *then* trace, so the descriptor's `trace`
            // callback may enqueue children that point back to this object
            // without re-tracing it infinitely.
            if page.test_and_set_mark(index) {
                continue;
            }
            let desc = header.descriptor();
            let payload = r.payload::<u8>();
            let mut enq = Enqueuer(&mut worklist);
            // SAFETY: `r` is a live, reachable object whose payload matches its
            // descriptor.
            unsafe { (desc.trace)(payload, &mut enq) };
        }
    }

    /// Sweep phase: finalize every allocated-but-unmarked block, release it for
    /// reuse, and clear the mark bitmap for the next cycle.
    ///
    /// A page in which nothing died costs one `alive & !marked == 0` test and at
    /// most two stores per 64 blocks. That — not the allocation path — is where
    /// most of this finding's win is: the registry walk touched every live
    /// object on every collection, twice over, once to test its colour and once
    /// to reset it.
    fn sweep(&self) {
        let mut reclaimed = 0usize;
        for page in self.walk_pages() {
            let words = page.words();
            if page.is_immortal() {
                // Nothing on an immortal page is ever finalized and no
                // `allocated` bit of it is ever cleared — that is what the flag
                // means. Its *mark* bits are cleared, though: a root may alias
                // an immortal, and a mark bit left set would make the next
                // cycle stop at it instead of tracing through it. Every
                // immortal payload is a scalar with no children today, so that
                // would be harmless — but "harmless because of what the payload
                // happens to be" is not an invariant, and one store per 64
                // blocks on three pages is not a cost worth taking the risk for.
                for word in 0..words {
                    if page.mark_word(word) != 0 {
                        page.clear_mark_word(word);
                    }
                }
                continue;
            }
            let mut freed = 0u32;
            for word in 0..words {
                let alive = page.allocated_word(word);
                let marked = page.mark_word(word);
                let mut dead = alive & !marked;
                if dead != 0 {
                    while dead != 0 {
                        let index = word * 64 + dead.trailing_zeros() as usize;
                        dead &= dead - 1;
                        // SAFETY: the block's `allocated` bit is set, so
                        // `alloc_raw` initialized a header and a payload of its
                        // descriptor there and nothing has finalized it since.
                        let header = unsafe { &*(page.block_ptr(index) as *const GcHeader) };
                        let desc = header.descriptor();
                        // SAFETY: payload matches `desc` and is about to become
                        // invalid.
                        unsafe { (desc.drop_value)(header.payload::<u8>()) };
                        // Poison before the bit is cleared, so a stale `GcRef`
                        // that still names this storage is rejected by the mark
                        // phase's provenance check instead of being traced
                        // through a finalized payload. This is also RT-01's
                        // precondition: between releasing the block and handing
                        // it out again, it must not claim to be a typed object,
                        // or a stale reference would be traced through whatever
                        // the allocator put there next (hazard H7).
                        header.poison();
                        freed += 1;
                    }
                    page.set_allocated_word(word, alive & marked);
                }
                if marked != 0 {
                    page.clear_mark_word(word);
                }
            }
            if freed != 0 {
                page.release_blocks(freed);
                reclaimed += freed as usize;
            }
        }
        self.live_count.set(self.live_count.get() - reclaimed);
        self.relink_pages();
    }

    /// Finalize **every** still-live allocation, reachable or not, and empty
    /// every page.
    ///
    /// Sweep only finalizes what it proved unreachable, so this is the other
    /// half: at teardown, whatever a program left live still owns the
    /// `Box<str>` / `Vec` / `HashMap` backing allocations its payload points
    /// at, and those are not in a page — releasing the pages reclaims the
    /// `[header|payload]` blocks and leaks everything they own (RT-02).
    ///
    /// This is the answer to "without a live registry, how do you enumerate
    /// every live object": the `allocated` bitmaps already know, exactly.
    ///
    /// Immortal pages are left alone, exactly as the unregistered immortals
    /// were: an immortal payload is `Copy` by
    /// [`ImmortalWitness`](crate::immortal::ImmortalWitness)'s argument, so
    /// there is nothing to finalize.
    ///
    /// After this the heap holds nothing collectable, so [`Heap::reset`] and
    /// `Drop` can both use it and neither can double-finalize — the bitmap it
    /// cleared is the same one that told it what to finalize.
    fn finalize_all(&self) {
        for page in self.walk_pages() {
            if page.is_immortal() {
                continue;
            }
            for word in 0..page.words() {
                let mut alive = page.allocated_word(word);
                while alive != 0 {
                    let index = word * 64 + alive.trailing_zeros() as usize;
                    alive &= alive - 1;
                    // SAFETY: as in `sweep` — an allocated bit means an
                    // initialized header and payload.
                    let header = unsafe { &*(page.block_ptr(index) as *const GcHeader) };
                    let desc = header.descriptor();
                    // SAFETY: payload matches `desc`, becomes invalid after this.
                    unsafe { (desc.drop_value)(header.payload::<u8>()) };
                    header.poison();
                }
            }
            page.clear_bitmaps();
        }
        self.live_count.set(0);
        self.relink_pages();
    }

    /// Reset the heap to empty, dropping everything. Used by tests and, later,
    /// runtime teardown. Immortal singletons must be re-allocated afterwards —
    /// which since the small-`Int` table ([`crate::small_int`]) means the whole
    /// `Immortals` value, not just the three singletons: a `RuntimeContext`
    /// minted before the reset holds `unit_ref`, `true_ref`, `false_ref` **and**
    /// a `small_ints` pointer, and every one of them names storage the heap is
    /// now free to hand out again.
    pub fn reset(&mut self) {
        // Finalize every live allocation before repudiating the pages.
        self.finalize_all();
        // A reset heap is a different heap: the immortals it handed out are
        // gone, and every `GcRef` minted before this point names storage the
        // heap is free to hand out again. A fresh identity makes those refs fail
        // the mark phase's provenance check rather than be traced.
        let id = HeapId::mint();
        // The pages are **kept**, and that is deliberate: a stale `GcRef` must
        // mask to storage that is still mapped, or the rejection above becomes a
        // use-after-free. What is repudiated is everything recorded on them —
        // every allocated bit (including the immortal pages', which is what
        // makes the immortal singletons genuinely gone) and the owning identity.
        for page in self.walk_pages() {
            page.clear_bitmaps();
            page.clear_immortal();
            page.set_heap_id(id.get());
        }
        self.immortal_pages.set(std::ptr::null_mut());
        self.relink_pages();
        // Pacing is part of the heap's state, so a reset heap paces like a fresh
        // one. Leaving the counter and the geometrically-grown threshold in
        // place meant a reset heap could run for megabytes before its first
        // collection, or collect on its very first allocation (RT-04).
        self.bytes_since_collect.set(0);
        self.collect_threshold.set(INITIAL_COLLECT_THRESHOLD);
        self.id = id;
    }

    /// Return every page to the global allocator.
    ///
    /// The only place a page is ever unmapped, and it runs after
    /// [`Heap::finalize_all`]. Everything else keeps pages mapped forever,
    /// because "a stale `GcRef` masks to a page that is still there" is what
    /// makes the rejection in `Heap::mark` a rejection rather than a wild read.
    fn release_pages(&mut self) {
        let mut current = self.pages.get();
        while !current.is_null() {
            // SAFETY: every page on this list came from `PageHeader::new_*` and
            // is released exactly once, here.
            let next = unsafe { (*current).next() };
            // SAFETY: as above; `&mut self` means nothing else can name a block.
            unsafe { PageHeader::release(current) };
            current = next;
        }
        self.pages.set(std::ptr::null_mut());
        for head in &self.partial {
            head.set(std::ptr::null_mut());
        }
        self.empty.set(std::ptr::null_mut());
        self.empty_large.set(std::ptr::null_mut());
        self.immortal_pages.set(std::ptr::null_mut());
    }
}

impl Drop for Heap {
    /// Finalize whatever the program left live (RT-02), then release the pages.
    ///
    /// Releasing the pages reclaims the `[header|payload]` blocks, and nothing
    /// else: the `Box<str>` behind a `Text`, the `Vec<GcRef>` behind a `Vec[T]`,
    /// the `HashMap` behind a `Map[K,V]` are ordinary Rust allocations no page
    /// ever owned. Without the finalize, every object still reachable at
    /// teardown leaked its backing store.
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
        self.release_pages();
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
        let stride = SizeClass::of(block)
            .expect("a Text is on the ladder")
            .block_size();
        assert_eq!(
            heap.bytes_since_collect.get() - after_owner,
            stride,
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
        let first_cycle_bytes = heap.committed_bytes();

        for cycle in 1..=8 {
            for i in 0..OBJECTS_PER_CYCLE {
                let _ = heap.alloc_unpaced(INT_PAYLOAD, (cycle * OBJECTS_PER_CYCLE + i) as i64);
            }
            heap.collect_with(&RootScope::new());
        }
        let final_bytes = heap.committed_bytes();

        assert!(
            final_bytes <= first_cycle_bytes.saturating_mul(2),
            "reclaiming the same bounded working set repeatedly grew the heap \
             from {first_cycle_bytes} to {final_bytes} bytes"
        );
    }

    /// The reason the free list became a pool of pages rather than a pool of
    /// blocks: a bucket keyed by layout is dead capital for every other layout,
    /// so a program that fills a heap with one shape and then another paid for
    /// both. An emptied page is re-classed, so it does not.
    #[test]
    fn an_emptied_page_is_reused_for_another_size_class() {
        use crate::text::{TextPayload, TEXT};
        let heap = Heap::new();
        // Enough `Text`s to need many pages of their own class.
        for _ in 0..8_000 {
            // SAFETY: TextPayload matches TEXT's size/align and is initialized.
            unsafe {
                heap.alloc_with_unpaced(
                    &TEXT,
                    std::mem::size_of::<TextPayload>(),
                    std::mem::align_of::<TextPayload>(),
                    |p| (p as *mut TextPayload).write(TextPayload::Owned("x".into())),
                );
            }
        }
        heap.collect_with(&RootScope::new());
        let after_texts = heap.page_count();
        assert!(after_texts > 1, "the fixture must span several pages");

        // The same count of `Int`s, which are a different class entirely and
        // pack more densely — so every page they need can come from the pool.
        for i in 0..8_000_i64 {
            let _ = heap.alloc_unpaced(INT_PAYLOAD, i);
        }

        assert_eq!(
            heap.page_count(),
            after_texts,
            "the pages the `Text`s emptied must have been re-classed for the `Int`s, \
             not left as dead capital beside fresh ones"
        );
    }

    /// An immortal is on a page sweep does not walk, so it is never finalized,
    /// never counted, and never handed back out — the three things the old
    /// "allocate then un-register" trick bought, now bought by a flag.
    #[test]
    fn an_immortal_is_invisible_to_sweep_and_to_finalize_all() {
        let drops = Arc::new(AtomicUsize::new(0));
        {
            let heap = Heap::new();
            // Through `Immortals::new`, which is the only route there is — the
            // `ImmortalWitness` seal is RT-03 and this test does not get to
            // widen it.
            let immortals = crate::immortal::Immortals::new(&heap);
            let immortal = immortals.small_int(7).expect("7 is interned");
            let address = immortal.as_ptr();
            // A collectable object of the same class, to prove sweep is running
            // and that the immortal's page is not simply unreachable.
            unsafe {
                heap.alloc_with_unpaced(
                    &DROP_PROBE,
                    std::mem::size_of::<DropProbe>(),
                    std::mem::align_of::<DropProbe>(),
                    |payload| (payload as *mut DropProbe).write(DropProbe(Arc::clone(&drops))),
                );
            }

            heap.collect_with(&RootScope::new());
            assert_eq!(drops.load(Ordering::SeqCst), 1, "the probe was reclaimed");
            assert_eq!(heap.stats().live_count, 0, "an immortal is not counted");
            assert!(!immortal.header().is_poisoned(), "an immortal is not swept");
            assert_eq!(immortal.header().heap_id(), Some(heap.id()));
            // SAFETY: the immortal is still a live `Int`.
            assert_eq!(unsafe { *immortal.payload::<i64>() }, 7);

            // Nothing else may be given the immortal's block.
            for i in 0..4_000_i64 {
                assert_ne!(heap.alloc_unpaced(INT_PAYLOAD, i).as_ptr(), address);
            }
        }
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "teardown must not finalize anything twice"
        );
    }

    /// Two heaps' pages are disjoint allocations, so one heap's mark bits can
    /// never be the other's. The mask is what makes this worth stating: it is
    /// arithmetic, and arithmetic does not check which heap it belongs to.
    #[test]
    fn two_heaps_pages_do_not_alias() {
        let first = Heap::new();
        let second = Heap::new();
        for i in 0..2_000_i64 {
            let _ = first.alloc_unpaced(INT_PAYLOAD, i);
            let _ = second.alloc_unpaced(INT_PAYLOAD, i);
        }
        let mine: Vec<usize> = first
            .walk_pages()
            .map(|page| page.base() as usize)
            .collect();
        for page in second.walk_pages() {
            assert!(!mine.contains(&(page.base() as usize)));
        }
        assert!(mine.len() > 1);
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

    /// Sweep poisons before it clears the block's `allocated` bit, so the
    /// storage stops claiming to be a typed object the moment it stops being
    /// one. This is the precondition for reusing swept storage (RT-01): without
    /// it, a stale `GcRef` would be traced into whatever the allocator put there
    /// next.
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

    /// A page is not keyed by the type that happened to occupy it first, so a
    /// reclaimed `Int` block houses the next `Float`. The reused object must be
    /// indistinguishable from a fresh one: re-headed with this heap's id,
    /// unpoisoned, and reading back as its new type.
    ///
    /// The exact-address assertion is what pins `relink_pages`'s cursor rewind:
    /// the next allocation of a class must take the *lowest* free block.
    #[test]
    fn a_reclaimed_block_is_reused_for_the_next_object_of_its_layout() {
        use crate::scalars::FLOAT_PAYLOAD;
        let heap = Heap::new();

        let doomed = heap.alloc_unpaced(INT_PAYLOAD, 1_i64);
        let address = doomed.as_ptr();
        heap.collect_with(&RootScope::new());
        assert!(doomed.header().is_poisoned());

        // `Float`'s payload has `Int`'s size and alignment, so it lands on the
        // same rung of the ladder.
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

    /// A reset heap keeps its storage — a stale `GcRef` must mask to a page
    /// that is still mapped, or the rejection below would be a use-after-free —
    /// and repudiates everything recorded on it.
    #[test]
    fn reset_repudiates_every_page_and_keeps_the_storage() {
        let mut heap = Heap::new();
        let doomed = heap.alloc_unpaced(INT_PAYLOAD, 1_i64);
        heap.collect_with(&RootScope::new());
        let live_ref = heap.alloc_unpaced(INT_PAYLOAD, 3_i64);
        let committed = heap.committed_bytes();
        assert!(committed > 0);

        heap.reset();

        assert_eq!(
            heap.committed_bytes(),
            committed,
            "reset keeps every page, so a stale reference still masks to mapped storage"
        );
        for page in heap.walk_pages() {
            assert_eq!(page.live_count(), 0, "no page may still claim a live block");
            assert!(
                !page.is_immortal(),
                "reset repudiates the immortal pages too"
            );
            assert_eq!(page.heap_id(), heap.id().get());
        }
        // Both the swept reference and the one that was still live before the
        // reset now belong to nobody this heap recognizes.
        assert!(!heap.owns(doomed));
        assert!(!heap.owns(live_ref));

        let fresh = heap.alloc_unpaced(INT_PAYLOAD, 2_i64);
        assert_eq!(fresh.header().heap_id(), Some(heap.id()));
    }

    /// The large path is not decoration: an over-aligned block must be
    /// reclaimed and reissued like any other, and must still land at its
    /// alignment. Nothing tested that before §3.1 — the free list only ever saw
    /// 8-aligned blocks in the suite.
    #[test]
    fn an_overaligned_block_round_trips_through_its_own_page() {
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
        let pages = heap.page_count();

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
        assert_eq!(
            heap.page_count(),
            pages,
            "the emptied large page must be reused, not left beside a fresh one"
        );
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
    /// no longer pass the provenance check even though the retained pages may
    /// hand their addresses out again.
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
