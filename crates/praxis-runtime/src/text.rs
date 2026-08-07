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
//! follows slice owners — sound because the collector is non-moving (ADR-011)
//! and owners are kept alive by GC reachability. Every walk of an owner chain
//! in this module is iterative, because nothing bounds its depth
//! (`reading_a_deep_slice_chain_does_not_recurse`).
//!
//! Owned payloads own a Rust allocation (`Box<str>`), so [`TEXT`]'s `drop_value`
//! releases it on sweep (§12.5). Slice payloads carry only a `GcRef` (traced, not
//! owned) plus offsets — no Rust resources to drop.
//!
//! An owned payload additionally carries a **lazily computed scalar count**
//! (ADR-115), which is what makes `t.len()` and `t[i]` O(1) on the texts a
//! program actually indexes. The count is the whole mechanism: `count ==
//! bytes.len()` is exactly "every scalar in this text is one byte", so it is
//! both the length answer and the byte-indexing licence, and a slice inherits
//! the licence from its owner because a view of one-byte scalars is one-byte
//! scalars. See [`text_char_count`] and [`text_ascii_bytes`].

use std::cell::Cell;
use std::fmt::Write as _;

#[cfg(test)]
use crate::descriptor::hash_value;
use crate::descriptor::{
    BuiltinTypeId, DynamicHasher, FormatSink, FormatStyle, Tracer, TypeDescriptor,
};
use crate::GcRef;

/// **The ADR-115 measurement toggle** (handover 26 §6), and the only difference
/// between the A/B arms this package was measured with.
///
/// `false` — enabled by the `adr115-arm-a` feature — keeps the representation
/// byte-for-byte identical and makes the cache never answer: every count is
/// recomputed from the bytes and [`text_ascii_bytes`] refuses, so `t.len()`
/// walks the text and `t[i]` decodes to the index, which is the complexity the
/// tree had before this decision. It exists so arm A differs from arm B in this
/// mechanism and in nothing else; measuring against `main` would also be
/// measuring [`SourceSlice::new`]'s dropped whole-owner revalidation, which is
/// a separate finding and is in both arms.
const COUNT_IS_CACHED: bool = !cfg!(feature = "adr115-arm-a");

/// The value [`OwnedText::char_count`] holds until someone asks.
///
/// A real count can never collide with it: a text with `u64::MAX` scalars would
/// need `u64::MAX` bytes, and the `Box<str>` holding them cannot be allocated
/// on a 64-bit host. So this is a sentinel in the strong sense — the state
/// "counted, and the count happens to be `NOT_COUNTED`" is unreachable rather
/// than merely unlikely.
const NOT_COUNTED: u64 = u64::MAX;

/// The `Text` payload: either an owned UTF-8 string or a zero-copy slice into
/// another `Text` (the input buffer) (§4.3, §7.10, ADR-013).
#[repr(C)]
pub enum TextPayload {
    /// An owned, heap-allocated UTF-8 string (string literals, runtime-built
    /// text) together with its lazily computed scalar count (ADR-115).
    Owned(OwnedText),
    /// A zero-copy view into another `Text`'s bytes (§7.10). `owner` is traced
    /// by the descriptor so the slice keeps its backing alive.
    Slice(SourceSlice),
}

/// An owned UTF-8 payload and the number of Unicode scalars in it, computed on
/// first demand (ADR-115).
///
/// The fields are private and [`OwnedText::new`] is the only constructor,
/// because a count that does not describe these bytes is not a slow `Text`, it
/// is a wrong one: `t.len()` would answer someone else's length and `t[i]`
/// would index bytes in a text that has multi-byte scalars. The only writer is
/// [`OwnedText::char_count`], which writes what it just counted from
/// `self.bytes`, and `Text` is immutable (ADR-085 allocates a fresh payload for
/// `+`), so there is no path that could invalidate one.
///
/// **This costs zero bytes.** `Box<str>` is 16 and [`SourceSlice`] is 24, so
/// the `#[repr(C)]` enum's union already reserved 8 bytes the owned variant
/// never used. The `const _` below is what holds that claim true.
#[repr(C)]
pub struct OwnedText {
    bytes: Box<str>,
    /// [`NOT_COUNTED`] until the first [`char_count`](Self::char_count).
    ///
    /// `Cell` because counting happens behind the `&TextPayload` every
    /// descriptor callback and every accessor already has — the runtime is
    /// single-threaded (`RuntimeContext` is not `Sync`) and this is the same
    /// interior mutability `GcHeader` uses for its own sweep-time writes.
    char_count: Cell<u64>,
}

/// `size_of::<TextPayload>()` is **32**, so a `Text` block is 48 and its size
/// class does not move (ADR-109's ladder is 16, 24, 32, …, 128 with a 16-byte
/// header). This is the claim ADR-115 rests on, and handover 26 §9 recorded it
/// as measured on a standalone copy of these declarations rather than on this
/// tree. It is measured here now, where a future field cannot silently move a
/// `Text` to the 56-byte class and charge the pacer 16.7% more per text object.
const _: () = {
    assert!(std::mem::size_of::<TextPayload>() == 32);
    assert!(std::mem::size_of::<OwnedText>() == 24);
    assert!(std::mem::size_of::<SourceSlice>() == 24);
    assert!(std::mem::align_of::<TextPayload>() == 8);
};

impl OwnedText {
    /// An owned payload over `bytes`, not yet counted.
    #[must_use]
    pub fn new(bytes: Box<str>) -> OwnedText {
        OwnedText {
            bytes,
            char_count: Cell::new(NOT_COUNTED),
        }
    }

    /// The payload's bytes as a `&str`. Owned text is a `Box<str>`, so this
    /// needs no validation and no decoding.
    #[inline]
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.bytes
    }

    /// The number of Unicode scalars in these bytes, counting them the first
    /// time and remembering the answer.
    ///
    /// **Lazy, not computed at construction**, and deliberately against
    /// handover 25 §5 F-8's phrasing: `praxis_get_input`'s buffer is one owned
    /// `Text` that can be tens of megabytes, and a program that reads its input
    /// and never indexes any text would pay a full scan of it for nothing.
    /// Every caller of this is already asking a question whose honest answer is
    /// a scan.
    #[inline]
    fn char_count(&self) -> u64 {
        let cached = self.char_count.get();
        if COUNT_IS_CACHED && cached != NOT_COUNTED {
            return cached;
        }
        let counted = count_scalars(self.bytes.as_bytes());
        if COUNT_IS_CACHED {
            self.char_count.set(counted);
        }
        counted
    }

    /// True iff every scalar in these bytes is one byte wide — that is, iff a
    /// byte index into them is a character index.
    ///
    /// **This is the count, not a second field.** A UTF-8 text has one byte per
    /// scalar exactly when its scalar count equals its byte length, so the
    /// cached count already answers it. A separate `is_ascii` flag would be a
    /// second thing to keep true about the same bytes, and the pair
    /// `(count, flag)` has states — a text that claims 3 scalars in 5 bytes and
    /// claims to be ASCII — that this cannot express.
    ///
    /// The equivalence needs the bytes to be **valid** UTF-8, and that is the
    /// payload's invariant (`text_str`, ADR-111): a leading byte at or above
    /// `0x80` is followed by at least one continuation byte, so "no
    /// continuation bytes" and "every byte below `0x80`" are the same statement
    /// about a valid encoding and different statements about arbitrary bytes.
    #[inline]
    fn is_one_byte_per_scalar(&self) -> bool {
        self.char_count() == self.bytes.len() as u64
    }
}

/// The number of Unicode scalars in `bytes`, which must be valid UTF-8.
///
/// Counting the bytes that are *not* continuation bytes is the same answer as
/// `str::chars().count()` and needs neither a `&str` nor a decode: in UTF-8
/// every scalar contributes exactly one leading byte. The `is_ascii` short
/// circuit is not an optimization of the answer but of the loop — it is the
/// case that holds for essentially all puzzle input, and the standard library's
/// `is_ascii` is word-at-a-time where a filtered count is byte-at-a-time.
#[inline]
fn count_scalars(bytes: &[u8]) -> u64 {
    if bytes.is_ascii() {
        return bytes.len() as u64;
    }
    bytes.iter().filter(|&&b| !is_continuation(b)).count() as u64
}

/// True iff `b` is a UTF-8 continuation byte — `0b10xx_xxxx`.
#[inline]
const fn is_continuation(b: u8) -> bool {
    (b as i8) < -0x40
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
        //
        // **Two byte tests, not a validation of the whole owner** (ADR-115).
        // This used to be `std::str::from_utf8(bytes)` followed by two
        // `str::is_char_boundary` calls, which re-validated every byte of the
        // owner on every slice allocation — so parsing an n-byte input into k
        // captures was O(n·k), a third quadratic nobody had named. The
        // revalidation answered a question that was already settled: since
        // ADR-111 the one door raw host bytes enter through
        // (`praxis_get_input`) validates them, `praxis_alloc_text`'s callers
        // owe UTF-8 as a precondition, and `text_str` states the invariant by
        // `expect`ing it. `is_scalar_boundary` is `str::is_char_boundary`'s
        // test spelled on bytes, and it is the whole of what the two calls did.
        if !is_scalar_boundary(bytes, start) || !is_scalar_boundary(bytes, end) {
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

/// True iff `at` begins a UTF-8 scalar in `bytes`, or is the end of them.
///
/// `str::is_char_boundary` without the `&str`: a byte begins a scalar iff it is
/// not a continuation byte, and the end position always does.
#[inline]
fn is_scalar_boundary(bytes: &[u8], at: usize) -> bool {
    match bytes.get(at) {
        None => at == bytes.len(),
        Some(&b) => !is_continuation(b),
    }
}

impl TextPayload {
    /// An owned payload over `bytes`, not yet counted.
    ///
    /// The variant's field is an [`OwnedText`] whose own field is private, so
    /// this is the only way to build one and there is no way to build one whose
    /// count does not describe its bytes.
    #[must_use]
    pub fn owned(bytes: impl Into<Box<str>>) -> TextPayload {
        TextPayload::Owned(OwnedText::new(bytes.into()))
    }

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
    // **Iterative, not recursive.** This used to recurse through `owner`, so a
    // chain of depth n cost O(n) per read and a long enough one overflowed the
    // stack and aborted the process — inside `extern "C"`, where an abort is
    // the one outcome §10.4 rules out. Nothing about a chain is illegal, so the
    // depth cannot be bounded by validation; the read just must not be
    // recursive. (The parser also stops *building* chains: `Input::new`
    // collapses to the root owner, see `text_root`.)
    let mut payload = payload;
    let mut start = 0usize;
    // The window is the OUTERMOST slice's length: each step inward widens the
    // owner, so only the first `len` describes the text being read.
    let mut len: Option<usize> = None;
    loop {
        // SAFETY: caller guarantees `payload` points at a valid TextPayload and
        // that every `owner` along the chain is a live Text.
        match unsafe { &*payload } {
            TextPayload::Owned(owned) => {
                let bytes = owned.as_str().as_bytes();
                // In range by construction: `SourceSlice::new` is the only
                // constructor and it rejects anything else. There used to be a
                // clamp here — `&owner_bytes[start..]` when the end ran past —
                // which turned a bad range into a *different, plausible* Text,
                // and panicked anyway when `start` itself was out of range
                // (RT-06).
                return match len {
                    None => bytes,
                    Some(len) => &bytes[start..start + len],
                };
            }
            TextPayload::Slice(slice) => {
                start += slice.start;
                if len.is_none() {
                    len = Some(slice.len);
                }
                // SAFETY: the owner is a live Text; the GC is non-moving
                // (ADR-011) and the slice's `trace` keeps the owner reachable.
                payload = slice.owner.payload::<TextPayload>() as *const TextPayload;
            }
        }
    }
}

/// The root **owned** `Text` behind `text`, and the absolute offset at which
/// `text`'s own bytes begin inside it.
///
/// A `SourceSlice` may name another `SourceSlice`, so "the owner" is in general
/// a chain. The parser refuses to extend one: `parse(t, P)` over a `t` that is
/// itself a slice would otherwise allocate slices of a slice, and every `Text`
/// that parse produced would pay the chain's depth on every read. Resolving
/// once, when the [`Input`](crate::parser::cursor::Input) is built, keeps every
/// slice the interpreter allocates exactly one level deep.
///
/// # Safety
/// `text` must be a live `Text` `GcRef`, and every `owner` along its chain must
/// point at a live `Text`.
#[must_use]
pub unsafe fn text_root(text: GcRef) -> (GcRef, usize) {
    let mut root = text;
    let mut base = 0usize;
    loop {
        // SAFETY: caller guarantees the chain is live.
        match unsafe { &*(root.payload::<TextPayload>() as *const TextPayload) } {
            TextPayload::Owned(_) => return (root, base),
            TextPayload::Slice(slice) => {
                base += slice.start;
                root = slice.owner;
            }
        }
    }
}

/// The root **owned** payload behind `payload`, following slice owners.
///
/// This is [`text_root`] over a raw payload pointer rather than a `GcRef`, and
/// it exists for the same reason [`text_bytes`] is iterative: the depth of an
/// owner chain is not bounded by anything (`reading_a_deep_slice_chain_does_not_recurse`),
/// so a read must not recurse through it. The parser separately refuses to
/// build chains at all — `Input::new` collapses to the root owner — so in
/// practice this is one step.
///
/// # Safety
/// See [`text_bytes`]: every `owner` along the chain must point at a live
/// `Text`.
unsafe fn text_owner(payload: *const TextPayload) -> &'static OwnedText {
    let mut payload = payload;
    loop {
        // SAFETY: caller guarantees `payload` points at a valid TextPayload and
        // that every `owner` along the chain is a live Text.
        match unsafe { &*payload } {
            TextPayload::Owned(owned) => return owned,
            TextPayload::Slice(slice) => {
                payload = slice.owner.payload::<TextPayload>() as *const TextPayload;
            }
        }
    }
}

/// The number of Unicode scalars in `payload` — `t.len()`'s answer (§4.3,
/// ADR-086) — in O(1) once the text or its owner has been counted once.
///
/// **Where the count lives is the decision** (ADR-115). An owned text caches
/// its own; a slice has no room for one, and takes the answer from its owner
/// instead: a view of a text whose scalars are all one byte has one scalar per
/// byte, so its length *is* its byte length. When the owner has a multi-byte
/// scalar anywhere the slice has to count its own bytes, which is O(its own
/// length) — the same cost `t[i]` pays on that text either way, so no loop that
/// was quadratic becomes linear by caching it and no loop that is linear
/// becomes quadratic by not.
///
/// # Safety
/// See [`text_bytes`].
#[must_use]
pub unsafe fn text_char_count(payload: *const TextPayload) -> usize {
    // SAFETY: caller guarantees the chain is live.
    match unsafe { &*payload } {
        TextPayload::Owned(owned) => owned.char_count() as usize,
        TextPayload::Slice(_) => {
            // SAFETY: same guarantee.
            let bytes = unsafe { text_bytes(payload) };
            // SAFETY: same guarantee.
            if COUNT_IS_CACHED && unsafe { text_owner(payload) }.is_one_byte_per_scalar() {
                bytes.len()
            } else {
                count_scalars(bytes) as usize
            }
        }
    }
}

/// The bytes of `payload` when a byte index into them is a character index —
/// that is, when every scalar in the text is one byte wide — and `None`
/// otherwise.
///
/// `t[i]` is defined on characters (§4.3, ADR-086); indexing bytes is an
/// optimization that is only valid here. The property is decided by the **root
/// owner's** count rather than by this text's own bytes, and that is what makes
/// it O(1) for a slice: scanning a slice to find out whether it is ASCII costs
/// exactly what decoding it to the index costs, so it would buy nothing,
/// whereas the owner's count is computed once and answers for every view of it
/// forever. `SourceSlice::new` refuses ends that split a scalar, so a slice of
/// a one-byte-per-scalar owner is itself one-byte-per-scalar with no further
/// check.
///
/// # Safety
/// See [`text_bytes`].
#[must_use]
pub unsafe fn text_ascii_bytes(payload: *const TextPayload) -> Option<&'static [u8]> {
    if !COUNT_IS_CACHED {
        return None;
    }
    // SAFETY: caller guarantees the chain is live.
    if unsafe { text_owner(payload) }.is_one_byte_per_scalar() {
        // SAFETY: same guarantee.
        Some(unsafe { text_bytes(payload) })
    } else {
        None
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

/// **The one descriptor callback that reads its sink's style**, and the reason
/// the style exists (§11.4, [`FormatStyle`]).
///
/// `Display` writes the characters: that is what `out(s)` means, what `"{s}"`
/// splices, and what `praxis run` prints for a program whose answer is a string.
///
/// `Debug` writes a quoted literal, because the debugger's displays give a value
/// one line and no other context, and a bare `Text` is ambiguous there in three
/// ways at once. An empty one writes nothing — which the renderer could only
/// report as `<unreadable>`, since "the descriptor wrote no bytes" and "the read
/// failed" were the same observation. One containing a `"` could not be told
/// from two values, and one containing a newline took a row that belonged to the
/// local underneath it.
unsafe fn text_format(payload: *const u8, out: &mut FormatSink<'_>) {
    // SAFETY: caller guarantees `payload` points at a TextPayload.
    let s = unsafe { text_str(payload as *const TextPayload) };
    let _ = match out.style() {
        FormatStyle::Display => out.write_str(s),
        // Through `praxis-syntax`, which owns the escape table this inverts, for
        // F3's reason: a second copy of the rule here would be free to disagree
        // with `decode_escape` about what `\t` is.
        FormatStyle::Debug => out.write_str(&praxis_syntax::literal::quote_text(s)),
    };
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
        TextPayload::Owned(owned) => owned.as_str().len(),
        TextPayload::Slice(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    /// `Text` is the one type whose two renderings differ, and this is the pair
    /// (§11.4, [`FormatStyle`]).
    ///
    /// `Display` must stay byte-for-byte what it was: it is `out(s)`, `"{s}"`
    /// and `praxis run`'s result line, and a quote appearing in any of those is
    /// a change to what programs print. `Debug` is the debugger's, and the empty
    /// string is the case that motivated it — zero bytes out is a value the
    /// renderer could only report as unreadable.
    #[test]
    fn text_renders_one_way_for_the_program_and_another_for_the_debugger() {
        let render = |s: &str, style| {
            let payload = TextPayload::owned(s);
            let mut buf = String::new();
            let mut sink = crate::FormatSink::styled(&mut buf, style);
            // SAFETY: `payload` is an initialized `TextPayload`.
            unsafe { (TEXT.format)(ptr::addr_of!(payload) as *const u8, &mut sink) };
            buf
        };
        use crate::FormatStyle::{Debug, Display};

        assert_eq!(render("hello", Display), "hello");
        assert_eq!(render("hello", Debug), "\"hello\"");

        // The empty string: nothing at all, versus something.
        assert_eq!(render("", Display), "");
        assert_eq!(render("", Debug), "\"\"");

        // And the escaping, so a value cannot end its own quoting or take a
        // second row of a display that allots it one.
        assert_eq!(render("a\"b", Display), "a\"b");
        assert_eq!(render("a\"b", Debug), r#""a\"b""#);
        assert_eq!(render("a\nb", Debug), r#""a\nb""#);
    }

    #[test]
    fn owned_text_descriptor_formats_and_compares() {
        let a = TextPayload::owned("hello");
        let b = TextPayload::owned("hello");
        let c = TextPayload::owned("world");

        let mut buf = String::new();
        unsafe {
            (TEXT.format)(
                ptr::addr_of!(a) as *const u8,
                &mut crate::FormatSink::display(&mut buf),
            )
        };
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
        let apple = TextPayload::owned("apple");
        let banana = TextPayload::owned("banana");
        let apple_again = TextPayload::owned("apple");
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
        let app = TextPayload::owned("app");
        assert_eq!(
            unsafe { cmp(at(&app), at(&apple)) },
            std::cmp::Ordering::Less
        );

        // UTF-8 byte order is code-point order: "é" (U+00E9) follows "z".
        let z = TextPayload::owned("z");
        let e_acute = TextPayload::owned("é");
        assert_eq!(
            unsafe { cmp(at(&z), at(&e_acute)) },
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn owned_text_hash_is_stable() {
        let a = TextPayload::owned("hello");
        let b = TextPayload::owned("hello");
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
        let owner = TextPayload::owned("hello, world");
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

    /// **Reading a `Text` costs no stack, however deep its owner chain is.**
    ///
    /// `text_bytes` used to recurse through `owner`, so a chain of depth n cost
    /// n frames per read and a long enough one overflowed the stack and
    /// aborted the process — inside `extern "C"`, which is the one outcome
    /// §10.4 rules out. A chain is not illegal, so the depth cannot be bounded
    /// by validation; the read simply must not be recursive.
    ///
    /// The thread's stack is deliberately small: on the recursive read this
    /// depth overflows it, and no assertion can catch that, so the test's
    /// passing *is* the assertion. The parser separately refuses to build
    /// chains at all (`Input::new` resolves to the root owner), which is why
    /// this has to be built by hand to be tested.
    #[test]
    fn reading_a_deep_slice_chain_does_not_recurse() {
        const DEPTH: usize = 4_000;
        std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(|| {
                let rt = crate::Runtime::new();
                let mut text = rt.alloc_text("hello world");
                let mut roots = crate::RootScope::new();
                for _ in 0..DEPTH {
                    // Each link is the whole of its owner, so the answer never
                    // changes and only the depth grows.
                    // SAFETY: `text` is the live Text from the previous step.
                    text = unsafe { rt.alloc_text_slice(text, 0, 11) }.expect("the whole owner");
                    roots.root(text);
                }
                assert_eq!(text.as_text(), "hello world");

                // And the root resolution the parser relies on is iterative for
                // the same reason, and lands on the owned text.
                // SAFETY: `text` is live and its chain is live (all rooted).
                let (root, base) = unsafe { text_root(text) };
                assert_eq!(base, 0);
                // SAFETY: `root` is a live Text.
                assert!(
                    unsafe { &*(root.payload::<TextPayload>() as *const TextPayload) }.is_owned(),
                    "text_root resolves to the owned text, not to another slice"
                );
            })
            .expect("spawn")
            .join()
            .expect("a deep chain must be readable without recursing");
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

    // ---- ADR-115: the scalar count ---------------------------------------

    /// The payload of a live `Text`, for the count tests below.
    ///
    /// # Safety
    /// `r` must be a live `Text`.
    unsafe fn payload_of(r: GcRef) -> *const TextPayload {
        r.payload::<TextPayload>() as *const TextPayload
    }

    /// **The count is lazy, and that is the decision** (ADR-115). An owned
    /// `Text` is allocated uncounted; `praxis_get_input`'s buffer is one such
    /// payload and can be tens of megabytes, so counting at construction would
    /// charge a full scan to every program that reads its input and never
    /// indexes a text.
    ///
    /// This and the three tests below observe the cache itself, so they are the
    /// tests that describe arm B rather than the language. Under the
    /// `adr115-arm-a` measurement feature there is no cache to observe by
    /// construction; the tests that state what a `Text` *answers* are not
    /// gated, and they are the ones that must hold in both arms.
    #[cfg(not(feature = "adr115-arm-a"))]
    #[test]
    fn a_text_is_allocated_uncounted_and_counts_itself_once_when_asked() {
        let rt = crate::Runtime::new();
        let text = rt.alloc_text("hello");
        // SAFETY: `text` is the live Text allocated above.
        let payload = unsafe { payload_of(text) };
        // SAFETY: same.
        let TextPayload::Owned(owned) = (unsafe { &*payload }) else {
            panic!("a literal is owned")
        };
        assert_eq!(
            owned.char_count.get(),
            NOT_COUNTED,
            "nothing has asked for the length yet"
        );

        // SAFETY: same.
        assert_eq!(unsafe { text_char_count(payload) }, 5);
        assert_eq!(
            owned.char_count.get(),
            5,
            "the first ask is what pays for the scan"
        );

        // And the second ask reads the cell rather than the bytes. Poisoning
        // the cell is how the read is observed: a recount would answer 5.
        owned.char_count.set(99);
        // SAFETY: same.
        assert_eq!(unsafe { text_char_count(payload) }, 99);
    }

    /// The count is the whole ASCII test. `char_count == bytes.len()` iff every
    /// scalar is one byte, so there is no second flag to keep true — and the
    /// byte-indexing licence follows from the length answer rather than sitting
    /// beside it.
    #[cfg(not(feature = "adr115-arm-a"))]
    #[test]
    fn the_count_equals_the_byte_length_exactly_when_every_scalar_is_one_byte() {
        let rt = crate::Runtime::new();
        for (src, chars, one_byte) in [
            ("", 0usize, true),
            ("hello", 5, true),
            ("héllo", 5, false),
            ("é", 1, false),
            ("aéb", 3, false),
            // A four-byte scalar, and a three-byte one.
            ("a\u{1F600}b", 3, false),
            ("\u{20AC}", 1, false),
            // Every ASCII byte, including the ones a naive `is_ascii` on
            // `char` boundaries would still accept.
            ("\u{0}\u{7f}", 2, true),
        ] {
            let text = rt.alloc_text(src);
            // SAFETY: `text` is live.
            let payload = unsafe { payload_of(text) };
            // SAFETY: same.
            assert_eq!(unsafe { text_char_count(payload) }, chars, "{src:?}");
            assert_eq!(chars == src.len(), one_byte, "{src:?}");
            // SAFETY: same.
            assert_eq!(
                unsafe { text_ascii_bytes(payload) }.is_some(),
                one_byte,
                "{src:?} must {} take the byte-index path",
                if one_byte { "" } else { "not" }
            );
            assert_eq!(chars, src.chars().count(), "{src:?}");
        }
    }

    /// **A slice takes the licence from its owner, and that is why the package
    /// works at all** (ADR-115). The `Text`s a program indexes are mostly the
    /// parser's captures, which are `Slice`s of the input buffer — putting the
    /// count only where the `Owned` variant's spare bytes are would have given
    /// the case the decision exists for nothing.
    #[cfg(not(feature = "adr115-arm-a"))]
    #[test]
    fn a_slice_of_a_one_byte_owner_answers_its_length_from_its_byte_length() {
        let rt = crate::Runtime::new();
        let owner = rt.alloc_text("abcdefghij");
        // SAFETY: `owner` is live.
        let slice = unsafe { rt.alloc_text_slice(owner, 3, 4) }.expect("[3, 7) is in range");
        // SAFETY: both are live.
        let (op, sp) = unsafe { (payload_of(owner), payload_of(slice)) };

        // SAFETY: live.
        assert_eq!(unsafe { text_char_count(sp) }, 4);
        // SAFETY: live.
        assert_eq!(unsafe { text_ascii_bytes(sp) }, Some(&b"defg"[..]));

        // The scan that answered it landed on the **owner**, so every other
        // view of the same buffer is now free.
        // SAFETY: live.
        let TextPayload::Owned(owned) = (unsafe { &*op }) else {
            panic!("the owner is owned")
        };
        assert_eq!(owned.char_count.get(), 10);
    }

    /// A slice of a multi-byte owner has no cached count of its own — there are
    /// no bytes left in the payload to put one in — so it counts its own bytes.
    /// The answer must still be scalars, and the byte-index path must stay
    /// shut even when the slice's *own* bytes happen to be one-byte, because
    /// deciding that per slice costs exactly what decoding it costs.
    #[test]
    fn a_slice_of_a_multi_byte_owner_still_answers_in_scalars() {
        let rt = crate::Runtime::new();
        // "héllo wörld" — 'é' at bytes 1..3 and 'ö' at bytes 8..10.
        let owner = rt.alloc_text("héllo wörld");
        assert_eq!(owner.as_text().len(), 13);

        // A view that contains the multi-byte scalar.
        // SAFETY: `owner` is live.
        let with = unsafe { rt.alloc_text_slice(owner, 0, 3) }.expect("[0, 3) is 'hé'");
        assert_eq!(with.as_text(), "hé");
        // SAFETY: live.
        assert_eq!(unsafe { text_char_count(payload_of(with)) }, 2);
        // SAFETY: live.
        assert!(unsafe { text_ascii_bytes(payload_of(with)) }.is_none());

        // A view that does not — the answer is the same either way, and the
        // fast path is still refused because the owner is what carries the
        // licence.
        // SAFETY: live.
        let without = unsafe { rt.alloc_text_slice(owner, 3, 4) }.expect("[3, 7) is 'llo '");
        assert_eq!(without.as_text(), "llo ");
        // SAFETY: live.
        assert_eq!(unsafe { text_char_count(payload_of(without)) }, 4);
        // SAFETY: live.
        assert!(unsafe { text_ascii_bytes(payload_of(without)) }.is_none());

        // An empty view at a scalar boundary.
        // SAFETY: live.
        let empty = unsafe { rt.alloc_text_slice(owner, 3, 0) }.expect("an empty view");
        // SAFETY: live.
        assert_eq!(unsafe { text_char_count(payload_of(empty)) }, 0);
    }

    /// `Text + Text` allocates a fresh owned payload (ADR-085), so the
    /// concatenation is uncounted and counts *its own* bytes. A count inherited
    /// from either operand would be a wrong program rather than a slow one.
    #[test]
    fn a_concatenation_counts_its_own_bytes_and_not_an_operands() {
        let rt = crate::Runtime::new();
        let left = rt.alloc_text("ab");
        let right = rt.alloc_text("é");
        // SAFETY: both live.
        unsafe {
            assert_eq!(text_char_count(payload_of(left)), 2);
            assert_eq!(text_char_count(payload_of(right)), 1);
        }

        let joined = rt.alloc_text(&format!("{}{}", left.as_text(), right.as_text()));
        // SAFETY: live.
        assert_eq!(unsafe { text_char_count(payload_of(joined)) }, 3);
        // SAFETY: live.
        assert!(
            unsafe { text_ascii_bytes(payload_of(joined)) }.is_none(),
            "an ASCII text joined to a multi-byte one is not byte-indexable"
        );
    }

    /// A slice taken from an owner that has **already** been counted reads the
    /// same answer as one taken before. There is nothing to invalidate — `Text`
    /// is immutable and a view cannot change what it views — but the pair is
    /// what makes that a tested property rather than an argued one.
    #[test]
    fn a_slice_reads_the_same_whether_its_owner_was_counted_before_or_after() {
        let rt = crate::Runtime::new();
        let owner = rt.alloc_text("wxyz");
        // SAFETY: live.
        let early = unsafe { rt.alloc_text_slice(owner, 1, 2) }.expect("[1, 3)");
        // SAFETY: live.
        assert_eq!(unsafe { text_char_count(payload_of(early)) }, 2);
        // SAFETY: live.
        let late = unsafe { rt.alloc_text_slice(owner, 1, 2) }.expect("[1, 3)");
        // SAFETY: live.
        assert_eq!(unsafe { text_char_count(payload_of(late)) }, 2);
        assert_eq!(early.as_text(), late.as_text());
    }

    /// The count and the byte view follow an owner chain **iteratively**, for
    /// the reason `reading_a_deep_slice_chain_does_not_recurse` gives: the
    /// depth is not bounded by anything, and this thread's stack is small
    /// enough that a recursive walk would overflow it. The test passing is the
    /// assertion.
    #[cfg(not(feature = "adr115-arm-a"))]
    #[test]
    fn counting_a_deep_slice_chain_does_not_recurse() {
        const DEPTH: usize = 4_000;
        std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(|| {
                let rt = crate::Runtime::new();
                let mut text = rt.alloc_text("hello world");
                let mut roots = crate::RootScope::new();
                for _ in 0..DEPTH {
                    // SAFETY: `text` is the live Text from the previous step.
                    text = unsafe { rt.alloc_text_slice(text, 0, 11) }.expect("the whole owner");
                    roots.root(text);
                }
                // SAFETY: `text` and its whole chain are live and rooted.
                unsafe {
                    assert_eq!(text_char_count(payload_of(text)), 11);
                    assert_eq!(
                        text_ascii_bytes(payload_of(text)),
                        Some(&b"hello world"[..])
                    );
                }
            })
            .expect("spawn")
            .join()
            .expect("a deep chain must be countable without recursing");
    }

    /// **Allocating a view is O(1), not O(the owner).** `SourceSlice::new` used
    /// to call `std::str::from_utf8` on the owner's whole byte range so it could
    /// ask `str::is_char_boundary` twice, which made parsing an n-byte input
    /// into k captures O(n·k) — a third quadratic, and the one handover 25's
    /// F-8 did not name. Since ADR-111 the owner's bytes are UTF-8 by a
    /// precondition checked at the one door raw bytes enter, so the boundary
    /// test is two byte comparisons.
    ///
    /// The sizes are chosen so the old code cannot finish this test: 8000 views
    /// of a 256 KiB owner is two billion byte validations. There is no
    /// assertion to make about that beyond the test returning.
    #[test]
    fn taking_a_view_does_not_walk_the_owner() {
        const OWNER_BYTES: usize = 256 * 1024;
        const VIEWS: usize = 8_000;
        let rt = crate::Runtime::new();
        let owner = rt.alloc_text(&"x".repeat(OWNER_BYTES));
        let mut roots = crate::RootScope::new();
        roots.root(owner);
        for i in 0..VIEWS {
            // SAFETY: `owner` is live and rooted for the whole loop.
            let view = unsafe { rt.alloc_text_slice(owner, i, 4) }.expect("in range");
            roots.root(view);
            assert_eq!(view.as_text(), "xxxx");
        }
    }

    #[test]
    fn hash_value_helper_compiles() {
        let mut h = crate::descriptor::StructHasher::new();
        hash_value(&mut h, &"x");
    }
}
