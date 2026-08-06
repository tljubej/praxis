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
//! - [`ident`] — the one identifier character class (§4.1).
//! - [`interp`] — the one rule for where a `"…"` literal ends and where its
//!   interpolation holes are (§8.1, ADR-147), shared by the lexer's pre-scan
//!   and its resume path.
//! - [`literal`] — the one text-literal decoder (§4.3, IP-08).
//! - [`numeric`] — the one digit-separator rule for numeric literals (§4.3).
//! - [`template`] — the one rule for where a backtick template ends (§7.2,
//!   D10), shared by the lexer and the input parser's template scanner.
//! - [`language`] — the rowan `Language` impl and node aliases.
//! - [`span_bridge`] — `Span` ↔ `rowan::TextRange` conversions (the only place
//!   the two offset worlds meet; Praxis `Span` stays the diagnostic source of
//!   truth).
//!
//! [`SyntaxNode`]: language::SyntaxNode

pub mod ident;
pub mod interp;
pub mod kind;
pub mod language;
pub mod literal;
pub mod numeric;
pub mod span_bridge;
pub mod template;

pub use kind::SyntaxKind;
pub use language::{PraxisLanguage, SyntaxElement, SyntaxNode, SyntaxToken};

use praxis_source::Span;

/// How deeply backtick templates may nest inside each other's captures (D10).
///
/// A capture body is a full parser expression, so `` `{g:choice(A: `{x:int}`)}` ``
/// is one template containing another — which makes the lexer's template run
/// and the input parser's `scan_template` mutually recursive with the file's
/// own text. Both must refuse deep nesting rather than overflow the stack, and
/// they must refuse it at the *same* depth or one of them accepts what the
/// other cannot read. It lives here because `praxis-syntax` is the crate they
/// both already depend on.
///
/// The bound is far above anything a person writes.
pub const MAX_TEMPLATE_NESTING: usize = 32;

/// How deeply `"…"` literals may nest inside each other's interpolation holes
/// (§8.1, ADR-147).
///
/// A hole holds a full expression, so `"{f("{y}")}"` is one literal containing
/// another and [`interp::text_end`] is recursive with the file's own text. The
/// bound is what keeps adversarial input off the stack.
///
/// Unlike [`MAX_TEMPLATE_NESTING`], reaching this bound does not change what a
/// delimiter *means* — it refuses to enter, and the literal is reported as
/// unterminated. That difference is deliberate: the lexer's resume path only
/// ever runs for a literal the pre-scan proved closes, so a bound that answered
/// "closed, measured differently" would put the two on different rules at
/// exactly the depth nobody writes.
pub const MAX_INTERPOLATION_NESTING: usize = 32;

/// A token the lexer emits before it is folded into the lossless tree: its kind,
/// the source span it covers, and whether a line break sits in front of it.
///
/// The parser consumes these into a `rowan::GreenNode`; the spans are kept so
/// diagnostics can point at lexer-level locations (§6).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Token {
    pub kind: SyntaxKind,
    pub span: Span,
    /// True iff the trivia run immediately before this token contained a `\n`
    /// or `\r`, or was a line comment.
    ///
    /// A newline is trivia, so folding it into the tree loses the one fact
    /// statement separation needs: whether the next token starts a new line
    /// (D8, ADR-049). Recording it on the token is what lets the parser ask
    /// without re-reading the source, and what keeps the answer available after
    /// the trivia has already been emitted into the green tree.
    pub preceded_by_newline: bool,
}

impl Token {
    #[must_use]
    pub fn new(kind: SyntaxKind, span: Span, preceded_by_newline: bool) -> Token {
        Token {
            kind,
            span,
            preceded_by_newline,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_carries_kind_and_span() {
        let t = Token::new(SyntaxKind::IntLit, Span::new(0, 2), false);
        assert_eq!(t.kind, SyntaxKind::IntLit);
        assert_eq!(t.span, Span::new(0, 2));
        assert!(!t.preceded_by_newline);
    }

    #[test]
    fn a_token_records_whether_a_line_break_precedes_it() {
        let t = Token::new(SyntaxKind::Ident, Span::new(0, 1), true);
        assert!(t.preceded_by_newline);
    }
}
