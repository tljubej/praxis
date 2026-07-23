//! Token and syntax-node definitions for the Praxis language.
//!
//! Per §14.1 of the design, this crate owns the token kinds and the lossless
//! syntax tree node kinds (the full lossless tree lands in Milestone 1). For
//! Milestone 0 it provides the [`TokenKind`] enumeration consumed by the lexer
//! stub in `praxis-parser`, plus a minimal [`Token`] that pairs a kind with a
//! source span.

use praxis_source::Span;

/// The lexical classification of a token.
///
/// This is the *vocabulary* the lexer produces; it is intentionally small for
/// Milestone 0 (just enough to walk a file and emit a real diagnostic). The
/// full token set, including every keyword and operator, is added in Milestone 1.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenKind {
    /// Whitespace: a run of spaces, tabs, and newlines outside a comment.
    Whitespace,
    /// A `//` line comment (not including the newline).
    LineComment,
    /// A nestable `/* ... */` block comment (entire span, including delimiters).
    BlockComment,
    /// An identifier or keyword. The lexer stub does not distinguish keywords
    /// yet; the M1 lexer will split them out.
    Ident,
    /// An integer literal, e.g. `42`.
    IntLit,
    /// A run of punctuation. The lexer stub collapses multi-char operators into
    /// one `Punct` token; M1 will split them precisely.
    Punct,
    /// A backtick-delimited parser template, e.g. `` `{x:int}` ``. The lexer
    /// stub treats the whole template as one token so the inner template lexer
    /// (M6) can re-scan it later.
    BacktickTemplate,
    /// A byte the lexer stub does not recognize. The lexer emits a real
    /// diagnostic for these rather than silently dropping them.
    Unknown,
    /// End of input.
    Eof,
}

impl TokenKind {
    /// Whether this kind is "trivia": whitespace or a comment. Trivia is kept
    /// in the lossless tree (§13.1) but ignored for parsing decisions.
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment
        )
    }
}

/// A token: its kind and the source span it covers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span) -> Token {
        Token { kind, span }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trivia_classification() {
        assert!(TokenKind::Whitespace.is_trivia());
        assert!(TokenKind::LineComment.is_trivia());
        assert!(TokenKind::BlockComment.is_trivia());
        assert!(!TokenKind::Ident.is_trivia());
        assert!(!TokenKind::Punct.is_trivia());
    }

    #[test]
    fn token_carries_span() {
        let t = Token::new(TokenKind::IntLit, Span::new(0, 2));
        assert_eq!(t.kind, TokenKind::IntLit);
        assert_eq!(t.span, Span::new(0, 2));
    }
}
