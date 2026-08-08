//! Byte offsets ⇄ LSP positions (ADR-096).
//!
//! **This is the only module in the workspace that knows what a UTF-16 code
//! unit is.** An LSP [`Position`]'s `character` counts code units in the
//! negotiated encoding; `praxis_source::LineMap`'s column counts *bytes*, by a
//! documented design choice that keeps it lossless and O(1) to invert. Pushing
//! the protocol's unit downward would put a protocol concern under every crate
//! that reports a span, to serve one consumer — so the conversion happens here,
//! at the boundary, and nowhere else.

use lsp_types::{Position, PositionEncodingKind, Range};
use praxis_source::{BytePos, LineCol, LineMap, Span};
use praxis_syntax::span_bridge::range_to_span;
use rowan::TextRange;

/// Which unit an LSP `character` counts.
///
/// UTF-32 is not offered: it is optional in the protocol, no major client
/// prefers it, and a third arm would be a third thing to keep the property test
/// honest about for no gain. A client that offers only UTF-32 gets UTF-16, which
/// every client must support.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Encoding {
    /// `character` is a byte offset within the line. Free to convert, because
    /// it is what the compiler already uses.
    Utf8,
    /// `character` counts UTF-16 code units. The protocol default, and what a
    /// client gets when it expresses no preference.
    #[default]
    Utf16,
}

impl Encoding {
    /// Pick the encoding from what the client offered at `initialize`.
    ///
    /// UTF-8 wins when offered, because then the conversion is the identity on
    /// the compiler's own unit. Anything else — including a client that offers
    /// nothing, which is every pre-3.17 client — is UTF-16, the protocol's
    /// default and the one encoding a client may not decline.
    #[must_use]
    pub fn negotiate(offered: Option<&[PositionEncodingKind]>) -> Encoding {
        match offered {
            Some(kinds) if kinds.contains(&PositionEncodingKind::UTF8) => Encoding::Utf8,
            _ => Encoding::Utf16,
        }
    }

    /// The kind to advertise back in `InitializeResult`.
    #[must_use]
    pub fn kind(self) -> PositionEncodingKind {
        match self {
            Encoding::Utf8 => PositionEncodingKind::UTF8,
            Encoding::Utf16 => PositionEncodingKind::UTF16,
        }
    }

    /// How many units `s` counts as under this encoding.
    fn units(self, s: &str) -> u32 {
        let n = match self {
            Encoding::Utf8 => s.len(),
            Encoding::Utf16 => s.chars().map(char::len_utf16).sum(),
        };
        u32::try_from(n).unwrap_or(u32::MAX)
    }
}

/// A document's text and its line table, the two things every conversion needs.
///
/// Carried together because a conversion that had only one of them would have to
/// rebuild the other per call — and because a `LineMap` built from *different*
/// text than the one being indexed is the silent-corruption case this pairing
/// makes hard to write.
#[derive(Clone, Copy)]
pub struct PositionMap<'a> {
    text: &'a str,
    lines: &'a LineMap,
}

impl<'a> PositionMap<'a> {
    #[must_use]
    pub fn new(text: &'a str, lines: &'a LineMap) -> PositionMap<'a> {
        PositionMap { text, lines }
    }

    /// The byte offset for an LSP position, clamped into the document.
    ///
    /// Clamping rather than failing is the protocol's own behaviour for an
    /// out-of-range position, and it is what a client sends legitimately: a
    /// caret at the end of a line arrives as a `character` one past the last
    /// unit.
    #[must_use]
    pub fn offset(&self, pos: Position, enc: Encoding) -> u32 {
        // LSP lines are 0-based; `LineMap`'s are 1-based.
        let line_1 = pos.line.saturating_add(1);
        let Some((start, end)) = self.lines.line_range(line_1) else {
            // Past the last line: the end of the document.
            return u32::try_from(self.text.len()).unwrap_or(u32::MAX);
        };
        // `line_range` hands back the extent *including* the terminator, so the
        // line a `character` indexes is what is left after trimming it.
        let content_end = LineMap::trim_line_terminator(self.text.as_bytes(), start, end);
        let line = &self.text[start.to_usize()..content_end.to_usize()];
        start.to_u32() + column_to_byte(line, pos.character, enc)
    }

    /// The LSP position of a byte offset, clamped into the document.
    #[must_use]
    pub fn position(&self, offset: u32, enc: Encoding) -> Position {
        let clamped = offset.min(u32::try_from(self.text.len()).unwrap_or(u32::MAX));
        let LineCol { line, col } = self.lines.offset_to_linecol(BytePos::from(clamped));
        let line_start = clamped - col;
        // Round down to a character boundary: a span that ends mid-scalar would
        // otherwise slice `text` and panic. Spans from the compiler are always
        // on boundaries; a span arriving from a client need not be.
        let mut byte_col = col as usize;
        let abs = line_start as usize;
        while byte_col > 0 && !self.text.is_char_boundary(abs + byte_col) {
            byte_col -= 1;
        }
        let prefix = &self.text[abs..abs + byte_col];
        Position {
            line: line.saturating_sub(1),
            character: enc.units(prefix),
        }
    }

    /// Whether an LSP position names this byte offset.
    ///
    /// Almost every offset has one. The exception is the **interior of a CRLF**:
    /// both bytes belong to the line terminator, which ends the line rather than
    /// living in it, so `(line, character)` addresses the byte before the `\r`
    /// and the byte after the `\n` and nothing between. Round-tripping through a
    /// position is therefore the identity everywhere this is true and nowhere
    /// else — which is the rule ADR-096's property test is stated against, so it
    /// lives here rather than being restated in each test that needs it.
    #[must_use]
    pub fn is_addressable(&self, offset: u32) -> bool {
        let bytes = self.text.as_bytes();
        let i = offset as usize;
        if i > bytes.len() || !self.text.is_char_boundary(i) {
            return false;
        }
        !(i > 0 && bytes.get(i - 1) == Some(&b'\r') && bytes.get(i) == Some(&b'\n'))
    }

    /// The LSP range covering a byte span.
    #[must_use]
    pub fn range(&self, span: Span, enc: Encoding) -> Range {
        Range {
            start: self.position(span.start().to_u32(), enc),
            end: self.position(span.end().to_u32(), enc),
        }
    }

    /// The LSP range covering a rowan node/token range.
    #[must_use]
    pub fn text_range(&self, range: TextRange, enc: Encoding) -> Range {
        self.range(range_to_span(range), enc)
    }

    /// The byte span an LSP range names.
    #[must_use]
    pub fn span(&self, range: Range, enc: Encoding) -> Span {
        let start = self.offset(range.start, enc);
        let end = self.offset(range.end, enc);
        // A client may send a reversed range; a `Span` with `end < start` would
        // be a slice that panics rather than a range that is merely odd.
        Span::new(start.min(end), start.max(end))
    }
}

/// Convert a `character` count into a byte offset within `line`.
///
/// A count past the end of the line clamps to the line's length — a caret at the
/// end of a line arrives that way, and so does a stale position from a client
/// that has not seen the latest edit.
fn column_to_byte(line: &str, character: u32, enc: Encoding) -> u32 {
    if character == 0 {
        return 0;
    }
    let mut units = 0u32;
    for (byte, ch) in line.char_indices() {
        if units >= character {
            return u32::try_from(byte).unwrap_or(u32::MAX);
        }
        units += match enc {
            Encoding::Utf8 => u32::try_from(ch.len_utf8()).unwrap_or(1),
            Encoding::Utf16 => u32::try_from(ch.len_utf16()).unwrap_or(1),
        };
    }
    u32::try_from(line.len()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(text: &str) -> LineMap {
        LineMap::new(text)
    }

    #[test]
    fn ascii_agrees_in_both_encodings() {
        let text = "var x = 1\nout(x)\n";
        let lines = map(text);
        let pm = PositionMap::new(text, &lines);
        for offset in 0..=text.len() as u32 {
            let a = pm.position(offset, Encoding::Utf8);
            let b = pm.position(offset, Encoding::Utf16);
            assert_eq!(a, b, "ASCII must agree at offset {offset}");
        }
    }

    /// Invisible in every English fixture: a two-byte character is one UTF-16
    /// unit, and a four-byte one is two.
    #[test]
    fn a_multibyte_line_counts_differently_per_encoding() {
        // "é" is 2 bytes / 1 UTF-16 unit; "𝄞" is 4 bytes / 2 UTF-16 units.
        let text = "var é = \"𝄞\"\n";
        let lines = map(text);
        let pm = PositionMap::new(text, &lines);
        let end_of_line = u32::try_from(text.find('\n').unwrap()).unwrap();
        let utf8 = pm.position(end_of_line, Encoding::Utf8);
        let utf16 = pm.position(end_of_line, Encoding::Utf16);
        assert_eq!(utf8.character, end_of_line, "UTF-8 characters are bytes");
        assert_ne!(
            utf16.character, utf8.character,
            "a multi-byte line must not report the same column in both encodings"
        );
        // 'v','a','r',' ' = 4, 'é' = 1, ' ','=',' ','"' = 4, '𝄞' = 2, '"' = 1.
        assert_eq!(utf16.character, 12);
    }

    #[test]
    fn round_trips_at_every_character_boundary() {
        for text in [
            "var x = 1\nout(x)\n",
            "var é = 1\r\nout(é)\r\n",
            "read lines(`{n:int} 𝄞 {m:int}`)\n",
            "",
            "\n\n\n",
            "no trailing newline",
        ] {
            let lines = map(text);
            let pm = PositionMap::new(text, &lines);
            for enc in [Encoding::Utf8, Encoding::Utf16] {
                for (offset, _) in text
                    .char_indices()
                    .chain(std::iter::once((text.len(), ' ')))
                {
                    let offset = u32::try_from(offset).unwrap();
                    if !pm.is_addressable(offset) {
                        continue;
                    }
                    let pos = pm.position(offset, enc);
                    let back = pm.offset(pos, enc);
                    assert_eq!(
                        back, offset,
                        "byte → position → byte at {offset} in {text:?} ({enc:?})"
                    );
                }
            }
        }
    }

    #[test]
    fn crlf_is_one_line_break() {
        let text = "a\r\nb\r\n";
        let lines = map(text);
        let pm = PositionMap::new(text, &lines);
        // Line 1 (0-based), column 0 is the `b`, at byte 3.
        let pos = Position {
            line: 1,
            character: 0,
        };
        assert_eq!(pm.offset(pos, Encoding::Utf16), 3);
    }

    #[test]
    fn a_column_past_the_line_end_clamps_to_it() {
        let text = "ab\ncd\n";
        let lines = map(text);
        let pm = PositionMap::new(text, &lines);
        let pos = Position {
            line: 0,
            character: 99,
        };
        assert_eq!(
            pm.offset(pos, Encoding::Utf16),
            2,
            "clamps to the line's content end, not into the newline"
        );
    }

    #[test]
    fn a_line_past_the_end_clamps_to_the_document() {
        let text = "ab\n";
        let lines = map(text);
        let pm = PositionMap::new(text, &lines);
        let pos = Position {
            line: 99,
            character: 0,
        };
        assert_eq!(pm.offset(pos, Encoding::Utf16), 3);
    }

    #[test]
    fn negotiation_prefers_utf8_and_defaults_to_utf16() {
        assert_eq!(Encoding::negotiate(None), Encoding::Utf16);
        assert_eq!(Encoding::negotiate(Some(&[])), Encoding::Utf16);
        assert_eq!(
            Encoding::negotiate(Some(&[PositionEncodingKind::UTF16])),
            Encoding::Utf16
        );
        assert_eq!(
            Encoding::negotiate(Some(&[
                PositionEncodingKind::UTF16,
                PositionEncodingKind::UTF8
            ])),
            Encoding::Utf8
        );
        assert_eq!(
            Encoding::negotiate(Some(&[PositionEncodingKind::UTF32])),
            Encoding::Utf16,
            "an encoding we do not implement must not be advertised back"
        );
    }

    #[test]
    fn a_reversed_range_becomes_an_ordered_span() {
        let text = "abcdef\n";
        let lines = map(text);
        let pm = PositionMap::new(text, &lines);
        let range = Range {
            start: Position {
                line: 0,
                character: 4,
            },
            end: Position {
                line: 0,
                character: 1,
            },
        };
        let span = pm.span(range, Encoding::Utf16);
        assert_eq!((span.start().to_u32(), span.end().to_u32()), (1, 4));
    }
}
