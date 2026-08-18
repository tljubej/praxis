//! Size-class pages: where an object's storage comes from, and where its
//! liveness is recorded (ADR-103).
//!
//! A page is one `PAGE_SIZE`-aligned allocation whose first bytes are a
//! [`PageHeader`] and whose remainder is an array of equal-sized **blocks**. A
//! block is one `[GcHeader | payload]` allocation. Because every block on a
//! page has the same stride and the page's base is `PAGE_SIZE`-aligned, three
//! questions that would otherwise need a side table are pointer arithmetic:
//!
//! * *which page does this address belong to?* — mask off the low bits
//!   ([`page_of`]);
//! * *which block is it?* — subtract [`PageHeader::first_block`] and divide by
//!   the stride ([`PageHeader::block_index`], a multiply-shift);
//! * *is that block live, and has the mark phase reached it?* — two bits in the
//!   page's `allocated` and `mark` bitmaps.
//!
//! ADR-011 declined to recover object boundaries, on the grounds that "the
//! design does not specify" them and a side `live` registry made it
//! unnecessary. Segregating by size class is what makes recovery a
//! multiply-shift instead of a scan, and so what lets this module stand in for
//! that registry.
//!
//! **The allocated bitmap is the intra-page free list, and it is a bitmap on
//! purpose.** Threading a next-pointer through a dead block's first eight bytes
//! would overwrite its `descriptor`, so [`GcHeader::is_poisoned`] would report
//! filed storage as a typed object — hazard H7 exactly. Nothing is ever written
//! into a free block: it keeps the null descriptor `poison` left there until the
//! allocator re-heads it.
//!
//! **A page is never handed back to the global allocator while its heap lives.**
//! An emptied page goes to the heap's own pool and is re-classed on demand. That
//! is a soundness rule, not a policy: the whole design rests on "a stale `GcRef`
//! masks to a page that is still mapped, and is rejected there". `Heap::drop`
//! releases them all — the same window hazard H8 already documents.

use std::alloc::Layout;
use std::cell::Cell;

use crate::descriptor::TypeDescriptor;
use crate::gc::GcHeader;
use crate::heap::BlockLayout;

/// The size and alignment of one page's backing allocation.
///
/// 32 KiB is chosen so that the bound on payload alignment comes out the same
/// as the one ADR-039 recorded (an over-aligned payload must still land inside
/// the first page unit, so its alignment is bounded by `PAGE_SIZE`), and so the
/// two bitmaps cost roughly one percent of the page at the smallest stride.
pub(crate) const PAGE_SIZE: usize = 1 << 15;

/// The mask that turns an address inside a page into that page's base.
/// Sound only because every page allocation is `PAGE_SIZE`-aligned.
pub(crate) const PAGE_MASK: usize = !(PAGE_SIZE - 1);

/// The granularity of the size-class ladder.
///
/// Eight bytes, not powers of two: the ladder exists to make *composites*
/// smaller, and a power-of-two ladder would round a `Vec`'s 56-byte block to 64
/// and a `Map`'s 88 to 128 — bigger than the arena gave them.
pub(crate) const BLOCK_GRANULE: usize = std::mem::align_of::<GcHeader>();

/// The smallest block the ladder holds: a header and nothing else, which is
/// exactly what a `Unit` is.
///
/// Derived from the header rather than written down, so the ladder follows the
/// header if the header ever shrinks.
pub(crate) const MIN_BLOCK: usize = std::mem::size_of::<GcHeader>();

/// The largest block the ladder holds. Everything above it, and everything
/// aligned more strictly than a `GcHeader`, takes the large-object path.
///
/// The set of block layouts a Praxis program can produce is *closed*:
/// [`TypeDescriptor::builtin`](crate::descriptor::TypeDescriptor) is the only
/// non-test constructor and [`crate::descriptor::BUILTINS`] is the whole list of
/// descriptors that call it. A record, enum, tuple or closure does not widen it
/// either — each boxes its schema behind a pointer and its fields behind a
/// `Vec<GcRef>`, so its payload is one fixed struct whatever its arity. The
/// largest block the language can ask for today is well under this bound, and
/// `the_ladder_covers_every_builtin_descriptor` is what keeps that honest: a
/// twenty-third built-in with a bigger payload fails that test rather than
/// silently costing a whole page each.
pub(crate) const MAX_BLOCK: usize = 128;

/// How many size classes the ladder has.
pub(crate) const NUM_CLASSES: usize = (MAX_BLOCK - MIN_BLOCK) / BLOCK_GRANULE + 1;

/// An upper bound on the blocks any one page can hold — the whole page divided
/// by the smallest stride. The real count is smaller (the header comes out of
/// the same page), so the bitmaps always have slack.
const MAX_BLOCKS: usize = PAGE_SIZE / MIN_BLOCK;

/// Bits in one bitmap word, and so the stride from a bitmap word index to the
/// index of the block its bit 0 names.
///
/// Derived from the word type rather than written down, for the reason
/// [`MIN_BLOCK`] is derived from the header: this is the bitmaps' geometry,
/// and the geometry is this module's to state once.
/// [`PageHeader::block_index_in`] is the same fact for a caller outside it.
const BITS_PER_WORD: usize = u64::BITS as usize;

/// Words in each of the two bitmaps.
const BITMAP_WORDS: usize = MAX_BLOCKS.div_ceil(BITS_PER_WORD);

/// [`PageHeader::class`] for a page that holds one over-sized or over-aligned
/// object. Not an index into the ladder.
const CLASS_LARGE: u8 = u8::MAX;

/// [`PageHeader::flags`]: sweep does not walk this page and never finalizes
/// anything on it.
const FLAG_IMMORTAL: u8 = 1;

const _: () = {
    assert!(PAGE_SIZE.is_power_of_two());
    assert!(MIN_BLOCK.is_multiple_of(BLOCK_GRANULE));
    assert!(MAX_BLOCK.is_multiple_of(BLOCK_GRANULE));
    assert!(MAX_BLOCK >= MIN_BLOCK);
    assert!(NUM_CLASSES < CLASS_LARGE as usize);
};

/// Round `n` up to the next multiple of `m`, which need **not** be a power of
/// two — the ladder's strides (24, 40, 56, …) are not.
const fn round_up_to_multiple(n: usize, m: usize) -> usize {
    n.div_ceil(m) * m
}

/// A rung of the size-class ladder.
///
/// The only constructor is [`SizeClass::of`], and it accepts a block only when
/// the block's alignment is exactly a `GcHeader`'s. **That is what keeps RT-01
/// exact.** RT-01's soundness rests on handing swept storage only to a request
/// the storage actually fits, and a ladder indexed by size alone would break it
/// for two layouts that share a size but not an alignment: file a `{48, 8}`
/// block, hand it to a `{48, 16}` request, and the payload lands at `base + 32`
/// where `base` is only 8-aligned. Everything `of` rejects goes to a page of its
/// own, keyed on the whole [`BlockLayout`].
///
/// Rounding a block *up* to its rung is sound for the same reason reusing an
/// exact match is: the stride is at least the block's size, the alignment is
/// uniform across the rung, and every block on the page starts its payload at
/// the same offset (see [`PageHeader::payload_offset`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SizeClass(u8);

impl SizeClass {
    /// `block`'s rung, or `None` if it is over-aligned or larger than the
    /// ladder — in which case it belongs on a large page.
    ///
    /// `const` for ADR-119: [`InlineClaimSite`](crate::InlineClaimSite) is a
    /// `const` initializer, so a descriptor whose block the ladder rejects fails
    /// the build rather than falling back to a wrapper call nobody noticed.
    #[inline]
    pub(crate) const fn of(block: BlockLayout) -> Option<SizeClass> {
        if block.align == BLOCK_GRANULE && block.size <= MAX_BLOCK {
            let size = if block.size < MIN_BLOCK {
                MIN_BLOCK
            } else {
                block.size
            };
            let index = (round_up_to_multiple(size, BLOCK_GRANULE) - MIN_BLOCK) / BLOCK_GRANULE;
            Some(SizeClass(index as u8))
        } else {
            None
        }
    }

    /// This rung's index into a per-class array.
    #[inline]
    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }

    /// The stride of every block on a page of this class.
    #[inline]
    pub(crate) const fn block_size(self) -> usize {
        MIN_BLOCK + self.0 as usize * BLOCK_GRANULE
    }

    /// The rung at `index`. Test-only: nothing on the allocation path needs to
    /// go this way, but the ladder's exhaustive tests do.
    #[cfg(test)]
    #[inline]
    pub(crate) fn from_index(index: usize) -> SizeClass {
        assert!(index < NUM_CLASSES);
        SizeClass(index as u8)
    }
}

/// The metadata at the base of every page.
///
/// `#[repr(C)]` for a predictable size, which the const assertion below the
/// struct depends on. Every mutable field is a [`Cell`]: a page is reached
/// through a `*mut PageHeader` that the heap hands around freely, so taking a
/// `&mut` to one would be an aliasing claim nothing can honour. `Cell` costs
/// nothing — these are plain loads and stores.
#[repr(C)]
pub(crate) struct PageHeader {
    /// The heap that owns this page, or 0 for a page in no heap.
    ///
    /// ADR-039 Decision 2's provenance, per page. Note that the *header* still
    /// carries the same id: the mark phase reads the header's copy first and
    /// only masks to the page once it has accepted it, so [`page_of`] is never
    /// applied to an address this heap did not allocate.
    heap_id: Cell<u32>,
    /// Byte stride between consecutive blocks.
    block_size: Cell<u32>,
    /// Byte offset of block 0 from the page's base. A multiple of `block_size`
    /// for a small page, so [`PageHeader::block_index`] is an exact division.
    first_block: Cell<u32>,
    /// How many blocks fit after `first_block`.
    block_count: Cell<u32>,
    /// Index of the last bitmap word that names a block — `block_count`
    /// rounded up to a word, minus one.
    ///
    /// Derived from `block_count`, and stored because the allocation loop reads
    /// it on every turn: deriving it there would put a divide-and-compare
    /// against a second field between the loop and its bitmap load.
    last_word: Cell<u32>,
    /// The bits of `allocated[last_word]` that name blocks this page really
    /// has. The tail of that word names nothing and must never be claimed.
    tail_mask: Cell<u64>,
    /// Byte offset from a block's base to its payload, written at page creation
    /// from [`GcHeader::payload_offset_for`] — ADR-039 Decision 1's authority,
    /// cached per page. Never an independent authority: the header still records
    /// what the allocator used, and `debug_assert`s here check the two agree.
    payload_offset: Cell<u32>,
    /// Lemire reciprocal of `block_size`: `(off * recip) >> 32 == off /
    /// block_size` exactly for every `off < PAGE_SIZE`. Pinned by
    /// `the_reciprocal_divides_exactly_for_every_stride_and_offset`, because it
    /// is a derivation and this codebase pins derivations.
    recip: Cell<u32>,
    /// Bytes in this page's backing allocation. `PAGE_SIZE` for a small page;
    /// a whole multiple of it for a large one.
    page_bytes: Cell<u32>,
    /// Set bits in `allocated`.
    live_count: Cell<u32>,
    /// The first bitmap word that may hold a free block. Sweep resets it to 0,
    /// which is what makes reuse lowest-address-first and therefore
    /// deterministic — see `a_reclaimed_block_is_reused_for_the_next_object_of_its_layout`.
    cursor: Cell<u32>,
    /// Ladder index, or [`CLASS_LARGE`].
    class: Cell<u8>,
    /// [`FLAG_IMMORTAL`].
    flags: Cell<u8>,
    _pad: [u8; 2],
    /// Next page of the owning heap. Every page is on exactly this one list.
    next: Cell<*mut PageHeader>,
    /// Next page on whichever *availability* list this page is on (the class's
    /// partial list, the empty pool, or the empty-large pool) — or null.
    next_of_class: Cell<*mut PageHeader>,
    /// 1 = the block holds an initialized `[GcHeader | payload]`.
    allocated: [Cell<u64>; BITMAP_WORDS],
    /// 1 = the mark phase reached the block this cycle.
    mark: [Cell<u64>; BITMAP_WORDS],
}

const _: () = {
    // The bitmaps must cover every block the *smallest* stride can fit, or a
    // page would hold blocks whose liveness has nowhere to live. This is a
    // compile-time check rather than an assert at page creation because
    // `size_of::<PageHeader>()` is a compile-time fact and the failure mode —
    // silently losing the tail of a page — is not one to discover at runtime.
    let first = round_up_to_multiple(std::mem::size_of::<PageHeader>(), MIN_BLOCK);
    assert!(first < PAGE_SIZE);
    assert!((PAGE_SIZE - first) / MIN_BLOCK <= BITMAP_WORDS * BITS_PER_WORD);
};

/// The page an address inside a page belongs to.
///
/// # Safety of the result
/// This is arithmetic, not a dereference, and it is *only* meaningful for an
/// address that really is inside a page this process allocated here. The one
/// caller that starts from a bare `GcRef` — `Heap::mark` — establishes that
/// first, by checking the header's own `heap_id` against its own (ADR-039
/// Decision 2). A `GcHeader` that no heap allocated carries id 0 or a minted id
/// no live heap holds, and is rejected before it ever reaches this function.
#[inline]
pub(crate) fn page_of(p: *const u8) -> *mut PageHeader {
    ((p as usize) & PAGE_MASK) as *mut PageHeader
}

impl PageHeader {
    /// Where each field the inline claim sequence touches sits within a
    /// `PageHeader` (ADR-119).
    ///
    /// Every one of these is `offset_of!` against a **private** field of this
    /// `#[repr(C)]` struct, for [`GcHeader::DESCRIPTOR_OFFSET`]'s reason: the
    /// alternative is a number written out in `lower.rs` that nothing keeps
    /// true. They are `pub(crate)` and reach the backend only inside an
    /// [`InlineClaimSite`](crate::InlineClaimSite), which is what stops them
    /// being six independent chances to pair one page's cursor with another's
    /// bitmap.
    ///
    /// **Repacking `PageHeader` is a generated-code change** (ADR-113 decision
    /// 2, widened by ADR-119), and the assertion that says so is
    /// `the_claim_site_displacements_name_the_fields_they_claim_to` in
    /// `heap.rs`, which reads a live page through every one of them.
    /// The word the next claim starts scanning from.
    pub(crate) const CURSOR_OFFSET: usize = core::mem::offset_of!(PageHeader, cursor);
    /// See [`PageHeader::CURSOR_OFFSET`].
    pub(crate) const LAST_WORD_OFFSET: usize = core::mem::offset_of!(PageHeader, last_word);
    /// See [`PageHeader::CURSOR_OFFSET`].
    pub(crate) const ALLOCATED_OFFSET: usize = core::mem::offset_of!(PageHeader, allocated);
    /// See [`PageHeader::CURSOR_OFFSET`].
    pub(crate) const LIVE_COUNT_OFFSET: usize = core::mem::offset_of!(PageHeader, live_count);

    /// Where block 0 of a **small** page of stride `block_size` begins.
    ///
    /// A function of the stride alone, which is why the inline claim sequence
    /// folds it to an immediate rather than loading `first_block`: both
    /// [`PageHeader::new_small`] and [`PageHeader::reclass`] compute exactly
    /// this, so every page on `Heap::partial[c]` has this value and no other.
    /// Stated here, once, so the fold is this module's derivation and not the
    /// backend's — `the_folded_first_block_is_the_one_every_page_of_the_class_has`
    /// checks it against a live page of every rung.
    pub(crate) const fn first_block_of(block_size: usize) -> usize {
        round_up_to_multiple(std::mem::size_of::<PageHeader>(), block_size)
    }

    /// A fresh page of `class`, owned by `heap_id`.
    ///
    /// Returns a raw pointer because a page outlives every borrow of it: the
    /// heap threads it onto two intrusive lists and reaches it again from a
    /// masked object address.
    pub(crate) fn new_small(class: SizeClass, heap_id: u32) -> *mut PageHeader {
        let block_size = class.block_size();
        let first_block = Self::first_block_of(block_size);
        let block_count = (PAGE_SIZE - first_block) / block_size;
        // Every small block is a `GcHeader` followed by a payload aligned no
        // more strictly than the header — `SizeClass::of` admits nothing else —
        // so one offset serves the whole page, and it is the one
        // `GcHeader::payload_offset_for` names.
        let payload_offset = GcHeader::payload_offset_for(BLOCK_GRANULE);
        Self::alloc_page(
            PAGE_SIZE,
            heap_id,
            class.index() as u8,
            block_size,
            first_block,
            block_count,
            payload_offset,
        )
    }

    /// A page holding exactly one block, for a descriptor the ladder rejects:
    /// too large, or aligned more strictly than a `GcHeader`.
    ///
    /// **No production descriptor takes this path** — every one of
    /// [`crate::descriptor::BUILTINS`] lands on the ladder, which
    /// `the_ladder_covers_every_builtin_descriptor` checks. It costs a whole
    /// page per object, which is right for a test fixture and would be wrong for
    /// anything a program allocates in a loop; nobody should "optimize" it
    /// without first making a real descriptor take it.
    ///
    /// # Panics
    /// Panics if the payload's alignment is at least [`PAGE_SIZE`]: the block's
    /// header must land inside the page's first `PAGE_SIZE` bytes or [`page_of`]
    /// would mask to the wrong address. ADR-039 records the same 32 KiB bound for
    /// its own reason — an alignment above it would not fit `payload_offset`.
    pub(crate) fn new_large(
        descriptor: &TypeDescriptor,
        payload_offset: usize,
        block: BlockLayout,
        heap_id: u32,
    ) -> *mut PageHeader {
        assert!(
            block.align < PAGE_SIZE,
            "payload alignment {} of descriptor {} exceeds the largest alignment \
             a GC page can place",
            descriptor.align(),
            descriptor.name
        );
        let first_block = round_up_to_multiple(std::mem::size_of::<PageHeader>(), block.align);
        let page_bytes = round_up_to_multiple(first_block + block.size, PAGE_SIZE);
        Self::alloc_page(
            page_bytes,
            heap_id,
            CLASS_LARGE,
            block.size,
            first_block,
            1,
            payload_offset,
        )
    }

    /// The one allocation site. Split out so `new_small` and `new_large` differ
    /// only in the geometry they compute, not in how they get memory.
    // Seven numbers describing one page's geometry; naming them in a struct
    // would only move the list.
    #[allow(clippy::too_many_arguments)]
    fn alloc_page(
        page_bytes: usize,
        heap_id: u32,
        class: u8,
        block_size: usize,
        first_block: usize,
        block_count: usize,
        payload_offset: usize,
    ) -> *mut PageHeader {
        debug_assert!(block_count <= BITMAP_WORDS * BITS_PER_WORD);
        debug_assert!(first_block + block_count * block_size <= page_bytes);
        let page_bytes_u32 =
            u32::try_from(page_bytes).expect("a GC page larger than 4 GiB is not a page");
        // Aligning the allocation to `PAGE_SIZE` is what makes `page_of` a mask
        // rather than a lookup. Nothing else about the page depends on it.
        let layout = Layout::from_size_align(page_bytes, PAGE_SIZE).expect("valid page layout");
        // SAFETY: `layout` has non-zero size.
        let raw = unsafe { std::alloc::alloc(layout) };
        if raw.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        let page = raw as *mut PageHeader;
        // The block area is deliberately *not* zeroed: nothing reads a block
        // before the allocator writes its header, and memset-ing 32 KiB per page
        // is the cost this design exists to avoid. The metadata below — both
        // bitmaps included — is fully written.
        //
        // SAFETY: `raw` is a fresh allocation of at least `size_of::<PageHeader>()`
        // bytes (the const assertion above proves the header fits before the
        // first block), aligned to `PAGE_SIZE` and therefore to the header.
        unsafe {
            std::ptr::write(
                page,
                PageHeader {
                    heap_id: Cell::new(heap_id),
                    block_size: Cell::new(block_size as u32),
                    first_block: Cell::new(first_block as u32),
                    block_count: Cell::new(block_count as u32),
                    last_word: Cell::new(last_word_for(block_count)),
                    tail_mask: Cell::new(tail_mask_for(block_count)),
                    payload_offset: Cell::new(payload_offset as u32),
                    recip: Cell::new(reciprocal(block_size)),
                    page_bytes: Cell::new(page_bytes_u32),
                    live_count: Cell::new(0),
                    cursor: Cell::new(0),
                    class: Cell::new(class),
                    flags: Cell::new(0),
                    _pad: [0; 2],
                    next: Cell::new(std::ptr::null_mut()),
                    next_of_class: Cell::new(std::ptr::null_mut()),
                    allocated: std::array::from_fn(|_| Cell::new(0)),
                    mark: std::array::from_fn(|_| Cell::new(0)),
                },
            );
        }
        page
    }

    /// Return `page`'s backing allocation to the global allocator.
    ///
    /// # Safety
    /// `page` must have come from [`PageHeader::new_small`] or
    /// [`PageHeader::new_large`] and must not be released twice, and no `GcRef`
    /// naming a block on it may be dereferenced afterwards — which is hazard H8,
    /// and is why only `Heap::drop` calls this.
    pub(crate) unsafe fn release(page: *mut PageHeader) {
        // SAFETY: the caller guarantees `page` is a live page.
        let bytes = unsafe { (*page).page_bytes.get() } as usize;
        let layout = Layout::from_size_align(bytes, PAGE_SIZE).expect("valid page layout");
        // SAFETY: the same layout the page was allocated with.
        unsafe { std::alloc::dealloc(page as *mut u8, layout) };
    }

    /// Re-purpose an **empty** page for another size class.
    ///
    /// This is what makes a page's storage reusable across layouts; a
    /// per-layout free list leaves an emptied bucket as dead capital for every
    /// other layout (RT-01).
    ///
    /// # Panics
    /// Panics if the page still holds a live block, or is a large page.
    pub(crate) fn reclass(&self, class: SizeClass) {
        assert_eq!(self.live_count.get(), 0, "reclassing a non-empty page");
        assert_ne!(self.class.get(), CLASS_LARGE, "reclassing a large page");
        let block_size = class.block_size();
        let first_block = Self::first_block_of(block_size);
        let block_count = (PAGE_SIZE - first_block) / block_size;
        self.class.set(class.index() as u8);
        self.block_size.set(block_size as u32);
        self.first_block.set(first_block as u32);
        self.block_count.set(block_count as u32);
        self.last_word.set(last_word_for(block_count));
        self.tail_mask.set(tail_mask_for(block_count));
        self.recip.set(reciprocal(block_size));
        self.payload_offset
            .set(GcHeader::payload_offset_for(BLOCK_GRANULE) as u32);
        self.cursor.set(0);
    }

    /// This page's base address, which is also this header's.
    #[inline]
    pub(crate) fn base(&self) -> *mut u8 {
        self as *const PageHeader as *mut u8
    }

    /// The address of block `index`.
    #[inline]
    pub(crate) fn block_ptr(&self, index: usize) -> *mut u8 {
        debug_assert!(index < self.block_count.get() as usize);
        // SAFETY: `index` is below `block_count`, and the geometry guarantees
        // `first_block + block_count * block_size <= page_bytes`.
        unsafe {
            self.base()
                .add(self.first_block.get() as usize + index * self.block_size.get() as usize)
        }
    }

    /// The index of the block that starts at `p`.
    ///
    /// `p` must be a block base on *this* page — the caller establishes that by
    /// having masked `p` to get here.
    #[inline]
    pub(crate) fn block_index(&self, p: *const u8) -> usize {
        let offset = (p as usize & !PAGE_MASK) - self.first_block.get() as usize;
        debug_assert_eq!(
            offset % self.block_size.get() as usize,
            0,
            "an address that is not a block base"
        );
        let index = ((offset as u64 * self.recip.get() as u64) >> 32) as usize;
        debug_assert_eq!(index, offset / self.block_size.get() as usize);
        debug_assert!(index < self.block_count.get() as usize);
        index
    }

    /// The index of the block named by bit `bit` of bitmap word `word` — the
    /// inverse of the split [`PageHeader::is_allocated`] does.
    ///
    /// Stated here because [`BITS_PER_WORD`] is: `Heap::sweep` and
    /// `Heap::finalize_all` walk the bitmaps a word at a time and need the
    /// index back, and reconstructing it there put this module's geometry in
    /// theirs.
    #[inline]
    pub(crate) fn block_index_in(&self, word: usize, bit: u32) -> usize {
        let index = word * BITS_PER_WORD + bit as usize;
        debug_assert!(index < self.block_count.get() as usize);
        index
    }

    /// Claim the lowest free block, or `None` if the page is full.
    ///
    /// This is the whole allocation fast path: one bitmap word load, an
    /// `andnot`, a `trailing_zeros`, one store.
    #[inline]
    pub(crate) fn claim_free_block(&self) -> Option<*mut u8> {
        let last = self.last_word.get() as usize;
        let mut w = self.cursor.get() as usize;
        while w <= last {
            let taken = self.allocated[w].get();
            // Only the last word can name blocks the page does not have, so the
            // mask is a select rather than the range comparison `valid_mask`
            // does — this loop runs once per allocation in the whole runtime.
            let free = if w == last {
                !taken & self.tail_mask.get()
            } else {
                !taken
            };
            if free != 0 {
                let bit = free.trailing_zeros();
                self.allocated[w].set(taken | (1u64 << bit));
                self.cursor.set(w as u32);
                self.live_count.set(self.live_count.get() + 1);
                return Some(self.block_ptr(self.block_index_in(w, bit)));
            }
            w += 1;
        }
        // Remember that everything below here is taken, so the next call does
        // not re-scan it. Sweep is what resets this.
        self.cursor.set((last + 1) as u32);
        None
    }

    /// Whether block `index` holds an initialized object.
    #[inline]
    pub(crate) fn is_allocated(&self, index: usize) -> bool {
        self.allocated[index / BITS_PER_WORD].get() & (1u64 << (index % BITS_PER_WORD)) != 0
    }

    /// Set block `index`'s mark bit and report whether it was **already** set.
    #[inline]
    pub(crate) fn test_and_set_mark(&self, index: usize) -> bool {
        let word = index / BITS_PER_WORD;
        let bit = 1u64 << (index % BITS_PER_WORD);
        let current = self.mark[word].get();
        self.mark[word].set(current | bit);
        current & bit != 0
    }

    /// Words of each bitmap this page actually uses.
    #[inline]
    pub(crate) fn words(&self) -> usize {
        self.last_word.get() as usize + 1
    }

    /// The bits of bitmap word `word` that name blocks this page really has.
    ///
    /// Test-only, and deliberately the *unoptimized* spelling: it reads
    /// `block_count` and does the arithmetic longhand, so
    /// `the_bitmap_tail_never_names_a_block` is pinning the cached `last_word`
    /// and `tail_mask` against a definition rather than against themselves.
    /// [`PageHeader::claim_free_block`] does not call it — the allocation loop
    /// only ever needs "is this the last word", which is a select.
    #[cfg(test)]
    fn valid_mask(&self, word: usize) -> u64 {
        let start = word * BITS_PER_WORD;
        let count = self.block_count.get() as usize;
        if start + BITS_PER_WORD <= count {
            u64::MAX
        } else if start >= count {
            0
        } else {
            (1u64 << (count - start)) - 1
        }
    }

    /// The `allocated` bitmap word at `word`.
    #[inline]
    pub(crate) fn allocated_word(&self, word: usize) -> u64 {
        self.allocated[word].get()
    }

    /// Overwrite the `allocated` bitmap word at `word`.
    #[inline]
    pub(crate) fn set_allocated_word(&self, word: usize, value: u64) {
        self.allocated[word].set(value);
    }

    /// Whether this page is a large page laid out for exactly `block`.
    ///
    /// The large pool is keyed on the whole layout, not on a size class, for
    /// the reason [`SizeClass::of`] rejects the block in the first place: an
    /// over-aligned payload's offset and its page's `first_block` both depend on
    /// the alignment, so two layouts that share a size do not share a page.
    #[inline]
    pub(crate) fn fits_large(&self, payload_offset: usize, block: BlockLayout) -> bool {
        self.class().is_none()
            && self.block_size() == block.size
            && self.payload_offset() == payload_offset
            && self.first_block.get() as usize
                == round_up_to_multiple(std::mem::size_of::<PageHeader>(), block.align)
    }

    /// The `mark` bitmap word at `word`.
    #[inline]
    pub(crate) fn mark_word(&self, word: usize) -> u64 {
        self.mark[word].get()
    }

    /// Clear the `mark` bitmap word at `word`, for the next cycle.
    #[inline]
    pub(crate) fn clear_mark_word(&self, word: usize) {
        self.mark[word].set(0);
    }

    /// Drop every allocated and mark bit, and rewind the allocation cursor.
    /// Used by `Heap::reset`, which keeps the storage but repudiates everything
    /// on it.
    pub(crate) fn clear_bitmaps(&self) {
        for word in 0..BITMAP_WORDS {
            self.allocated[word].set(0);
            self.mark[word].set(0);
        }
        self.live_count.set(0);
        self.cursor.set(0);
    }

    /// Which heap owns this page. 0 for none.
    #[inline]
    pub(crate) fn heap_id(&self) -> u32 {
        self.heap_id.get()
    }

    /// Re-stamp the owning heap, for `Heap::reset`'s freshly minted identity.
    #[inline]
    pub(crate) fn set_heap_id(&self, id: u32) {
        self.heap_id.set(id);
    }

    /// The stride of this page's blocks.
    #[inline]
    pub(crate) fn block_size(&self) -> usize {
        self.block_size.get() as usize
    }

    /// Byte offset from a block's base to its payload.
    #[inline]
    pub(crate) fn payload_offset(&self) -> usize {
        self.payload_offset.get() as usize
    }

    /// Where block 0 starts, relative to this page's base.
    ///
    /// Test-only, and that is the point: nothing on the allocation path reads
    /// this field through an accessor, and generated code folds
    /// [`PageHeader::first_block_of`] instead of loading it (ADR-119). The two
    /// tests that use this — `the_folded_first_block_is_the_one_every_page_of_the_class_has`
    /// here and `the_claim_site_displacements_name_the_fields_they_claim_to` in
    /// `heap.rs` — are pinning the fold against the field it stands in for.
    #[cfg(test)]
    #[inline]
    pub(crate) fn first_block(&self) -> usize {
        self.first_block.get() as usize
    }

    /// How many blocks this page has.
    #[inline]
    pub(crate) fn block_count(&self) -> usize {
        self.block_count.get() as usize
    }

    /// How many of them are live.
    #[inline]
    pub(crate) fn live_count(&self) -> u32 {
        self.live_count.get()
    }

    /// Record that `freed` blocks stopped being live.
    #[inline]
    pub(crate) fn release_blocks(&self, freed: u32) {
        self.live_count.set(self.live_count.get() - freed);
    }

    /// Bytes of address space this page occupies.
    #[inline]
    pub(crate) fn page_bytes(&self) -> usize {
        self.page_bytes.get() as usize
    }

    /// Rewind the allocation cursor to the lowest word, so the next claim takes
    /// the lowest free block.
    #[inline]
    pub(crate) fn rewind_cursor(&self) {
        self.cursor.set(0);
    }

    /// This page's ladder rung, or `None` if it is a large page.
    #[inline]
    pub(crate) fn class(&self) -> Option<SizeClass> {
        let class = self.class.get();
        if class == CLASS_LARGE {
            None
        } else {
            Some(SizeClass(class))
        }
    }

    /// Whether sweep must leave this page alone entirely.
    #[inline]
    pub(crate) fn is_immortal(&self) -> bool {
        self.flags.get() & FLAG_IMMORTAL != 0
    }

    /// Mark this page immortal: sweep never walks it, never finalizes anything
    /// on it, and never clears one of its bits.
    #[inline]
    pub(crate) fn set_immortal(&self) {
        self.flags.set(self.flags.get() | FLAG_IMMORTAL);
    }

    /// Clear the immortal flag, so `Heap::reset` can return the page to the
    /// ordinary pool.
    #[inline]
    pub(crate) fn clear_immortal(&self) {
        self.flags.set(self.flags.get() & !FLAG_IMMORTAL);
    }

    /// The next page of the owning heap.
    #[inline]
    pub(crate) fn next(&self) -> *mut PageHeader {
        self.next.get()
    }

    /// Link this page onto the owning heap's page list.
    #[inline]
    pub(crate) fn set_next(&self, page: *mut PageHeader) {
        self.next.set(page);
    }

    /// The next page on whichever availability list this page is on.
    #[inline]
    pub(crate) fn next_of_class(&self) -> *mut PageHeader {
        self.next_of_class.get()
    }

    /// Link this page onto an availability list.
    #[inline]
    pub(crate) fn set_next_of_class(&self, page: *mut PageHeader) {
        self.next_of_class.set(page);
    }
}

/// The multiplier that turns division by `block_size` into a multiply-shift.
///
/// `floor(off * recip / 2^32) == off / block_size` for every `off < PAGE_SIZE`.
/// The proof is the usual one for round-up reciprocals: with `M` set to
/// `floor(2^32/d) + 1`, the error term is `k*(d - 2^32 mod d) + M*(off mod d)`,
/// and both parts stay below `2^32` while `off < 2^15` and `d <= MAX_BLOCK`. It is a
/// derivation, so it is pinned exhaustively rather than argued —
/// `the_reciprocal_divides_exactly_for_every_stride_and_offset`. Raising
/// `PAGE_SIZE` past 2^24 or `MAX_BLOCK` past 256 without re-deriving it would
/// start returning off-by-one indices, which corrupts liveness silently.
///
/// A large page's stride is outside that bound, and does not need to be inside
/// it: such a page has one block at offset zero, and zero divides exactly by
/// anything. `block_index`'s `debug_assert` against `/` is what would catch it
/// if that ever stopped being true.
#[inline]
fn reciprocal(block_size: usize) -> u32 {
    ((1u64 << 32) / block_size as u64) as u32 + 1
}

/// The index of the last bitmap word that names a block. Never negative: every
/// page has at least one block.
fn last_word_for(block_count: usize) -> u32 {
    debug_assert!(block_count > 0);
    (block_count.div_ceil(BITS_PER_WORD) - 1) as u32
}

/// The bits of that last word which name blocks the page really has.
fn tail_mask_for(block_count: usize) -> u64 {
    match block_count % BITS_PER_WORD {
        0 => u64::MAX,
        tail => (1u64 << tail) - 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every page created by a test is released by it, so the suite does not
    /// leak 32 KiB per assertion under a sanitizer.
    struct OwnedPage(*mut PageHeader);

    impl OwnedPage {
        fn small(class: SizeClass) -> OwnedPage {
            OwnedPage(PageHeader::new_small(class, 1))
        }
        fn get(&self) -> &PageHeader {
            // SAFETY: the page is live for this guard's lifetime.
            unsafe { &*self.0 }
        }
    }

    impl Drop for OwnedPage {
        fn drop(&mut self) {
            // SAFETY: the page came from `new_small`/`new_large` and nothing
            // outlives this guard.
            unsafe { PageHeader::release(self.0) };
        }
    }

    /// The ladder must cover the whole language, not just the scalars a
    /// benchmark happened to allocate. `BUILTINS` is the closed set of
    /// descriptors a program can allocate through, so this makes "the fast path
    /// covers everything" a checked invariant rather than a hope.
    #[test]
    fn the_ladder_covers_every_builtin_descriptor() {
        for descriptor in crate::descriptor::BUILTINS {
            let (payload_offset, block) = BlockLayout::of(descriptor);
            let class = SizeClass::of(block).unwrap_or_else(|| {
                panic!(
                    "descriptor {} has block {{size: {}, align: {}}}, which the \
                     ladder does not hold (MIN_BLOCK = {MIN_BLOCK}, MAX_BLOCK = \
                     {MAX_BLOCK}, BLOCK_GRANULE = {BLOCK_GRANULE})",
                    descriptor.name, block.size, block.align
                )
            });
            assert!(
                class.block_size() >= block.size,
                "class {} is too small for {}",
                class.index(),
                descriptor.name
            );
            assert!(
                class.block_size() - block.size < BLOCK_GRANULE,
                "class {} wastes a whole granule on {}",
                class.index(),
                descriptor.name
            );
            assert_eq!(
                payload_offset,
                GcHeader::payload_offset_for(BLOCK_GRANULE),
                "every small block starts its payload at the page's offset"
            );
        }
    }

    /// **An `Int` costs 24 bytes, and the ladder holds a 24-byte rung to put it
    /// on.** Both halves have to be true for ADR-109's density win, and they are
    /// separate facts: a 16-byte `GcHeader` makes the *block* 24, and
    /// `MIN_BLOCK` following the header is what puts a *rung* exactly there
    /// rather than a granule above it.
    ///
    /// `Int` is the descriptor to pin because it is the one the benchmarks
    /// allocate in a loop — `collatz`, `primes` and `mandelbrot` are essentially
    /// `Int` allocation with arithmetic in between — so it is where the density
    /// win either shows up or does not. Without this test, a change that grew
    /// the header or coarsened `BLOCK_GRANULE` would move `Int` to the next rung
    /// and the only symptom would be a benchmark number nobody attributed.
    ///
    /// This is deliberately a literal 24 rather than a re-derivation. Every
    /// other assertion in this module derives, because derivations are what this
    /// codebase pins; this one is the *claim* ADR-109 makes, and a claim checked
    /// against its own derivation is checked against nothing.
    #[test]
    fn an_int_block_is_the_header_plus_eight() {
        let (payload_offset, block) = BlockLayout::of(&crate::scalars::INT);
        assert_eq!(payload_offset, 16, "the header, and no padding");
        assert_eq!(block.size, 24, "16 bytes of header and 8 of payload");
        assert_eq!(block.align, BLOCK_GRANULE);
        let class = SizeClass::of(block).expect("an Int is on the ladder");
        assert_eq!(
            class.block_size(),
            24,
            "an Int must land on a rung that is exactly its block, not above it"
        );
    }

    /// The reciprocal is a derivation, and this is the derivation's proof.
    /// Exhaustive over every stride the ladder can hold and every offset a page
    /// can address — if `PAGE_SIZE` or `MAX_BLOCK` ever move past the bound the
    /// identity holds on, this is what fails.
    #[test]
    fn the_reciprocal_divides_exactly_for_every_stride_and_offset() {
        for index in 0..NUM_CLASSES {
            let stride = SizeClass::from_index(index).block_size();
            let recip = reciprocal(stride);
            for offset in 0..PAGE_SIZE {
                let derived = ((offset as u64 * recip as u64) >> 32) as usize;
                assert_eq!(
                    derived,
                    offset / stride,
                    "reciprocal for stride {stride} disagrees with division at offset {offset}"
                );
            }
        }
    }

    /// The round trip that liveness rests on: an address handed out by
    /// `block_ptr` must mask back to its page and index back to its block.
    #[test]
    fn every_block_round_trips_through_the_mask_and_the_index() {
        for index in 0..NUM_CLASSES {
            let class = SizeClass::from_index(index);
            let page = OwnedPage::small(class);
            let p = page.get();
            assert!(p.block_count() > 0);
            for block in 0..p.block_count() {
                let address = p.block_ptr(block);
                assert_eq!(
                    page_of(address),
                    page.0,
                    "class {index} block {block} masked to the wrong page"
                );
                assert_eq!(
                    p.block_index(address),
                    block,
                    "class {index} block {block} indexed wrong"
                );
                assert_eq!(
                    address as usize % BLOCK_GRANULE,
                    0,
                    "every block base must be header-aligned"
                );
            }
            // One past the end must still be inside the page, or the geometry
            // has handed out storage the allocation does not cover.
            let end = p.first_block.get() as usize + p.block_count() * p.block_size();
            assert!(end <= p.page_bytes());
        }
    }

    #[test]
    fn first_block_is_a_multiple_of_block_size_and_clears_the_header() {
        for index in 0..NUM_CLASSES {
            let class = SizeClass::from_index(index);
            let page = OwnedPage::small(class);
            let p = page.get();
            let first = p.first_block.get() as usize;
            assert_eq!(first % p.block_size(), 0, "class {index}");
            assert!(first >= std::mem::size_of::<PageHeader>(), "class {index}");
            assert!(p.block_count() <= BITMAP_WORDS * 64, "class {index}");
        }
    }

    /// A page hands out exactly its block count and then reports itself full —
    /// the property that keeps `Heap::alloc_raw`'s page-walking loop finite.
    #[test]
    fn claiming_exhausts_a_page_exactly_block_count_times() {
        let class = SizeClass::from_index(0);
        let page = OwnedPage::small(class);
        let p = page.get();
        let mut claimed = Vec::new();
        while let Some(block) = p.claim_free_block() {
            claimed.push(block);
        }
        assert_eq!(claimed.len(), p.block_count());
        assert_eq!(p.live_count(), p.block_count() as u32);
        assert!(p.claim_free_block().is_none(), "a full page stays full");
        // Distinct, ascending, one stride apart.
        for pair in claimed.windows(2) {
            assert_eq!(pair[1] as usize - pair[0] as usize, p.block_size());
        }
        for (index, block) in claimed.iter().enumerate() {
            assert!(p.is_allocated(index));
            assert_eq!(p.block_index(*block), index);
        }
    }

    /// The tail of the last bitmap word names blocks the page does not have.
    /// Claiming one would hand out storage past the allocation.
    #[test]
    fn the_bitmap_tail_never_names_a_block() {
        for index in 0..NUM_CLASSES {
            let class = SizeClass::from_index(index);
            let page = OwnedPage::small(class);
            let p = page.get();
            let mut total = 0u32;
            for word in 0..p.words() {
                total += p.valid_mask(word).count_ones();
            }
            assert_eq!(total, p.block_count() as u32, "class {index}");
            assert_eq!(p.valid_mask(p.words()), 0, "class {index}");
            // The cached pair the allocation loop actually reads must agree
            // with the longhand definition above.
            assert_eq!(p.last_word.get() as usize, p.words() - 1, "class {index}");
            assert_eq!(
                p.tail_mask.get(),
                p.valid_mask(p.words() - 1),
                "class {index}"
            );
        }
    }

    /// Where block 0 starts is a function of the stride and nothing else, on
    /// every rung and after a re-class.
    ///
    /// **This is what licenses ADR-119's fold.** The inline claim sequence does
    /// not load `first_block`; it adds an immediate, because every page on a
    /// class's availability list has the same one. That is true because
    /// [`PageHeader::new_small`] and [`PageHeader::reclass`] both compute it
    /// through [`PageHeader::first_block_of`] — and a page that reached the list
    /// with a different value would have generated code writing headers one
    /// stride out of place, silently, on someone else's block.
    #[test]
    fn the_folded_first_block_is_the_one_every_page_of_the_class_has() {
        for index in 0..NUM_CLASSES {
            let class = SizeClass::from_index(index);
            let page = OwnedPage::small(class);
            let p = page.get();
            assert_eq!(
                p.first_block(),
                PageHeader::first_block_of(class.block_size()),
                "a fresh page of class {index}"
            );
            // …and after the page has been emptied and handed to another rung,
            // which is the path `Heap::grow_class` takes from the empty pool.
            for other in (0..NUM_CLASSES).rev() {
                let other_class = SizeClass::from_index(other);
                p.reclass(other_class);
                assert_eq!(
                    p.first_block(),
                    PageHeader::first_block_of(other_class.block_size()),
                    "class {index} re-classed to {other}"
                );
                assert_eq!(p.block_size(), other_class.block_size());
            }
        }
    }

    #[test]
    fn a_freed_block_is_reclaimed_lowest_first() {
        let page = OwnedPage::small(SizeClass::from_index(0));
        let p = page.get();
        let first = p.claim_free_block().expect("a fresh page has room");
        let second = p.claim_free_block().expect("a fresh page has room");
        // Free the lower of the two and rewind, the way sweep does.
        p.set_allocated_word(0, p.allocated_word(0) & !1);
        p.release_blocks(1);
        p.rewind_cursor();
        assert_eq!(
            p.claim_free_block().expect("the hole is claimable"),
            first,
            "the lowest hole must be reused first"
        );
        assert_ne!(first, second);
    }

    #[test]
    fn marking_reports_the_previous_state_and_starts_clear() {
        let page = OwnedPage::small(SizeClass::from_index(1));
        let p = page.get();
        for index in [0usize, 1, 63, 64, 65] {
            assert!(!p.test_and_set_mark(index), "the bitmap starts clear");
            assert!(p.test_and_set_mark(index), "the second visit sees the bit");
        }
        p.clear_mark_word(0);
        assert!(!p.test_and_set_mark(0));
        assert!(p.test_and_set_mark(64), "clearing word 0 left word 1 alone");
    }

    /// The point of a page pool: storage emptied by one layout is usable by
    /// another, which a per-layout free list could never do.
    #[test]
    fn a_reclassed_page_takes_the_new_geometry() {
        let page = OwnedPage::small(SizeClass::from_index(0));
        let p = page.get();
        let before = p.block_count();
        let target = SizeClass::from_index(NUM_CLASSES - 1);
        p.reclass(target);
        assert_eq!(p.block_size(), MAX_BLOCK);
        assert!(p.block_count() < before);
        assert_eq!(p.class(), Some(target));
        let block = p.claim_free_block().expect("a reclassed page has room");
        assert_eq!(p.block_index(block), 0);
        assert_eq!(page_of(block), page.0);
    }

    /// The large path exists for exactly one thing: a payload aligned more
    /// strictly than a `GcHeader`. Its whole job is to place that payload at its
    /// alignment while keeping the header inside the first page unit, so
    /// `page_of` still answers.
    #[test]
    fn a_large_page_places_an_overaligned_payload_at_its_alignment() {
        for align in [64usize, 256, 4096, PAGE_SIZE / 2] {
            let block = BlockLayout {
                size: GcHeader::payload_offset_for(align) + 8,
                align,
            };
            let payload_offset = GcHeader::payload_offset_for(align);
            let raw = PageHeader::new_large(&crate::scalars::INT, payload_offset, block, 0);
            // SAFETY: freshly created, released at the end of this iteration.
            let p = unsafe { &*raw };
            assert_eq!(p.block_count(), 1);
            assert_eq!(p.class(), None);
            let base = p.claim_free_block().expect("a large page has one block");
            assert_eq!(base as usize % align, 0, "align {align}");
            // SAFETY: `payload_offset` is inside the block.
            let payload = unsafe { base.add(payload_offset) };
            assert_eq!(payload as usize % align, 0, "align {align}");
            assert_eq!(page_of(base), raw, "the header stays in the first unit");
            assert_eq!(p.block_index(base), 0);
            assert!(p.claim_free_block().is_none());
            // SAFETY: nothing else names this page.
            unsafe { PageHeader::release(raw) };
        }
    }

    #[test]
    #[should_panic(expected = "exceeds the largest alignment a GC page can place")]
    fn an_alignment_a_page_cannot_place_is_a_panic_naming_the_descriptor() {
        let block = BlockLayout {
            size: PAGE_SIZE + 8,
            align: PAGE_SIZE,
        };
        let _ = PageHeader::new_large(&crate::scalars::INT, PAGE_SIZE, block, 0);
    }

    /// Everything the ladder rejects, and why.
    #[test]
    fn the_ladder_rejects_over_alignment_and_over_size() {
        assert_eq!(
            SizeClass::of(BlockLayout {
                size: 48,
                align: BLOCK_GRANULE
            })
            .map(SizeClass::block_size),
            Some(48)
        );
        assert_eq!(
            SizeClass::of(BlockLayout {
                size: 48,
                align: 16
            }),
            None,
            "same size, stricter alignment: not the same page"
        );
        assert_eq!(
            SizeClass::of(BlockLayout {
                size: MAX_BLOCK + 1,
                align: BLOCK_GRANULE
            }),
            None,
            "one past the ladder is a large page, not rung zero"
        );
        // A block smaller than the header cannot exist, but the floor is still
        // the floor: rounding must never produce a negative index.
        assert_eq!(
            SizeClass::of(BlockLayout {
                size: 1,
                align: BLOCK_GRANULE
            })
            .map(SizeClass::index),
            Some(0)
        );
        // Every size in between lands on a rung that holds it.
        for size in MIN_BLOCK..=MAX_BLOCK {
            let class = SizeClass::of(BlockLayout {
                size,
                align: BLOCK_GRANULE,
            })
            .expect("inside the ladder");
            assert!(class.block_size() >= size);
            assert!(class.index() < NUM_CLASSES);
        }
    }

    /// Two heaps' pages must not alias, or one heap's mark phase would clear
    /// the other's liveness. The mask is per-allocation, so this is really a
    /// statement about the global allocator, and it is cheap to check.
    #[test]
    fn distinct_pages_mask_to_distinct_bases() {
        let a = OwnedPage::small(SizeClass::from_index(0));
        let b = OwnedPage::small(SizeClass::from_index(0));
        assert_ne!(a.0, b.0);
        assert_eq!(a.0 as usize % PAGE_SIZE, 0);
        assert_eq!(b.0 as usize % PAGE_SIZE, 0);
        let block = b.get().claim_free_block().expect("room");
        assert_eq!(page_of(block), b.0);
        assert_ne!(page_of(block), a.0);
    }
}
