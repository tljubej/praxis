//! The `Text` scalar descriptor (§4.3, ADR-013).
//!
//! `Text` is an immutable UTF-8 payload referenced through a `GcRef` (§4.3).
//! §4.3 allows two representations:
//!
//! - An **owned** UTF-8 payload (`Box<str>`).
//! - A **source slice** carrying `(owner: GcRef, start, length)` (§7.10) — a
//!   zero-copy view into another `Text` (typically the process-input buffer).
//!
//! Both are produced in M6: the input parser allocates source slices pointing
//! into the immutable stdin buffer, and string literals remain owned. The
//! descriptor callbacks handle both variants through [`text_bytes`], which
//! recurses through slice owners — sound because the collector is non-moving
//! (ADR-011) and owners are kept alive by GC reachability.
//!
//! Owned payloads own a Rust allocation (`Box<str>`), so [`TEXT`]'s `drop_value`
//! releases it on sweep (§12.5). Slice payloads carry only a `GcRef` (traced, not
//! owned) plus offsets — no Rust resources to drop.

use std::fmt;

#[cfg(test)]
use crate::descriptor::hash_value;
use crate::descriptor::{BuiltinTypeId, DynamicHasher, Tracer, TypeDescriptor};
use crate::GcRef;

/// The `Text` payload: either an owned UTF-8 string or a zero-copy slice into
/// another `Text` (the input buffer) (§4.3, §7.10, ADR-013).
#[repr(C)]
pub enum TextPayload {
    /// An owned, heap-allocated UTF-8 string (string literals, runtime-built text).
    Owned(Box<str>),
    /// A zero-copy view into `owner`'s bytes, spanning `[start, start+len)` (§7.10).
    /// `owner` is traced by the descriptor so the slice keeps its backing alive.
    Slice {
        owner: GcRef,
        start: usize,
        len: usize,
    },
}

impl TextPayload {
    /// True iff this is the [`Owned`](Self::Owned) variant.
    pub fn is_owned(&self) -> bool {
        matches!(self, Self::Owned(_))
    }
}

/// Read the UTF-8 bytes of a `TextPayload`, following slice owners.
///
/// For a [`TextPayload::Slice`], this reads through the `owner` reference. This
/// is sound because the GC is non-moving (ADR-011): the owner stays at its
/// address as long as it is reachable, and the slice's `trace` keeps it reachable.
///
/// # Safety
/// `payload` must point at a fully initialized, validly-linked `TextPayload` —
/// every `owner` along the chain must point at a live `Text` object.
pub unsafe fn text_bytes(payload: *const TextPayload) -> &'static [u8] {
    // SAFETY: caller guarantees `payload` points at a valid TextPayload.
    match unsafe { &*payload } {
        TextPayload::Owned(boxed) => boxed.as_bytes(),
        TextPayload::Slice { owner, start, len } => {
            // SAFETY: caller guarantees the owner chain is valid; non-moving GC.
            let owner_payload = owner.payload::<TextPayload>() as *const TextPayload;
            let owner_bytes = unsafe { text_bytes(owner_payload) };
            let end = *start + *len;
            // Offsets are byte-accurate by construction (the parser computes them
            // from byte positions); defensive bounds check never fires in practice.
            if end <= owner_bytes.len() {
                &owner_bytes[*start..end]
            } else {
                &owner_bytes[*start..]
            }
        }
    }
}

/// Read a `TextPayload` as a `&str`, following slice owners.
///
/// # Safety
/// See [`text_bytes`]; additionally the bytes must be valid UTF-8 (always true
/// for Text by construction — the parser only splits on UTF-8 boundaries).
pub unsafe fn text_str(payload: *const TextPayload) -> &'static str {
    // SAFETY: Text payloads are always valid UTF-8 by construction. The input
    // buffer is read as UTF-8 (lossy on malformed input at the ABI boundary);
    // all offsets land on scalar boundaries.
    let bytes = unsafe { text_bytes(payload) };
    std::str::from_utf8(bytes).unwrap_or("")
}

// ---- descriptor callbacks -------------------------------------------------

unsafe fn text_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized TextPayload.
    match unsafe { &*(payload as *const TextPayload) } {
        // Owned text has no nested GcRef; trace is a no-op.
        TextPayload::Owned(_) => {}
        // A slice must keep its owner alive (ADR-013).
        TextPayload::Slice { owner, .. } => tracer.trace(*owner),
    }
}

unsafe fn text_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized TextPayload.
    // `drop_in_place` frees the owned Box<str> (Owned variant); for Slice it
    // drops the GcRef (a no-op pointer copy) without touching the owner object.
    unsafe { std::ptr::drop_in_place(payload as *mut TextPayload) };
}

unsafe fn text_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at a TextPayload.
    let s = unsafe { text_str(payload as *const TextPayload) };
    let _ = out.write_str(s);
}

unsafe fn text_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at TextPayloads.
    let a = unsafe { text_bytes(a as *const TextPayload) };
    let b = unsafe { text_bytes(b as *const TextPayload) };
    a == b
}

unsafe fn text_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at a TextPayload.
    let bytes = unsafe { text_bytes(payload as *const TextPayload) };
    hasher.write_bytes(bytes);
}

/// Descriptor for the `Text` scalar (§4.3). Handles both owned and source-slice
/// payloads (ADR-013, M6). A single descriptor serves all `Text` values.
pub static TEXT: TypeDescriptor = TypeDescriptor::builtin::<TextPayload>(
    BuiltinTypeId::Text,
    "Text",
    text_trace,
    text_drop,
    text_format,
    Some(text_equals),
    Some(text_hash),
    // Ordering: see the ordering ADR; no built-in declares `compare` yet.
    None,
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    #[test]
    fn owned_text_descriptor_formats_and_compares() {
        let a = TextPayload::Owned("hello".into());
        let b = TextPayload::Owned("hello".into());
        let c = TextPayload::Owned("world".into());

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

    #[test]
    fn owned_text_hash_is_stable() {
        let a = TextPayload::Owned("hello".into());
        let b = TextPayload::Owned("hello".into());
        let mut ha = crate::descriptor::StructHasher::new();
        let mut hb = crate::descriptor::StructHasher::new();
        unsafe {
            (TEXT.hash.unwrap())(ptr::addr_of!(a) as *const u8, &mut ha);
            (TEXT.hash.unwrap())(ptr::addr_of!(b) as *const u8, &mut hb);
        }
        assert_eq!(ha.finish(), hb.finish());
    }

    #[test]
    fn owned_text_bytes_can_be_borrowed_as_a_manual_subslice() {
        let owner = TextPayload::Owned("hello, world".into());
        let owner_ptr = ptr::addr_of!(owner);
        let bytes = unsafe { text_bytes(owner_ptr) };
        assert_eq!(&bytes[7..12], b"world");
        let s = unsafe { text_str(owner_ptr) };
        assert_eq!(s, "hello, world");
    }

    #[test]
    fn source_slice_traces_its_owner_during_collection() {
        let rt = crate::Runtime::new();
        let owner = rt.alloc_text("hello");
        let slice = rt.alloc_text_slice(owner, 1, 3);
        let mut roots = crate::RootScope::new();
        roots.root(slice);

        rt.collect_with(&roots);

        assert_eq!(
            rt.heap().stats().live_count,
            2,
            "the rooted slice and its otherwise-unrooted owner must both survive"
        );
        assert_eq!(slice.as_text(), "ell");
    }

    #[test]
    fn hash_value_helper_compiles() {
        let mut h = crate::descriptor::StructHasher::new();
        hash_value(&mut h, &"x");
    }
}
