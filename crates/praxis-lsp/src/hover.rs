//! Hover (WS4, §15.2, §19.11 criterion 3; documentation is M12's).
//!
//! The whole query is a lookup: every type shown here was computed by inference
//! and recorded under a range or a node key. What this module adds is **which**
//! of those to prefer at a position, and Markdown around the answer.
//!
//! Types are rendered by `db.render` — the same function `praxis check` prints
//! through — so the editor and the CLI name a type the same way. A second
//! renderer here would be a second opinion about what `Vec[{ x: Int }]` is
//! called.
//!
//! # Documentation (§19.12)
//!
//! A method's is `MethodEntry::doc`, from the catalog dispatch selected — the
//! *entry*, not a lookup by name, so an overload-free catalog row and the
//! sentence shown are the same row. A parser constructor's and an atomic's are
//! `Constructor::doc`/`AtomicKind::doc`, the closed tables in
//! `praxis-input-parser`. None of the three is written here: a language server
//! that carried its own description of `lines` would be free to describe a
//! constructor the compiler no longer has.

use crate::position::Encoding;
use crate::query::Snapshot;
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind};
use praxis_hir::ParserMode;
use praxis_input_parser::{AtomicKind, Constructor};
use praxis_source::Span;

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
        // The constructor or atomic the cursor is *on*, when it is on one: its
        // own signature and documentation say more than the type its whole
        // subtree synthesizes (§19.12's "parser documentation in hover").
        let mut body = match parser_name_at(snapshot, offset) {
            Some(named) => format!(
                "```praxis\n{}\n```\n\n{}\n\n---\n\n```praxis\n{}\n```\n\n*{}*",
                named.signature, named.doc, info.rendered, title
            ),
            None => format!("```praxis\n{}\n```\n\n*{}*", info.rendered, title),
        };
        body.push('\n');
        return Some(markdown(body, range));
    }

    let token = snapshot.token_at(offset)?;
    let range = token.text_range();

    // A method name, then a name reference — `Analysis::hover` already decides
    // between those two, and it is the M2 query the LSP was always meant to
    // reuse rather than reimplement.
    if let Some(info) = analysis.hover(range) {
        let mut body = format!("```praxis\n{}: {}\n```", info.name, info.scheme);
        // A method call has a catalog entry behind it, and the entry carries the
        // full signature and one line of documentation (§16.2).
        if let Some(m) = analysis.method_refs.get(&range) {
            let db = &analysis.db;
            let params: Vec<String> = m.entry.params.iter().map(ToString::to_string).collect();
            body = format!(
                "```praxis\n{}.{}({}) -> {}\n```\n\n{}",
                db.render(db.follow(m.receiver)),
                m.entry.name,
                params.join(", "),
                db.render(db.follow(m.result)),
                m.entry.doc
            );
        }
        return Some(markdown(body, Some(positions.text_range(range, enc))));
    }

    // A declaration site: `var x = 1`'s `x` is in `decls`, not in `refs`.
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

/// A parser name the cursor is on, with what to say about it.
struct NamedParser {
    signature: String,
    doc: &'static str,
}

/// The constructor or atomic whose **name** covers `offset`, if any.
///
/// Read from ADR-098's index — `constructors()` and `atomics()` carry the spans
/// the compiler computed — so "is the cursor on `lines`" is not a question this
/// crate answers by scanning text. The innermost wins, which for a capture whose
/// parser is a constructor (`{xs:csv(int)}`) is the constructor rather than the
/// capture around it.
fn parser_name_at(snapshot: &Snapshot, offset: u32) -> Option<NamedParser> {
    let index = snapshot.input_parser_at(offset)?;
    let covers = |span: Span| (span.start().to_u32()..=span.end().to_u32()).contains(&offset);

    if let Some((_, keyword)) = index.constructors().into_iter().find(|(s, _)| covers(*s)) {
        let ctor = Constructor::from_keyword(keyword)?;
        return Some(NamedParser {
            signature: crate::signature::constructor_signature(ctor),
            doc: ctor.doc(),
        });
    }
    let (_, kind) = index.atomics().into_iter().find(|(s, _)| covers(*s))?;
    Some(NamedParser {
        signature: atomic_signature(kind),
        doc: kind.doc(),
    })
}

/// An atomic's one-line signature: its name and the type it reads.
///
/// The result type comes from the synthesizer's own table rather than a second
/// list here — `synthesize` is what decides that `digit` is an `Int`.
fn atomic_signature(kind: AtomicKind) -> String {
    let mut db = praxis_types::TypeDb::new();
    let ty = praxis_input_parser::synthesize(
        &praxis_input_parser::ParserAst::Atomic {
            kind,
            span: Span::new(0u32, 0u32),
        },
        &mut db,
    );
    match ty {
        Ok(t) => format!("{} -> {}", kind.keyword(), db.render(db.follow(t))),
        Err(_) => kind.keyword().to_string(),
    }
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
