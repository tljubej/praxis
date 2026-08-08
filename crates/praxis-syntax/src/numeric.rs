//! The one digit-separator rule for numeric literals (§4.3).
//!
//! `1_000_000` is one literal, and the `_`s in it are punctuation the *value*
//! does not contain. Two places have to agree about that: the lexer, which
//! decides how far a numeric literal runs, and lowering, which turns its text
//! into an `i64` or an `f64`. If the lexer refuses a separator the decoder
//! strips, the literal is broken up before the decoder ever sees it and the
//! strip is unreachable.
//!
//! Both halves live here so neither can drift: [`separator_run_len`] is what
//! the lexer consumes, [`strip_digit_separators`] is what the decoder removes,
//! and the rule is stated once.
//!
//! The rule: **a separator is one or more `_` with a digit on each side.** So
//! `1_0`, `1_000_000` and `1__0` are literals; `_1` is an identifier (the lexer
//! never reaches here — dispatch routes on the first byte) and `1_` is the
//! literal `1` followed by the `_` token. Nothing here decides where digits may
//! appear; the caller owns that, which is why a separator is legal in a fraction
//! and an exponent too (`1_0.5_5e1_0`).

use std::borrow::Cow;

/// The length in bytes of the digit-separator run at `at`, or `0` if there is
/// none.
///
/// Total on any input: it checks *both* sides, so a caller cannot get a nonzero
/// answer for a `_` that is not between digits. The lexer's three digit runs
/// each ask only after consuming at least one digit, so the left-hand check is
/// always satisfied there — it is written out anyway rather than leaving this
/// predicate's correctness dependent on where the caller stands.
#[must_use]
pub fn separator_run_len(bytes: &[u8], at: usize) -> usize {
    if at == 0 || !matches!(bytes.get(at - 1), Some(d) if d.is_ascii_digit()) {
        return 0;
    }
    let mut end = at;
    while matches!(bytes.get(end), Some(b'_')) {
        end += 1;
    }
    if end == at || !matches!(bytes.get(end), Some(d) if d.is_ascii_digit()) {
        return 0;
    }
    end - at
}

/// `s` with every digit separator removed, ready for `str::parse`.
///
/// Borrows when there is nothing to remove, which is every literal anyone
/// actually writes. Only the separators the lexer accepts can be present, so
/// this is a filter and not a validator: a malformed `_` never reaches it.
#[must_use]
pub fn strip_digit_separators(s: &str) -> Cow<'_, str> {
    if s.contains('_') {
        Cow::Owned(s.chars().filter(|c| *c != '_').collect())
    } else {
        Cow::Borrowed(s)
    }
}

/// The `i64` an `IntLit` token's text names, or `None` when it names a value
/// outside `Int` (§4.3).
///
/// Three passes decode an integer literal — inference, which is where `Y013` is
/// decided; the pattern builder, at a literal pattern; and lowering, which puts
/// the value in the typed tree — and "out of range" has to mean the same thing
/// in all three or one of them reports a literal another one accepts. The strip
/// and the `parse` are one line each; keeping them together is what makes the
/// range one rule instead of three copies of it.
#[must_use]
pub fn parse_int_literal(text: &str) -> Option<i64> {
    strip_digit_separators(text).parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule, at the level it is decided: a separator has digits on both
    /// sides. Each case here is a *lexing* outcome — a nonzero length is how
    /// much of the literal the `_`s occupy, and zero means the literal ended.
    #[test]
    fn a_separator_is_underscores_with_a_digit_on_each_side() {
        // `1_0`: one `_`, digits both sides.
        assert_eq!(separator_run_len(b"1_0", 1), 1);
        // `1__0`: a run, still one separator.
        assert_eq!(separator_run_len(b"1__0", 1), 2);
        // `1_`: nothing follows, so the literal is `1` and the `_` is a token.
        assert_eq!(separator_run_len(b"1_", 1), 0);
        // `1_a`: an identifier follows, not a digit.
        assert_eq!(separator_run_len(b"1_a", 1), 0);
        // `_1`: nothing precedes. Unreachable from the lexer, answered anyway.
        assert_eq!(separator_run_len(b"_1", 0), 0);
        // `a_1`: a letter precedes — this is one identifier, not a literal.
        assert_eq!(separator_run_len(b"a_1", 1), 0);
        // Not at a `_` at all.
        assert_eq!(separator_run_len(b"10", 1), 0);
        assert_eq!(separator_run_len(b"", 0), 0);
    }

    /// The other half: what the decoder removes is exactly what the lexer let
    /// through, and a literal with none is not copied.
    #[test]
    fn stripping_removes_every_separator_and_borrows_when_there_are_none() {
        assert_eq!(strip_digit_separators("1_000"), "1000");
        assert_eq!(strip_digit_separators("1_0_0"), "100");
        assert_eq!(strip_digit_separators("3.141_592"), "3.141592");
        assert!(matches!(strip_digit_separators("1000"), Cow::Borrowed(_)));
        assert!(matches!(strip_digit_separators("1_000"), Cow::Owned(_)));
    }

    /// The range is one rule: separators come out first, and the boundary is
    /// `i64`'s own. Everything past it is `None`, which is what `Y013` is.
    #[test]
    fn an_int_literal_decodes_through_the_range_or_not_at_all() {
        assert_eq!(parse_int_literal("0"), Some(0));
        assert_eq!(parse_int_literal("1_000"), Some(1000));
        assert_eq!(parse_int_literal("9223372036854775807"), Some(i64::MAX));
        assert_eq!(parse_int_literal("9223372036854775808"), None);
        // The separated spelling is the same literal and the same answer, which
        // is why the strip and the range test are one function.
        assert_eq!(parse_int_literal("9_223_372_036_854_775_808"), None);
        assert_eq!(parse_int_literal("99999999999999999999999"), None);
    }
}
