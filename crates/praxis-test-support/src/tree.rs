//! Pretty-print a Praxis lossless syntax tree for golden-tree tests (§17.1).
//!
//! The output is a stable, indented dump in the rust-analyzer style:
//!
//! ```text
//! SOURCE_FILE@0..11
//!   VAR_STMT@0..10
//!     KW_VAR "var "@0..4
//!     IDENT "x"@4..5
//!     ...
//! ```
//!
//! Every node and token carries its `SyntaxKind` and its byte range; tokens also
//! carry their (escaped) text. Trivia is included because the tree is lossless
//! (§13.1) — that is exactly what the formatter and the LSP rely on, so the
//! golden trees must show it. The format is deterministic and human-reviewable,
//! which is what `insta` snapshots want.

use praxis_syntax::{SyntaxKind, SyntaxNode};
use rowan::NodeOrToken;

/// Render `node` and all its descendants into the stable golden-tree format.
///
/// Indentation is two spaces per level. Token text is debug-quoted so newlines
/// and quotes inside it do not break the snapshot layout.
#[must_use]
pub fn format_syntax_tree(node: &SyntaxNode) -> String {
    let mut out = String::new();
    render_node(node, 0, &mut out);
    out
}

fn render_node(node: &SyntaxNode, depth: usize, out: &mut String) {
    indent(depth, out);
    out.push_str(&kind_label(node.kind()));
    out.push_str(&range_suffix(&node.text_range()));
    out.push('\n');
    for child in node.children_with_tokens() {
        match child {
            NodeOrToken::Node(child) => render_node(&child, depth + 1, out),
            NodeOrToken::Token(token) => render_token(
                token.kind(),
                token.text(),
                token.text_range(),
                depth + 1,
                out,
            ),
        }
    }
}

fn render_token(
    kind: SyntaxKind,
    text: &str,
    range: rowan::TextRange,
    depth: usize,
    out: &mut String,
) {
    indent(depth, out);
    out.push_str(&kind_label(kind));
    out.push(' ');
    // Debug-format the text so escapes are visible and newlines don't wrap.
    out.push_str(&format!("{text:?}"));
    out.push_str(&range_suffix(&range));
    out.push('\n');
}

fn indent(depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// The label for a kind in the dump. Node kinds and tokens print as their
/// `SyntaxKind` name; this keeps the output stable and greppable.
fn kind_label(kind: SyntaxKind) -> String {
    // Use the Debug name (`KW_VAR`, `VAR_STMT`, …) without the enum path.
    let name = format!("{kind:?}");
    name
}

fn range_suffix(range: &rowan::TextRange) -> String {
    let start: u32 = (*range).start().into();
    let end: u32 = (*range).end().into();
    format!("@{start}..{end}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_syntax::{PraxisLanguage, SyntaxKind};
    use rowan::{GreenNodeBuilder, Language};

    /// Build a tiny tree by hand to verify the printer format without depending
    /// on the parser (which is the consumer of this helper, not a dependency).
    fn manual_tree() -> SyntaxNode {
        // Emulates `var x= 1` as: SOURCE_FILE > VAR_STMT > { KW_VAR "var ", IDENT "x", EQ "=", WS " ", INT "1" }
        let mut b = GreenNodeBuilder::new();
        b.start_node(PraxisLanguage::kind_to_raw(SyntaxKind::SOURCE_FILE));
        b.start_node(PraxisLanguage::kind_to_raw(SyntaxKind::VAR_STMT));
        b.token(PraxisLanguage::kind_to_raw(SyntaxKind::KW_VAR), "var ");
        b.token(PraxisLanguage::kind_to_raw(SyntaxKind::Ident), "x");
        b.token(PraxisLanguage::kind_to_raw(SyntaxKind::EQ), "=");
        b.token(PraxisLanguage::kind_to_raw(SyntaxKind::Whitespace), " ");
        b.token(PraxisLanguage::kind_to_raw(SyntaxKind::IntLit), "1");
        b.finish_node();
        b.finish_node();
        let green = b.finish();
        SyntaxNode::new_root(green)
    }

    #[test]
    fn prints_nodes_tokens_and_ranges() {
        let tree = manual_tree();
        let dump = format_syntax_tree(&tree);
        insta::assert_snapshot!(dump, @r#"
        SOURCE_FILE@0..8
          VAR_STMT@0..8
            KW_VAR "var "@0..4
            Ident "x"@4..5
            EQ "="@5..6
            Whitespace " "@6..7
            IntLit "1"@7..8
        "#);
    }

    #[test]
    fn round_trip_text_matches_source() {
        // The lossless property: concatenating all token text reproduces source.
        let tree = manual_tree();
        assert_eq!(tree.to_string(), "var x= 1");
        // (The manually built tree's text is exactly what we put in.)
    }
}
