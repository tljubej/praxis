//! Typed wrappers over lossless syntax nodes (§13.2, §14.1).
//!
//! Each wrapper ([`SourceFile`], [`VarStmt`], [`PathExpr`], …) is a strongly-typed
//! view over a [`SyntaxNode`] whose [`SyntaxKind`] is fixed by construction. They
//! avoid copying source strings: accessors borrow into the underlying green tree.
//!
//! The wrappers are deliberately minimal — only the nodes M2 (name resolution +
//! type inference) needs were added (ADR-009). More land as later milestones
//! consume them.
//!
//! Naming note: source-language *names* (binding sites and references) are bare
//! `Ident` tokens in M1's tree; there is no `NAME`/`NAME_REF` wrapper node. The
//! wrappers here expose those `Ident` tokens directly. Re-nesting names is a
//! refinement that could land later without breaking these accessors.

pub mod nodes;

pub use nodes::*;
pub use praxis_syntax::SyntaxNode;

use praxis_source::Span;
use praxis_syntax::span_bridge::range_to_span;
use rowan::NodeOrToken;

/// A typed view over a [`SyntaxNode`] of a fixed kind.
///
/// Implementors carry a `const KIND` and provide `cast`, `syntax`, and (via this
/// trait) `span`. `cast` returns `None` when the node's kind does not match, so a
/// wrongly-typed wrapper is unrepresentable.
pub trait AstNode {
    /// The single [`SyntaxKind`] this wrapper accepts.
    const KIND: praxis_syntax::SyntaxKind;

    /// Wrap `syntax`, or `None` if its kind is not [`KIND`](Self::KIND).
    fn cast(syntax: SyntaxNode) -> Option<Self>
    where
        Self: Sized,
    {
        if syntax.kind() == Self::KIND {
            Some(Self::from_syntax(syntax))
        } else {
            None
        }
    }

    /// Construct from a node whose kind is already known to be correct. Used by
    /// the default `cast`; implementors provide this to store the node.
    #[must_use]
    fn from_syntax(syntax: SyntaxNode) -> Self
    where
        Self: Sized;

    /// The wrapped node.
    fn syntax(&self) -> &SyntaxNode;

    /// The source span of this node (via the span bridge).
    #[must_use]
    fn span(&self) -> Span {
        range_to_span(self.syntax().text_range())
    }
}

/// Find the first child of `parent` whose kind is `N::KIND` and cast it.
pub fn child<N: AstNode>(parent: &SyntaxNode) -> Option<N> {
    parent.children().find_map(N::cast)
}

/// All children of `parent` whose kind is `N::KIND`, in order.
pub fn children<N: AstNode>(parent: &SyntaxNode) -> impl Iterator<Item = N> {
    parent.children().filter_map(N::cast)
}

/// The first `Ident` token child of `node` (the spelling of a name, since names
/// are bare `Ident` tokens — see the module docs). Returns `None` if absent
/// (e.g. a malformed `var` recovered by the parser).
pub fn name_token(node: &SyntaxNode) -> Option<praxis_syntax::SyntaxToken> {
    node.children_with_tokens()
        .filter_map(|e| match e {
            NodeOrToken::Token(t) => Some(t),
            NodeOrToken::Node(_) => None,
        })
        .find(|t| t.kind() == praxis_syntax::SyntaxKind::Ident)
}

#[cfg(test)]
#[path = "ast_tests.rs"]
mod ast_tests;
