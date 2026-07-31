//! The parser interpreter's one position representation (F14, IPR-01, IPR-03).
//!
//! Before this module the interpreter had two: `walk` returned
//! `Result<(GcRef, usize), ParseFail>` whose `usize` was documented as "bytes
//! consumed" and was produced as `bytes.len() - offset` by twelve helpers while
//! four callers assigned it to an absolute cursor. Nesting at a non-zero offset
//! therefore moved the cursor backwards. Separately, five sites re-sliced a
//! sub-buffer and walked it at offset 0 while source-slice `Text`s were still
//! allocated against the *whole* input, so a word in section 2 named bytes at
//! the start of the file.
//!
//! Both defects are the same defect: a bare `usize` cannot say whether it is an
//! absolute position, a length, or a position in some other buffer. The types
//! here make the wrong answers unwritable rather than merely fixed:
//!
//! * [`Cursor`] is an absolute offset into one [`Input`]. It has no `usize`
//!   constructor, so `bytes.len() - offset` cannot become one; the only mints
//!   are [`Input::whole`] and [`Cursor::advance`], both of which start from a
//!   position that is already absolute.
//! * [`Input`] carries the `GcRef` its bytes belong to, so
//!   `alloc_text_slice(input.owner(), …)` is right by construction and there is
//!   no second opinion to disagree with (the deleted `rt_owner` read
//!   `ctx.input_source`, which is the wrong buffer entirely for `parse(t, P)`).
//! * [`ByteRegion`] is a pair of cursors whose only operation is
//!   [narrowing](ByteRegion::subregion). A child parser is handed a *narrower
//!   region of the same buffer*, never a fresh buffer starting at zero, so its
//!   offsets are already absolute and nothing has to be rebased.
//!
//! [`Input`] holds a validated `&str`, which is what retires the three
//! `str::from_utf8(..).unwrap_or("")` calls the interpreter used to make: a
//! region that is not UTF-8 is now impossible rather than silently empty.

// The substrate lands before the interpreter that consumes it, because the
// conversion of `walk` and its seventeen helpers has to be one commit (IPR-01:
// a half-converted interpreter mixes absolute and relative cursors with no type
// telling them apart, which is the current bug and harder to see). The very
// next commit adopts every item here and this allowance goes with it.

use crate::text::{text_bytes, TextPayload};
use crate::GcRef;

/// An absolute byte offset into one [`Input`].
///
/// There is deliberately no `Cursor::new(usize)`. Every cursor descends from
/// [`Input::whole`] by [`advance`](Cursor::advance), so a relative length can
/// only become a position by being added to one that is already absolute —
/// which is what the twelve `bytes.len() - offset` returns were not doing.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Cursor(usize);

impl Cursor {
    /// This position as a byte offset into the input buffer.
    #[inline]
    pub(crate) fn offset(self) -> usize {
        self.0
    }

    /// The position `n` bytes later. Saturating, so arithmetic on a bad length
    /// cannot wrap into a huge offset; every consumer bounds the result against
    /// a region anyway.
    #[inline]
    pub(crate) fn advance(self, n: usize) -> Cursor {
        Cursor(self.0.saturating_add(n))
    }

    /// The number of bytes between `earlier` and this position.
    ///
    /// Saturating rather than panicking: this is the length handed to
    /// `SourceSlice::new`, which validates it again, so a wrong answer here is
    /// a parse fault rather than an abort across the ABI (§10.4).
    #[inline]
    pub(crate) fn delta_from(self, earlier: Cursor) -> usize {
        self.0.saturating_sub(earlier.0)
    }
}

/// The buffer a parse runs against, together with the `GcRef` that owns it.
///
/// The owner travels with the bytes on purpose. `run_plan` receives the input
/// as a `GcRef` argument, and for `parse(text, P)` that reference is *not*
/// `ctx.input_source`; reading the owner from the context, as the deleted
/// `rt_owner` did, produced `Text` values that named bytes of the stdin buffer
/// (IPR-03).
#[derive(Clone, Copy)]
pub(crate) struct Input<'a> {
    owner: GcRef,
    text: &'a str,
}

impl<'a> Input<'a> {
    /// Read `owner`'s payload as the buffer to parse, or `None` if it is not
    /// UTF-8.
    ///
    /// A `Text` is UTF-8 by construction (RT-06 validates every `SourceSlice`),
    /// so `None` means the caller handed us something that is not a `Text`. It
    /// is still a `None` and not an `expect`: this runs inside `extern "C"` and
    /// a panic there is undefined behaviour (§10.4, D12).
    ///
    /// # Safety
    /// `owner` must be a live `Text` `GcRef`.
    pub(crate) unsafe fn new(owner: GcRef) -> Option<Input<'a>> {
        // SAFETY: caller guarantees `owner` is a live Text.
        let bytes = unsafe { text_bytes(owner.payload::<TextPayload>() as *const TextPayload) };
        let text = std::str::from_utf8(bytes).ok()?;
        Some(Input { owner, text })
    }

    /// The reference every source-slice `Text` this parse produces is a view of.
    #[inline]
    pub(crate) fn owner(&self) -> GcRef {
        self.owner
    }

    /// The whole buffer as one region — the root of every narrowing chain, and
    /// the only place a [`Cursor`] is minted from nothing.
    #[inline]
    pub(crate) fn whole(&self) -> ByteRegion {
        ByteRegion {
            start: Cursor(0),
            end: Cursor(self.text.len()),
        }
    }

    /// The whole buffer's text. Regions read through this, never around it.
    #[inline]
    fn text(&self) -> &'a str {
        self.text
    }
}

/// A half-open `[start, end)` window on an [`Input`], in absolute cursors.
///
/// The only way to make a new one from an existing one is
/// [`subregion`](ByteRegion::subregion), which cannot widen. That single
/// restriction is what closes IPR-02, IPR-04, IPR-05 and IPR-06 structurally: a
/// child parser handed `region.subregion(token_start, token_end)` *cannot* read
/// past its token, however the child is written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ByteRegion {
    start: Cursor,
    end: Cursor,
}

impl ByteRegion {
    /// Where this region begins.
    #[inline]
    pub(crate) fn start(self) -> Cursor {
        self.start
    }

    /// One past where this region ends.
    #[inline]
    pub(crate) fn end(self) -> Cursor {
        self.end
    }

    /// The region's length in bytes.
    #[inline]
    pub(crate) fn len(self) -> usize {
        self.end.delta_from(self.start)
    }

    /// True when the region spans no bytes.
    #[inline]
    pub(crate) fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// The bytes this region spans.
    #[inline]
    pub(crate) fn bytes<'a>(self, i: &Input<'a>) -> &'a [u8] {
        i.text()
            .as_bytes()
            .get(self.start.offset()..self.end.offset())
            .unwrap_or(&[])
    }

    /// The text this region spans, or `None` if its ends are not scalar
    /// boundaries.
    ///
    /// Fallible rather than lossy: the predecessor wrote
    /// `str::from_utf8(region).unwrap_or("")` in three places, which turned a
    /// mis-computed region into a silently empty one — a zero-row `Grid`
    /// instead of a parse failure (IPR-05).
    #[inline]
    pub(crate) fn str<'a>(self, i: &Input<'a>) -> Option<&'a str> {
        i.text().get(self.start.offset()..self.end.offset())
    }

    /// A narrower window on the same buffer.
    ///
    /// Debug builds assert containment, because a widening `subregion` is
    /// always a bug in the interpreter rather than a property of the input.
    /// Release builds clamp, so the invariant "a child never sees more than its
    /// parent" holds unconditionally and a slip is a parse failure instead of a
    /// read past the end.
    #[inline]
    pub(crate) fn subregion(self, start: Cursor, end: Cursor) -> ByteRegion {
        debug_assert!(
            self.start <= start && start <= end && end <= self.end,
            "a subregion can only narrow: {:?}..{:?} is not inside {:?}..{:?}",
            start,
            end,
            self.start,
            self.end
        );
        let start = start.max(self.start).min(self.end);
        let end = end.max(start).min(self.end);
        ByteRegion { start, end }
    }

    /// The tail of this region from `at`.
    #[inline]
    pub(crate) fn from(self, at: Cursor) -> ByteRegion {
        self.subregion(at, self.end)
    }

    /// The position one Unicode scalar past `at`, or `None` at the region's end.
    ///
    /// The interpreter used to step cells and scan positions one **byte** at a
    /// time, so a non-ASCII row was both the wrong width and matched at
    /// continuation bytes (IPR-06, IPR-08). Stepping is a scalar operation
    /// because §7.5's grid is a grid of characters.
    /// The number of Unicode scalars in this region, or `None` if its ends are
    /// not scalar boundaries.
    ///
    /// This is a `grid`'s width. It used to be the region's *byte* length, so
    /// a row containing one `é` was two columns wide and was parsed twice —
    /// once at the scalar and once at its continuation byte (IPR-06).
    pub(crate) fn scalar_count(self, i: &Input<'_>) -> Option<usize> {
        Some(self.str(i)?.chars().count())
    }

    /// The position one Unicode scalar past `at`, or `None` at the region's end.
    pub(crate) fn next_scalar(self, i: &Input<'_>, at: Cursor) -> Option<Cursor> {
        if at >= self.end {
            return None;
        }
        let rest = i.text().get(at.offset()..self.end.offset())?;
        let ch = rest.chars().next()?;
        Some(at.advance(ch.len_utf8()))
    }
}

/// A value and the absolute position parsing stopped at.
///
/// This replaces `(GcRef, usize)`, whose second element four callers read as a
/// position and twelve producers wrote as a length (IPR-01). `next` is a
/// [`Cursor`], so neither reading is available: it is a position, and the only
/// positions in existence descend from the region being walked.
#[derive(Clone, Copy)]
pub(crate) struct Walked {
    pub(crate) value: GcRef,
    pub(crate) next: Cursor,
}

/// Split `region` into logical lines, stripping a trailing `\r` and excluding
/// the `\n`. A trailing newline does not produce a final empty line.
///
/// Absolute by construction: each returned region is a narrowing of `region`,
/// so a line's `Text` names the bytes the line actually occupies.
pub(crate) fn split_lines(i: &Input<'_>, region: ByteRegion) -> Vec<ByteRegion> {
    let bytes = region.bytes(i);
    let base = region.start();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let start = pos;
        while pos < bytes.len() && bytes[pos] != b'\n' {
            pos += 1;
        }
        let mut end = pos;
        if end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        if pos < bytes.len() {
            pos += 1; // consume the `\n`
        }
        out.push(region.subregion(base.advance(start), base.advance(end)));
    }
    out
}

/// Split `region` on blank lines into sections, **excluding** each section's
/// trailing line ending.
///
/// The predecessor returned `"first\n"` for the first section of
/// `"first\n\nsecond"`. That was invisible while a section's child parser was
/// walked against the whole buffer and its cursor discarded; once a section's
/// child must consume its region exactly (`walk_exact`, IPR-02), a trailing
/// newline no `word` can eat would fail every `sections(word)`. A section is
/// the span from its first non-blank line's start to its last non-blank line's
/// content end, which is also what makes [`split_lines`] of a section agree
/// with [`split_lines`] of the whole input.
pub(crate) fn split_sections(i: &Input<'_>, region: ByteRegion) -> Vec<ByteRegion> {
    let mut out = Vec::new();
    let mut current: Option<(Cursor, Cursor)> = None;
    for line in split_lines(i, region) {
        if line.is_empty() {
            if let Some((start, end)) = current.take() {
                out.push(region.subregion(start, end));
            }
        } else {
            current = Some(match current {
                Some((start, _)) => (start, line.end()),
                None => (line.start(), line.end()),
            });
        }
    }
    if let Some((start, end)) = current {
        out.push(region.subregion(start, end));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an `Input` over `text`, keeping the `Runtime` alive alongside it.
    fn input_over(text: &str) -> (crate::Runtime, GcRef) {
        let rt = crate::Runtime::new();
        let owner = rt.alloc_text(text);
        (rt, owner)
    }

    #[test]
    fn a_subregion_can_only_narrow() {
        // The property in the positive direction: narrowing works, and the
        // narrowed region reads the bytes it names.
        let (_rt, owner) = input_over("hello world");
        let i = unsafe { Input::new(owner) }.expect("a Text is UTF-8");
        let whole = i.whole();
        let inner = whole.subregion(whole.start().advance(6), whole.end());
        assert_eq!(inner.str(&i), Some("world"));
        let innermost = inner.subregion(inner.start(), inner.start().advance(3));
        assert_eq!(innermost.str(&i), Some("wor"));
    }

    #[test]
    #[should_panic(expected = "a subregion can only narrow")]
    fn a_subregion_that_would_widen_is_a_bug_and_says_so() {
        let (_rt, owner) = input_over("hello world");
        let i = unsafe { Input::new(owner) }.expect("a Text is UTF-8");
        let whole = i.whole();
        let inner = whole.subregion(whole.start().advance(6), whole.end());
        // Reaching back before the parent's start is the shape every one of the
        // five re-slice sites had: a child looking at bytes its parent excluded.
        let _ = inner.subregion(whole.start(), inner.end());
    }

    #[test]
    fn next_scalar_steps_one_unicode_scalar() {
        let (_rt, owner) = input_over("aéb");
        let i = unsafe { Input::new(owner) }.expect("a Text is UTF-8");
        let r = i.whole();
        let mut at = r.start();
        let mut seen = Vec::new();
        while let Some(next) = r.next_scalar(&i, at) {
            seen.push(r.subregion(at, next).str(&i).expect("a scalar is a str"));
            at = next;
        }
        assert_eq!(seen, vec!["a", "é", "b"], "three scalars, not four bytes");
        assert_eq!(at, r.end());
    }

    #[test]
    fn a_section_region_excludes_its_trailing_line_ending() {
        let (_rt, owner) = input_over("first\n\nsecond");
        let i = unsafe { Input::new(owner) }.expect("a Text is UTF-8");
        let sections: Vec<&str> = split_sections(&i, i.whole())
            .into_iter()
            .map(|s| s.str(&i).expect("a section is a str"))
            .collect();
        assert_eq!(
            sections,
            vec!["first", "second"],
            "a section that keeps its own newline cannot be consumed exactly by `word`"
        );
    }

    #[test]
    fn split_lines_and_split_sections_agree_on_a_single_line_region() {
        let (_rt, owner) = input_over("only\n");
        let i = unsafe { Input::new(owner) }.expect("a Text is UTF-8");
        let lines = split_lines(&i, i.whole());
        let sections = split_sections(&i, i.whole());
        assert_eq!(lines.len(), 1);
        assert_eq!(sections.len(), 1);
        assert_eq!(lines[0], sections[0]);
        assert_eq!(lines[0].str(&i), Some("only"));
    }

    #[test]
    fn split_lines_strips_crlf_and_drops_a_trailing_empty_line() {
        let (_rt, owner) = input_over("abc\r\ndef\nghi");
        let i = unsafe { Input::new(owner) }.expect("a Text is UTF-8");
        let lines: Vec<&str> = split_lines(&i, i.whole())
            .into_iter()
            .map(|l| l.str(&i).expect("a line is a str"))
            .collect();
        assert_eq!(lines, vec!["abc", "def", "ghi"]);

        let (_rt2, owner2) = input_over("a\nb\n");
        let i2 = unsafe { Input::new(owner2) }.expect("a Text is UTF-8");
        let lines2: Vec<&str> = split_lines(&i2, i2.whole())
            .into_iter()
            .map(|l| l.str(&i2).expect("a line is a str"))
            .collect();
        assert_eq!(lines2, vec!["a", "b"]);
    }

    #[test]
    fn a_line_region_of_a_section_names_the_inputs_own_bytes() {
        // The IPR-03 property at the substrate level: nothing is re-based, so a
        // second section's line starts where it really starts.
        let (_rt, owner) = input_over("alpha\n\nbeta\n");
        let i = unsafe { Input::new(owner) }.expect("a Text is UTF-8");
        let sections = split_sections(&i, i.whole());
        assert_eq!(sections.len(), 2);
        let second = split_lines(&i, sections[1]);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].start().offset(), 7, "\"beta\" begins at byte 7");
        assert_eq!(second[0].str(&i), Some("beta"));
    }
}
