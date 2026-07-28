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
    /// A zero-copy view into another `Text`'s bytes (§7.10). `owner` is traced
    /// by the descriptor so the slice keeps its backing alive.
    Slice(SourceSlice),
}

/// A validated zero-copy view of `owner`'s bytes over `[start, start + len)`.
///
/// The fields are private and [`SourceSlice::new`] is the only constructor,
/// because the range is not a hint: a view whose end runs past the owner, or
/// whose ends fall inside a multi-byte scalar, is not a `Text`. Both used to be
/// constructible — the safe host helper only `debug_assert`'d the range and
/// nothing checked scalar boundaries at all — and the damage surfaced far from
/// its cause, as a release-build panic slicing out of range or as `text_str`
/// quietly returning `""` for a `Text` that had content (RT-06).
///
/// `#[repr(C)]` and field-ordered as the old inline variant was, so the payload
/// layout is unchanged.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SourceSlice {
    owner: GcRef,
    start: usize,
    len: usize,
}

impl SourceSlice {
    /// A view of `owner`'s bytes over `[start, start + len)`, or `None` if that
    /// is not a `Text`: a range past the end, a length that overflows, or ends
    /// that are not UTF-8 scalar boundaries.
    ///
    /// # Safety
    /// `owner` must be a live `Text` `GcRef` — its payload is read to validate
    /// the range.
    #[must_use]
    pub unsafe fn new(owner: GcRef, start: usize, len: usize) -> Option<SourceSlice> {
        // SAFETY: caller guarantees `owner` is a live Text.
        let bytes = unsafe { text_bytes(owner.payload::<TextPayload>() as *const TextPayload) };
        let end = start.checked_add(len)?;
        if end > bytes.len() {
            return None;
        }
        // Both ends must begin a scalar. A view that splits one is not UTF-8,
        // and reading it as a `&str` would fail.
        let whole = std::str::from_utf8(bytes).ok()?;
        if !whole.is_char_boundary(start) || !whole.is_char_boundary(end) {
            return None;
        }
        Some(SourceSlice { owner, start, len })
    }

    /// The `Text` this view borrows from. Traced, so the backing stays alive.
    #[inline]
    #[must_use]
    pub fn owner(self) -> GcRef {
        self.owner
    }
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
        TextPayload::Slice(slice) => {
            // SAFETY: caller guarantees the owner chain is valid; non-moving GC.
            let owner_payload = slice.owner.payload::<TextPayload>() as *const TextPayload;
            let owner_bytes = unsafe { text_bytes(owner_payload) };
            // In range by construction: `SourceSlice::new` is the only
            // constructor and it rejects anything else. There used to be a
            // clamp here — `&owner_bytes[start..]` when the end ran past —
            // which turned a bad range into a *different, plausible* Text, and
            // panicked anyway when `start` itself was out of range (RT-06).
            &owner_bytes[slice.start..slice.start + slice.len]
        }
    }
}

/// Read a `TextPayload` as a `&str`, following slice owners.
///
/// # Safety
/// See [`text_bytes`]; additionally the bytes must be valid UTF-8 (always true
/// for Text by construction — the parser only splits on UTF-8 boundaries).
pub unsafe fn text_str(payload: *const TextPayload) -> &'static str {
    // SAFETY: Text payloads are always valid UTF-8 by construction. An owned
    // payload is a `Box<str>`; a slice comes from `TextPayload::slice`, which
    // rejects ends that are not scalar boundaries. The `unwrap_or("")` this
    // replaces is why a mis-sliced Text read as empty instead of failing
    // (RT-06); `from_utf8_unchecked` would be the same lie without the
    // diagnosis, so the error case panics with one.
    let bytes = unsafe { text_bytes(payload) };
    std::str::from_utf8(bytes)
        .expect("a Text payload is UTF-8 by construction; SourceSlice::new enforces it")
}

// ---- descriptor callbacks -------------------------------------------------

unsafe fn text_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized TextPayload.
    match unsafe { &*(payload as *const TextPayload) } {
        // Owned text has no nested GcRef; trace is a no-op.
        TextPayload::Owned(_) => {}
        // A slice must keep its owner alive (ADR-013).
        TextPayload::Slice(slice) => tracer.trace(slice.owner),
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

/// Lexicographic order over the text's bytes (ADR-045). UTF-8 byte order *is*
/// code-point order, so this needs no decoding — and it is the whole content of
/// P0-12's Text half: the old lowering compared the first eight bytes of the
/// `TextPayload` enum, which is a `Box<str>` pointer for an owned text and a
/// `GcRef` for a slice. Two texts were ordered by where they happened to live.
///
/// # Safety
/// Both pointers must point at `TextPayload`s.
unsafe fn text_compare(a: *const u8, b: *const u8) -> std::cmp::Ordering {
    // SAFETY: caller guarantees both pointers point at TextPayloads.
    let a = unsafe { text_bytes(a as *const TextPayload) };
    let b = unsafe { text_bytes(b as *const TextPayload) };
    a.cmp(b)
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
    // Lexicographic by UTF-8 bytes (ADR-045).
    Some(text_compare),
)
.with_owned_bytes(text_owned_bytes);

/// The heap bytes a `Text` owns beyond its payload (RT-04).
///
/// An `Owned` text is a `Box<str>` whose length is the whole point: charging
/// pacing 40 bytes for a megabyte of input is what made a text-heavy program
/// invisible to the collector. A `Slice` owns nothing — it borrows its owner's
/// buffer, and charging its length would count the same bytes once per slice.
///
/// # Safety
/// `payload` must point at an initialized `TextPayload`.
unsafe fn text_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized TextPayload.
    match unsafe { &*(payload as *const TextPayload) } {
        TextPayload::Owned(s) => s.len(),
        TextPayload::Slice(_) => 0,
    }
}

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

    /// ADR-045: `Text` orders by its bytes, and the ordering is the same
    /// whether the text is owned or a zero-copy slice of another. Comparing
    /// the payload's first eight bytes — a `Box<str>` pointer here, a `GcRef`
    /// there — ordered texts by *address* (P0-12).
    #[test]
    fn text_compares_lexicographically_whatever_its_representation() {
        let cmp = TEXT.compare.expect("Text is orderable");
        let apple = TextPayload::Owned("apple".into());
        let banana = TextPayload::Owned("banana".into());
        let apple_again = TextPayload::Owned("apple".into());
        let at = |p: &TextPayload| ptr::addr_of!(*p) as *const u8;

        assert_eq!(
            unsafe { cmp(at(&apple), at(&banana)) },
            std::cmp::Ordering::Less
        );
        assert_eq!(
            unsafe { cmp(at(&banana), at(&apple)) },
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            unsafe { cmp(at(&apple), at(&apple_again)) },
            std::cmp::Ordering::Equal,
            "two separately allocated `apple`s are one value"
        );

        // A prefix precedes what extends it.
        let app = TextPayload::Owned("app".into());
        assert_eq!(
            unsafe { cmp(at(&app), at(&apple)) },
            std::cmp::Ordering::Less
        );

        // UTF-8 byte order is code-point order: "é" (U+00E9) follows "z".
        let z = TextPayload::Owned("z".into());
        let e_acute = TextPayload::Owned("é".into());
        assert_eq!(
            unsafe { cmp(at(&z), at(&e_acute)) },
            std::cmp::Ordering::Less
        );
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
        // SAFETY: `owner` is the live Text allocated above.
        let slice = unsafe { rt.alloc_text_slice(owner, 1, 3) }.expect("[1, 4) is in range");
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

    /// The range is not a hint. A view past the owner's end, one whose length
    /// overflows, or one whose ends split a multi-byte scalar is not a `Text`,
    /// and must be unconstructible rather than clamped: the old code
    /// `debug_assert`'d the range only, so a release build either sliced out of
    /// range or produced a `Text` that `text_str` read as `""` (RT-06).
    #[test]
    fn an_out_of_range_or_non_boundary_slice_is_unconstructible() {
        let rt = crate::Runtime::new();
        // "héllo" — 'é' is two bytes, so byte 1 starts it and byte 2 splits it.
        let owner = rt.alloc_text("héllo");
        let bytes = owner.as_text().len();
        assert_eq!(bytes, 6);

        // SAFETY: `owner` is a live Text for every call below.
        unsafe {
            assert!(
                rt.alloc_text_slice(owner, 0, bytes).is_some(),
                "the whole owner is a valid slice of itself"
            );
            assert!(
                rt.alloc_text_slice(owner, bytes, 0).is_some(),
                "an empty slice at the end is in range"
            );
            assert!(
                rt.alloc_text_slice(owner, 0, bytes + 1).is_none(),
                "a slice past the end is not a Text"
            );
            assert!(
                rt.alloc_text_slice(owner, bytes + 1, 0).is_none(),
                "a start past the end is not a Text"
            );
            assert!(
                rt.alloc_text_slice(owner, 1, usize::MAX).is_none(),
                "an overflowing length is not a Text"
            );
            assert!(
                rt.alloc_text_slice(owner, 2, 1).is_none(),
                "a start inside a multi-byte scalar is not a Text"
            );
            assert!(
                rt.alloc_text_slice(owner, 1, 1).is_none(),
                "an end inside a multi-byte scalar is not a Text"
            );
            // The boundaries either side of 'é' are fine.
            let e = rt
                .alloc_text_slice(owner, 1, 2)
                .expect("[1, 3) is a scalar");
            assert_eq!(e.as_text(), "é");
        }
    }

    #[test]
    fn hash_value_helper_compiles() {
        let mut h = crate::descriptor::StructHasher::new();
        hash_value(&mut h, &"x");
    }
}
