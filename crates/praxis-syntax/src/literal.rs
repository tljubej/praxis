//! The one literal decoder for the whole workspace (§4.3).
//!
//! There were two, and they disagreed (IP-08). `praxis-hir`'s `unquote_text`
//! stripped exactly one quote at each end and decoded `\n \t \r \" \\ \0`;
//! `parser_lower`'s copy was `raw.trim_start_matches('"').trim_end_matches('"')`
//! — it never unescaped anything, so `sep("\t", int)` split on the two
//! characters `\` and `t`, and it stripped a *run* of quotes at each end, so
//! `sep("\"\"", int)` lost both of its real quotes and became the empty
//! separator that cannot advance a cursor.
//!
//! `praxis-syntax` depends only on `praxis-source`, so both the HIR lowerer and
//! the input-parser's capture-body parser can reach this.
//!
//! [`decode_char_literal`] is here for the same rule, before it has had a chance
//! to be broken twice: a `'a'` is decoded by the **lexer** (which is where its
//! one-scalar rule is enforced, ADR-141), by [`crate::SyntaxKind::CharLit`]'s
//! expression lowering and by its pattern lowering. Three callers, one decoder,
//! and one escape table — [`decode_escape`] — shared with `"…"` so the two
//! spellings of `\n` cannot drift apart.

/// Whether `raw` is a well-formed text literal: at least `""`, quoted at both
/// ends.
#[must_use]
pub fn is_text_literal(raw: &str) -> bool {
    raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"')
}

/// Strip the surrounding quotes and decode the escapes of a `"…"` literal.
///
/// Exactly **one** quote comes off each end. An unrecognized escape is
/// preserved verbatim (backslash and all) rather than being silently dropped —
/// the lexer has already reported it, and rewriting it here would make the
/// value disagree with the diagnostic.
///
/// A `raw` that is not a text literal is returned unchanged; callers that need
/// to reject one should ask [`is_text_literal`] first.
#[must_use]
pub fn unquote_text(raw: &str) -> String {
    if !is_text_literal(raw) {
        return raw.to_string();
    }
    decode_text_body(&raw[1..raw.len() - 1])
}

/// Decode the escapes of a text literal's body — the part with the delimiters
/// already off.
///
/// [`unquote_text`] is this with `"` stripped from each end. The other caller is
/// an **interpolation fragment** (§8.1, ADR-147), whose delimiters are not both
/// quotes: `"Part 2: {` opens with `"` and closes with `{`, and the fragments
/// between holes open and close with braces. One byte still comes off each end,
/// and the body is decoded by the same table — which is what keeps `"a\tb"` and
/// `"a\tb{x}"` agreeing about what `\t` is.
#[must_use]
pub fn decode_text_body(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(esc) => match decode_escape(esc) {
                    Some(decoded) => out.push(decoded),
                    None => {
                        out.push('\\');
                        out.push(esc);
                    }
                },
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The escape table both literal spellings read (§4.3).
///
/// `None` means "not an escape this language recognizes", and the two callers
/// answer it differently — see [`unquote_text`] and [`decode_char_literal`] —
/// because they have different amounts of room to preserve the mistake in. What
/// they may not do is disagree about what `\n` *is*, which is why the eight rows
/// live here rather than in each of them.
///
/// `\{` and `\}` joined the table with §8.1's interpolation (ADR-147): a `{` in
/// a text literal now opens a hole, so a literal brace needs a spelling. It is
/// an escape rather than a doubling rule (`{{`) because the language has one
/// escape table and every other literal brace-free character already goes
/// through it — a doubling rule would be a second mechanism that only
/// interpolation used. `\}` is accepted so a pair can be written symmetrically;
/// it is not *required*, since outside a hole a `}` closes nothing.
#[must_use]
pub fn decode_escape(esc: char) -> Option<char> {
    match esc {
        'n' => Some('\n'),
        't' => Some('\t'),
        'r' => Some('\r'),
        '"' => Some('"'),
        '\\' => Some('\\'),
        '0' => Some('\0'),
        '{' => Some('{'),
        '}' => Some('}'),
        _ => None,
    }
}

/// Write `s` as a quoted, escaped text literal — the direction
/// [`decode_text_body`] does not go.
///
/// The debugger renders values, and a `Text` rendered as itself is ambiguous in
/// three ways it cannot afford: `""` writes zero bytes and is indistinguishable
/// from a failed read, a value containing `"` cannot be told from two values,
/// and a value containing a newline takes a second line on a display that gives
/// each value one. Quoting answers all three, and the escaping is what keeps the
/// quoting honest.
///
/// ### What round-trips, and what does not
///
/// `decode_text_body(&quote_text(s)[1..len-1]) == s` for every `s` — that is the
/// property, and [`quoting_round_trips_through_the_decoder`] is it as a test.
///
/// It is deliberately *not* "the output re-lexes as a literal spelling `s`".
/// `{` opens an interpolation hole in source (§8.1) and is left unescaped here,
/// because this text is read by a person looking at a locals pane and not by the
/// lexer, and `{"a": 1}` is worth more on that pane than `\{"a": 1\}`. The
/// round-trip above still holds through it: `decode_escape` only ever looks at
/// the character *after* a backslash, so an unescaped brace decodes to itself.
///
/// [`quoting_round_trips_through_the_decoder`]: #
#[must_use]
pub fn quote_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            // The two that would make the quoting a lie, and the three that
            // would make the value take more than its line.
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Why a `'…'` run is not a character literal.
///
/// Three variants rather than one, because the lexer's message is what makes the
/// difference between the three legible: `''` names no character, `'ab'` names
/// two, and `'a` never closed. Collapsing them would report the length rule for
/// a literal whose real problem is a missing quote.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CharLitError {
    /// No closing `'` — the token ran to the end of its line or the file.
    Unterminated,
    /// `''`: the body is empty, so there is no character to name.
    Empty,
    /// `'ab'`: the body decodes to more than one Unicode scalar.
    ///
    /// This is the variant the feature exists for. `"ab"[0]` is a well-typed
    /// program that silently means `a`; `'ab'` is a lexical error.
    TooLong,
}

/// Whether `raw` is a well-formed character literal: at least `''`, quoted at
/// both ends.
#[must_use]
pub fn is_char_literal(raw: &str) -> bool {
    raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'')
}

/// Strip the surrounding quotes and decode the escapes of a `'…'` literal,
/// answering the **one** Unicode scalar it names (ADR-141).
///
/// One quote comes off each end and the body must decode to exactly one scalar;
/// anything else is a [`CharLitError`], which is the whole of the one-character
/// rule and the reason the lexer asks this function rather than counting bytes.
/// `'é'` is one character, not two.
///
/// An **unrecognized** escape decodes to the escaped scalar itself (`'\q'` is
/// `q`), where [`unquote_text`] preserves the backslash verbatim. The two differ
/// deliberately: the lexer has already reported `T005` either way, and a char
/// literal that preserved `\q` would then have two scalars in it and earn a
/// second, spurious "not one character" on top of the report the author already
/// has.
pub fn decode_char_literal(raw: &str) -> Result<char, CharLitError> {
    if !is_char_literal(raw) {
        return Err(CharLitError::Unterminated);
    }
    let inner = &raw[1..raw.len() - 1];
    let mut chars = inner.chars();
    let decoded = match chars.next() {
        None => return Err(CharLitError::Empty),
        Some('\\') => match chars.next() {
            // `'\` at the end of the body: the closing quote was eaten by the
            // escape, so the literal never closed.
            None => return Err(CharLitError::Unterminated),
            Some(esc) => decode_escape(esc).unwrap_or(esc),
        },
        Some(c) => c,
    };
    if chars.next().is_some() {
        return Err(CharLitError::TooLong);
    }
    Ok(decoded)
}

/// **These four are characterization tests, not gates.**
///
/// `unquote_text` moved here verbatim from `praxis-hir`'s `lower.rs`; its body
/// is unchanged apart from the `is_text_literal` extraction, so every assertion
/// below also passes against the predecessor. They are worth having — the
/// function is newly public and this is now the workspace's only text decoder,
/// so its behaviour should be written down where it lives — but they must not
/// be counted as evidence that IP-08 was fixed.
///
/// The gate for IP-08 is
/// `praxis_hir::infer_tests::a_parser_string_literal_is_decoded_once_like_every_other_literal`,
/// which fails against the predecessor: the defect was that `parser_lower` had
/// a *second*, worse decoder (`trim_start_matches('"').trim_end_matches('"')`,
/// which never unescaped and stripped a run of quotes at each end), not that
/// this one was wrong.
#[cfg(test)]
mod tests {
    use super::*;

    /// Every string survives quoting and decoding back, which is what makes
    /// [`quote_text`] an inverse of the table rather than a second opinion about
    /// it.
    #[test]
    fn quoting_round_trips_through_the_decoder() {
        for s in [
            "", "asdf", "\"", "\\", "a\"b\\c", "\n\t\r\0", "{x}", "a\\nb", "héllo",
        ] {
            let quoted = quote_text(s);
            assert!(is_text_literal(&quoted), "quoted is a literal: {quoted:?}");
            assert_eq!(unquote_text(&quoted), s, "round trip of {s:?}");
        }
    }

    /// The visible half: an empty `Text` becomes two characters instead of
    /// nothing at all, which is the whole reason the debugger quotes.
    #[test]
    fn an_empty_text_still_renders_as_something() {
        assert_eq!(quote_text(""), "\"\"");
        assert_eq!(quote_text("a\nb"), r#""a\nb""#, "and stays on one line");
        // A brace is left alone: this is a rendering for a person, not a
        // spelling for the lexer — see `quote_text`'s note.
        assert_eq!(quote_text("{x}"), r#""{x}""#);
    }

    #[test]
    fn one_quote_comes_off_each_end() {
        assert_eq!(unquote_text(r#""a""#), "a");
        assert_eq!(unquote_text(r#""""#), "");
        // A literal whose content is two escaped quotes keeps both. (This is
        // the case `parser_lower`'s decoder lost, not this one — see the module
        // note above.)
        assert_eq!(unquote_text(r#""\"\"""#), "\"\"");
    }

    #[test]
    fn the_six_escapes_decode() {
        assert_eq!(unquote_text(r#""\n\t\r\"\\\0""#), "\n\t\r\"\\\0");
    }

    #[test]
    fn an_unknown_escape_is_preserved_verbatim() {
        assert_eq!(unquote_text(r#""\q""#), r"\q");
        assert_eq!(unquote_text(r#""a\""#), r#"a\"#);
    }

    #[test]
    fn a_non_literal_is_not_a_literal() {
        assert!(!is_text_literal("a"));
        assert!(!is_text_literal("\""));
        assert!(!is_text_literal("\"a"));
        assert!(is_text_literal("\"\""));
    }

    // --- the character literal (ADR-141) ---

    #[test]
    fn one_quote_comes_off_each_end_of_a_char() {
        assert_eq!(decode_char_literal("'a'"), Ok('a'));
        assert_eq!(decode_char_literal("'#'"), Ok('#'));
        assert_eq!(decode_char_literal("'\"'"), Ok('"'));
    }

    #[test]
    fn a_char_takes_texts_escapes_plus_the_quote() {
        assert_eq!(decode_char_literal(r"'\n'"), Ok('\n'));
        assert_eq!(decode_char_literal(r"'\t'"), Ok('\t'));
        assert_eq!(decode_char_literal(r"'\r'"), Ok('\r'));
        assert_eq!(decode_char_literal(r"'\0'"), Ok('\0'));
        assert_eq!(decode_char_literal(r"'\\'"), Ok('\\'));
        assert_eq!(decode_char_literal(r#"'\"'"#), Ok('"'));
        // The one escape a `"…"` does not need and a `'…'` does.
        assert_eq!(decode_char_literal(r"'\''"), Ok('\''));
    }

    /// A multi-byte scalar is **one** character. An implementation that counted
    /// bytes would call `'é'` two and refuse it.
    #[test]
    fn a_multibyte_scalar_is_one_character() {
        assert_eq!(decode_char_literal("'é'"), Ok('é'));
        assert_eq!(decode_char_literal("'😀'"), Ok('😀'));
        assert_eq!(decode_char_literal("'字'"), Ok('字'));
    }

    /// The three failure modes `"##"[0]` and `""[0]` used to have at run time,
    /// or not at all (ADR-141 Decision 2).
    #[test]
    fn a_char_literal_names_exactly_one_character() {
        assert_eq!(decode_char_literal("''"), Err(CharLitError::Empty));
        assert_eq!(decode_char_literal("'ab'"), Err(CharLitError::TooLong));
        assert_eq!(decode_char_literal("'éé'"), Err(CharLitError::TooLong));
        assert_eq!(decode_char_literal(r"'\na'"), Err(CharLitError::TooLong));
        assert_eq!(decode_char_literal("'a"), Err(CharLitError::Unterminated));
        assert_eq!(decode_char_literal("'"), Err(CharLitError::Unterminated));
        // `'\` — the escape ate the closing quote, so nothing closed it.
        assert_eq!(decode_char_literal(r"'\'"), Err(CharLitError::Unterminated));
    }

    /// An unknown escape answers the escaped scalar, where a `"…"` keeps the
    /// backslash. Both are already reported as `T005`; this is the difference
    /// between one diagnostic and two.
    #[test]
    fn an_unknown_char_escape_is_the_escaped_scalar() {
        assert_eq!(decode_char_literal(r"'\q'"), Ok('q'));
        assert_eq!(unquote_text(r#""\q""#), r"\q");
    }

    #[test]
    fn a_non_char_literal_is_not_a_char_literal() {
        assert!(!is_char_literal("a"));
        assert!(!is_char_literal("'"));
        assert!(!is_char_literal("'a"));
        assert!(is_char_literal("''"));
    }
}
