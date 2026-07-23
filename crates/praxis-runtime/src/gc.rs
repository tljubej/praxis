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
use std::ptr::NonNull;

use crate::descriptor::TypeDescriptor;

/// Tri-color mark values used by the collector (ADR-011). Stored in the
/// header's `mark` byte.
pub(crate) const WHITE: u8 = 0;
pub(crate) const GREY: u8 = 1;
pub(crate) const BLACK: u8 = 2;

/// Header prepended to every GC allocation (§12.2).
///
/// Layout is `#[repr(C)]` and the payload follows immediately after this header
/// in the same allocation. The header is addressable as `*mut GcHeader` and the
/// payload is reached via [`GcHeader::payload`].
#[repr(C)]
pub struct GcHeader {
    /// The descriptor that centralizes every payload-aware operation (§11.4).
    /// Stored as a typed pointer so the header's layout does not depend on the
    /// descriptor's definition, yet access is type-safe.
    pub(crate) descriptor: *const TypeDescriptor,
    /// Tri-color mark byte for the collector (ADR-011). `Cell` so the mark phase
    /// can recolor a header reached through a shared `&GcHeader`.
    pub(crate) mark: Cell<u8>,
    /// Size of the payload in bytes (excludes the header). Used for stats and
    /// debugging; precise sweep uses the live-set registry, not this field.
    pub(crate) size: u32,
}

impl GcHeader {
    /// The descriptor describing this object's payload (§11.4).
    ///
    /// Descriptors are always `'static` (built-in constants or compiler-emitted
    /// statics), so the returned lifetime is unconstrained.
    #[inline]
    pub fn descriptor(&self) -> &'static TypeDescriptor {
        // SAFETY: every live `GcHeader` is allocated with a descriptor pointer
        // that points at a `'static TypeDescriptor`. The allocator is the only
        // constructor of headers, and it upholds this.
        unsafe { &*self.descriptor }
    }

    /// Pointer to the payload bytes immediately following this header.
    ///
    /// The caller is responsible for knowing the payload type (via the
    /// descriptor); this is the low-level escape hatch used by descriptor
    /// callbacks and typed accessors.
    #[inline]
    pub fn payload<T>(&self) -> *mut T {
        // SAFETY: the payload follows the header in the same allocation. This is
        // a raw pointer calculation; dereferencing safely is the caller's job.
        let header_ptr = self as *const GcHeader as *mut u8;
        unsafe { header_ptr.add(std::mem::size_of::<GcHeader>()) as *mut T }
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
        let mut header = GcHeader {
            descriptor: std::ptr::null(),
            mark: Cell::new(WHITE),
            size: 0,
        };
        let nn = NonNull::from(&mut header);
        // SAFETY: `nn` points at a live, aligned `GcHeader`.
        let r = unsafe { GcRef::from_non_null(nn) };
        assert_eq!(r.as_ptr(), nn.as_ptr());
        assert_eq!(r.as_non_null(), nn);
    }
}
