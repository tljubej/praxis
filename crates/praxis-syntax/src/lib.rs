//! Token and syntax-node definitions for the Praxis language.
//!
//! Per §14.1 of the design, this crate owns the token kinds and the lossless
//! syntax tree node kinds. From Milestone 1 onward (ADR-003) the tree is a
//! [`rowan`]-backed lossless tree: this crate contributes the [`SyntaxKind`]
//! vocabulary, the [`PraxisLanguage`] tag that binds it to rowan, and the
//! [`SyntaxNode`]/[`SyntaxToken`]/[`SyntaxElement`] type aliases.
//!
//! The modules:
//! - [`kind`] — the single `SyntaxKind` enum (tokens, trivia, tree nodes).
//! - [`language`] — the rowan `Language` impl and node aliases.
//! - [`span_bridge`] — `Span` ↔ `rowan::TextRange` conversions (the only place
//!   the two offset worlds meet; Praxis `Span` stays the diagnostic source of
//!   truth).
//!
//! [`SyntaxNode`]: language::SyntaxNode

pub mod ident;
pub mod kind;
pub mod language;
pub mod span_bridge;

pub use kind::SyntaxKind;
pub use language::{PraxisLanguage, SyntaxElement, SyntaxNode, SyntaxToken};

use praxis_source::Span;

/// A token the lexer emits before it is folded into the lossless tree: its kind
/// and the source span it covers.
///
/// The parser consumes these into a `rowan::GreenNode`; the spans are kept so
/// diagnostics can point at lexer-level locations (§6).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Token {
    pub kind: SyntaxKind,
    pub span: Span,
}

impl Token {
    #[must_use]
    pub fn new(kind: SyntaxKind, span: Span) -> Token {
        Token { kind, span }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_carries_kind_and_span() {
        let t = Token::new(SyntaxKind::IntLit, Span::new(0, 2));
        assert_eq!(t.kind, SyntaxKind::IntLit);
        assert_eq!(t.span, Span::new(0, 2));
    }
}
