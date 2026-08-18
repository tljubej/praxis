//! Typed wrappers over lossless syntax nodes (§13.2, §14.1).
//!
//! Each wrapper ([`SourceFile`], [`VarStmt`], [`PathExpr`], …) is a strongly-typed
//! view over a [`SyntaxNode`] whose [`SyntaxKind`] is fixed by construction. They
//! avoid copying source strings: accessors borrow into the underlying green tree.
//!
//! The wrappers are deliberately minimal: a node is wrapped when a consumer
//! needs it (ADR-009), not ahead of one.
//!
//! Naming note: source-language *names* (binding sites and references) are bare
//! `Ident` tokens in the tree; there is no `NAME`/`NAME_REF` wrapper node. The
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

/// Declare an [`AstNode`] wrapper: the newtype over [`SyntaxNode`] and the
/// trait `impl` that fixes its kind.
///
/// Every wrapper in [`nodes`] is the same four items — doc comment, the
/// `Clone, Debug` derive, a one-field struct, and an `impl` whose only content
/// is the kind constant. Written out, each copy would be a place for a
/// copy-paste to name the *wrong* [`SyntaxKind`](praxis_syntax::SyntaxKind):
/// that compiles, and then the wrapper silently casts nodes it was never meant
/// to accept. Here the kind is written once, beside the name it belongs to.
///
/// A wrapper needing more than the trait's default `cast` — [`TypeRef`], which
/// accepts all three type-node kinds — is written out by hand instead.
macro_rules! ast_node {
    ($(#[$attr:meta])* $name:ident, $kind:ident) => {
        $(#[$attr])*
        #[derive(Clone, Debug)]
        pub struct $name {
            syntax: $crate::SyntaxNode,
        }

        impl $crate::AstNode for $name {
            const KIND: praxis_syntax::SyntaxKind = praxis_syntax::SyntaxKind::$kind;

            fn from_syntax(syntax: $crate::SyntaxNode) -> Self {
                Self { syntax }
            }

            fn syntax(&self) -> &$crate::SyntaxNode {
                &self.syntax
            }
        }
    };
}
pub(crate) use ast_node;

/// Find the first child of `parent` whose kind is `N::KIND` and cast it.
pub fn child<N: AstNode>(parent: &SyntaxNode) -> Option<N> {
    parent.children().find_map(N::cast)
}

/// All children of `parent` whose kind is `N::KIND`, in order.
pub fn children<N: AstNode>(parent: &SyntaxNode) -> impl Iterator<Item = N> + use<N> {
    parent.children().filter_map(N::cast)
}

/// The first **direct** token child of `node` whose kind satisfies `pred`.
///
/// *Direct* is the load-bearing word: a token nested inside a child node belongs
/// to that node and is skipped here. It is what lets [`MethodCallExpr`]'s method
/// name be found past the receiver's `Ident` (which lives inside the receiver's
/// own node), and what keeps [`Pattern::kind`] from reading a tuple element's or
/// record field's token as the pattern's own.
pub fn token_matching(
    node: &SyntaxNode,
    pred: impl Fn(praxis_syntax::SyntaxKind) -> bool,
) -> Option<praxis_syntax::SyntaxToken> {
    node.children_with_tokens().find_map(|e| match e {
        NodeOrToken::Token(t) if pred(t.kind()) => Some(t),
        _ => None,
    })
}

/// The first `Ident` token child of `node` (the spelling of a name, since names
/// are bare `Ident` tokens — see the module docs). Returns `None` if absent
/// (e.g. a malformed `var` recovered by the parser).
///
/// Every accessor that reads a name goes through here, and that is what makes
/// the module doc's promise good: re-nesting names into `NAME` nodes would be an
/// edit to this one function rather than a hunt for hand-written `Ident` scans.
pub fn name_token(node: &SyntaxNode) -> Option<praxis_syntax::SyntaxToken> {
    token_matching(node, |k| k == praxis_syntax::SyntaxKind::Ident)
}

#[cfg(test)]
#[path = "ast_tests.rs"]
mod ast_tests;
