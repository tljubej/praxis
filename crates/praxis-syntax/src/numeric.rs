//! The one digit-separator rule for numeric literals (§4.3).
//!
//! `1_000_000` is one literal, and the `_`s in it are punctuation the *value*
//! does not contain. Two places have to agree about that: the lexer, which
//! decides how far a numeric literal runs, and lowering, which turns its text
//! into an `i64` or an `f64`. They did not — lowering stripped separators the
//! lexer never let through, so `1_000` was a `P002` at the `_` and
//! `9_223_372_036_854_775_808` lexed as `9` followed by an identifier, while the
//! strip on the other side was unreachable code (REP-11).
//!
//! Both halves live here so neither can drift again: [`separator_run_len`] is
//! what the lexer consumes, [`strip_digit_separators`] is what the decoder
//! removes, and the rule is stated once.
//!
//! The rule: **a separator is one or more `_` with a digit on each side.** So
//! `1_0`, `1_000_000` and `1__0` are literals; `_1` is an identifier (the lexer
//! never reaches here — dispatch routes on the first byte) and `1_` is the
//! literal `1` followed by the `_` token, which is what FE-02 made `_` into.
//! Nothing here decides where digits may appear; the caller owns that, which is
//! why a separator is legal in a fraction and an exponent too (`1_0.5_5e1_0`).

use std::borrow::Cow;

/// The length in bytes of the digit-separator run at `at`, or `0` if there is
/// none.
///
/// Total on any input: it checks *both* sides, so a caller cannot get a nonzero
/// answer for a `_` that is not between digits. The lexer's three digit runs
/// each ask only after consuming at least one digit, so the left-hand check is
/// always satisfied there — it is written out anyway because a predicate whose
/// correctness depends on the caller's position is the shape this finding had.
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
}
