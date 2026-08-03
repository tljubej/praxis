//! The uniform object reference type and its header.
//!
//! Every runtime language value — `Int`, `Bool`, a record, a vector element —
//! is a [`GcRef`] (§4.3, §11.1). The reference is a non-null pointer to a
//! [`GcHeader`]; generated code treats it as opaque and passes it by value.
//!
//! `GcRef` is `#[repr(transparent)]` over `NonNull<GcHeader>`, which is itself
//! pointer-representable, so it is FFI-safe and matches the calling convention
//! in §10.3.
//!
//! See §12.2 for the conceptual header layout. The concrete fields here are the
//! M3 realization (ADR-011) as amended by ADR-039, ADR-103 and ADR-109: a typed
//! descriptor pointer, the offset the allocator laid the payload at, and the
//! owning heap's identity. Two things that used to be here are not, and both
//! left for the same reason — a field every object pays for must be a field
//! something reads. The mark colour is a bit in the object's page
//! ([`crate::page`]), because a per-object colour byte cost a random-access
//! store per surviving object per collection. The payload size is nowhere: it
//! was recorded "for debugging" and had no readers at all, and deleting it took
//! the header from 24 bytes to 16 and every block in the heap down by eight
//! (ADR-109). The descriptor answers the size question for anyone who asks it.

use std::cell::Cell;
use std::num::NonZeroU32;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::descriptor::TypeDescriptor;

/// The identity of the heap that owns an allocation.
///
/// Every [`Heap`](crate::Heap) mints one at construction (and a fresh one at
/// `reset`), and every header it allocates carries it. That makes "is this
/// object mine?" an O(1) test the collector can run *before* it dereferences
/// anything the header points at — which is what lets `Heap::mark` reject a
/// root belonging to another heap, or a header the sweep has already poisoned.
///
/// `NonZeroU32` because 0 is reserved as the poisoned/unowned encoding in the
/// header's `heap_id` field.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct HeapId(NonZeroU32);

impl HeapId {
    /// Mint a fresh, process-unique identity.
    ///
    /// # Panics
    /// Panics after `u32::MAX - 1` heaps have been created in one process,
    /// which no real program reaches (it would require minting one heap per
    /// microsecond for over an hour).
    pub(crate) fn mint() -> HeapId {
        static NEXT: AtomicU32 = AtomicU32::new(1);
        let raw = NEXT.fetch_add(1, Ordering::Relaxed);
        HeapId(NonZeroU32::new(raw).expect("HeapId space exhausted"))
    }

    /// The raw value stored in a header. Never 0.
    #[inline]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// Header prepended to every GC allocation (§12.2).
///
/// Layout is `#[repr(C)]` and the payload follows this header in the same
/// allocation, at [`GcHeader::payload_offset_for`] bytes from the header's
/// address — *not* necessarily at `size_of::<GcHeader>()`, because an
/// over-aligned payload is padded forward. The header is addressable as
/// `*mut GcHeader` and the payload is reached via [`GcHeader::payload`].
///
/// The fields are private: the allocator ([`Heap::alloc_raw`](crate::Heap)) is
/// the only constructor, so an initialized header is the only kind that exists,
/// and `payload_offset` cannot disagree with the address the allocator handed
/// to the payload initializer.
///
/// **Sixteen bytes, and every field in them has a reader on a hot path**
/// (ADR-109). This prefixes every allocation in the language, so a field here is
/// a tax on every object a program makes: the `size: u32` that used to sit
/// between `descriptor` and `payload_offset` was written by the allocator and
/// read by nothing, and it cost eight bytes per object once `#[repr(C)]`
/// padding is counted. Adding a field here is not a local decision — it moves
/// `page::MIN_BLOCK`, the whole size-class ladder, and the immediate generated
/// code folds to reach a payload, so it owes an ABI bump and an ADR.
#[repr(C)]
pub struct GcHeader {
    /// The descriptor that centralizes every payload-aware operation (§11.4).
    /// Stored as a typed pointer so the header's layout does not depend on the
    /// descriptor's definition, yet access is type-safe.
    ///
    /// Null means **poisoned**: the storage has been swept and its payload
    /// finalized. `Cell` so `poison` can run through a shared reference during
    /// the sweep, which reaches every block through a `&PageHeader`.
    descriptor: Cell<*const TypeDescriptor>,
    /// Distance in bytes from this header's address to its payload's. **The
    /// single layout authority** — written by the allocator from the same
    /// calculation that produced the address it initialized, and read by
    /// [`GcHeader::payload`], by the collector, and by generated code.
    payload_offset: u16,
    /// Which heap owns this allocation ([`HeapId`]). 0 means poisoned/unowned.
    /// `Cell` for the same reason as `descriptor`.
    ///
    /// The page carries the same id, and could answer for it — but this copy is
    /// what the mark phase reads *first*, and reading it first is what makes
    /// masking the address to find the page sound at all (ADR-103): only a
    /// header this heap allocated carries this heap's id, and every header this
    /// heap allocated is inside one of its pages.
    heap_id: Cell<u32>,
}

impl GcHeader {
    /// Where the descriptor pointer sits, relative to the header's address.
    ///
    /// Generated code reads it: since ADR-102 an `Inst::ExtractScalar` proves
    /// the object's type inline — one load from here, one compare against the
    /// scalar descriptor's address — instead of calling `praxis_int_load` and
    /// letting the wrapper prove it. The check is what makes the folded payload
    /// offset below the offset the allocator actually used, and it is what keeps
    /// REP-56 (a `praxis check`-clean program extracting an `Int` from a `Unit`)
    /// a refusal rather than an out-of-bounds read.
    ///
    /// Exported from here, derived with `offset_of!`, because ADR-039 decision 1
    /// made the fields **private** to this module: the backend cannot reach for
    /// the offset itself, and the alternative — writing `0` in the backend —
    /// is exactly the re-derived literal that decision exists to prevent.
    /// [`payload_offset_for`](Self::payload_offset_for) is the same idea one
    /// step further along.
    pub const DESCRIPTOR_OFFSET: usize = core::mem::offset_of!(GcHeader, descriptor);

    /// Where the payload begins, relative to the header's address, for a
    /// payload with the given alignment.
    ///
    /// This is **the** object-layout calculation: `Heap::alloc_raw` uses it to
    /// place the payload, `payload_offset` records what it returned, and
    /// generated code calls it to reach a payload directly. `const` so codegen
    /// can fold it into an immediate.
    ///
    /// # Panics
    /// Panics if `payload_align` is not a power of two.
    #[inline]
    pub const fn payload_offset_for(payload_align: usize) -> usize {
        assert!(
            payload_align.is_power_of_two(),
            "payload alignment must be a power of two"
        );
        round_up(std::mem::size_of::<GcHeader>(), payload_align)
    }

    /// Construct an initialized header. Only the allocator calls this.
    #[inline]
    pub(crate) fn new(
        descriptor: &'static TypeDescriptor,
        payload_offset: u16,
        heap_id: HeapId,
    ) -> GcHeader {
        GcHeader {
            descriptor: Cell::new(descriptor as *const TypeDescriptor),
            payload_offset,
            heap_id: Cell::new(heap_id.get()),
        }
    }

    /// The descriptor describing this object's payload (§11.4).
    ///
    /// Descriptors are always `'static` (built-in constants or compiler-emitted
    /// statics), so the returned lifetime is unconstrained.
    ///
    /// # Panics
    /// Panics if the header has been poisoned by the sweep. Callers that may
    /// hold a stale reference must check [`GcHeader::is_poisoned`] first; the
    /// collector does this via [`GcHeader::heap_id`].
    #[inline]
    pub fn descriptor(&self) -> &'static TypeDescriptor {
        let ptr = self.descriptor.get();
        assert!(
            !ptr.is_null(),
            "descriptor read from a poisoned (swept) GcHeader"
        );
        // SAFETY: every live `GcHeader` is allocated with a descriptor pointer
        // that points at a `'static TypeDescriptor`. The allocator is the only
        // constructor of headers, and it upholds this; the null case — the only
        // other value the field ever holds — is rejected above.
        unsafe { &*ptr }
    }

    /// Pointer to this header's payload bytes.
    ///
    /// The caller is responsible for knowing the payload type (via the
    /// descriptor); this is the low-level escape hatch used by descriptor
    /// callbacks and typed accessors.
    #[inline]
    pub fn payload<T>(&self) -> *mut T {
        // SAFETY: the payload lives `payload_offset` bytes into the same
        // allocation, at the exact address the allocator initialized. This is a
        // raw pointer calculation; dereferencing safely is the caller's job.
        let header_ptr = self as *const GcHeader as *mut u8;
        unsafe { header_ptr.add(self.payload_offset as usize) as *mut T }
    }

    /// The heap that owns this allocation, or `None` if the header is poisoned.
    #[inline]
    pub fn heap_id(&self) -> Option<HeapId> {
        NonZeroU32::new(self.heap_id.get()).map(HeapId)
    }

    /// Whether this header's storage has been swept.
    ///
    /// A poisoned header is not an object: its payload has been finalized and
    /// its bytes may be reused. Reading anything but this predicate off it is a
    /// bug.
    ///
    /// **"May be reused" is why this predicate has a shelf life.** It answers
    /// "has this block been reclaimed" only until the allocator reissues the
    /// block and writes a fresh header over the poison. The collector's weak
    /// arm ([`crate::debug::DebugFrameStackHeader::clear_reclaimed`], ADR-106)
    /// is the one caller that depends on that, and it runs inside the
    /// collection — after the sweep and before any allocation — for exactly
    /// this reason.
    #[inline]
    pub fn is_poisoned(&self) -> bool {
        self.descriptor.get().is_null()
    }

    /// Mark this header's storage as reclaimed: no descriptor, no owning heap.
    ///
    /// Called by the sweep *after* finalizing the payload and before the block's
    /// `allocated` bit is cleared, so a stale `GcRef` that reaches it afterwards
    /// is rejected by [`GcHeader::heap_id`] instead of being traced through
    /// freed storage.
    #[inline]
    pub(crate) fn poison(&self) {
        self.descriptor.set(std::ptr::null());
        self.heap_id.set(0);
    }

    /// A header owned by no heap, for tests that need a non-null `GcRef`
    /// address and never dereference the object behind it.
    ///
    /// The zero `heap_id` is what keeps this safe now that `Heap::mark` masks an
    /// accepted address to find its page: no heap's id is zero, so a detached
    /// header is rejected by the provenance check *before* anything derives a
    /// page from its address.
    #[cfg(test)]
    pub(crate) fn detached() -> GcHeader {
        GcHeader {
            descriptor: Cell::new(std::ptr::null()),
            payload_offset: std::mem::size_of::<GcHeader>() as u16,
            heap_id: Cell::new(0),
        }
    }
}

/// Round `n` up to the next multiple of `align` (which must be a power of two).
///
/// The object-layout primitive behind [`GcHeader::payload_offset_for`]; kept
/// `const` so the offset folds into a compile-time immediate.
pub(crate) const fn round_up(n: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (n + align - 1) & !(align - 1)
}

/// A non-null, uniformly-typed reference to a garbage-collected object.
///
/// Construction is `unsafe` because the caller must guarantee the pointer
/// points to a valid, live allocation of the right shape. The safe accessors
/// are the ordinary way to interact with a `GcRef` from Rust runtime wrappers.
///
/// `PartialEq`/`Eq`/`Hash` are by **pointer identity**: two `GcRef`s are equal
/// iff they point at the same object. (Structural value equality goes through
/// [`GcRef::equals`](crate::GcRef::equals) and the descriptors, §5.5.)
#[repr(transparent)]
pub struct GcRef(NonNull<GcHeader>);

impl PartialEq for GcRef {
    #[inline]
    fn eq(&self, other: &GcRef) -> bool {
        self.as_ptr() == other.as_ptr()
    }
}
impl Eq for GcRef {}

impl std::hash::Hash for GcRef {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_ptr().hash(state);
    }
}

impl GcRef {
    /// Wrap a non-null pointer. The pointer must point to a valid `GcHeader`
    /// allocation; the caller (always internal runtime code) upholds this.
    ///
    /// # Safety
    /// `ptr` must be non-null, properly aligned, and dereferenceable for the
    /// full object it heads.
    #[inline]
    pub unsafe fn from_non_null(ptr: NonNull<GcHeader>) -> GcRef {
        GcRef(ptr)
    }

    /// Wrap a non-null raw header pointer. Internal convenience for callers
    /// (e.g. the shadow frame) that hold a `*mut GcHeader` already known to be
    /// non-null.
    ///
    /// # Safety
    /// `ptr` must be non-null, properly aligned, and point at a valid live
    /// `GcHeader`.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut GcHeader) -> GcRef {
        // SAFETY: forwarded to the caller's contract.
        let nn = unsafe { NonNull::new_unchecked(ptr) };
        GcRef(nn)
    }

    /// The raw pointer this reference carries. Never null.
    #[inline]
    pub fn as_ptr(self) -> *mut GcHeader {
        self.0.as_ptr()
    }

    /// The underlying non-null pointer, for safe interior access in runtime code.
    #[inline]
    pub fn as_non_null(self) -> NonNull<GcHeader> {
        self.0
    }

    /// The header this reference points at.
    #[inline]
    pub fn header(&self) -> &GcHeader {
        // SAFETY: `self.0` is a non-null pointer to a live `GcHeader` for as
        // long as the `GcRef` is live (the caller of `from_non_null` upholds
        // this; the GC does not move objects — ADR-011).
        unsafe { self.0.as_ref() }
    }

    /// The descriptor describing this object's payload (§11.4).
    #[inline]
    pub fn descriptor(&self) -> &'static TypeDescriptor {
        self.header().descriptor()
    }

    /// Pointer to the payload bytes immediately following this object's header.
    ///
    /// This is the low-level escape hatch; prefer the typed accessors on
    /// [`crate::Runtime`] / the descriptor callbacks where possible.
    #[inline]
    pub fn payload<T>(&self) -> *mut T {
        self.header().payload::<T>()
    }
}

impl Clone for GcRef {
    #[inline]
    fn clone(&self) -> GcRef {
        *self
    }
}
impl Copy for GcRef {}

impl std::fmt::Debug for GcRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GcRef({:p})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `GcRef` must be exactly pointer-sized and FFI-safe (§10.3). A regression
    /// here would silently break the generated calling convention.
    #[test]
    fn gcref_is_pointer_sized() {
        assert_eq!(
            std::mem::size_of::<GcRef>(),
            std::mem::size_of::<*mut u8>(),
            "GcRef must be exactly one pointer"
        );
        assert_eq!(
            std::mem::align_of::<GcRef>(),
            std::mem::align_of::<*mut u8>()
        );
    }

    #[test]
    fn gcref_round_trips_a_real_header() {
        let mut header = GcHeader::detached();
        let nn = NonNull::from(&mut header);
        // SAFETY: `nn` points at a live, aligned `GcHeader`.
        let r = unsafe { GcRef::from_non_null(nn) };
        assert_eq!(r.as_ptr(), nn.as_ptr());
        assert_eq!(r.as_non_null(), nn);
    }

    #[test]
    fn round_up_is_correct() {
        assert_eq!(round_up(0, 8), 0);
        assert_eq!(round_up(1, 8), 8);
        assert_eq!(round_up(8, 8), 8);
        assert_eq!(round_up(9, 8), 16);
        assert_eq!(round_up(16, 1), 16);
    }

    /// The header must stay small and 8-aligned: it prefixes every allocation,
    /// and `#[repr(C)]` plus this assertion is what lets generated code compute
    /// a payload address (see `payload_offset_for`).
    #[test]
    fn header_layout_is_fixed() {
        assert_eq!(std::mem::size_of::<GcHeader>(), 16);
        assert_eq!(std::mem::align_of::<GcHeader>(), 8);
    }

    /// **One test for every number generated code depends on.**
    ///
    /// Three separate facts have to hold together for a Praxis binary to read
    /// its own objects, and they are asserted in one place so that the next
    /// person who repacks the header trips exactly one assertion and is sent to
    /// exactly one decision record:
    ///
    /// * the header is 16 bytes and 8-aligned, so `page::MIN_BLOCK` and
    ///   `page::BLOCK_GRANULE` — which derive from those two numbers — put the
    ///   ladder's floor where ADR-109 says it is;
    /// * `DESCRIPTOR_OFFSET` is 0, which is ADR-102's inline type proof: the
    ///   backend folds it into the load that precedes every inlined scalar read;
    /// * `payload_offset_for(8)` is 16, which is the immediate `Inst::EnumTag`
    ///   and `emit_scalar_load` fold into an `iadd_imm`.
    ///
    /// The failure this guards is silent. Compiler and runtime are the same
    /// binary, so `assert_abi_version` is trivially satisfied and would not
    /// notice a header that changed width; the protection is that
    /// `payload_offset_for` is the single `const` authority (ADR-039 Decision 1)
    /// and that this test pins what it folds to. Nobody may hand-write 16.
    #[test]
    fn the_header_is_descriptor_offset_and_heap_id_and_nothing_else() {
        assert_eq!(std::mem::size_of::<GcHeader>(), 16);
        assert_eq!(std::mem::align_of::<GcHeader>(), 8);
        assert_eq!(GcHeader::DESCRIPTOR_OFFSET, 0);
        assert_eq!(GcHeader::payload_offset_for(8), 16);
    }

    /// **The header shrank, so the folded immediate moved, and ABI v19 is what
    /// that bump is called.**
    ///
    /// This test replaces `removing_the_mark_byte_did_not_move_the_payload_offset`,
    /// whose premise was the opposite one: moving the mark colour into the page
    /// left the struct 20 bytes of fields in 24 bytes of `#[repr(C)]` padding,
    /// so nothing moved and no bump was owed. Deleting `size: u32` (ADR-109)
    /// *did* move it — 24 to 16 — because the field's four bytes plus the four
    /// bytes of padding they attracted were a whole eight.
    ///
    /// `Inst::EnumTag` reaches an enum's tag by calling `payload_offset_for` at
    /// compile time and folding the answer into an `iadd_imm`, and
    /// `emit_scalar_load` does the same for an inlined scalar read. Neither has
    /// a literal to update — that is ADR-039 Decision 1 working — but a runtime
    /// and a compiler from either side of this change disagree about where every
    /// payload in the language begins, and the compiler-runtime version check
    /// cannot catch it because they are one binary. So the guard is this: the
    /// immediate is pinned here, and it is pinned beside the version number that
    /// declares the disagreement, so the two can only be updated together.
    ///
    /// **The pin rides the current version, not v19**, and ADR-116's bump to
    /// v20 is what forced the distinction. The offset moved *at* v19 and has not
    /// moved since; what this asserts is that whoever bumps the version comes
    /// through here and re-confirms the immediate, which is the tripwire the
    /// paragraph above asks for. Pinning it to 19 forever would have made the
    /// next bump a mechanical edit of a failing number, which is the same thing
    /// as deleting the test.
    #[test]
    fn the_folded_payload_offset_moved_at_v19_and_is_pinned_here() {
        assert_eq!(std::mem::size_of::<GcHeader>(), 16);
        assert_eq!(
            GcHeader::payload_offset_for(std::mem::align_of::<GcHeader>()),
            16
        );
        assert_eq!(
            GcHeader::payload_offset_for(std::mem::align_of::<crate::enums::EnumPayload>()),
            16,
            "the offset lower.rs:Inst::EnumTag folds into an immediate"
        );
        assert_eq!(
            crate::abi::RUNTIME_ABI_VERSION,
            20,
            "the offset above last moved at v19 and is 16 at this version; a \
             bump must re-confirm it here rather than orphan this test"
        );
    }

    /// The ladder's floor is the header, and the granule is the header's
    /// alignment — **and that derivation is why ADR-109 was a one-field
    /// deletion.**
    ///
    /// `page::MIN_BLOCK` and `page::BLOCK_GRANULE` are written as
    /// `size_of::<GcHeader>()` and `align_of::<GcHeader>()`, so shrinking the
    /// header re-derived `NUM_CLASSES`, `MAX_BLOCKS`, `BITMAP_WORDS` and the
    /// whole size-class ladder without a single edit to `page.rs`. Nothing
    /// checked that derivation before — it was true by inspection of two `const`
    /// lines — and this codebase pins derivations, because the alternative is
    /// that someone "simplifies" `MIN_BLOCK` to a literal 16 and the next header
    /// change silently strands the ladder one rung above the smallest block.
    #[test]
    fn the_ladder_floor_follows_the_header() {
        assert_eq!(crate::page::MIN_BLOCK, std::mem::size_of::<GcHeader>());
        assert_eq!(crate::page::BLOCK_GRANULE, std::mem::align_of::<GcHeader>());
    }

    /// `payload_offset_for` is the single layout authority. For any alignment
    /// up to the header's own it is the header size; beyond that it pads.
    ///
    /// The 16 case is the one that moved with ADR-109 and is worth stating: a
    /// 16-aligned payload used to be padded forward past a 24-byte header, and
    /// now the header is itself 16-aligned-friendly, so nothing is padded. The
    /// 64 case is unchanged, which is what keeps `heap::tests::OVERALIGNED` and
    /// the large-page path testing the same thing they always did.
    #[test]
    fn payload_offset_pads_only_for_overaligned_payloads() {
        let header = std::mem::size_of::<GcHeader>();
        for align in [1_usize, 2, 4, 8, 16] {
            assert_eq!(GcHeader::payload_offset_for(align), header);
        }
        assert_eq!(GcHeader::payload_offset_for(64), 64);
    }

    /// The descriptor is the first word of the header, and generated code reads
    /// it there (ADR-102).
    ///
    /// Asserting the *value* as well as the round trip is deliberate: the
    /// backend folds `DESCRIPTOR_OFFSET` into an immediate, so a field reorder
    /// that moved the descriptor would be a silent miscompile of every scalar
    /// extract in the language if nothing here noticed. The round trip is what
    /// proves the constant names the field rather than merely being small.
    #[test]
    fn the_descriptor_is_at_the_offset_generated_code_reads() {
        assert_eq!(GcHeader::DESCRIPTOR_OFFSET, 0);
        let header = GcHeader::new(
            &crate::scalars::INT,
            GcHeader::payload_offset_for(8) as u16,
            HeapId::mint(),
        );
        let base = &header as *const GcHeader as *const u8;
        // SAFETY: `DESCRIPTOR_OFFSET` is within the header by construction, and
        // the field is a `Cell<*const TypeDescriptor>` — one pointer, so reading
        // it as a `*const TypeDescriptor` is reading it at its own width.
        let read_back = unsafe {
            base.add(GcHeader::DESCRIPTOR_OFFSET)
                .cast::<*const TypeDescriptor>()
                .read()
        };
        assert!(
            std::ptr::eq(read_back, &crate::scalars::INT),
            "the word at DESCRIPTOR_OFFSET is the descriptor the header was built with"
        );
    }

    /// The offset a header records must be the one `payload_offset_for`
    /// computes — the invariant that makes `payload()` and the allocator agree.
    #[test]
    fn payload_offset_is_recorded_in_the_header() {
        let header = GcHeader::new(
            &crate::scalars::INT,
            GcHeader::payload_offset_for(8) as u16,
            HeapId::mint(),
        );
        let base = &header as *const GcHeader as usize;
        assert_eq!(
            header.payload::<i64>() as usize - base,
            GcHeader::payload_offset_for(8)
        );
    }

    #[test]
    fn a_poisoned_header_has_no_heap_and_reports_itself() {
        let header = GcHeader::new(
            &crate::scalars::INT,
            GcHeader::payload_offset_for(8) as u16,
            HeapId::mint(),
        );
        assert!(!header.is_poisoned());
        assert!(header.heap_id().is_some());

        header.poison();

        assert!(header.is_poisoned());
        assert_eq!(header.heap_id(), None);
    }

    #[test]
    fn minted_heap_ids_are_distinct_and_non_zero() {
        let a = HeapId::mint();
        let b = HeapId::mint();
        assert_ne!(a, b);
        assert_ne!(a.get(), 0);
    }
}
