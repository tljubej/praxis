//! Line/column mapping for a source file.
//!
//! [`LineMap`] precomputes the byte offset at the start of each line so that
//! [`BytePos`] → [`LineCol`] conversion is a binary search, not a rescan. All
//! offsets are **byte** offsets (§4.1: source is UTF-8); a column is the byte
//! offset from the start of the line, so multi-byte characters occupy more than
//! one column. This keeps the mapping lossless and O(1) to invert.

use crate::span::BytePos;

/// A 1-based `(line, column)` position. `line` starts at 1, `col` is the byte
/// offset from the line start (so the first byte of a line is column 0).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LineCol {
    /// 1-based line number.
    pub line: u32,
    /// 0-based byte offset from the start of the line.
    pub col: u32,
}

/// A precomputed table of line-start byte offsets for a single source file.
///
/// Constructed once per file via [`LineMap::new`]; the compiler and diagnostics
/// layer read it cheaply thereafter.
#[derive(Clone, Debug)]
pub struct LineMap {
    /// Byte offset of the first byte of each line. Always begins with `0`
    /// (the start of line 1) and is strictly increasing.
    line_starts: Vec<u32>,
    /// Total byte length of the source, used to clamp out-of-range queries.
    len: u32,
    /// The source bytes. Kept so trimming/inspection helpers (e.g. stripping a
    /// trailing terminator when clamping a column) can read the actual bytes.
    text: Vec<u8>,
}

impl LineMap {
    /// Build a line map from source text. `\n`, `\r\n`, and `\r` all count as
    /// line terminators, matching how the input parser treats logical lines.
    pub fn new(text: &str) -> LineMap {
        let mut line_starts = vec![0u32];
        // Iterate over bytes so we can record byte offsets directly. We look for
        // any of `\n`, `\r`, or `\r\n`; a `\r` immediately followed by `\n` is
        // one line break, not two.
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'\n' {
                push_start(&mut line_starts, i + 1);
            } else if b == b'\r' {
                let next_is_lf = bytes.get(i + 1) == Some(&b'\n');
                let after = if next_is_lf { i + 2 } else { i + 1 };
                push_start(&mut line_starts, after);
                i = after;
                continue;
            }
            i += 1;
        }
        let len = u32::try_from(bytes.len()).expect("source files must be < 4 GiB");
        LineMap {
            line_starts,
            len,
            text: bytes.to_vec(),
        }
    }

    /// Convert a byte offset to a 1-based `(line, column)`.
    ///
    /// Offsets past the end of the file clamp to the last byte of the last line
    /// rather than overflowing, so a slightly-out-of-range span still renders
    /// something sensible.
    pub fn offset_to_linecol(&self, offset: BytePos) -> LineCol {
        let pos = offset.to_u32().min(self.len);
        let line = self.line_index(pos);
        let line_start = self.line_starts[line];
        LineCol {
            line: line as u32 + 1,
            col: pos - line_start,
        }
    }

    /// Convert a 1-based `(line, column)` back to a byte offset.
    ///
    /// Returns `None` if `line` is zero or beyond the number of lines. A column
    /// past the end of the line clamps to the line's last byte.
    pub fn linecol_to_offset(&self, lc: LineCol) -> Option<BytePos> {
        if lc.line == 0 {
            return None;
        }
        let line_idx = (lc.line - 1) as usize;
        let line_start = *self.line_starts.get(line_idx)?;
        // The next line starts where this one's terminator ends. For clamping
        // a column we want the line's *content* end (before the terminator),
        // so a too-large column points just past the last content byte rather
        // than at the `\n`.
        let next_start = self
            .line_starts
            .get(line_idx + 1)
            .copied()
            .unwrap_or(self.len);
        let content_end =
            Self::trim_line_terminator(&self.text, BytePos(line_start), BytePos(next_start));
        let col = lc.col.min(content_end.saturating_sub(BytePos(line_start)));
        Some(BytePos(line_start + col))
    }

    /// The total number of lines.
    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Index into `line_starts` for the line containing byte offset `pos`.
    fn line_index(&self, pos: u32) -> usize {
        // Find the last line_start <= pos. `binary_search_by` returns Err(i)
        // where i is the insertion point, i.e. the index after the wanted line.
        match self.line_starts.binary_search_by(|&start| start.cmp(&pos)) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }

    /// The byte range `[start, end)` of a given 1-based line number, or `None`
    /// if the line number is out of range.
    ///
    /// `end` is the byte offset where the next line begins (or the file end for
    /// the final line), so it includes any line terminator. Callers that render
    /// the line text should trim it with [`LineMap::trim_line_terminator`].
    pub fn line_range(&self, line: u32) -> Option<(BytePos, BytePos)> {
        if line == 0 {
            return None;
        }
        let idx = (line - 1) as usize;
        let start = *self.line_starts.get(idx)?;
        let next_start = self.line_starts.get(idx + 1).copied().unwrap_or(self.len);
        Some((BytePos(start), BytePos(next_start)))
    }

    /// The content end of a line extent `[start, end)`: `end` minus the bytes of
    /// the single line terminator (`\n`, `\r`, or `\r\n`) that separates this
    /// line from the next. Reads the actual bytes, so a CRLF is trimmed as one
    /// terminator and not as two.
    ///
    /// [`LineMap::line_range`] deliberately hands back the extent *including*
    /// the terminator and tells the caller to trim it — this is that trim, and
    /// the only copy of it. The terminator set has to stay in step with the
    /// scanner in [`LineMap::new`], so the rule lives beside the scanner: a
    /// change there is one edit rather than several that can silently desync.
    ///
    /// Takes the bytes rather than reading `self.text` so that a caller which
    /// already holds the source trims against the very bytes it is about to
    /// slice. An extent with nothing in it, and a final line with no terminator
    /// at all, are both returned unchanged.
    pub fn trim_line_terminator(text: &[u8], start: BytePos, end: BytePos) -> BytePos {
        // `end` may run past `text` for a synthetic extent, so clamp before
        // slicing; the guard then covers the empty and the out-of-range case at
        // once. Only trim when there is something to trim — the final line may
        // have no terminator.
        let s = start.to_usize();
        let e = end.to_usize().min(text.len());
        if e <= s {
            return start;
        }
        let gap = &text[s..e];
        if gap.ends_with(b"\r\n") {
            BytePos(end.to_u32() - 2)
        } else if gap.ends_with(b"\n") || gap.ends_with(b"\r") {
            BytePos(end.to_u32() - 1)
        } else {
            // No recognizable terminator (e.g. the final line without a trailing
            // newline): content end is the whole extent.
            end
        }
    }
}

/// Push a new line start, collapsing accidental duplicates (e.g. an empty line
/// right after a CRLF could otherwise produce a stale entry).
fn push_start(line_starts: &mut Vec<u32>, offset: usize) {
    let offset = u32::try_from(offset).expect("source files must be < 4 GiB");
    if line_starts.last() != Some(&offset) {
        line_starts.push(offset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_line_no_terminator() {
        let map = LineMap::new("hello");
        assert_eq!(map.line_count(), 1);
        assert_eq!(
            map.offset_to_linecol(BytePos(0)),
            LineCol { line: 1, col: 0 }
        );
        assert_eq!(
            map.offset_to_linecol(BytePos(4)),
            LineCol { line: 1, col: 4 }
        );
    }

    #[test]
    fn unix_newlines() {
        let map = LineMap::new("aa\nbb\ncc");
        assert_eq!(map.line_count(), 3);
        assert_eq!(
            map.offset_to_linecol(BytePos(0)),
            LineCol { line: 1, col: 0 }
        );
        assert_eq!(
            map.offset_to_linecol(BytePos(3)),
            LineCol { line: 2, col: 0 }
        );
        assert_eq!(
            map.offset_to_linecol(BytePos(6)),
            LineCol { line: 3, col: 0 }
        );
    }

    #[test]
    fn crlf_counts_as_one_break() {
        let map = LineMap::new("a\r\nb");
        assert_eq!(map.line_count(), 2, "CRLF must be a single line break");
        assert_eq!(
            map.offset_to_linecol(BytePos(3)),
            LineCol { line: 2, col: 0 }
        );
    }

    #[test]
    fn bare_cr_counts_as_break() {
        let map = LineMap::new("a\rb");
        assert_eq!(map.line_count(), 2);
        assert_eq!(
            map.offset_to_linecol(BytePos(2)),
            LineCol { line: 2, col: 0 }
        );
    }

    #[test]
    fn trailing_newline_yields_no_phantom_line() {
        // For "a\n" the newline byte itself (offset 1) terminates line 1, so it
        // is reported as line 1, col 1 — it does not start a phantom line 2.
        // Line 2 *starts* at offset 2 (after the newline), which is past the
        // content, and maps back cleanly.
        let map = LineMap::new("a\n");
        assert_eq!(map.line_count(), 2);
        assert_eq!(
            map.offset_to_linecol(BytePos(0)),
            LineCol { line: 1, col: 0 }
        );
        assert_eq!(
            map.offset_to_linecol(BytePos(1)),
            LineCol { line: 1, col: 1 }
        );
    }

    #[test]
    fn round_trip_linecol_offset() {
        let map = LineMap::new("out(1)\nout(2)\n");
        for offset in 0..12u32 {
            let lc = map.offset_to_linecol(BytePos(offset));
            let back = map
                .linecol_to_offset(lc)
                .expect("round trip should succeed");
            assert_eq!(
                back,
                BytePos(offset),
                "round trip failed at offset {offset} -> {lc:?}"
            );
        }
    }

    #[test]
    fn round_trip_with_multibyte_utf8() {
        // Bytes, not chars: 'λ' is two bytes, so columns advance by 2.
        //  "λx\nλy" -> bytes: [0,1] 'λ', [2] 'x', [3] '\n', [4,5] 'λ', [6] 'y'
        let map = LineMap::new("λx\nλy");
        assert_eq!(
            map.offset_to_linecol(BytePos(0)),
            LineCol { line: 1, col: 0 }
        );
        assert_eq!(
            map.offset_to_linecol(BytePos(2)),
            LineCol { line: 1, col: 2 }
        );
        // The newline byte (offset 3) terminates line 1; it does not start line 2.
        assert_eq!(
            map.offset_to_linecol(BytePos(3)),
            LineCol { line: 1, col: 3 }
        );
        // Line 2 starts at offset 4 (after the newline).
        assert_eq!(
            map.offset_to_linecol(BytePos(4)),
            LineCol { line: 2, col: 0 }
        );
        // And the round trip holds for each offset.
        for offset in 0..6u32 {
            let lc = map.offset_to_linecol(BytePos(offset));
            let back = map.linecol_to_offset(lc).unwrap();
            assert_eq!(back, BytePos(offset), "utf8 round trip at {offset}");
        }
    }

    #[test]
    fn offset_past_end_clamps() {
        let map = LineMap::new("ab");
        let lc = map.offset_to_linecol(BytePos(99));
        assert_eq!(lc, LineCol { line: 1, col: 2 }, "clamps to last byte");
    }

    #[test]
    fn linecol_zero_line_is_none() {
        let map = LineMap::new("ab\ncd");
        assert!(map.linecol_to_offset(LineCol { line: 0, col: 0 }).is_none());
    }

    #[test]
    fn linecol_past_last_line_is_none() {
        let map = LineMap::new("ab\ncd");
        assert!(
            map.linecol_to_offset(LineCol { line: 99, col: 0 })
                .is_none()
        );
    }

    #[test]
    fn linecol_column_clamps_to_line_end() {
        let map = LineMap::new("ab\ncd");
        let off = map.linecol_to_offset(LineCol { line: 1, col: 50 }).unwrap();
        assert_eq!(off, BytePos(2), "column past line end clamps");
    }

    /// Exactly one terminator is trimmed, whatever its spelling. Pinned at the
    /// single copy of the rule, because a change to `LineMap::new`'s scanner has
    /// to move in step with it.
    #[test]
    fn trimming_takes_exactly_one_terminator() {
        for (text, start, end, want) in [
            // CRLF is two bytes but a single terminator.
            ("a\r\nb", 0u32, 3u32, 1u32),
            ("a\nb", 0, 2, 1),
            // A bare `\r` terminates a line too.
            ("a\rb", 0, 2, 1),
            // A final line without a terminator keeps its whole extent.
            ("ab", 0, 2, 2),
            // An empty extent — the line after a trailing newline.
            ("a\n", 2, 2, 2),
        ] {
            assert_eq!(
                LineMap::trim_line_terminator(text.as_bytes(), BytePos(start), BytePos(end)),
                BytePos(want),
                "trimming {text:?}[{start}..{end}]"
            );
        }
    }

    #[test]
    fn empty_source_has_one_line() {
        let map = LineMap::new("");
        assert_eq!(map.line_count(), 1);
        assert_eq!(
            map.offset_to_linecol(BytePos(0)),
            LineCol { line: 1, col: 0 }
        );
    }
}
