//! The uniform object reference type and its header.
//!
//! Every runtime language value — `Int`, `Bool`, a record, a vector element —
//! is a `GcRef` (§4.3, §11.1). The reference is a non-null pointer to a
//! [`GcHeader`]; generated code treats it as opaque and passes it by value.
//!
//! `GcRef` is `#[repr(transparent)]` over `NonNull<GcHeader>`, which is itself
//! pointer-representable, so it is FFI-safe and matches the calling convention
//! in §10.3.

use std::ptr::NonNull;

/// Header prepended to every GC allocation. The fields here are intentionally
/// schematic for Milestone 0; the real mark bits, size class, and allocator
/// metadata land in Milestone 3 (§12.2).
///
/// It must remain `?Sized`-compatible and addressable as `*mut GcHeader`.
#[repr(C)]
pub struct GcHeader {
    /// Reserved for the descriptor pointer (§12.2). Stored as a raw pointer so
    /// the header's layout does not depend on `TypeDescriptor`'s definition.
    pub descriptor: *const u8,
    /// Reserved mark / flag bits for the collector (§12.2).
    pub flags: u32,
}

/// A non-null, uniformly-typed reference to a garbage-collected object.
///
/// Construction is `unsafe` because the caller must guarantee the pointer
/// points to a valid, live allocation of the right shape. The safe accessors
/// are the ordinary way to interact with a `GcRef` from Rust runtime wrappers.
#[repr(transparent)]
pub struct GcRef(NonNull<GcHeader>);

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
            flags: 0,
        };
        let nn = NonNull::from(&mut header);
        // SAFETY: `nn` points at a live, aligned `GcHeader`.
        let r = unsafe { GcRef::from_non_null(nn) };
        assert_eq!(r.as_ptr(), nn.as_ptr());
        assert_eq!(r.as_non_null(), nn);
    }
}
