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
//! M3 realization (ADR-011): a typed descriptor pointer, a tri-color mark byte
//! (interior-mutable so the collector can color through a shared reference),
//! and the payload size in bytes for precise sweep and debugging.

use std::cell::Cell;
use std::num::NonZeroU32;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::descriptor::TypeDescriptor;

/// Tri-color mark values used by the collector (ADR-011). Stored in the
/// header's `mark` byte.
pub(crate) const WHITE: u8 = 0;
pub(crate) const GREY: u8 = 1;
pub(crate) const BLACK: u8 = 2;

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
#[repr(C)]
pub struct GcHeader {
    /// The descriptor that centralizes every payload-aware operation (§11.4).
    /// Stored as a typed pointer so the header's layout does not depend on the
    /// descriptor's definition, yet access is type-safe.
    ///
    /// Null means **poisoned**: the storage has been swept and its payload
    /// finalized. `Cell` so `poison` can run through a shared reference during
    /// the sweep, which walks the live registry by shared borrow.
    descriptor: Cell<*const TypeDescriptor>,
    /// Size of the payload in bytes (excludes the header). Used for stats and
    /// debugging; precise sweep uses the live-set registry, not this field.
    size: u32,
    /// Distance in bytes from this header's address to its payload's. **The
    /// single layout authority** — written by the allocator from the same
    /// calculation that produced the address it initialized, and read by
    /// [`GcHeader::payload`], by the collector, and by generated code.
    payload_offset: u16,
    /// Tri-color mark byte for the collector (ADR-011). `Cell` so the mark phase
    /// can recolor a header reached through a shared `&GcHeader`.
    mark: Cell<u8>,
    /// Explicit padding, so the `#[repr(C)]` layout has no implicit holes.
    _pad: u8,
    /// Which heap owns this allocation ([`HeapId`]). 0 means poisoned/unowned.
    /// `Cell` for the same reason as `descriptor`.
    heap_id: Cell<u32>,
}

impl GcHeader {
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
        size: u32,
        payload_offset: u16,
        heap_id: HeapId,
    ) -> GcHeader {
        GcHeader {
            descriptor: Cell::new(descriptor as *const TypeDescriptor),
            size,
            payload_offset,
            mark: Cell::new(WHITE),
            _pad: 0,
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

    /// The payload size in bytes recorded at allocation.
    #[inline]
    pub fn size(&self) -> u32 {
        self.size
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
    #[inline]
    pub fn is_poisoned(&self) -> bool {
        self.descriptor.get().is_null()
    }

    /// Mark this header's storage as reclaimed: no descriptor, no owning heap.
    ///
    /// Called by the sweep *after* finalizing the payload and before the header
    /// leaves the live registry, so a stale `GcRef` that reaches it afterwards
    /// is rejected by [`GcHeader::heap_id`] instead of being traced through
    /// freed storage.
    #[inline]
    pub(crate) fn poison(&self) {
        self.descriptor.set(std::ptr::null());
        self.heap_id.set(0);
    }

    /// Current mark color (ADR-011).
    #[inline]
    pub(crate) fn mark_color(&self) -> u8 {
        self.mark.get()
    }

    /// Recolor this header.
    #[inline]
    pub(crate) fn set_mark_color(&self, color: u8) {
        self.mark.set(color);
    }

    /// A header owned by no heap, for tests that need a non-null `GcRef`
    /// address and never dereference the object behind it.
    #[cfg(test)]
    pub(crate) fn detached() -> GcHeader {
        GcHeader {
            descriptor: Cell::new(std::ptr::null()),
            size: 0,
            payload_offset: std::mem::size_of::<GcHeader>() as u16,
            mark: Cell::new(WHITE),
            _pad: 0,
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
        assert_eq!(std::mem::size_of::<GcHeader>(), 24);
        assert_eq!(std::mem::align_of::<GcHeader>(), 8);
    }

    /// `payload_offset_for` is the single layout authority. For any alignment
    /// up to the header's own it is the header size; beyond that it pads.
    #[test]
    fn payload_offset_pads_only_for_overaligned_payloads() {
        let header = std::mem::size_of::<GcHeader>();
        for align in [1_usize, 2, 4, 8] {
            assert_eq!(GcHeader::payload_offset_for(align), header);
        }
        assert_eq!(GcHeader::payload_offset_for(16), 32);
        assert_eq!(GcHeader::payload_offset_for(64), 64);
    }

    /// The offset a header records must be the one `payload_offset_for`
    /// computes — the invariant that makes `payload()` and the allocator agree.
    #[test]
    fn payload_offset_is_recorded_in_the_header() {
        let header = GcHeader::new(
            &crate::scalars::INT,
            8,
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
        let header = GcHeader::new(&crate::scalars::INT, 8, 24, HeapId::mint());
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
