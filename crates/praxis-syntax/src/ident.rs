//! The one identifier character class for the whole workspace (§4.1).
//!
//! §4.1 allows Unicode identifiers. Before this module there were three
//! independent, mutually inconsistent rules: the lexer classified *bytes* and
//! accepted every byte `>= 0x80` as an identifier continuation (so a stray
//! UTF-8 continuation byte extended a name, and a leading Unicode scalar was
//! split into "unexpected byte" diagnostics), the input parser's capture-name
//! splitter used an ASCII-only rule, and the debugger rewrote names with a
//! third rule.
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

    /// §4.1: "Unicode identifiers are allowed". The lexer's old byte
    /// classifier rejected these outright.
    #[test]
    fn unicode_scalars_may_start_and_continue_an_identifier() {
        assert!(is_ident("λ"));
        assert!(is_ident("Ünicode"));
        assert!(is_ident("δx"));
        assert!(is_ident("日本語"));
    }

    /// The old byte rule accepted every byte `>= 0x80`, which is every symbol,
    /// emoji and UTF-8 continuation byte. None of those is an identifier.
    #[test]
    fn non_letter_scalars_are_not_identifiers() {
        assert!(!is_ident("+"));
        assert!(!is_ident("→"));
        assert!(!is_ident("🦀"));
        assert!(!is_ident(""));
        assert!(!is_ident("a b"));
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
