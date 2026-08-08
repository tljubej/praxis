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
//!   3. **Clear the weak set** — scan the one set that names objects without
//!      keeping them alive (the crash debugger's value slots) and turn every
//!      entry naming a block step 2 just reclaimed into an absence (ADR-106).
//!      This runs inside the collection because a reclaimed block is only
//!      *recognisable* as one between the sweep and the next allocation.

use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::ptr::NonNull;
use std::sync::OnceLock;

use crate::descriptor::{Payload, TypeDescriptor};
use crate::gc::{GcHeader, GcRef, HeapId};
use crate::page::{self, PageHeader, SizeClass, NUM_CLASSES};
use crate::roots::{RootSet, RuntimeRoots, WeakSet};
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
///
/// # Generated code holds no token, and does not need one (ADR-113, ADR-119)
///
/// Since ADR-113 the Cranelift backend reproduces [`Heap::collection_is_due`]
/// inline and, when it answers `false`, reads an interned small `Int` out of
/// [`crate::small_int`]'s table without entering this module at all. That is not
/// a forged token, and the reason is what this type means: **the token is
/// permission to *collect*, not permission to allocate.** It takes that branch
/// only where `maybe_collect` would have returned `false`, which is the branch
/// on which `pace` mints a token having done nothing at all. Where the predicate
/// answers `true` the inline path branches to `praxis_alloc_int`, which paces
/// through `pace` exactly as before.
///
/// **ADR-113 also said "the inline path allocates nothing — it hands back an
/// immortal the runtime minted before `main` ran". Since ADR-119 that sentence
/// is false**: on the branch the predicate leaves open, generated code claims a
/// block out of a page's `allocated` bitmap, writes the header and the payload
/// itself, and bumps both live counters and the pacing charge — everything
/// `alloc_raw` → `claim_block` → `occupy` does, in that order, without entering
/// this module. What replaced the sentence is not weaker, and it is three parts:
///
/// 1. **Entry.** Every store the sequence performs is dominated, in the emitted
///    Cranelift CFG, by the branch on [`Heap::collection_is_due`]. Asserted with
///    a dominator tree over a function with two claim sites, so it is a
///    dominance claim and not a claim about one lowering's shape.
/// 2. **Duration**, which is the part that carries the weight. Between that
///    branch and the last store there is **no call**, and a collection begins
///    only inside `Heap::collect_inner`, which generated code reaches only
///    through a `praxis_*` wrapper. So *not due on entry* implies *not due
///    throughout*: no sweep can observe the block half-written, and a re-entrant
///    claim cannot be handed the same free bit because there is nothing to
///    re-enter through.
/// 3. **State.** The heap is left field-for-field as the paced path would have
///    left it, both live counters included — each is only ever *decremented*
///    elsewhere and never recomputed, so a skipped increment underflows rather
///    than decays.
///
/// The store order (header, payload, `allocated` bit, counters) is a severity
/// ranking against a collection part 2 says cannot occur — **not** the safety
/// argument. All three parts are claims about an instruction stream, so all
/// three are carried by tests in `crates/praxis-codegen-cranelift/src/lower.rs`
/// that read the emitted Cranelift, and the displacements they name are checked
/// against live objects by `the_claim_site_displacements_name_the_fields_they_claim_to`
/// below. [`InlineInternSite`] and [`InlineClaimSite`] carry the half a type can:
/// which table may be probed, and which descriptors have a claim sequence at all.
#[must_use = "a Safepoint is the permission to allocate; dropping it wasted a pacing check"]
pub struct Safepoint<'a>(PhantomData<&'a Heap>);

/// The pacing predicate's operands, as displacements: where the `Heap` hangs off
/// a [`RuntimeContext`](crate::RuntimeContext), and where its two pacing words
/// sit inside it.
///
/// # Not a constructor argument, anywhere
///
/// [`Self::new`] takes nothing and reads all three off `Heap`'s own
/// [`Heap::BYTES_SINCE_COLLECT_OFFSET`] and [`Heap::COLLECT_THRESHOLD_OFFSET`],
/// so a site that described a table to probe — or a block to claim — *without*
/// carrying the pacing predicate's operands has no spelling. That is the one
/// thing [`InlineInternSite`] exists to withhold, and skipping the predicate is
/// the one thing ADR-040's [`Safepoint`] exists to make unwritable.
///
/// # One value, because the compare has one authority
///
/// [`Heap::collection_is_due`] is the one statement of the predicate and
/// generated code is the one reader that cannot call it: it loads these two
/// words and compares them. The direction (`>=`, so that a zero threshold is
/// *always* due) and the operand order are therefore transcribed from a rule the
/// compiler cannot check, and every transcription is a fresh chance to write it
/// backwards. Both [`InlineInternSite`] and [`InlineClaimSite`] carry *this*
/// value rather than three fields each, so the backend's `emit_pacing_test` —
/// the one place the transcription lives — cannot pair one site's `since` with
/// another's `threshold`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacingOffsets {
    heap_offset: usize,
    bytes_since_collect_offset: usize,
    collect_threshold_offset: usize,
}

impl PacingOffsets {
    /// No parameters, and that is the point — see the type's doc.
    const fn new() -> PacingOffsets {
        PacingOffsets {
            heap_offset: core::mem::offset_of!(crate::RuntimeContext, heap),
            bytes_since_collect_offset: Heap::BYTES_SINCE_COLLECT_OFFSET,
            collect_threshold_offset: Heap::COLLECT_THRESHOLD_OFFSET,
        }
    }

    /// Where the `Heap` pointer sits in a `RuntimeContext`. The first load of
    /// the sequence, and the base the two pacing loads are relative to.
    #[must_use]
    pub const fn heap_offset(self) -> usize {
        self.heap_offset
    }

    /// Where [`Heap::bytes_since_collect`] sits within a `Heap`.
    ///
    /// The predicate's left operand — and, for the claim sequence, the word it
    /// loads a second time and stores back, once, at the end: the pacing charge,
    /// which is not the predicate.
    #[must_use]
    pub const fn bytes_since_collect_offset(self) -> usize {
        self.bytes_since_collect_offset
    }

    /// Where [`Heap::collect_threshold`] sits within a `Heap`.
    ///
    /// Generated code loads this word and the one above and takes the branch
    /// `since >= threshold` to its cold path. That is
    /// [`Heap::collection_is_due`] transcribed, and that function's doc is where
    /// the obligation is written down.
    #[must_use]
    pub const fn collect_threshold_offset(self) -> usize {
        self.collect_threshold_offset
    }
}

/// Everything generated code may bake in to answer an interned scalar inline,
/// and nothing else (ADR-113).
///
/// # What this makes unrepresentable
///
/// The Cranelift backend's inline sequence for `Inst::Materialize { Int }` is
/// four displacements and three immediates: where `Heap` hangs off the context,
/// where the two pacing words sit inside it, where the intern table's base
/// pointer sits in the context, and the range and stride of the table itself.
/// Six of those seven numbers name a **private** field of a `#[repr(C)]` struct
/// in this crate. Handed to the backend as loose constants they would be six
/// independent chances to pair the `Int` table's base with the `Char` table's
/// bounds — a read past the end of a table whose length is the only thing
/// keeping the probe in bounds.
///
/// So they are one value with private fields, and there is exactly one of it:
/// [`crate::small_int::INLINE_INTERN_SITE`]. `InlineInternSite::new` is
/// `pub(crate)`, so a site can only be minted inside this crate — and the one
/// place it is minted is beside the range constants it describes, in the module
/// whose doc already calls itself "the one statement of the range". "Inline-probe
/// a table the backend has no right to probe" has no spelling, because there is
/// no second value to name; a future `Char` arm (P-4a) mints its own in
/// `small_char.rs`, next to *its* bounds, and cannot get `Int`'s by accident.
///
/// # And the half it cannot make unrepresentable
///
/// The pacing offsets are **not** arguments to `new`: it fills them from
/// [`PacingOffsets::new`], which reads `Heap`'s own
/// [`Heap::BYTES_SINCE_COLLECT_OFFSET`] and
/// [`Heap::COLLECT_THRESHOLD_OFFSET`], so a site cannot exist that describes an
/// intern table without also carrying the pacing predicate's operands. That is
/// as far as a type can go. A type cannot force the backend to *emit* the
/// compare — that claim is about an instruction stream, and it is carried by
/// `an_inline_int_box_tests_the_pacing_counter_before_it_reads_the_table` in the
/// backend, which reads the emitted IR. ADR-113 says so plainly rather than
/// implying the type proved more than it did.
#[derive(Clone, Copy, Debug)]
pub struct InlineInternSite {
    pacing: PacingOffsets,
    table_offset: usize,
    min: i64,
    span: u64,
    stride_shift: u8,
}

impl InlineInternSite {
    /// The site for an intern table whose base pointer sits at `table_offset`
    /// within a [`RuntimeContext`](crate::RuntimeContext) and which holds one
    /// object per value in `min..=max`, `stride` bytes apart.
    ///
    /// `pub(crate)`, for [`crate::immortal::ImmortalWitness`]'s reason: minting
    /// is confined to the modules that own the ranges, so the set of tables
    /// generated code may probe is a list this crate wrote rather than anything
    /// a caller can assemble.
    ///
    /// # Panics
    /// Panics at compile time (in a `const` context) if `max < min`, if the
    /// range does not fit a `u64`, or if `stride` is not a power of two — the
    /// three assumptions the emitted sequence's arithmetic rests on. Every call
    /// is a `const` initializer, so "panics" here means "fails the build".
    pub(crate) const fn new(
        table_offset: usize,
        min: i64,
        max: i64,
        stride: usize,
    ) -> InlineInternSite {
        assert!(min <= max, "an intern table's range runs upwards");
        assert!(stride.is_power_of_two(), "the index scale must be a shift");
        InlineInternSite {
            // Not a parameter: a site that described a table but not the pacing
            // predicate would be permission to skip the predicate, which is the
            // one thing this type exists to withhold. `PacingOffsets::new`
            // takes nothing, so there is nothing here to get wrong.
            pacing: PacingOffsets::new(),
            table_offset,
            min,
            // `max - min` as an unsigned width, which is the immediate the
            // one-compare range test uses. See [`Self::span`].
            span: max.wrapping_sub(min) as u64,
            stride_shift: stride.trailing_zeros() as u8,
        }
    }

    /// The pacing predicate's operands — the three displacements generated code
    /// emits the compare from, before it looks at the value at all.
    ///
    /// The *same* value [`InlineClaimSite::pacing`] answers, which is what lets
    /// the backend transcribe [`Heap::collection_is_due`] in one place. See
    /// [`PacingOffsets`].
    #[must_use]
    pub const fn pacing(self) -> PacingOffsets {
        self.pacing
    }

    /// Where the table's base pointer sits in a `RuntimeContext`.
    #[must_use]
    pub const fn table_offset(self) -> usize {
        self.table_offset
    }

    /// The lowest interned value — the addend that turns a value into an index.
    #[must_use]
    pub const fn min(self) -> i64 {
        self.min
    }

    /// `max - min`, as the unsigned bound of the **one** compare that decides
    /// membership.
    ///
    /// Generated code tests `(value - min) as u64 <= span` rather than comparing
    /// against `min` and `max` separately: in two's complement that single
    /// unsigned compare is exactly `min <= value <= max` for every `i64`
    /// including the wrapping ones, it reuses the subtract the index needs
    /// anyway, and it costs one branch where the two-compare form costs two.
    /// `crate::small_int`'s
    /// `the_unsigned_range_test_generated_code_emits_answers_index_of` is the
    /// proof, over the boundary values and both extremes of the type.
    #[must_use]
    pub const fn span(self) -> u64 {
        self.span
    }

    /// `log2(stride)` — the shift that scales an index to a byte offset.
    ///
    /// A shift rather than a multiply because the stride is a pointer width.
    /// That was load-bearing when the sequence was emitted at
    /// `opt_level = "none"`, where nothing would strength-reduce an `imul` for
    /// us; the tree is at `"speed"` now and the mid-end would, so the shift is
    /// free rather than necessary and is kept for being the thing that is
    /// actually meant. `new` asserts the stride is a power of two, so this is
    /// exact rather than approximate.
    #[must_use]
    pub const fn stride_shift(self) -> u8 {
        self.stride_shift
    }
}

/// Everything generated code may bake in to **claim and initialize a block**
/// inline, and nothing else (ADR-119).
///
/// This is the value ADR-113 decision 3 declined to write and left the name
/// `InlineAllocSite` free for. It was declined because P-1a allocated nothing at
/// all, and "four fields nothing reads are four fields that go stale before
/// their first reader arrives". This is that reader.
///
/// # What it makes unrepresentable
///
/// [`InlineInternSite`] confines *which table* the backend may probe. This
/// confines something stronger: **which descriptors have a claim sequence at
/// all.** [`InlineClaimSite::of`] is a `const fn` returning `Option`, and it
/// answers `None` for a descriptor the sequence cannot reproduce the runtime's
/// bookkeeping for —
///
/// - one that carries an [`owned_bytes`](TypeDescriptor::owned_bytes) callback,
///   because `Heap::occupy` charges `stride + owned_bytes_of(payload)` against
///   the pacing counter and the second term is a call the sequence has no way to
///   make. Every scalar descriptor answers `None` to it; every `Text` and `Vec`
///   answers `Some`, and those are exactly the descriptors this refuses;
/// - one whose block [`SizeClass::of`] rejects, because a large page is claimed
///   by a linear scan of `empty_large` keyed on the whole layout, which is not a
///   bitmap claim in any sense.
///
/// Both refusals are `const`, so a `praxis-codegen-cranelift` arm that named a
/// descriptor with an `owned_bytes` charge would fail to build rather than
/// silently under-charge the collector — the failure mode ADR-113's "What was
/// deliberately not done" identified as this path's whole risk. The one place a
/// site is minted is [`crate::scalars`], beside the descriptors it describes.
///
/// # And the half it cannot make unrepresentable
///
/// The same half as [`InlineInternSite`]'s, and one more. It cannot force the
/// backend to emit the pacing compare, to emit the stores in an order, or to
/// emit all of them — those are claims about an instruction stream, and ADR-119
/// decision 4 carries all three with tests that read the emitted Cranelift. What
/// this type does is make the *numbers* one authority's, so that the tests are
/// checking a shape rather than checking arithmetic.
#[derive(Clone, Copy, Debug)]
pub struct InlineClaimSite {
    pacing: PacingOffsets,
    heap_id_offset: usize,
    heap_live_count_offset: usize,
    partial_head_offset: usize,
    page_cursor_offset: usize,
    page_last_word_offset: usize,
    page_allocated_offset: usize,
    page_live_count_offset: usize,
    header_descriptor_offset: usize,
    header_payload_offset_offset: usize,
    header_heap_id_offset: usize,
    first_block: usize,
    stride: usize,
    payload_offset: usize,
}

impl InlineClaimSite {
    /// The claim site for `descriptor`, or `None` if the inline sequence cannot
    /// reproduce what [`Heap::alloc_raw`] would have done for it.
    ///
    /// See the type's doc for the two refusals and why each is total rather than
    /// conservative.
    pub(crate) const fn of(descriptor: &'static TypeDescriptor) -> Option<InlineClaimSite> {
        // The charge `Heap::occupy` makes is `stride + owned_bytes_of(payload)`.
        // The sequence can reproduce the first term (it is `class.block_size()`,
        // a compile-time fact) and not the second (it is an indirect call
        // through the descriptor, on a payload that does not exist yet). A
        // descriptor with the callback therefore has no inline form, and this is
        // the only place that is decided.
        if descriptor.owned_bytes.is_some() {
            return None;
        }
        let (payload_offset, block) = BlockLayout::of(descriptor);
        let Some(class) = SizeClass::of(block) else {
            return None;
        };
        let stride = class.block_size();
        Some(InlineClaimSite {
            // Not a parameter, for `InlineInternSite::new`'s reason: a site that
            // described a block to claim but not the pacing predicate would be
            // permission to skip the predicate, and skipping it is the one thing
            // ADR-040's token exists to make unwritable.
            pacing: PacingOffsets::new(),
            heap_id_offset: core::mem::offset_of!(Heap, id),
            heap_live_count_offset: core::mem::offset_of!(Heap, live_count),
            // The class's availability-list head, folded: `partial` is an array
            // and the index is a compile-time fact, so the backend names one
            // displacement rather than an array base and a scale it could pair
            // with the wrong class.
            partial_head_offset: core::mem::offset_of!(Heap, partial)
                + class.index() * core::mem::size_of::<Cell<*mut PageHeader>>(),
            page_cursor_offset: PageHeader::CURSOR_OFFSET,
            page_last_word_offset: PageHeader::LAST_WORD_OFFSET,
            page_allocated_offset: PageHeader::ALLOCATED_OFFSET,
            page_live_count_offset: PageHeader::LIVE_COUNT_OFFSET,
            header_descriptor_offset: GcHeader::DESCRIPTOR_OFFSET,
            header_payload_offset_offset: GcHeader::PAYLOAD_OFFSET_FIELD_OFFSET,
            header_heap_id_offset: GcHeader::HEAP_ID_OFFSET,
            first_block: PageHeader::first_block_of(stride),
            stride,
            payload_offset,
        })
    }

    /// The pacing predicate's operands — the three displacements the guard in
    /// front of this sequence is emitted from, and the one whose
    /// `bytes_since_collect` the sequence loads a second time and stores back,
    /// once, at the end.
    ///
    /// The *same* value [`InlineInternSite::pacing`] answers: see
    /// [`Heap::collection_is_due`], which is the one statement of the predicate
    /// those two words are the operands of, and [`PacingOffsets`], which is the
    /// one value the backend transcribes it from.
    #[must_use]
    pub const fn pacing(self) -> PacingOffsets {
        self.pacing
    }

    /// Where the owning [`HeapId`] sits within a `Heap` — the `u32` the sequence
    /// copies into every header it writes.
    #[must_use]
    pub const fn heap_id_offset(self) -> usize {
        self.heap_id_offset
    }

    /// Where `Heap::live_count` sits. One of the two counters ADR-119 decision 1
    /// part 3 is about: sweep *decrements* it and never recomputes it, so a
    /// claim that skips this bump underflows it on the first collection.
    #[must_use]
    pub const fn heap_live_count_offset(self) -> usize {
        self.heap_live_count_offset
    }

    /// Where this descriptor's size class's availability-list head sits within a
    /// `Heap`. A null here is the sequence's first bail-out: growing a class is
    /// `Heap::grow_class`, which allocates a page.
    #[must_use]
    pub const fn partial_head_offset(self) -> usize {
        self.partial_head_offset
    }

    /// Where `PageHeader::cursor` sits. The word the scan starts at, and — since
    /// the inline sequence scans exactly one word — the word it claims from.
    #[must_use]
    pub const fn page_cursor_offset(self) -> usize {
        self.page_cursor_offset
    }

    /// Where `PageHeader::last_word` sits. The sequence bails when
    /// `cursor >= last_word`, which is both the "past the end" test
    /// `claim_free_block`'s loop condition performs and the tail-word refusal
    /// ADR-119 decision 3 measures — one compare doing both.
    #[must_use]
    pub const fn page_last_word_offset(self) -> usize {
        self.page_last_word_offset
    }

    /// Where the `allocated` bitmap begins. Indexed by the cursor word, scaled
    /// by eight.
    #[must_use]
    pub const fn page_allocated_offset(self) -> usize {
        self.page_allocated_offset
    }

    /// Where `PageHeader::live_count` sits. The *other* counter of decision 1
    /// part 3: `relink_pages` reads it to decide which availability list a page
    /// joins, so a skipped bump puts a page holding live blocks on the empty
    /// pool, where `reclass` hands its storage to another layout.
    #[must_use]
    pub const fn page_live_count_offset(self) -> usize {
        self.page_live_count_offset
    }

    /// Where a [`GcHeader`]'s descriptor pointer sits. The first store, and the
    /// one whose absence is unrecoverable — see ADR-119 decision 1's severity
    /// ranking.
    #[must_use]
    pub const fn header_descriptor_offset(self) -> usize {
        self.header_descriptor_offset
    }

    /// Where a [`GcHeader`]'s recorded payload displacement sits. A `u16`.
    #[must_use]
    pub const fn header_payload_offset_offset(self) -> usize {
        self.header_payload_offset_offset
    }

    /// Where a [`GcHeader`]'s owning-heap id sits. A `u32`.
    #[must_use]
    pub const fn header_heap_id_offset(self) -> usize {
        self.header_heap_id_offset
    }

    /// Byte offset of block 0 from a page's base, for this descriptor's class.
    ///
    /// Folded rather than loaded from `PageHeader::first_block`, because it is a
    /// function of the stride alone and every page on this class's list has the
    /// same one — [`PageHeader::first_block_of`] is the derivation, stated in
    /// the module that owns the geometry.
    #[must_use]
    pub const fn first_block(self) -> usize {
        self.first_block
    }

    /// The byte stride between blocks of this descriptor's class, which is also
    /// exactly what `Heap::occupy` charges against the pacing counter for one of
    /// them — the `owned_bytes` term being `None` is what
    /// [`InlineClaimSite::of`] refused a descriptor for.
    #[must_use]
    pub const fn stride(self) -> usize {
        self.stride
    }

    /// Where this descriptor's payload begins within its block, which is also
    /// the value the header records. [`GcHeader::payload_offset_for`]'s answer,
    /// carried beside the offset it is stored at so the two cannot be paired
    /// wrongly.
    #[must_use]
    pub const fn payload_offset(self) -> usize {
        self.payload_offset
    }
}

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
    /// Recomputed after each *paced* collection by [`Heap::pacer`]'s
    /// [`Pacer::next_threshold`] from this value and [`Heap::live_bytes`]: the
    /// ratchet doubles it up to a ceiling, and the live set can push it past
    /// that ceiling when a program legitimately holds more (ADR-112).
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
    /// Block bytes the last sweep found still live — the input the pacer's
    /// mandatory term is computed from (ADR-112).
    ///
    /// **Block bytes only.** The `Box<str>` behind a `Text` and the `HashMap`
    /// table behind a `Map` are charged to [`Heap::bytes_since_collect`] at
    /// allocation, but they are not recoverable at sweep without an
    /// `owned_bytes_of` call per *survivor* — which is precisely the O(live)
    /// walk ADR-103 deleted. So this number under-counts, and it under-counts
    /// in the safe direction: a smaller `live` makes the next threshold
    /// smaller, so the collector runs **more** often, never less. It cannot
    /// produce an unbounded heap; it can only cost time, and only on a program
    /// whose live set is mostly owned bytes — where mark cost is O(live
    /// *objects*), which is small by construction for exactly that shape.
    ///
    /// Immortals are excluded, for [`Heap::live_count`]'s reason and RT-04's:
    /// an object no collection can reclaim exerts no pressure, so it must not
    /// buy the program a larger budget either.
    live_bytes: Cell<usize>,
    /// How the next paced threshold is chosen. Not a `Cell` — see [`Pacer`].
    pacer: Pacer,
    /// The mark phase's grey set, kept across collections so the collector does
    /// not allocate a buffer proportional to the live set on every one of them.
    /// See [`Heap::mark`], which is where the reason is written down.
    mark_worklist: RefCell<Vec<GcRef>>,
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
    ///
    /// `const` since ADR-119, for [`SizeClass::of`]'s reason: the inline claim
    /// sequence's stride and payload displacement come off this calculation in a
    /// `const` initializer, so they are this function's answer at build time and
    /// not a second derivation in the backend.
    pub(crate) const fn of(descriptor: &TypeDescriptor) -> (usize, BlockLayout) {
        let payload_align = descriptor.align();
        let payload_offset = GcHeader::payload_offset_for(payload_align);
        let size = match payload_offset.checked_add(descriptor.size()) {
            Some(size) => size,
            None => panic!("allocation size overflow"),
        };
        let header_align = std::mem::align_of::<GcHeader>();
        let align = if payload_align > header_align {
            payload_align
        } else {
            header_align
        };
        (payload_offset, BlockLayout { size, align })
    }
}

/// The initial collection threshold (bytes). Small enough that the first
/// collection runs early in a program's life (catching rooting bugs fast in
/// tests), then grows under whichever [`Pacer`] the heap was built with.
pub const INITIAL_COLLECT_THRESHOLD: usize = 1 << 16; // 64 KiB

/// The ceiling on the *speculative* half of the pacing rule: six doublings of
/// [`INITIAL_COLLECT_THRESHOLD`] and then no more (ADR-112, amended by ADR-129).
///
/// Chosen by measurement, not by derivation — and **re-measured once the cost it
/// prices had changed**. ADR-112 swept 8/16/64/256 MiB against its own build and
/// put the knee at 64 MiB, where 8 MiB cost 4%. The only ceiling-dependent cost
/// is the *per-collection fixed cost* — ADR-112's own prediction, and the reason
/// total sweep work is independent of this constant — and ADR-114 and ADR-128
/// cut precisely that, the first by taking two `malloc`s out of the rooting call
/// and the second by narrowing the frame `push_roots` scans at every collection.
/// So the knee moved: on this tree 8 MiB costs 1.6% and 4 MiB costs 1.9%, and
/// 4 MiB peaks 3.3× lower — the suite goes from 3.6× CPython's resident set to
/// 1.1× (ADR-129's Measurements). A figure derived from physical RAM would be
/// more principled, but this workspace has no platform code for `sysctl
/// hw.memsize` / `sysconf(_SC_PHYS_PAGES)`, and a constant is honest and
/// testable where a derivation would make every Praxis program's schedule depend
/// on the machine that ran it.
pub const MAX_COLLECT_THRESHOLD: usize = INITIAL_COLLECT_THRESHOLD << 6; // 4 MiB

/// How many times the measured live set the threshold must leave room for.
///
/// `k = 2` means a program whose live set is *L* bytes may allocate another *L*
/// bytes before the next collection, so the collector's marginal mark cost is
/// capped at one mark of the live set per equal quantity of fresh allocation —
/// and the resident set is bounded at `(1 + k) × live`, which is the whole
/// deliverable of ADR-112 and is why this is 2 and not 4. Raising it to 4
/// measurably buys back time on a mark-bound program and costs every program
/// in the language a `5 × live` bound instead of a `3 × live` one; ADR-112's
/// Measurements price both.
pub const LIVE_HEADROOM: usize = 2;

/// How [`Heap::collect_inner`] chooses the next paced collection threshold.
///
/// Fixed at construction (there is no `Cell`): a collector that could change
/// its own schedule mid-run would make "when does this program collect" a
/// function of history rather than of the heap it was built with, and every
/// pacing test would become order-dependent.
///
/// Both arms exist in one binary so the A/B behind [`Pacer::from_env`] is a
/// single build rather than two, and the branch is taken once per *collection*
/// — never on the allocation path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pacer {
    /// ADR-011's original heuristic: `max(previous × 2, INITIAL)`, unbounded.
    /// Retained as the measured-against arm; see ADR-112.
    Doubling,
    /// `max(min(previous × 2, ceiling), live × live_factor, INITIAL)`.
    ///
    /// Constructible only through [`Pacer::bounded`], which clamps both fields,
    /// so "a ceiling below the first threshold" and "zero headroom" have no
    /// spelling.
    Bounded {
        /// The largest the *ratchet* term may reach. It does **not** bound the
        /// whole expression; see [`Pacer::next_threshold`].
        ceiling: NonZeroUsize,
        /// The multiple of the measured live set the threshold must leave room
        /// for, whatever the ceiling says.
        live_factor: NonZeroUsize,
    },
}

impl Pacer {
    /// What a [`Heap`] paces with when nothing says otherwise.
    ///
    /// One named constant rather than a literal at the sites that need it, so
    /// "what does this workspace's collector actually do" has exactly one
    /// answer to read.
    pub const DEFAULT: Pacer = Pacer::bounded(MAX_COLLECT_THRESHOLD, LIVE_HEADROOM);

    /// The bounded rule, with both parameters clamped into the range in which
    /// they mean something.
    ///
    /// A ceiling below [`INITIAL_COLLECT_THRESHOLD`] is raised to it: the first
    /// threshold is already `INITIAL`, so a lower ceiling would describe a
    /// heap that had exceeded its own bound before its first allocation. A
    /// `live_factor` of zero is raised to one: it would delete the mandatory
    /// term, which is the whole anti-thrash half of the rule. Neither clamp is
    /// a convenience — they are why the two illegal states have no spelling
    /// (`a_bounded_pacer_cannot_be_built_with_a_ceiling_below_the_first_threshold`).
    pub const fn bounded(ceiling: usize, live_factor: usize) -> Pacer {
        // Written out rather than `usize::max`, which is not a `const fn`. This
        // has to be `const` so `Pacer::DEFAULT` can be one, which is what keeps
        // the shipped ceiling and factor readable as two named constants
        // instead of as whatever `Heap::new` happens to pass.
        let ceiling = if ceiling < INITIAL_COLLECT_THRESHOLD {
            INITIAL_COLLECT_THRESHOLD
        } else {
            ceiling
        };
        let live_factor = if live_factor < 1 { 1 } else { live_factor };
        Pacer::Bounded {
            ceiling: match NonZeroUsize::new(ceiling) {
                Some(ceiling) => ceiling,
                None => panic!("clamped to at least INITIAL_COLLECT_THRESHOLD, which is non-zero"),
            },
            live_factor: match NonZeroUsize::new(live_factor) {
                Some(factor) => factor,
                None => panic!("clamped to at least 1, which is non-zero"),
            },
        }
    }

    /// The threshold the collection that just finished sets for the next one.
    ///
    /// `previous` is the threshold that was in force; `live` is the block bytes
    /// [`Heap::sweep`] just measured.
    ///
    /// **The ceiling clamps the ratchet term only, and never the whole
    /// expression.** `min(ceiling)` applied to the result would make a program
    /// whose live set legitimately exceeds the ceiling collect on essentially
    /// every allocation, which is a thrash bug and not a memory bound. The
    /// ceiling bounds *speculative* growth — the part of the threshold that is
    /// a guess about the future; `live × live_factor` is *mandatory* headroom,
    /// a statement about the present, and it must be allowed to exceed the
    /// ceiling. ADR-112 decision 2 is the argument, and
    /// `a_bounded_pacer_gives_a_large_live_set_its_headroom` is the test that
    /// fails if someone folds the ceiling over the max.
    ///
    /// The rule is monotonically non-decreasing up to the ceiling — once
    /// `previous >= ceiling`, `min(previous × 2, ceiling) == ceiling` — so no
    /// separate growth floor is needed: the ratchet-to-ceiling *is* the floor,
    /// and it is what keeps a shrinking live set from dragging the threshold
    /// back down toward it
    /// (`a_shrinking_live_set_does_not_lower_the_threshold_below_the_ceiling`).
    pub fn next_threshold(self, previous: usize, live: usize) -> usize {
        match self {
            Pacer::Doubling => previous.saturating_mul(2).max(INITIAL_COLLECT_THRESHOLD),
            Pacer::Bounded {
                ceiling,
                live_factor,
            } => previous
                .saturating_mul(2)
                .min(ceiling.get())
                .max(live.saturating_mul(live_factor.get()))
                .max(INITIAL_COLLECT_THRESHOLD),
        }
    }

    /// The pacer every [`Heap::new`] is built with, read once per process from
    /// `PRAXIS_GC_PACER`.
    ///
    /// **This is the only `std::env` read in any `src` file in this
    /// workspace**, and ADR-112 decision 4 is why it earns that. Read through a
    /// [`OnceLock`] so a process that mints several heaps — the debugger mints
    /// a second one (ADR-032) — cannot have two of them disagree about the
    /// collector's schedule, and so repeated `Heap::new` does not re-parse.
    fn from_env() -> Pacer {
        static PACER: OnceLock<Pacer> = OnceLock::new();
        *PACER.get_or_init(|| Pacer::from_spec(std::env::var("PRAXIS_GC_PACER").ok().as_deref()))
    }

    /// [`Pacer::from_env`]'s parse, split out so it is testable without an
    /// ambient environment.
    ///
    /// An unparseable value prints one line to stderr and falls back. A
    /// *silent* fallback would let a typo in one arm of an A/B measure the
    /// wrong build and report the result as if it were the right one, which is
    /// the exact failure mode this knob exists to serve.
    fn from_spec(spec: Option<&str>) -> Pacer {
        let Some(spec) = spec else {
            return Pacer::DEFAULT;
        };
        match Pacer::parse(spec) {
            Ok(pacer) => pacer,
            Err(reason) => {
                eprintln!(
                    "praxis: ignoring PRAXIS_GC_PACER={spec:?} ({reason}); \
                     using the default pacer {:?}",
                    Pacer::DEFAULT
                );
                Pacer::DEFAULT
            }
        }
    }

    /// The grammar: `doubling` | `bounded` | `bounded:<ceiling>` |
    /// `bounded:<ceiling>:<k>`, where `<ceiling>` accepts a `K`/`M`/`G` suffix.
    fn parse(spec: &str) -> Result<Pacer, String> {
        let mut parts = spec.trim().split(':');
        let head = parts.next().unwrap_or_default();
        let pacer = match head {
            "doubling" => Pacer::Doubling,
            "bounded" => {
                let ceiling = match parts.next() {
                    Some(text) => {
                        parse_bytes(text).ok_or_else(|| format!("{text:?} is not a byte count"))?
                    }
                    None => MAX_COLLECT_THRESHOLD,
                };
                let factor = match parts.next() {
                    Some(text) => text
                        .parse::<usize>()
                        .map_err(|_| format!("{text:?} is not a live-set factor"))?,
                    None => LIVE_HEADROOM,
                };
                Pacer::bounded(ceiling, factor)
            }
            other => return Err(format!("{other:?} is not a pacer")),
        };
        match parts.next() {
            Some(extra) => Err(format!("trailing {extra:?}")),
            None => Ok(pacer),
        }
    }
}

/// A byte count with an optional binary suffix: `65536`, `64K`, `8M`, `1G`.
fn parse_bytes(text: &str) -> Option<usize> {
    let (digits, scale) = match text.as_bytes().last()? {
        b'k' | b'K' => (&text[..text.len() - 1], 1_usize << 10),
        b'm' | b'M' => (&text[..text.len() - 1], 1_usize << 20),
        b'g' | b'G' => (&text[..text.len() - 1], 1_usize << 30),
        _ => (text, 1),
    };
    digits.parse::<usize>().ok()?.checked_mul(scale)
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
    /// Block bytes the last sweep found still live. Zero on a heap that has
    /// never collected, and block bytes only — see [`Heap::live_bytes`].
    ///
    /// On [`HeapStats`] rather than behind a `#[cfg(test)]` accessor because
    /// the property it makes checkable — a long-running program's heap stops
    /// growing — is an *end-to-end* property, and the test that says so belongs
    /// where generated code runs (`praxis-codegen-cranelift`'s `jit.rs`), not
    /// in this crate.
    pub live_bytes: usize,
}

impl Heap {
    /// Where [`Heap::bytes_since_collect`] and [`Heap::collect_threshold`] sit
    /// within a `Heap`, for the one caller outside this crate that needs
    /// them: the Cranelift backend, which loads both and compares them inline
    /// (ADR-113).
    ///
    /// Exported from here, with `offset_of!`, for
    /// [`GcHeader::DESCRIPTOR_OFFSET`](crate::GcHeader::DESCRIPTOR_OFFSET)'s
    /// reason — the fields are **private** and this struct is their one layout
    /// authority, so the alternative is a number written out in the backend that
    /// nothing keeps true. `the_pacing_predicate_is_one_unsigned_compare_of_the_two_exported_words`
    /// reads a live `Heap` through exactly these two displacements and asserts
    /// the words it finds are the ones [`Heap::collection_is_due`] compares, so
    /// the offsets and the predicate cannot drift apart.
    ///
    /// **This pair is the whole export surface, and its narrowness is
    /// deliberate.** A pacer whose predicate needed a third term would have
    /// nothing to hand the backend, which is the point at which whoever writes
    /// it has to read [`Heap::collection_is_due`]'s doc.
    pub const BYTES_SINCE_COLLECT_OFFSET: usize = core::mem::offset_of!(Heap, bytes_since_collect);
    /// See [`Heap::BYTES_SINCE_COLLECT_OFFSET`].
    pub const COLLECT_THRESHOLD_OFFSET: usize = core::mem::offset_of!(Heap, collect_threshold);

    /// A fresh, empty heap, paced by [`Pacer::from_env`].
    ///
    /// No page is created here: the first allocation of a class creates that
    /// class's first page. A heap that never allocates costs nothing, which
    /// matters because the debugger mints a second one (ADR-032).
    pub fn new() -> Self {
        Heap::with_pacer(Pacer::from_env())
    }

    /// A fresh, empty heap paced by an explicit [`Pacer`].
    ///
    /// The door every pacing test goes through, so no test's result depends on
    /// the ambient environment — including the debug-profile pass ADR-112's
    /// Consequences requires, which runs the whole suite with
    /// `PRAXIS_GC_PACER` set.
    pub fn with_pacer(pacer: Pacer) -> Self {
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
            live_bytes: Cell::new(0),
            pacer,
            mark_worklist: RefCell::new(Vec::new()),
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

    /// Current allocation count, and what the last sweep measured.
    pub fn stats(&self) -> HeapStats {
        HeapStats {
            live_count: self.live_count.get(),
            live_bytes: self.live_bytes.get(),
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

    /// Charge `bytes` of *owned* growth — a collection's backing buffer
    /// reallocating — against the pacing counter.
    ///
    /// # The gap this closes, and why it only became visible with ADR-121
    ///
    /// [`Heap::alloc_raw`] charges `stride + owned_bytes_of(payload)` once, at
    /// construction. Its comment then explains why growth afterwards was left
    /// uncharged:
    ///
    /// > Growth *after* this point — a `push` that reallocates — is still
    /// > uncharged; **its elements are themselves paced allocations**, so the
    /// > residual under-count is the spine, not the contents.
    ///
    /// That was true and load-bearing, and ADR-121 falsified the premise. When
    /// every scalar the program computed was a heap object, an allocation-light
    /// program did not exist: the arithmetic feeding a `push` paced the
    /// collector even when the `push` itself did not. Promotion deletes exactly
    /// those allocations, so a program whose memory is mostly *buffers* — `bfs`,
    /// whose adjacency lists are a `Vec` of `Vec`s — stopped advancing the
    /// counter at all. Measured: `bfs` went from **41 collections to 6**, and
    /// its peak resident set from 61 MiB to 224, with an identical live set and
    /// a *smaller* GC page heap. The collector was not running because nothing
    /// told it anything had happened.
    ///
    /// So the spine is charged now too, and the pacer's input is the memory the
    /// program actually took rather than the share of it that happened to be
    /// shaped like an object.
    ///
    /// Cheap by construction: callers invoke this only on the reallocation path,
    /// which amortized doubling already makes rare, and it is a load, an add and
    /// a store. It deliberately does **not** collect — the caller decides where
    /// its safepoint is, and every one of them already polls
    /// [`Heap::maybe_collect`] on entry.
    pub fn charge_owned_growth(&self, bytes: usize) {
        self.bytes_since_collect
            .set(self.bytes_since_collect.get().saturating_add(bytes));
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
    /// Reach for [`Heap::alloc_payload`] instead unless `init` genuinely needs
    /// the raw pointer: it derives both numbers *and* the write from the payload
    /// type, leaving nothing for a caller to keep in agreement.
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

    /// [`Heap::alloc_with`] for a payload the caller can hand over **by value**:
    /// the size, the alignment and the write are all derived from `P`.
    ///
    /// This is the shape a non-`Copy` allocation wants. `alloc_with` takes the
    /// layout as two loose numbers and the write as a closure over a `*mut u8`,
    /// so every caller names its payload type three times and nothing but the
    /// assertions in [`Heap::alloc_with_unpaced`] holds the three together. Here
    /// it is named once and the compiler derives the rest — what [`Payload<T>`]
    /// does for the `Copy` path (REP-02), carried as far as a payload that owns
    /// Rust resources can carry it.
    ///
    /// # Safety
    /// `descriptor` must be `P`'s own descriptor. A mismatched *layout* is
    /// caught by [`Heap::alloc_with_unpaced`]'s assertions; a same-layout
    /// mismatch is not, and the descriptor's `drop_value`, `trace` and `format`
    /// callbacks are dispatched against these bytes.
    pub unsafe fn alloc_payload<P>(
        &self,
        safepoint: Safepoint<'_>,
        descriptor: &'static TypeDescriptor,
        payload: P,
    ) -> GcRef {
        // SAFETY: writing an owned `P` initializes the payload completely and
        // cannot panic partway, so `alloc_with`'s contract holds by
        // construction, and the layout passed is `P`'s own.
        unsafe {
            self.alloc_with(
                safepoint,
                descriptor,
                std::mem::size_of::<P>(),
                std::mem::align_of::<P>(),
                |p| (p as *mut P).write(payload),
            )
        }
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

    /// [`Heap::alloc_payload`] **without** pacing the collector. See
    /// [`Heap::alloc_unpaced`] for who may call this and why.
    ///
    /// # Safety
    /// As [`Heap::alloc_payload`].
    pub(crate) unsafe fn alloc_payload_unpaced<P>(
        &self,
        descriptor: &'static TypeDescriptor,
        payload: P,
    ) -> GcRef {
        // SAFETY: as `alloc_payload` — an owned `P` written by value initializes
        // the payload completely, and the layout passed is `P`'s own.
        unsafe {
            self.alloc_with_unpaced(
                descriptor,
                std::mem::size_of::<P>(),
                std::mem::align_of::<P>(),
                |p| (p as *mut P).write(payload),
            )
        }
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
    /// **Generated code reproduces that sequence inline (ADR-119)**, for the two
    /// descriptors [`InlineClaimSite::of`] admits, behind the same pacing branch
    /// [`Heap::collection_is_due`] states. A change to what this function writes
    /// — a third counter, a header field, a different charge — is a change the
    /// Cranelift backend's `emit_inline_claim` owes too, and the test that
    /// notices is `the_inline_claim_writes_every_word_the_wrapper_would`, which
    /// asserts the emitted store list against the displacements the site
    /// carries.
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
                GcHeader::new(descriptor, recorded_offset, self.id),
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
        // The block is only part of what the object costs. A `Text` is 48 bytes
        // of block and a `Box<str>` of whatever length the program read; a
        // freshly built `Vec` is 48 bytes and a buffer of `capacity` refs. The
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
    ///
    /// `roots` is passed twice, as both the strong and the weak set, because a
    /// [`RuntimeRoots`] is both: five arms say what must survive and a sixth
    /// says what must be told when something did not (ADR-106). The sealed set
    /// being the source of both is what keeps them from disagreeing about which
    /// collection they belong to.
    pub fn collect(&self, roots: &RuntimeRoots<'_>) {
        self.collect_inner(roots, roots, Trigger::Explicit);
    }

    /// [`Heap::collect`] against an arbitrary root set.
    ///
    /// Test-only: production collection roots from a
    /// [`RuntimeRoots`](crate::roots::RuntimeRoots), which is constructible
    /// only from a live `RuntimeContext` and is exhaustive over the runtime's
    /// owners. Accepting a `&dyn RootSet` on the production path is what let
    /// the automatic collector root from the shadow chain alone (P0-06).
    ///
    /// The weak set is `()`: a bare `RootScope` has no debug frames behind it,
    /// so there is nothing to clear. [`Heap::collect_with_weak`] is for the
    /// tests that do have one.
    #[cfg(test)]
    pub fn collect_with(&self, roots: &dyn RootSet) {
        self.collect_inner(roots, &(), Trigger::Explicit);
    }

    /// [`Heap::collect_with`] against an explicit weak set as well.
    ///
    /// Test-only. Production collection takes both from one `RuntimeRoots`, so
    /// the two cannot describe different runtimes; this is how an in-crate test
    /// drives the weak path without building a whole context.
    #[cfg(test)]
    pub fn collect_with_weak(&self, roots: &dyn RootSet, weak: &dyn WeakSet) {
        self.collect_inner(roots, weak, Trigger::Explicit);
    }

    /// [`Heap::maybe_collect`] against an arbitrary root set. Test-only, and
    /// the counterpart of [`Heap::collect_with`]: it is the only way an
    /// in-crate test can exercise the *paced* path, which is the one that grows
    /// the threshold.
    #[cfg(test)]
    pub fn maybe_collect_with(&self, roots: &dyn RootSet) -> bool {
        let should = self.collection_is_due();
        if should {
            self.collect_inner(roots, &(), Trigger::Paced);
        }
        should
    }

    fn collect_inner(&self, roots: &dyn RootSet, weak: &dyn WeakSet, trigger: Trigger) {
        self.mark(roots);
        self.sweep();
        // Step 3, and its position is the decision (ADR-106 decision 2). After
        // the sweep, so every block this collection reclaimed is poisoned and
        // therefore recognisable; before anything else can allocate, because the
        // first `claim_free_block` to reissue one of those blocks writes a live
        // header over the poison and the weak set's entry silently becomes a
        // reference to an object of another type. Between those two points is
        // the only place the question "did this die?" has an answer, and this is
        // that place.
        weak.clear_reclaimed();
        self.bytes_since_collect.set(0);
        // Re-pace — but only when *pacing* was what ran this collection.
        //
        // Growing the threshold on an explicit collection too meant a host that
        // collected on a schedule (the debugger between REPL commands, a test
        // between phases) pushed the automatic threshold up without any
        // allocation pressure having caused it, and after a few such calls the
        // program was effectively running without a collector (RT-04).
        //
        // `self.sweep()` ran two lines up, so `live_bytes` is the live set
        // *this* collection measured rather than the previous one's — which is
        // the whole reason the pacer can be a pure function of two numbers
        // (ADR-112 decision 1).
        if trigger == Trigger::Paced {
            self.collect_threshold.set(
                self.pacer
                    .next_threshold(self.collect_threshold.get(), self.live_bytes.get()),
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
        let should = self.collection_is_due();
        if should {
            self.collect_inner(roots, roots, Trigger::Paced);
        }
        should
    }

    /// Has allocation pressure reached the threshold? — **the one statement of
    /// the pacing predicate**, and the second-most-copied line in the runtime
    /// (ADR-113).
    ///
    /// [`Heap::maybe_collect`] and [`Heap::maybe_collect_with`] are its two
    /// callers here. Its third reader is not in this crate and cannot call it:
    /// since ADR-113 the Cranelift backend **reproduces this expression inline**
    /// in every `Inst::Materialize { Int }` it emits — two loads at
    /// [`Heap::BYTES_SINCE_COLLECT_OFFSET`] and
    /// [`Heap::COLLECT_THRESHOLD_OFFSET`], an unsigned compare, and a branch to
    /// a cold block that calls `praxis_alloc_int` — so that the overwhelmingly
    /// common case (a loop counter inside [`crate::small_int`]'s range, on a
    /// heap that is nowhere near its threshold) is a table read rather than a
    /// guarded call into this module.
    ///
    /// # The obligation, and what a third term would cost
    ///
    /// ADR-040's [`Safepoint`] exists so that "allocate on the paced path
    /// without pacing" has no spelling. Generated code does not breach it,
    /// because it takes that path **only** where this function answers `false`,
    /// which is exactly the branch on which `maybe_collect` returns without
    /// doing anything. That is the entire argument, and it holds only while this
    /// expression is what the backend emits.
    ///
    /// **Since ADR-119 the branch this guards does allocate**, so the argument
    /// carries more weight than it did: generated code claims a block and writes
    /// its header on the far side of it. What makes that sound is that between
    /// this branch and the last store there is no call, so *not due here* is
    /// *not due throughout*. See [`Safepoint`], which states all three parts.
    ///
    /// So: **a term added here must be added to `emit_pacing_test` in
    /// `crates/praxis-codegen-cranelift/src/lower.rs`, or generated code
    /// allocates on a branch where the collector was due.** That is the one
    /// place the backend transcribes this expression — `emit_inline_intern` and
    /// `emit_inline_claim_box` both call it — and [`PacingOffsets`] is the one
    /// value it reads the displacements off. The failure mode is not a wrong
    /// answer — it is a collection that silently does not happen, which looks
    /// like a memory leak in a program the reader will not connect to a pacer
    /// change. Two things fire when it is forgotten:
    /// `the_pacing_predicate_is_one_unsigned_compare_of_the_two_exported_words`
    /// below, which compares this function's answer against the two words the
    /// backend loads, and the deliberately narrow export surface — a third term
    /// has no offset constant to be baked from, so writing one is a decision
    /// rather than an oversight.
    ///
    /// It is `pub` because it is part of that contract, not an implementation
    /// detail: what the backend inlines should be nameable, and a host that
    /// wants to know whether the next allocation will collect should ask this
    /// rather than reconstruct it.
    #[inline]
    #[must_use]
    pub fn collection_is_due(&self) -> bool {
        self.bytes_since_collect.get() >= self.collect_threshold.get()
    }

    /// Mark phase: set the page bit of every reachable object.
    fn mark(&self, roots: &dyn RootSet) {
        // **The grey set is reused across collections, and that is a memory
        // decision rather than a speed one.**
        //
        // This used to be a fresh `Vec::new()` per collection, grown by doubling
        // to the size of the transitive closure and dropped at the end of the
        // phase. On `pipeline`'s 1M-element working set that is an 8 MiB buffer
        // reached through the whole doubling ladder — 1, 2, 4, 8 — and freed
        // again, sixty-odd times in one run. macOS's allocator does not return
        // large freed regions to the OS promptly; it caches them, and `vmmap`
        // showed **64 cached `MALLOC_LARGE (empty)` regions holding 489 MiB**
        // against 4 regions and 16 MiB before ADR-121. Live malloc bytes were
        // ~84 MiB in both, so none of that half-gigabyte was in use — it was
        // resident, and `peak_rss` counts resident.
        //
        // ADR-121 did not create the churn; it made the same churn happen in
        // less wall-clock time, which is what pushed the cache from 4 regions to
        // 64. That distinction matters for who owns the fix: the collector
        // should not allocate a buffer proportional to the live set on every
        // collection, whatever the compiler above it is doing.
        //
        // `clear()` keeps the capacity, so after the first collection the mark
        // phase allocates nothing at all. The retained buffer is bounded by the
        // largest transitive closure the program has ever had — never more than
        // the live set, which the heap is already holding.
        //
        // A `RefCell` rather than the `Cell` the rest of this struct uses,
        // because a `Vec` is not `Copy`. Collection is not re-entrant, so the
        // borrow cannot overlap; if some future tracer callback made it
        // re-entrant, `borrow_mut` panics loudly rather than corrupting the grey
        // set, which is the right failure for a collector invariant.
        let mut worklist = self.mark_worklist.borrow_mut();
        worklist.clear();
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
    ///
    /// It also measures the live set in bytes, for the pacer (ADR-112). That
    /// costs **one multiply per page** — `live_count × block_size`, both of
    /// which the page already knows and neither of which is on a survivor — so
    /// ADR-103's "sweep does not touch survivors" property is preserved
    /// exactly. Reconstructing the same number from the objects would be the
    /// O(live) walk this design exists to have deleted.
    fn sweep(&self) {
        let mut reclaimed = 0usize;
        let mut live_bytes = 0usize;
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
                        let index = page.block_index_in(word, dead.trailing_zeros());
                        dead &= dead - 1;
                        // SAFETY: the block's `allocated` bit is set and nothing
                        // has finalized it since it was, and the
                        // `set_allocated_word` below is what clears it.
                        unsafe { Self::finalize_block(page, index) };
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
            // The page's liveness is final here, so this is the one point in the
            // cycle at which the live set is knowable without touching an
            // object. A large page reports its one block's stride rather than
            // its padded `page_bytes`, which under-counts by the padding — safe
            // in the direction `Heap::live_bytes` documents, and no production
            // descriptor takes the large path at all. Immortal pages `continue`
            // above and are excluded on purpose (RT-04).
            live_bytes += page.live_count() as usize * page.block_size();
        }
        // Accumulated into a local and stored once, so this line stays beside
        // an unchecked subtraction without making that subtraction's ordering
        // subtle.
        self.live_count.set(self.live_count.get() - reclaimed);
        self.live_bytes.set(live_bytes);
        self.relink_pages();
    }

    /// Finalize the object in block `index` of `page`, then poison its header.
    ///
    /// The whole of what happens to a dead block, and the two callers differ
    /// only in which bits they walk — [`Heap::sweep`] takes what it proved
    /// unreachable, [`Heap::finalize_all`] takes everything still allocated.
    ///
    /// # Safety
    /// The block's `allocated` bit must be set, so that `alloc_raw` initialized
    /// a header and a payload of its descriptor there, and nothing may have
    /// finalized it since. The caller must clear that bit before anything can
    /// reach the block again — the poison below is only half of that protocol.
    unsafe fn finalize_block(page: &PageHeader, index: usize) {
        // SAFETY: the caller's contract is that this block holds an initialized
        // `[GcHeader | payload]`.
        let header = unsafe { &*(page.block_ptr(index) as *const GcHeader) };
        let desc = header.descriptor();
        // SAFETY: the payload matches `desc` and is about to become invalid.
        unsafe { (desc.drop_value)(header.payload::<u8>()) };
        // Poison before the caller clears the `allocated` bit, so a stale
        // `GcRef` that still names this storage is rejected by the mark phase's
        // provenance check instead of being traced through a finalized payload.
        // This is also RT-01's precondition: between releasing the block and
        // handing it out again, it must not claim to be a typed object, or a
        // stale reference would be traced through whatever the allocator put
        // there next (hazard H7).
        header.poison();
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
                    let index = page.block_index_in(word, alive.trailing_zeros());
                    alive &= alive - 1;
                    // SAFETY: as in `sweep` — an allocated bit means an
                    // initialized header and payload, and the `clear_bitmaps`
                    // below is what clears it.
                    unsafe { Self::finalize_block(page, index) };
                }
            }
            page.clear_bitmaps();
        }
        self.live_count.set(0);
        // The other place liveness is repudiated wholesale, and the one both
        // `Heap::reset` and `Drop` go through. A stale `live_bytes` here would
        // let a reset heap's first paced collection inherit the *previous*
        // program's live set as headroom.
        self.live_bytes.set(0);
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
    unsafe fn probe_format(_: *const u8, _: &mut crate::FormatSink<'_>) {}

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
        // SAFETY: VecPayload is VEC's payload type.
        let vec_ref = unsafe {
            heap.alloc_payload_unpaced(
                &VEC,
                VecPayload {
                    element_descriptor: &INT,
                    items: elems.into(),
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
        unsafe {
            (desc.format)(
                vec_ref.payload::<u8>() as *const u8,
                &mut crate::FormatSink::display(&mut out),
            )
        };
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
            // SAFETY: VecPayload is VEC's payload type.
            unsafe {
                heap.alloc_payload_unpaced(
                    &VEC,
                    VecPayload {
                        element_descriptor: &INT,
                        items: elems.into(),
                    },
                )
            }
        };

        let inner0 = inner_alloc(&[1, 2]);
        let inner1 = inner_alloc(&[3]);
        // SAFETY: VecPayload is VEC's payload type.
        let outer = unsafe {
            heap.alloc_payload_unpaced(
                &VEC,
                VecPayload {
                    // The element descriptor of a Vec-of-X is VEC itself.
                    element_descriptor: &VEC,
                    items: vec![inner0, inner1].into(),
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
        unsafe {
            (outer.descriptor().format)(
                outer.payload::<u8>() as *const u8,
                &mut crate::FormatSink::display(&mut out),
            )
        };
        assert_eq!(out, "[[1, 2], [3]]");
    }

    #[test]
    fn collect_finalizes_unreachable_owned_payload_exactly_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let heap = Heap::new();
        // SAFETY: DropProbe is DROP_PROBE's payload type.
        unsafe {
            heap.alloc_payload_unpaced(&DROP_PROBE, DropProbe(Arc::clone(&drops)));
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
            // SAFETY: DropProbe is DROP_PROBE's payload type.
            unsafe {
                heap.alloc_payload_unpaced(&DROP_PROBE, DropProbe(Arc::clone(&drops)));
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
            // SAFETY: DropProbe is DROP_PROBE's payload type.
            let probe =
                unsafe { heap.alloc_payload_unpaced(&DROP_PROBE, DropProbe(Arc::clone(&drops))) };
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
            // SAFETY: DropProbe is DROP_PROBE's payload type.
            unsafe {
                heap.alloc_payload_unpaced(&DROP_PROBE, DropProbe(Arc::clone(&drops)));
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
        // The one allocation here that cannot go through `alloc_payload_unpaced`:
        // the property under test *is* the address `init` was handed, so this
        // needs the raw-pointer closure rather than a payload by value.
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

    /// **The ADR-113 obligation, as an assertion rather than a comment.**
    ///
    /// Generated code does not call [`Heap::collection_is_due`]; it loads two
    /// words at [`Heap::BYTES_SINCE_COLLECT_OFFSET`] and
    /// [`Heap::COLLECT_THRESHOLD_OFFSET`] and compares them. This test *is* that
    /// sequence, in Rust, run against a live `Heap` whose two fields are driven
    /// across the boundary — so it fails in three separate ways, each of which
    /// is a real defect:
    ///
    /// 1. an offset that names the wrong field (the reads disagree with the
    ///    fields);
    /// 2. a `Cell` that stops being `#[repr(transparent)]` over its contents, or
    ///    a `Heap` that stops being `#[repr(C)]` (same symptom);
    /// 3. **a third term in the predicate** — the one this is really for. The
    ///    moment `collection_is_due` is anything other than these two words
    ///    compared, some state below makes the two answers differ, and whoever
    ///    added the term arrives at that function's doc, which tells them the
    ///    backend has to change too.
    ///
    /// It deliberately does not go through `maybe_collect`: that would collect,
    /// which resets the counter, and the states worth checking are the ones on
    /// either side of the boundary.
    #[test]
    fn the_pacing_predicate_is_one_unsigned_compare_of_the_two_exported_words() {
        let heap = Heap::new();
        // Straddle the boundary in both directions, and include the degenerate
        // pair (0, 0) — a zero threshold means *always* due, and `>=` is what
        // makes that true where `>` would not.
        for (since, threshold) in [
            (0_usize, 0_usize),
            (0, 1),
            (0, INITIAL_COLLECT_THRESHOLD),
            (INITIAL_COLLECT_THRESHOLD - 1, INITIAL_COLLECT_THRESHOLD),
            (INITIAL_COLLECT_THRESHOLD, INITIAL_COLLECT_THRESHOLD),
            (INITIAL_COLLECT_THRESHOLD + 1, INITIAL_COLLECT_THRESHOLD),
            (usize::MAX, INITIAL_COLLECT_THRESHOLD),
            (INITIAL_COLLECT_THRESHOLD, usize::MAX),
        ] {
            heap.bytes_since_collect.set(since);
            heap.collect_threshold.set(threshold);

            let base = std::ptr::from_ref(&heap).cast::<u8>();
            // SAFETY: `Heap` is `#[repr(C)]` and both constants are
            // `offset_of!` of a `Cell<usize>` field of it, and `Cell<T>` is
            // `#[repr(transparent)]` over `T`. This is byte-for-byte the load
            // `emit_inline_intern` emits.
            let (read_since, read_threshold) = unsafe {
                (
                    *base.add(Heap::BYTES_SINCE_COLLECT_OFFSET).cast::<usize>(),
                    *base.add(Heap::COLLECT_THRESHOLD_OFFSET).cast::<usize>(),
                )
            };
            assert_eq!(read_since, since, "BYTES_SINCE_COLLECT_OFFSET names it");
            assert_eq!(read_threshold, threshold, "COLLECT_THRESHOLD_OFFSET does");
            assert_eq!(
                read_since >= read_threshold,
                heap.collection_is_due(),
                "the predicate generated code emits (since={since}, \
                 threshold={threshold}) is no longer the predicate \
                 `collection_is_due` applies — see its doc: the backend's \
                 `emit_inline_intern` owes the same change, or generated code \
                 answers from the intern table on a branch where the collector \
                 was due"
            );
        }
    }

    /// The site the backend is handed carries the *same* two offsets, so
    /// permission to probe the table and the obligation to pace cannot come
    /// apart. `InlineInternSite::new` fills them itself rather than taking them,
    /// and this is the assertion that it filled them from here.
    ///
    /// And that the claim site's are not a second set: both carry one
    /// [`PacingOffsets`], which is what lets the backend emit the compare in one
    /// place rather than transcribing `collection_is_due` twice.
    #[test]
    fn an_inline_intern_site_carries_the_heaps_own_pacing_offsets() {
        let pacing = crate::small_int::INLINE_INTERN_SITE.pacing();
        assert_eq!(
            pacing.bytes_since_collect_offset(),
            Heap::BYTES_SINCE_COLLECT_OFFSET
        );
        assert_eq!(
            pacing.collect_threshold_offset(),
            Heap::COLLECT_THRESHOLD_OFFSET
        );
        assert_ne!(
            pacing.bytes_since_collect_offset(),
            pacing.collect_threshold_offset(),
            "two distinct fields, or the compare is `x >= x`"
        );
        assert_eq!(
            pacing.heap_offset(),
            core::mem::offset_of!(crate::RuntimeContext, heap),
            "and the base those two are relative to is the context's `heap`"
        );
        assert_eq!(
            crate::scalars::INT_CLAIM_SITE.pacing(),
            pacing,
            "and the claim site's are the same three, because they are the same \
             value — one predicate, one authority"
        );
    }

    /// **The other half of ADR-119 decision 1 part 3, as an assertion.**
    ///
    /// The IR test in the backend says *which displacements* the claim sequence
    /// stores to and in what order. Nothing there can say those displacements
    /// name the fields they are supposed to — that is this test, and it is the
    /// `the_pacing_predicate_is_one_unsigned_compare_of_the_two_exported_words`
    /// shape widened from two words to a heap, a page and a header.
    ///
    /// Every read below is byte-for-byte a load the emitted sequence performs,
    /// against a live heap that has just allocated one `Int` through the
    /// wrapper — so a `#[repr(C)]` that stopped being one, a reordered
    /// `PageHeader`, or an `offset_of!` naming a neighbouring field fails here
    /// rather than in a program that silently writes a `heap_id` over a
    /// `payload_offset`.
    #[test]
    fn the_claim_site_displacements_name_the_fields_they_claim_to() {
        let site = crate::scalars::INT_CLAIM_SITE;
        let heap = Heap::new();
        let value = heap.alloc_unpaced(INT_PAYLOAD, 7_i64);

        let heap_base = std::ptr::from_ref(&heap).cast::<u8>();
        // SAFETY: `Heap` is `#[repr(C)]`, every constant below is an
        // `offset_of!` of one of its fields, and `Cell<T>` is
        // `#[repr(transparent)]` over `T`.
        let (read_id, read_live, read_head) = unsafe {
            (
                *heap_base.add(site.heap_id_offset()).cast::<u32>(),
                *heap_base.add(site.heap_live_count_offset()).cast::<usize>(),
                *heap_base
                    .add(site.partial_head_offset())
                    .cast::<*mut PageHeader>(),
            )
        };
        assert_eq!(read_id, heap.id.get(), "heap_id_offset names `Heap::id`");
        assert_eq!(
            read_live,
            heap.live_count.get(),
            "heap_live_count_offset names `Heap::live_count`"
        );
        assert!(
            !read_head.is_null(),
            "partial_head_offset names the `Int` class's list head, and the \
             allocation above put a page on it"
        );

        // SAFETY: the head of an availability list is one of this heap's pages.
        let page = unsafe { &*read_head };
        let page_base = std::ptr::from_ref(page).cast::<u8>();
        // SAFETY: `PageHeader` is `#[repr(C)]` and each constant is an
        // `offset_of!` of one of its `Cell` fields.
        let (read_cursor, read_last, read_page_live, read_word) = unsafe {
            (
                *page_base.add(site.page_cursor_offset()).cast::<u32>(),
                *page_base.add(site.page_last_word_offset()).cast::<u32>(),
                *page_base.add(site.page_live_count_offset()).cast::<u32>(),
                *page_base.add(site.page_allocated_offset()).cast::<u64>(),
            )
        };
        assert_eq!(
            read_cursor, 0,
            "the first claim leaves the cursor at word 0"
        );
        assert_eq!(
            read_last,
            page.words() as u32 - 1,
            "page_last_word_offset names `PageHeader::last_word`"
        );
        // The inline sequence bails at `cursor >= last_word`, ceding the tail
        // word to the wrapper (ADR-119 decision 3). On a page with **one**
        // bitmap word that bail would fire on every claim and the inline arm
        // would be dead code — a claim about reach rather than correctness,
        // asserted here because nothing else would notice it going false.
        assert!(
            read_last >= 1,
            "a claimable class must have more than one bitmap word, or the \
             tail-word bail-out cedes the whole page to the wrapper"
        );
        assert_eq!(
            read_page_live,
            page.live_count(),
            "page_live_count_offset names `PageHeader::live_count`"
        );
        assert_eq!(
            read_word,
            page.allocated_word(0),
            "page_allocated_offset names the base of the `allocated` bitmap"
        );

        // The geometry the sequence folds rather than loads.
        assert_eq!(
            site.stride(),
            page.block_size(),
            "the stride the pacer is charged is the page's own"
        );
        assert_eq!(
            site.first_block(),
            page.first_block(),
            "the folded `first_block` is the page's own — see \
             `PageHeader::first_block_of`"
        );
        assert_eq!(
            site.payload_offset(),
            page.payload_offset(),
            "and the payload displacement the header will record is the one \
             the page was laid out with (ADR-039 decision 1)"
        );

        // And the header the sequence writes, read back through *its* three
        // displacements against the one the wrapper just wrote.
        let header_base = value.as_ptr().cast::<u8>();
        // SAFETY: `value` is a live object this heap allocated, and `GcHeader`
        // is `#[repr(C)]` with these three fields.
        let (read_desc, read_payload_offset, read_header_id) = unsafe {
            (
                *header_base
                    .add(site.header_descriptor_offset())
                    .cast::<*const TypeDescriptor>(),
                *header_base
                    .add(site.header_payload_offset_offset())
                    .cast::<u16>(),
                *header_base.add(site.header_heap_id_offset()).cast::<u32>(),
            )
        };
        assert!(
            std::ptr::eq(read_desc, &crate::scalars::INT),
            "header_descriptor_offset names the descriptor pointer"
        );
        assert_eq!(
            read_payload_offset as usize,
            site.payload_offset(),
            "header_payload_offset_offset names the recorded displacement, and \
             it is the one the site carries"
        );
        assert_eq!(
            read_header_id,
            heap.id.get(),
            "header_heap_id_offset names the provenance word"
        );
    }

    /// A descriptor whose payload owns bytes outside its block has **no** claim
    /// site, and that refusal is the whole of why the inline sequence may charge
    /// `stride` and nothing else.
    ///
    /// `Heap::occupy` charges `stride + owned_bytes_of(payload)`. Generated code
    /// can reproduce the first term and cannot make the indirect call the second
    /// needs, so a `Text` or a `Vec` claimed inline would under-charge the pacer
    /// by its entire buffer — RT-04, re-introduced. This walks every built-in
    /// descriptor and asserts the refusal is exactly the `owned_bytes` set,
    /// rather than a list someone kept in step by hand.
    #[test]
    fn only_a_descriptor_with_no_owned_bytes_charge_has_a_claim_site() {
        for descriptor in crate::descriptor::BUILTINS {
            let claimable = InlineClaimSite::of(descriptor).is_some();
            let charges_outside = descriptor.owned_bytes.is_some();
            let on_the_ladder = SizeClass::of(BlockLayout::of(descriptor).1).is_some();
            assert_eq!(
                claimable,
                !charges_outside && on_the_ladder,
                "{}: a claim site exists exactly when the pacing charge is the \
                 stride alone and the block is on the ladder",
                descriptor.name
            );
        }
        assert!(
            InlineClaimSite::of(&crate::scalars::INT).is_some(),
            "and `Int` is on the claimable side, which is the whole package"
        );
        assert!(
            InlineClaimSite::of(&crate::text::TEXT).is_none(),
            "…and `Text` is not: its `owned_bytes` is the `Box<str>` the \
             sequence has no way to measure"
        );
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

    /// The RT-04 property above is the pacer's, not the doubling rule's, and a
    /// bounded pacer must not smuggle threshold growth in through an explicit
    /// collection either. Written as a twin rather than by widening the test
    /// above, so that one keeps pinning the *exact* doubling constant.
    #[test]
    fn an_explicit_collection_does_not_grow_a_bounded_pacers_threshold() {
        let heap = Heap::with_pacer(Pacer::bounded(1 << 20, LIVE_HEADROOM));
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
            "with nothing rooted the ratchet term is what grows it, exactly as before"
        );
    }

    /// The stride an `Int`'s block is taken at — derived through the same two
    /// calls the allocator makes rather than written as a literal, so a change
    /// to the ladder moves the expectation with it.
    fn int_stride() -> usize {
        let (_, block) = BlockLayout::of(&INT);
        SizeClass::of(block)
            .expect("an Int is on the ladder")
            .block_size()
    }

    /// The input the bounded pacer's mandatory term is computed from. It is
    /// measured from the pages' own counts in the walk sweep already performs,
    /// so this is also the assertion that the multiply is reading the right two
    /// numbers.
    #[test]
    fn sweep_measures_the_live_set_in_bytes() {
        const ROOTED: usize = 100;
        let heap = Heap::with_pacer(Pacer::Doubling);
        let mut roots = RootScope::new();
        for i in 0..1_000_i64 {
            let r = heap.alloc_unpaced(INT_PAYLOAD, i);
            if (i as usize) < ROOTED {
                roots.root(r);
            }
        }

        heap.collect_with(&roots);

        assert_eq!(heap.stats().live_count, ROOTED);
        assert_eq!(
            heap.stats().live_bytes,
            ROOTED * int_stride(),
            "the live set is the survivors' blocks and nothing else"
        );
    }

    /// The RT-04 twin of `an_immortal_is_invisible_to_sweep_and_to_finalize_all`
    /// and of `minting_the_immortals_costs_the_collector_nothing`: an object no
    /// collection can ever reclaim exerts no pressure, so it must not buy the
    /// program a larger budget between collections either. Without this, every
    /// program in the language would start with the ~40 KiB interned small-`Int`
    /// table (ADR-100) counted as live and the first bounded threshold moved by
    /// it.
    #[test]
    fn an_immortal_is_not_counted_in_the_live_set() {
        let heap = Heap::with_pacer(Pacer::Doubling);
        let immortals = crate::immortal::Immortals::new(&heap);
        assert!(immortals.small_int(7).is_some(), "7 is interned");
        let mut roots = RootScope::new();
        roots.root(heap.alloc_unpaced(INT_PAYLOAD, 1_i64));

        heap.collect_with(&roots);

        assert_eq!(
            heap.stats().live_bytes,
            int_stride(),
            "the immortal tables are on pages sweep never walks, so they are not live bytes"
        );
    }

    /// The property this item exists for: the threshold's speculative half
    /// ratchets to a bound and stops, where the doubling rule's grows without
    /// limit for as long as the program runs.
    #[test]
    fn a_bounded_pacer_stops_doubling_at_the_ceiling() {
        const CEILING: usize = 1 << 18; // 256 KiB — two rungs above INITIAL.
        let heap = Heap::with_pacer(Pacer::bounded(CEILING, LIVE_HEADROOM));

        for round in 0..40 {
            assert!(
                allocate_until_paced(&heap, 100_000),
                "round {round} did not reach the threshold"
            );
            assert!(
                heap.collect_threshold.get() <= CEILING,
                "round {round} left the threshold at {} above the {CEILING}-byte ceiling",
                heap.collect_threshold.get()
            );
        }
        assert_eq!(
            heap.collect_threshold.get(),
            CEILING,
            "and it ratchets all the way up to it rather than oscillating below"
        );
    }

    /// The ceiling clamps the ratchet term and **not** the whole expression.
    /// Folding `min(ceiling)` over the max is a one-character edit that turns a
    /// memory bound into a thrash bug: a program whose live set exceeds the
    /// ceiling would collect on essentially every allocation, having proved on
    /// each one that it cannot reclaim anything. This test is what fails.
    #[test]
    fn a_bounded_pacer_gives_a_large_live_set_its_headroom() {
        const CEILING: usize = 1 << 20; // 1 MiB
        let heap = Heap::with_pacer(Pacer::bounded(CEILING, LIVE_HEADROOM));
        let mut roots = RootScope::new();
        // Two ceilings' worth of live blocks, held across the collection.
        for i in 0..(2 * CEILING / int_stride()) as i64 {
            roots.root(heap.alloc_unpaced(INT_PAYLOAD, i));
        }

        assert!(
            drive_one_paced_collection(&heap, &roots, 200_000),
            "the rooted fixture is already past the threshold"
        );

        let live = heap.stats().live_bytes;
        assert!(
            live > CEILING,
            "the fixture must hold more than the ceiling, and holds {live}"
        );
        assert_eq!(
            heap.collect_threshold.get(),
            live * LIVE_HEADROOM,
            "the mandatory term must be allowed to exceed the ceiling"
        );
    }

    /// The exact difference from the naive `live × k` rule the pre-perf-fixes
    /// experiment measured, and the reason no separate growth floor is needed:
    /// the ratchet-to-ceiling *is* the floor. A program that briefly holds a
    /// large live set and then drops it does not go back to collecting every
    /// 64 KiB.
    #[test]
    fn a_shrinking_live_set_does_not_lower_the_threshold_below_the_ceiling() {
        const CEILING: usize = 1 << 20; // 1 MiB
        let heap = Heap::with_pacer(Pacer::bounded(CEILING, LIVE_HEADROOM));
        {
            let mut roots = RootScope::new();
            for i in 0..(2 * CEILING / int_stride()) as i64 {
                roots.root(heap.alloc_unpaced(INT_PAYLOAD, i));
            }
            assert!(drive_one_paced_collection(&heap, &roots, 200_000));
            assert!(heap.collect_threshold.get() > CEILING);
        }

        // The roots are gone; the next collection finds nothing live.
        assert!(drive_one_paced_collection(
            &heap,
            &RootScope::new(),
            1_000_000
        ));

        assert_eq!(heap.stats().live_bytes, 0);
        assert_eq!(
            heap.collect_threshold.get(),
            CEILING,
            "an empty live set must leave the threshold at the ceiling, not at INITIAL"
        );
    }

    /// RT-01 restated for the pacer, and the property the whole item is for: a
    /// program that holds a bounded working set has a bounded heap, whatever
    /// its total allocation. The doubling arm below is not decoration — it is
    /// the measurement of what the bound is worth, and it fails the same
    /// assertion by construction.
    #[test]
    fn a_bounded_heap_stops_growing() {
        const CEILING: usize = 1 << 18; // 256 KiB
        const RETAINED: usize = 1_024;
        const CHURN: i64 = 512 * 1_024;

        fn churn(pacer: Pacer) -> (usize, usize) {
            let heap = Heap::with_pacer(pacer);
            let mut roots = RootScope::new();
            for i in 0..RETAINED as i64 {
                roots.root(heap.alloc_unpaced(INT_PAYLOAD, i));
            }
            for i in 0..CHURN {
                let _ = heap.alloc_unpaced(INT_PAYLOAD, i);
                heap.maybe_collect_with(&roots);
            }
            (heap.committed_bytes(), heap.stats().live_bytes)
        }

        // One page of slack per rung is the worst case ADR-103 names for a
        // heap that has touched every class; this fixture touches one.
        let slack = 14 * page::PAGE_SIZE;
        let (bounded_bytes, live) = churn(Pacer::bounded(CEILING, LIVE_HEADROOM));
        assert_eq!(live, RETAINED * int_stride());
        assert!(
            bounded_bytes <= live + CEILING + slack,
            "a bounded pacer left {bounded_bytes} bytes committed against a {live}-byte \
             live set and a {CEILING}-byte ceiling"
        );

        let (doubling_bytes, _) = churn(Pacer::Doubling);
        assert!(
            doubling_bytes > live + CEILING + slack,
            "the doubling rule is supposed to fail this bound, and committed only \
             {doubling_bytes} bytes — the fixture is no longer measuring anything"
        );
    }

    /// Allocate against `roots` until one paced collection runs, and report
    /// whether it did within `limit` allocations. The rooted counterpart of
    /// `allocate_until_paced`, for the tests whose whole subject is what a
    /// *non-empty* live set does to the next threshold.
    fn drive_one_paced_collection(heap: &Heap, roots: &dyn RootSet, limit: usize) -> bool {
        for i in 0..limit {
            let _ = heap.alloc_unpaced(INT_PAYLOAD, i as i64);
            if heap.maybe_collect_with(roots) {
                return true;
            }
        }
        false
    }

    /// `Heap::reset` re-seeds the pacer's *previous*, and `finalize_all` —
    /// which reset goes through — repudiates its *live*. Both halves matter: a
    /// reset heap that inherited the previous program's live set as headroom
    /// would run for megabytes before its first collection (RT-04), which is
    /// the same failure `reset_restores_collection_pacing` pins for the
    /// threshold.
    #[test]
    fn reset_repudiates_the_measured_live_set() {
        let mut heap = Heap::with_pacer(Pacer::bounded(1 << 20, LIVE_HEADROOM));
        {
            let mut roots = RootScope::new();
            for i in 0..8_192_i64 {
                roots.root(heap.alloc_unpaced(INT_PAYLOAD, i));
            }
            assert!(drive_one_paced_collection(&heap, &roots, 200_000));
            assert_ne!(heap.stats().live_bytes, 0);
        }

        heap.reset();

        assert_eq!(heap.stats().live_bytes, 0);
        assert_eq!(heap.collect_threshold.get(), INITIAL_COLLECT_THRESHOLD);
    }

    /// The flip ADR-112's Measurements bought, at the ceiling ADR-129 re-measured
    /// it to. The exact ceiling and factor are pinned here rather than left
    /// implicit, so a future re-tuning is a visible edit to a test that names the
    /// numbers, not a silent change to every Praxis program's memory profile.
    ///
    /// It has already earned that once: ADR-129 lowered the ceiling 64 MiB → 4
    /// MiB, and this assertion is what made the change announce itself. The
    /// factor is unchanged and ADR-129 prices the direction ADR-112 did not.
    #[test]
    fn the_default_pacer_is_bounded_at_the_measured_ceiling() {
        assert_eq!(Pacer::from_spec(None), Pacer::DEFAULT);
        assert_eq!(Pacer::DEFAULT, Pacer::bounded(4 << 20, 2));
        assert_eq!(
            Pacer::DEFAULT.next_threshold(1 << 30, 0),
            MAX_COLLECT_THRESHOLD
        );
    }

    /// The "make illegal states unrepresentable" gate on the constructor. A
    /// ceiling below the first threshold would describe a heap that had
    /// exceeded its own bound before its first allocation; a zero factor would
    /// delete the mandatory term and with it the anti-thrash half of the rule.
    #[test]
    fn a_bounded_pacer_cannot_be_built_with_a_ceiling_below_the_first_threshold() {
        assert_eq!(
            Pacer::bounded(0, 0),
            Pacer::bounded(INITIAL_COLLECT_THRESHOLD, 1)
        );
        assert_eq!(
            Pacer::bounded(1, 0).next_threshold(INITIAL_COLLECT_THRESHOLD, 1_000_000),
            1_000_000,
            "a clamped factor of one still gives the live set its own bytes"
        );
    }

    /// The knob's grammar, and — the part that matters for an A/B — that a
    /// value it cannot read is a loud fallback rather than a silent one. A
    /// silent fallback lets a typo in one arm measure the other build and
    /// report the result as if it were the right one.
    #[test]
    fn a_pacer_spec_parses_its_grammar_and_rejects_everything_else() {
        assert_eq!(Pacer::parse("doubling"), Ok(Pacer::Doubling));
        assert_eq!(
            Pacer::parse("bounded"),
            Ok(Pacer::bounded(MAX_COLLECT_THRESHOLD, LIVE_HEADROOM))
        );
        assert_eq!(
            Pacer::parse("bounded:8M"),
            Ok(Pacer::bounded(8 << 20, LIVE_HEADROOM))
        );
        assert_eq!(Pacer::parse("bounded:1G:3"), Ok(Pacer::bounded(1 << 30, 3)));
        assert_eq!(
            Pacer::parse("bounded:65536"),
            Ok(Pacer::bounded(INITIAL_COLLECT_THRESHOLD, LIVE_HEADROOM))
        );

        for bad in [
            "",
            "bounde",
            "bounded:huge",
            "bounded:8M:x",
            "bounded:8M:2:2",
        ] {
            assert!(Pacer::parse(bad).is_err(), "{bad:?} must not parse");
            assert_eq!(
                Pacer::from_spec(Some(bad)),
                Pacer::DEFAULT,
                "{bad:?} must fall back to the default"
            );
        }
    }

    /// Pacing counts what an object *costs*, not the size of its fixed block.
    /// A `Text` is 48 bytes of block plus a `Box<str>` of whatever the program
    /// read; charging only the block made a text-heavy program invisible to the
    /// collector — it under-reported its footprint by essentially all of it
    /// (RT-04).
    #[test]
    fn pacing_charges_the_bytes_a_payload_owns() {
        use crate::text::{TextPayload, TEXT};

        let alloc_text = |heap: &Heap, len: usize| {
            let owned: Box<str> = "x".repeat(len).into_boxed_str();
            // SAFETY: TextPayload is TEXT's payload type.
            unsafe { heap.alloc_payload_unpaced(&TEXT, TextPayload::owned(owned)) }
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

    /// **The pacer is charged the narrower stride, which is the whole of
    /// ADR-109's memory win.**
    ///
    /// Deleting `GcHeader::size` saves eight bytes per object in the heap, but
    /// that saving only turns into a smaller resident set because
    /// [`Heap::occupy`] charges the *stride* against `bytes_since_collect`. A
    /// 24-byte `Int` is 25% fewer bytes charged than the 32-byte one it
    /// replaces, so an `Int`-dominated program reaches `collect_threshold` after
    /// a third more allocations and takes one fewer doubling to hold the same
    /// live set. If pacing ever moved to counting *objects*, or to charging the
    /// payload rather than the block, the header shrink would go on being true
    /// and stop paying — and nothing else in the suite would notice.
    ///
    /// So this asserts the product, not the factors: N `Int`s cost N × 24, and
    /// the 24 is written out rather than re-derived from `SizeClass`, for the
    /// reason `page::tests::an_int_block_is_the_header_plus_eight` gives.
    #[test]
    fn the_pacer_is_charged_the_narrower_stride() {
        const N: usize = 100;
        let heap = Heap::new();
        assert_eq!(
            heap.bytes_since_collect.get(),
            0,
            "a fresh heap owes nothing"
        );

        for i in 0..N {
            // Above the interned range is irrelevant here — `alloc_unpaced`
            // goes to the heap whatever the value — but pacing must not trip a
            // collection mid-count, which is exactly what `_unpaced` guarantees.
            let _ = heap.alloc_unpaced(INT_PAYLOAD, i as i64);
        }

        assert_eq!(
            heap.bytes_since_collect.get(),
            N * 24,
            "an Int must be charged its 24-byte block and nothing else"
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
        // SAFETY: TextPayload is TEXT's payload type.
        let owner_ref = unsafe { heap.alloc_payload_unpaced(&TEXT, TextPayload::owned(owner)) };
        let after_owner = heap.bytes_since_collect.get();

        // SAFETY: `owner_ref` is the live Text allocated just above, and the
        // range lands inside it.
        let slice = unsafe { crate::text::SourceSlice::new(owner_ref, 0, 4096) }
            .expect("the whole owner is a valid slice of itself");
        // SAFETY: TextPayload is TEXT's payload type.
        unsafe {
            heap.alloc_payload_unpaced(&TEXT, TextPayload::Slice(slice));
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
            // SAFETY: TextPayload is TEXT's payload type.
            unsafe {
                heap.alloc_payload_unpaced(&TEXT, TextPayload::owned("x"));
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
            // SAFETY: DropProbe is DROP_PROBE's payload type.
            unsafe {
                heap.alloc_payload_unpaced(&DROP_PROBE, DropProbe(Arc::clone(&drops)));
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

        // SAFETY: Overaligned is OVERALIGNED's payload type.
        let over = unsafe { heap.alloc_payload_unpaced(&OVERALIGNED, Overaligned(1)) };
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

    /// A stand-in for the crash debugger's value slots (ADR-106): references
    /// held somewhere the collector does not trace.
    ///
    /// It records what each slot's header looked like *at the moment the
    /// collector called it*, which is the only way a test can observe where in
    /// `collect_inner` the scan sits. The real arm reaches its slots through a
    /// `DebugFrameEntry` and is tested against a live `Runtime` in
    /// `crate::debug`; this one exists to pin the heap's half of the contract.
    struct WeakSlots {
        slots: std::cell::RefCell<Vec<Option<GcRef>>>,
        poisoned_at_scan: std::cell::RefCell<Vec<bool>>,
    }

    impl WeakSlots {
        fn holding(refs: &[GcRef]) -> WeakSlots {
            WeakSlots {
                slots: std::cell::RefCell::new(refs.iter().copied().map(Some).collect()),
                poisoned_at_scan: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    impl crate::roots::WeakSet for WeakSlots {
        fn clear_reclaimed(&self) -> usize {
            let mut cleared = 0;
            let mut seen = self.poisoned_at_scan.borrow_mut();
            for slot in self.slots.borrow_mut().iter_mut() {
                let Some(r) = *slot else {
                    seen.push(false);
                    continue;
                };
                let poisoned = r.header().is_poisoned();
                seen.push(poisoned);
                if poisoned {
                    *slot = None;
                    cleared += 1;
                }
            }
            cleared
        }
    }

    /// The weak set is a *clear*, not a second sweep: it nulls exactly the
    /// entries whose objects this collection reclaimed and leaves every entry
    /// naming a survivor alone.
    ///
    /// Nulling everything would satisfy "no dangling reference" and destroy the
    /// debugger; retaining everything would satisfy the debugger and undo
    /// MIR-01. This is the statement that it does neither.
    #[test]
    fn the_weak_scan_nulls_only_what_this_collection_reclaimed() {
        let heap = Heap::new();
        let doomed = heap.alloc_unpaced(INT_PAYLOAD, 1_i64);
        let kept = heap.alloc_unpaced(INT_PAYLOAD, 2_i64);
        let weak = WeakSlots::holding(&[doomed, kept]);
        let mut scope = RootScope::new();
        scope.root(kept);

        heap.collect_with_weak(&scope, &weak);

        assert_eq!(
            *weak.poisoned_at_scan.borrow(),
            vec![true, false],
            "the scan ran before the sweep, or the sweep did not poison"
        );
        assert_eq!(
            *weak.slots.borrow(),
            vec![None, Some(kept)],
            "exactly the reclaimed entry becomes an absence"
        );
        assert_eq!(heap.stats().live_count, 1);
    }

    /// The scan's position inside `collect_inner`, as an observation rather than
    /// a comment (ADR-106 decision 2).
    ///
    /// Two facts pin it from both sides. The slot's header was already poisoned
    /// when the scan looked at it, so the scan runs *after* the sweep. And the
    /// very next allocation of that layout takes the same block back and reads
    /// as a `Float`, so the scan ran *before* the reissue — which is the moment
    /// after which no predicate could have told the two apart.
    #[test]
    fn the_weak_scan_runs_after_the_sweep_and_before_the_block_is_reissued() {
        use crate::scalars::FLOAT_PAYLOAD;
        let heap = Heap::new();
        let doomed = heap.alloc_unpaced(INT_PAYLOAD, 1_i64);
        let address = doomed.as_ptr();
        let weak = WeakSlots::holding(&[doomed]);

        heap.collect_with_weak(&RootScope::new(), &weak);

        assert_eq!(*weak.poisoned_at_scan.borrow(), vec![true]);
        assert_eq!(*weak.slots.borrow(), vec![None]);

        let reused = heap.alloc_unpaced(FLOAT_PAYLOAD, 2.5_f64);
        assert_eq!(
            reused.as_ptr(),
            address,
            "this test only says anything if the block really came back"
        );
        assert_eq!(reused.descriptor().name, "Float");
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
        // SAFETY: Overaligned is OVERALIGNED's payload type.
        let doomed = unsafe { heap.alloc_payload_unpaced(&OVERALIGNED, Overaligned(1)) };
        let address = doomed.as_ptr();

        heap.collect_with(&RootScope::new());
        let pages = heap.page_count();

        // SAFETY: as above.
        let reused = unsafe { heap.alloc_payload_unpaced(&OVERALIGNED, Overaligned(2)) };
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

    /// One half of the pair `a_swept_block_is_never_handed_to_a_request_of_another_alignment`
    /// needs: a payload that agrees with [`Aligned16`]'s in width and differs
    /// from it in alignment. **The two widths are deliberately identical, and
    /// they did not used to be.**
    ///
    /// The test asserts equal block sizes as its own precondition, and whether
    /// that holds is a fact about the *header*, not about these structs. While
    /// `GcHeader` was 24 bytes, `payload_offset_for(16)` padded an over-aligned
    /// payload forward to 32, so the pair reached 48 bytes only by cancellation
    /// — 24 + 24 against 32 + 16, two different payload widths landing on one
    /// block size. ADR-109 took the header to 16, which is itself a multiple of
    /// 16, so the padding disappeared and the cancellation with it: the fixtures
    /// fell to 40 and 32 and the test died on its precondition rather than on
    /// the property it exists to check.
    ///
    /// Giving both the same 32-byte payload makes the parity structural instead
    /// of coincidental — it now holds for any header whose size is a multiple of
    /// 16, rather than for one particular header size — and leaves the block at
    /// the 48 bytes this test has always talked about.
    #[repr(C)]
    struct Aligned8([u64; 4]);

    /// [`Aligned8`]'s payload at [`Aligned8`]'s width, aligned twice as
    /// strictly. See [`Aligned8`] for why the widths must match.
    #[repr(C, align(16))]
    struct Aligned16([u64; 4]);

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
    ///
    /// The first assertion is a precondition, not the property. If it is what
    /// fails, the fixtures have stopped sharing a block size and nothing about
    /// alignment reuse has regressed — read [`Aligned8`]'s doc, which explains
    /// what the shared size depends on and what moved it last time.
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
        // SAFETY: Aligned8 is ALIGNED_8's payload type.
        let doomed = unsafe { heap.alloc_payload_unpaced(&ALIGNED_8, Aligned8([1, 2, 3, 4])) };
        let address = doomed.as_ptr();
        heap.collect_with(&RootScope::new());

        // SAFETY: Aligned16 is ALIGNED_16's payload type.
        let other = unsafe { heap.alloc_payload_unpaced(&ALIGNED_16, Aligned16([5, 6, 7, 8])) };
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
