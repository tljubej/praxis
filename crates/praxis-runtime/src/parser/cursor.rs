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
//!
//! # Trailing whitespace belongs to nobody, and that is not an error
//!
//! Real input is full of whitespace no parser was asked to read: the file's own
//! terminator, a blank final line, a stray space before a newline. **A run of
//! whitespace that no parser could read is not data and not a mismatch.** The
//! rule is stated once and inherited; it is not a byte count, and it is not a
//! special case at the root. It has exactly two halves, because a region has
//! two ends of the question:
//!
//! * **Extent** — decided before any parser runs, so it cannot depend on one.
//!   *A region does not end in blank lines.* [`split_lines`] drops the trailing
//!   run of lines that hold nothing but whitespace, which is the general form of
//!   the rule it always had for one terminator.
//! * **Bound** — decided by the parser that was asked. *A child that leaves only
//!   whitespace has filled its bound.* `walk_exact`, `walk_characters` and
//!   `walk_grid_row` in the parent module share one predicate,
//!   [`ByteRegion::is_all_whitespace`]. Whether a trailing run is data is the
//!   child's answer: `int` cannot read `"1 "`'s space, so it is padding, while
//!   `char` reads it as a cell, so `grid(char)` over `"ab\ncd \n"` is a *ragged
//!   grid* — an error about the data, not about a file convention.
//!
//! Together they leave the file's terminator to nobody, so **the root region is
//! the whole buffer** and there is no root special case to get the count wrong
//! in. Two earlier attempts made one: the first applied the bound rule at the
//! root with the terminator inside it, the second trimmed exactly one
//! terminator off the buffer — and a file ending `"\n\n"` reproduced the first
//! verbatim. A trim count is the wrong kind of answer. `read <parser>` and
//! `parse(text, P)` reach this module through one function over one buffer, so
//! `parse(t, rest)` is the identity on `t` again (ADR-078).

// The substrate lands before the interpreter that consumes it, because the
// conversion of `walk` and its seventeen helpers has to be one commit (IPR-01:
// a half-converted interpreter mixes absolute and relative cursors with no type
// telling them apart, which is the current bug and harder to see). The very
// next commit adopts every item here and this allowance goes with it.

use crate::text::{text_bytes, text_root, TextPayload};
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
    /// The **root owned** `Text`. Never a slice: see [`Input::new`].
    owner: GcRef,
    /// Where this input's bytes begin inside `owner`.
    base: usize,
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
    /// **The owner chain is collapsed here, once.** `parse(t, P)` takes its
    /// owner from the argument, and that argument may itself be a slice — of a
    /// slice, of a slice. Naming it directly would make every `Text` the parse
    /// produced one link longer than the last, and `text_bytes` walks the chain
    /// on every read: `t = parse(t, rest)` in a loop went quadratic and then
    /// overflowed the stack. Resolving to the root owned `Text` and carrying
    /// the base offset keeps every slice the interpreter allocates exactly one
    /// level deep, whatever it was handed.
    ///
    /// # Safety
    /// `owner` must be a live `Text` `GcRef`.
    pub(crate) unsafe fn new(owner: GcRef) -> Option<Input<'a>> {
        // SAFETY: caller guarantees `owner` is a live Text.
        let bytes = unsafe { text_bytes(owner.payload::<TextPayload>() as *const TextPayload) };
        let text = std::str::from_utf8(bytes).ok()?;
        // SAFETY: same guarantee; `text_root` only walks `owner` links.
        let (root, base) = unsafe { text_root(owner) };
        Some(Input {
            owner: root,
            base,
            text,
        })
    }

    /// The reference every source-slice `Text` this parse produces is a view of
    /// — the **root** owned `Text`, so no slice this parse allocates names
    /// another slice.
    #[inline]
    pub(crate) fn owner(&self) -> GcRef {
        self.owner
    }

    /// `at`, an offset into this input's bytes, as an offset into
    /// [`owner`](Input::owner)'s bytes.
    ///
    /// The two differ exactly when the parse was handed a slice: the input's
    /// byte 0 is `base` in the root. Every `alloc_text_slice` the interpreter
    /// makes goes through this, which is what keeps "owner is the root" and
    /// "offsets name real bytes" one statement rather than two that can drift.
    #[inline]
    pub(crate) fn owner_offset(&self, at: usize) -> usize {
        self.base.saturating_add(at)
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

    /// True when this region holds nothing but whitespace (including when it
    /// holds nothing at all).
    ///
    /// This is the *bound* half of the module's rule, in one place so that no
    /// two constructors can disagree about one byte — which is exactly what
    /// happened when `grid` learned to forgive a row's trailing run and `lines`
    /// did not. Unicode whitespace, not an ASCII subset: a region's leftovers
    /// are leftovers whatever encodes them, and `char::is_whitespace` is the
    /// same predicate [`super::whitespace_tokens`] splits on.
    #[inline]
    pub(crate) fn is_all_whitespace(self, i: &Input<'_>) -> bool {
        match self.str(i) {
            Some(s) => s.chars().all(char::is_whitespace),
            // A region whose ends split a scalar is an interpreter bug, and
            // `region_str` reports it as a mismatch. It is not "whitespace".
            None => false,
        }
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
/// the `\n`.
///
/// **A region does not end in blank lines** — the *extent* half of the module's
/// rule. The trailing run of lines holding nothing but whitespace is dropped,
/// however long it is and whatever it is made of: the file's own `\n`, the
/// `"\n\n"` an editor leaves behind, a final line of spaces. The predecessor
/// dropped exactly one empty line, as a side effect of consuming the `\n` that
/// ended the one before it, and so `lines(int)` faulted on `"1\n2\n\n"` and
/// `grid(char)` called `"ab\ncd\n\n"` ragged.
///
/// It is the *extent* and not a *bound*, so it is decided here, before any
/// parser runs, and does not ask the child what it can read: a line with no
/// content is not a line any parser was offered. A line *with* content keeps
/// every byte it has, including a trailing space — whether that space is data is
/// the child's answer, and `walk_exact` asks it.
///
/// This is also what `sections` needs and does not have to be told: a section is
/// a run of non-blank lines, so a region that does not end in blank lines ends
/// with a section rather than with an empty one.
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
    while out.last().is_some_and(|l| l.is_all_whitespace(i)) {
        out.pop();
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
        // "Blank" is the same word [`split_lines`] uses when it decides where
        // the region ends: a line holding nothing but whitespace holds nothing.
        // Testing emptiness instead made a separator of spaces part of the
        // section on either side of it.
        if line.is_all_whitespace(i) {
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
    fn a_region_does_not_end_in_blank_lines_however_many_there_are() {
        // The extent half of the rule, at the substrate level. Two earlier
        // attempts wrote a *count* here — the bound rule applied to a region
        // that still held the terminator, then a trim of exactly one terminator
        // — and each was defeated by one more newline. There is no count now:
        // the trailing run of blank lines is not part of the region, whatever
        // it is made of.
        for (text, want) in [
            ("1\n2\n", vec!["1", "2"]),
            ("1\n2", vec!["1", "2"]),
            ("1\r\n2\r\n", vec!["1", "2"]),
            // The ending that reproduced the closed blocker verbatim.
            ("1\n2\n\n", vec!["1", "2"]),
            ("1\n2\n\n\n\n", vec!["1", "2"]),
            // A final line of nothing but spaces is a blank line.
            ("1\n2\n   \n", vec!["1", "2"]),
            ("1\n2\n\t\n \n", vec!["1", "2"]),
            // A line WITH content keeps every byte it has, trailing space
            // included: whether that space is data is the child parser's
            // answer, not the region's.
            ("1 \n2 \n", vec!["1 ", "2 "]),
            // An INTERIOR blank line is structure, not trailing whitespace.
            ("1\n\n2\n", vec!["1", "", "2"]),
            ("\n\n", vec![]),
            ("", vec![]),
        ] {
            let (_rt, owner) = input_over(text);
            let i = unsafe { Input::new(owner) }.expect("a Text is UTF-8");
            let lines: Vec<&str> = split_lines(&i, i.whole())
                .into_iter()
                .map(|l| l.str(&i).expect("a line is a str"))
                .collect();
            assert_eq!(lines, want, "lines of {text:?}");
        }
    }

    #[test]
    fn the_root_region_is_the_whole_buffer_and_no_terminator_is_trimmed() {
        // `read <parser>` and `parse(text, P)` are one function over one
        // buffer. Trimming here made them differ: `parse("abc\n", rest)` and
        // `parse("abc", rest)` answered the same Text, and the `\n` the program
        // wrote into its own literal could not be recovered. Nothing needs the
        // trim — `split_lines` above and `walk_exact`'s bound rule leave a
        // terminator to nobody without one.
        for text in ["1 2 3\n", "a -> b\r\n", "^v<>\n", "no terminator", "\n", ""] {
            let (_rt, owner) = input_over(text);
            let i = unsafe { Input::new(owner) }.expect("a Text is UTF-8");
            assert_eq!(i.whole().str(&i), Some(text), "root region of {text:?}");
        }
    }

    #[test]
    fn a_blank_line_of_spaces_separates_sections() {
        // `split_sections` calls a line blank by the same predicate
        // `split_lines` uses to decide where the region ends, so a separator
        // that carries a space is still a separator.
        let (_rt, owner) = input_over("first\n   \nsecond\n\n");
        let i = unsafe { Input::new(owner) }.expect("a Text is UTF-8");
        let sections: Vec<&str> = split_sections(&i, i.whole())
            .into_iter()
            .map(|s| s.str(&i).expect("a section is a str"))
            .collect();
        assert_eq!(sections, vec!["first", "second"]);
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
