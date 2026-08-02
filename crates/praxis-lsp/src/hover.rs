//! Hover (WS4, §15.2, §19.11 criterion 3).
//!
//! The whole query is a lookup: every type M11 can show was computed by
//! inference and recorded under a range or a node key. What this module adds is
//! **which** of those to prefer at a position, and Markdown around the answer.
//!
//! Types are rendered by `db.render` — the same function `praxis check` prints
//! through — so the editor and the CLI name a type the same way. A second
//! renderer here would be a second opinion about what `Vec[{ x: Int }]` is
//! called.

use crate::position::Encoding;
use crate::query::Snapshot;
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind};
use praxis_hir::ParserMode;

/// Hover at `offset`.
///
/// The preference order is innermost-wins, and it is deliberate:
///
/// 1. a parser expression, because inside a `read` body every other map is
///    silent and the enclosing `READ_EXPR`'s type would answer the root's type
///    for a cursor on an inner constructor;
/// 2. a method name, which is not a name reference and so is not in `refs`;
/// 3. a name reference, then a declaration site;
/// 4. the innermost expression node with a recorded type.
#[must_use]
pub fn hover(snapshot: &Snapshot, offset: u32, enc: Encoding) -> Option<Hover> {
    let analysis = snapshot.analyze();
    let positions = snapshot.positions();

    if let Some(info) = analysis.hover_parser(offset) {
        let title = match (info.is_root, info.mode) {
            (true, _) => "input parser result".to_string(),
            (false, ParserMode::AtomicName) => "capture parser".to_string(),
            (false, ParserMode::Capture) => "capture".to_string(),
            _ => "parser expression".to_string(),
        };
        let range = snapshot
            .input_parser_at(offset)
            .map(|idx| positions.text_range(idx.expr_range, enc));
        return Some(markdown(
            format!("```praxis\n{}\n```\n\n*{}*", info.rendered, title),
            range,
        ));
    }

    let token = snapshot.token_at(offset)?;
    let range = token.text_range();

    // A method name, then a name reference — `Analysis::hover` already decides
    // between those two, and it is the M2 query the LSP was always meant to
    // reuse rather than reimplement.
    if let Some(info) = analysis.hover(range) {
        return Some(markdown(
            format!("```praxis\n{}: {}\n```", info.name, info.scheme),
            Some(positions.text_range(range, enc)),
        ));
    }

    // A declaration site: `let x = 1`'s `x` is in `decls`, not in `refs`.
    if let Some(info) = analysis.hover_decl(range) {
        return Some(markdown(
            format!("```praxis\n{}: {}\n```", info.name, info.scheme),
            Some(positions.text_range(range, enc)),
        ));
    }

    // Any expression node with a recorded type. `expr_types` is keyed by
    // `NodeKey` — range **and** kind — so walking outward cannot pick up a
    // same-ranged node of the wrong kind.
    let db = &analysis.db;
    let (node_range, ty) = token.parent_ancestors().find_map(|node| {
        analysis
            .expr_types
            .get(&praxis_hir::NodeKey::of(&node))
            .map(|t| (node.text_range(), *t))
    })?;
    Some(markdown(
        format!("```praxis\n{}\n```", db.render(db.follow(ty))),
        Some(positions.text_range(node_range, enc)),
    ))
}

fn markdown(value: String, range: Option<lsp_types::Range>) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range,
    }
}
