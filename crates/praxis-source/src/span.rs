//! Source spans.
//!
//! A [`Span`] is a half-open byte range `[start, end)` within a single source
//! file. It is stored as a start offset plus a **length**, never as two
//! independent offsets, so an inverted span (`end < start`) is literally
//! unrepresentable.

use std::fmt;

/// A byte offset into a source file.
///
/// This is a `u32`, not a `usize`: source files are capped at 4 GiB, and a
/// 32-bit offset halves the storage cost of every span.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BytePos(pub u32);

impl BytePos {
    /// The zero offset — the start of every file.
    pub const ZERO: BytePos = BytePos(0);

    #[inline]
    pub const fn to_u32(self) -> u32 {
        self.0
    }

    #[inline]
    pub const fn to_usize(self) -> usize {
        self.0 as usize
    }

    /// Saturating addition. Offsets never overflow; they clamp at `u32::MAX`.
    #[inline]
    pub const fn saturating_add(self, bytes: u32) -> BytePos {
        BytePos(self.0.saturating_add(bytes))
    }

    /// Difference between two offsets, clamped at zero so subtraction can never
    /// underflow or produce a negative span.
    #[inline]
    pub const fn saturating_sub(self, other: BytePos) -> u32 {
        self.0.saturating_sub(other.0)
    }
}

impl fmt::Debug for BytePos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BytePos({})", self.0)
    }
}

impl From<u32> for BytePos {
    fn from(value: u32) -> Self {
        BytePos(value)
    }
}

/// A half-open byte range `[start, end)` within one file.
///
/// Internally stored as `start` plus `len`, so the only constructor
/// [`Span::new`] can reject inversion at construction time and the invariant is
/// preserved by construction thereafter. An empty span (`len == 0`) is valid and
/// marks a position rather than a range.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    start: BytePos,
    len: u32,
}

impl Span {
    /// The only public constructor. `end` must be `>= start`.
    ///
    /// Panics (debug only) if `end < start`. In release builds an inverted range
    /// collapses to an empty span at `start`, so the type invariant is never
    /// violated even if a caller bypasses the debug assertion.
    #[track_caller]
    pub fn new(start: impl Into<BytePos>, end: impl Into<BytePos>) -> Span {
        let start = start.into();
        let end = end.into();
        debug_assert!(
            start <= end,
            "Span::new called with inverted range {start:?}..{end:?}"
        );
        // `end.saturating_sub(start)` is a plain u32 that can never underflow;
        // in the inverted case (release builds only, since debug would have
        // panicked above) it clamps to 0, preserving the type invariant.
        let len = end.saturating_sub(start);
        Span { start, len }
    }

    /// An empty span anchored at a single offset. Useful for "at this position".
    #[inline]
    pub fn at(pos: impl Into<BytePos>) -> Span {
        let pos = pos.into();
        Span { start: pos, len: 0 }
    }

    /// An empty span at the very start of a file. A sensible default.
    pub const EMPTY: Span = Span {
        start: BytePos::ZERO,
        len: 0,
    };

    #[inline]
    pub const fn start(self) -> BytePos {
        self.start
    }

    #[inline]
    pub const fn end(self) -> BytePos {
        BytePos(self.start.0.saturating_add(self.len))
    }

    #[inline]
    pub const fn len(self) -> u32 {
        self.len
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// True if `pos` lies within `[start, end)`.
    #[inline]
    pub fn contains(self, pos: BytePos) -> bool {
        pos >= self.start && pos < self.end()
    }

    /// The smallest span covering both `self` and `other`. If the two spans are
    /// in different files the caller must use [`FileSpan::union`] instead.
    ///
    /// An empty span acts as a neutral element: unioning with one returns the
    /// other span unchanged.
    pub fn cover(self, other: Span) -> Span {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let start = self.start.min(other.start);
        let end = self.end().max(other.end());
        Span::new(start, end)
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}..{} (len {})",
            self.start.to_u32(),
            self.end().to_u32(),
            self.len
        )
    }
}

/// A [`Span`] together with the file it belongs to.
///
/// Combining the file into the type means there is no such thing as an "orphan"
/// span whose file has to be guessed from context. Operations that combine two
/// spans (`FileSpan::union`) require them to share a file, enforced at runtime.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileSpan {
    pub file: FileId,
    pub span: Span,
}

impl FileSpan {
    #[inline]
    pub fn new(file: FileId, span: Span) -> FileSpan {
        FileSpan { file, span }
    }

    /// The smallest span covering both `self` and `other`.
    ///
    /// Returns `None` if the two spans belong to different files — there is no
    /// well-defined union across files, and `None` forces the caller to handle
    /// that case rather than silently picking one.
    pub fn union(self, other: FileSpan) -> Option<FileSpan> {
        if self.file != other.file {
            return None;
        }
        Some(FileSpan::new(self.file, self.span.cover(other.span)))
    }
}

impl fmt::Debug for FileSpan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}@{:?}", self.span, self.file)
    }
}

// FileSpan needs FileId, which is declared in `file.rs`. The `file` module is a
// sibling, so the reference above resolves once `file.rs` is compiled.
use crate::file::FileId;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_round_trip_and_endpoints() {
        let s = Span::new(10, 25);
        assert_eq!(s.start(), BytePos(10));
        assert_eq!(s.end(), BytePos(25));
        assert_eq!(s.len(), 15);
        assert!(!s.is_empty());

        let empty = Span::new(7, 7);
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn at_and_empty_constants() {
        assert_eq!(Span::at(5), Span::new(5, 5));
        assert!(Span::EMPTY.is_empty());
        assert_eq!(Span::EMPTY.start(), BytePos::ZERO);
    }

    #[test]
    fn contains_respects_half_open_semantics() {
        let s = Span::new(10, 20);
        assert!(!s.contains(9.into()));
        assert!(s.contains(10.into()));
        assert!(s.contains(19.into()));
        assert!(!s.contains(20.into())); // half-open: end excluded
    }

    #[test]
    fn cover_smallest_enclosing() {
        let a = Span::new(10, 20);
        let b = Span::new(15, 30);
        assert_eq!(a.cover(b), Span::new(10, 30));

        let c = Span::new(100, 110);
        assert_eq!(a.cover(c), Span::new(10, 110));
    }

    #[test]
    fn cover_empty_is_neutral() {
        let a = Span::new(10, 20);
        assert_eq!(a.cover(Span::EMPTY), a);
        assert_eq!(Span::EMPTY.cover(a), a);
        assert_eq!(Span::EMPTY.cover(Span::EMPTY), Span::EMPTY);
    }

    #[test]
    fn bytepos_saturating_arithmetic() {
        assert_eq!(BytePos(5).saturating_add(10), BytePos(15));
        assert_eq!(BytePos(u32::MAX).saturating_add(1), BytePos(u32::MAX));
        assert_eq!(BytePos(10).saturating_sub(BytePos(3)), 7);
        assert_eq!(BytePos(3).saturating_sub(BytePos(10)), 0); // clamped
    }

    #[test]
    #[should_panic(expected = "inverted range")]
    fn inverted_span_is_rejected_in_debug() {
        // "Make illegal states unrepresentable" (AGENTS.md): `Span::new` stores
        // `start + len`, so an inverted span cannot be constructed. The
        // `debug_assert!` catches the bug in test/debug builds (the default test
        // profile); release builds clamp to an empty span so the invariant still
        // holds even if a caller bypasses the assertion.
        let _ = Span::new(25, 10);
    }
}
