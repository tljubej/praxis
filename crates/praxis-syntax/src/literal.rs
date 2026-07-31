//! The one text-literal decoder for the whole workspace (§4.3).
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
    let inner = &raw[1..raw.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('0') => out.push('\0'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
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
}
