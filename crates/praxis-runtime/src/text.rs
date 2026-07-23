//! The `Text` scalar descriptor (§4.3).
//!
//! `Text` is an immutable UTF-8 payload referenced through a `GcRef` (§4.3).
//! §4.3 allows two representations: an owned UTF-8 payload, or "source-slice
//! metadata" (`owner: GcRef, start, length`, §12.6). Source slices are produced
//! by the input-parser (M6); M3 ships **owned `Text` only** (ADR-013).
//!
//! The owned payload is a [`Box<str>`], which owns its heap allocation and
//! therefore has a non-trivial [`DropFn`] (§12.5): sweep must release it. There
//! are no nested `GcRef`s in an owned text, so `trace` is a no-op (the
//! nested-reference guarantee is exercised by `Vec[T]`, ADR-013).

use std::fmt;

use crate::descriptor::{hash_value, DynamicHasher, Tracer, TypeDescriptor, TypeId};

/// The owned `Text` payload: an immutable, heap-allocated UTF-8 string.
pub type OwnedText = Box<str>;

unsafe fn text_trace(_: *mut u8, _: &mut dyn Tracer) {}
unsafe fn text_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized `OwnedText`.
    // `drop_in_place` runs its destructor, freeing the backing allocation.
    unsafe { std::ptr::drop_in_place(payload as *mut OwnedText) };
}
unsafe fn text_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an `OwnedText`.
    let s: &OwnedText = unsafe { &*(payload as *const OwnedText) };
    let _ = out.write_str(s);
}
unsafe fn text_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at `OwnedText`s.
    let a: &OwnedText = unsafe { &*(a as *const OwnedText) };
    let b: &OwnedText = unsafe { &*(b as *const OwnedText) };
    a == b
}
unsafe fn text_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an `OwnedText`.
    let s: &OwnedText = unsafe { &*(payload as *const OwnedText) };
    hash_value(hasher, &**s);
}

/// Descriptor for the `Text` scalar (§4.3). Owned UTF-8 form in M3.
pub const TEXT: &TypeDescriptor = &TypeDescriptor {
    id: TypeId(5),
    name: "Text",
    size: std::mem::size_of::<OwnedText>(),
    align: std::mem::align_of::<OwnedText>(),
    trace: text_trace,
    drop_value: text_drop,
    format: text_format,
    equals: Some(text_equals),
    hash: Some(text_hash),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_descriptor_formats_and_compares() {
        use std::ptr;
        let a: OwnedText = "hello".into();
        let b: OwnedText = "hello".into();
        let c: OwnedText = "world".into();

        let mut buf = String::new();
        unsafe { (TEXT.format)(ptr::addr_of!(a) as *const u8, &mut buf) };
        assert_eq!(buf, "hello");

        assert!(unsafe {
            (TEXT.equals.unwrap())(ptr::addr_of!(a) as *const u8, ptr::addr_of!(b) as *const u8)
        });
        assert!(!unsafe {
            (TEXT.equals.unwrap())(ptr::addr_of!(a) as *const u8, ptr::addr_of!(c) as *const u8)
        });
    }
}
