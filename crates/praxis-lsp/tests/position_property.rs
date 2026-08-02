//! ADR-096's gate: `byte → position → byte` is the identity in **both**
//! encodings, and the two agree exactly where the text is ASCII.
//!
//! The generator is deliberately **not English-only**. A position conversion
//! that returns the byte column for `character` passes every test written in
//! ASCII and puts the squiggle in the wrong place the first time a program
//! contains `é` — which is the whole class of bug this property exists to
//! catch, and the reason the alphabet below includes a two-byte character, a
//! three-byte one, an astral-plane one (two UTF-16 code units), CRLF, and a
//! backtick template's own punctuation.

use praxis_lsp::position::{Encoding, PositionMap};
use praxis_source::LineMap;
use proptest::prelude::*;

/// The alphabet the generator draws from. Every entry is a *string* rather than
/// a `char` so CRLF can be one atom: a `\r` and a `\n` generated independently
/// would mostly not land next to each other.
const ALPHABET: &[&str] = &[
    "a", "Z", " ", "\t", "(", ")", "{", "}", ":", ",", "`", "\"", "\\", "\n", "\r\n", "\r",
    // Two bytes, one UTF-16 unit.
    "é", "λ", // Three bytes, one UTF-16 unit.
    "→", "中",
    // Four bytes, **two** UTF-16 units — the case that separates the encodings.
    "𝄞", "😀", // Fragments of the shapes the language actually contains.
    "read ", "lines(", "int", "{n:int}", "let x = ",
];

fn text() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        proptest::sample::select(ALPHABET).prop_map(str::to_string),
        0..24,
    )
    .prop_map(|parts| parts.concat())
}

proptest! {
    /// Every character boundary survives the round trip, in both encodings.
    #[test]
    fn byte_to_position_to_byte_is_the_identity(source in text()) {
        let lines = LineMap::new(&source);
        let map = PositionMap::new(&source, &lines);
        for enc in [Encoding::Utf8, Encoding::Utf16] {
            for (offset, _) in source
                .char_indices()
                .chain(std::iter::once((source.len(), ' ')))
            {
                let offset = u32::try_from(offset).unwrap();
                // The interior of a CRLF is the one byte boundary no LSP
                // position names; `is_addressable` is where that rule lives.
                if !map.is_addressable(offset) {
                    continue;
                }
                let position = map.position(offset, enc);
                let back = map.offset(position, enc);
                prop_assert_eq!(
                    back,
                    offset,
                    "round trip failed at {} in {:?} ({:?}) via {:?}",
                    offset,
                    source,
                    enc,
                    position
                );
            }
        }
    }

    /// Where the text is ASCII the two encodings agree **exactly**. This is the
    /// half that makes the property meaningful: without it, an implementation
    /// that returned `(0, 0)` for everything would round-trip nothing and still
    /// satisfy a weaker identity.
    #[test]
    fn the_two_encodings_agree_on_ascii(source in text()) {
        let lines = LineMap::new(&source);
        let map = PositionMap::new(&source, &lines);
        for (offset, _) in source.char_indices() {
            let offset = u32::try_from(offset).unwrap();
            if !source[..offset as usize].is_ascii() {
                continue;
            }
            let a = map.position(offset, Encoding::Utf8);
            let b = map.position(offset, Encoding::Utf16);
            prop_assert_eq!(a, b, "ASCII prefix must convert identically at {}", offset);
        }
    }

    /// A position the client invented — a line past the end, a column past the
    /// line — clamps into the document instead of panicking or wrapping.
    #[test]
    fn an_out_of_range_position_clamps(
        source in text(),
        line in 0u32..40,
        character in 0u32..80,
    ) {
        let lines = LineMap::new(&source);
        let map = PositionMap::new(&source, &lines);
        for enc in [Encoding::Utf8, Encoding::Utf16] {
            let offset = map.offset(lsp_types::Position { line, character }, enc);
            prop_assert!(
                offset as usize <= source.len(),
                "clamped offset {} is past the document {:?}",
                offset,
                source
            );
            prop_assert!(
                source.is_char_boundary(offset as usize),
                "clamped offset {} is inside a character in {:?}",
                offset,
                source
            );
        }
    }
}

/// A multi-byte **template interior**, which is where the difference matters
/// most: a capture's name and type spans come out of the scanner in bytes, and
/// the editor has to be told about them in code units.
#[test]
fn a_multibyte_template_interior_converts_per_encoding() {
    let source = "let v = read lines(`{ключ:word} 𝄞 {n:int}`)\n";
    let lines = LineMap::new(source);
    let map = PositionMap::new(source, &lines);

    let n_at = u32::try_from(source.find("{n:int}").unwrap()).unwrap();
    let utf8 = map.position(n_at, Encoding::Utf8);
    let utf16 = map.position(n_at, Encoding::Utf16);

    assert_eq!(utf8.character, n_at, "UTF-8 characters are bytes");
    assert!(
        utf16.character < utf8.character,
        "the Cyrillic name and the astral clef are fewer UTF-16 units than bytes: \
         utf8={}, utf16={}",
        utf8.character,
        utf16.character
    );
    assert_eq!(map.offset(utf16, Encoding::Utf16), n_at);
    assert_eq!(map.offset(utf8, Encoding::Utf8), n_at);
}
