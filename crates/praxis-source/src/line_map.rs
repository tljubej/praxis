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
        let content_end = trim_one_terminator(&self.text, line_start, next_start);
        let col = lc.col.min(content_end.saturating_sub(line_start));
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
    /// the line text should trim trailing `\n` / `\r` / `\r\n` themselves.
    pub fn line_range(&self, line: u32) -> Option<(BytePos, BytePos)> {
        if line == 0 {
            return None;
        }
        let idx = (line - 1) as usize;
        let start = *self.line_starts.get(idx)?;
        let next_start = self.line_starts.get(idx + 1).copied().unwrap_or(self.len);
        Some((BytePos(start), BytePos(next_start)))
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

/// Return the content end offset of a line: `next_start` minus the bytes of the
/// single line terminator (`\n`, `\r`, or `\r\n`) that separates this line from
/// the next. Reads the actual bytes so CRLF is handled correctly.
fn trim_one_terminator(text: &[u8], line_start: u32, next_start: u32) -> u32 {
    // Only trim when there actually is a next line (next_start > line_start and
    // there's a gap). The final line may have no terminator.
    if next_start <= line_start {
        return line_start;
    }
    let s = line_start as usize;
    let e = next_start as usize;
    let gap = &text[s..e.min(text.len())];
    if gap.ends_with(b"\r\n") {
        next_start - 2
    } else if gap.ends_with(b"\n") || gap.ends_with(b"\r") {
        next_start - 1
    } else {
        // No recognizable terminator (e.g. the final line without a trailing
        // newline): content end is the whole extent.
        next_start
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
        assert!(map
            .linecol_to_offset(LineCol { line: 99, col: 0 })
            .is_none());
    }

    #[test]
    fn linecol_column_clamps_to_line_end() {
        let map = LineMap::new("ab\ncd");
        let off = map.linecol_to_offset(LineCol { line: 1, col: 50 }).unwrap();
        assert_eq!(off, BytePos(2), "column past line end clamps");
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
