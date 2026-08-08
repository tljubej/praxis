//! The one identifier character class for the whole workspace (§4.1).
//!
//! §4.1 allows Unicode identifiers, and the lexer, the input parser's
//! capture-name splitter and the debugger all have to agree about which scalars
//! they are — so the class is stated once, here, and never re-derived.
//!
//! `praxis-syntax` depends only on `praxis-source`, so every front-end crate
//! can reach these predicates.

/// Whether `c` may start an identifier (§4.1).
///
/// XID-Start plus `_`, matching Rust and UAX #31's `Default Identifier`.
#[inline]
pub fn is_ident_start(c: char) -> bool {
    c == '_' || unicode_ident::is_xid_start(c)
}

/// Whether `c` may continue an identifier (§4.1).
///
/// XID-Continue plus `_`. Note XID-Continue already includes the ASCII digits,
/// so `x9` continues as one identifier while `9x` does not start one.
#[inline]
pub fn is_ident_continue(c: char) -> bool {
    c == '_' || unicode_ident::is_xid_continue(c)
}

/// Whether the whole of `s` is a well-formed identifier: non-empty, starting
/// with [`is_ident_start`] and continuing with [`is_ident_continue`].
///
/// This is the predicate a *consumer* of a name should use — a name that the
/// lexer would not have produced must be rejected, never rewritten into a
/// different name (rewriting is not injective and silently merges distinct
/// symbols).
pub fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        None => false,
        Some(first) if !is_ident_start(first) => false,
        Some(_) => chars.all(is_ident_continue),
    }
}

/// The length in bytes of the identifier run at the start of `s`, or `0` when
/// `s` does not start one.
///
/// The scanning counterpart to [`is_ident`]: the same character class, measuring
/// a prefix instead of judging a whole string. This is the predicate a
/// *scanner* wants, and it lives here for the same reason the class does —
/// every place that re-derived the run around the class is another place the
/// rule can drift.
///
/// A run is a run of *scalars*, so a caller holding bytes must decode first; a
/// stray UTF-8 continuation byte is not an identifier continuation.
pub fn ident_run_len(s: &str) -> usize {
    let mut chars = s.char_indices();
    match chars.next() {
        Some((_, first)) if is_ident_start(first) => chars
            .find(|(_, c)| !is_ident_continue(*c))
            .map_or(s.len(), |(i, _)| i),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_identifiers_are_accepted() {
        assert!(is_ident("x"));
        assert!(is_ident("_"));
        assert!(is_ident("_x9"));
        assert!(is_ident("snake_case_9"));
    }

    #[test]
    fn a_digit_may_continue_but_not_start() {
        assert!(is_ident_continue('9'));
        assert!(!is_ident_start('9'));
        assert!(is_ident("x9"));
        assert!(!is_ident("9x"));
    }

    /// §4.1: "Unicode identifiers are allowed".
    #[test]
    fn unicode_scalars_may_start_and_continue_an_identifier() {
        assert!(is_ident("λ"));
        assert!(is_ident("Ünicode"));
        assert!(is_ident("δx"));
        assert!(is_ident("日本語"));
    }

    /// A scalar outside the class is not an identifier, whatever its encoding:
    /// an ASCII symbol, an arrow and an emoji are all outside XID-Start, and so
    /// are the empty string and a name with a space in it.
    #[test]
    fn non_letter_scalars_are_not_identifiers() {
        assert!(!is_ident("+"));
        assert!(!is_ident("→"));
        assert!(!is_ident("🦀"));
        assert!(!is_ident(""));
        assert!(!is_ident("a b"));
    }

    #[test]
    fn a_run_ends_at_the_first_character_outside_the_class() {
        assert_eq!(ident_run_len("x9 rest"), 2);
        assert_eq!(ident_run_len("snake_case_9("), 12);
        assert_eq!(ident_run_len("x"), 1);
    }

    /// The run is measured in bytes but scanned in scalars, so a multi-byte
    /// scalar contributes its whole encoding and never a prefix of it.
    #[test]
    fn a_run_is_measured_in_bytes_over_whole_scalars() {
        assert_eq!(ident_run_len("δx+"), 3);
        assert_eq!(ident_run_len("日本語"), 9);
        assert_eq!(ident_run_len("λ→"), 2);
    }

    #[test]
    fn a_run_that_does_not_start_one_is_zero() {
        assert_eq!(ident_run_len(""), 0);
        assert_eq!(ident_run_len("9x"), 0);
        assert_eq!(ident_run_len("+x"), 0);
        assert_eq!(ident_run_len("\u{0301}x"), 0);
    }

    /// [`ident_run_len`] and [`is_ident`] are the same rule seen from two
    /// sides: a whole string is an identifier exactly when the run covers it.
    #[test]
    fn a_full_length_run_agrees_with_is_ident() {
        for s in ["x", "_x9", "Ünicode", "9x", "", "a b", "🦀"] {
            assert_eq!(is_ident(s), !s.is_empty() && ident_run_len(s) == s.len());
        }
    }

    /// A combining mark continues a name but cannot begin one, so `e\u{0301}`
    /// is an identifier and a bare combining acute is not.
    #[test]
    fn a_combining_mark_may_continue_but_not_start() {
        assert!(is_ident_continue('\u{0301}'));
        assert!(!is_ident_start('\u{0301}'));
        assert!(is_ident("e\u{0301}"));
        assert!(!is_ident("\u{0301}"));
    }
}
