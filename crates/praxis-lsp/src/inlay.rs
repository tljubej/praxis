//! Inlay hints (§15.2, §19.12).
//!
//! The editor writes the type the compiler inferred where the source does not:
//!
//! ```text
//! fn foo(a, b) { a + b }        fn foo(a: Int, b: Int) { a + b }
//! var total = 0            →    var total: Int = 0
//! for line in lines             for line: Text in lines
//! ```
//!
//! # What is hinted, and what is not
//!
//! **Every binding whose type the source does not already state**: a `fn`
//! parameter, a closure parameter, a `var`, a `for` variable, and a name a
//! pattern introduces. They are one rule because they are one thing — a name
//! bound to a value (ADR-125) — and the rule is read off `Analysis::decls`,
//! which is where every one of them already is. A hint next to an annotation the
//! author wrote would be the editor repeating the source back.
//!
//! A **parser root** (§15.2's fourth bullet) is hinted only where its type is
//! not already on screen: `var v = read lines(int)` gets one hint, on `v`, and
//! not two saying the same thing.
//!
//! # `?T` is shown, not hidden
//!
//! A parameter inference could not pin renders `?T`, and that is the hint —
//! `db.render`'s own spelling for an unbound variable, the same one hover and
//! `praxis check` use (§5.1). Suppressing it would make "no hint" mean two
//! different things: a type the source already states, and a type the compiler
//! does not know. Those are the two cases a reader most needs told apart.
//!
//! # The accept edit
//!
//! A hint carries a `TextEdit` that writes the annotation into the file — but
//! **only where the annotation is both legal and spellable**: on a `fn`/closure
//! parameter or a `var` (a `for` variable has no annotation syntax), and only
//! when the rendered type is one the parser would read back. `?T` is not, and
//! neither is an anonymous record; those hints show and cannot be accepted,
//! which is better than an edit that does not compile.

use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, Range, TextEdit};
use praxis_hir::SymbolKind;
use praxis_syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use rowan::{NodeOrToken, TextRange};

use crate::position::Encoding;
use crate::query::Snapshot;

/// Every hint in `range`, in source order.
///
/// `range` is the client's visible window; hints outside it are computed and
/// dropped rather than not computed, because the analysis they read is memoized
/// and the filter is a comparison.
#[must_use]
pub fn hints(snapshot: &Snapshot, range: Range, enc: Encoding) -> Vec<InlayHint> {
    let positions = snapshot.positions();
    let visible = positions.span(range, enc);
    let mut out: Vec<(u32, InlayHint)> = Vec::new();

    for (offset, hint) in binding_hints(snapshot, enc)
        .into_iter()
        .chain(parser_root_hints(snapshot, enc))
    {
        if offset < visible.start().to_u32() || offset > visible.end().to_u32() {
            continue;
        }
        out.push((offset, hint));
    }
    out.sort_by_key(|(offset, _)| *offset);
    out.into_iter().map(|(_, hint)| hint).collect()
}

/// One hint per declaration the source does not annotate.
fn binding_hints(snapshot: &Snapshot, enc: Encoding) -> Vec<(u32, InlayHint)> {
    let analysis = snapshot.analyze();
    let db = &analysis.db;
    let mut out = Vec::new();

    for (range, symbol_id) in &analysis.decls {
        let Some(symbol) = analysis.names.get(*symbol_id) else {
            continue;
        };
        // A `fn`, `struct` or `enum` declares its own name; only *bindings*
        // have an inferred type to show.
        if !matches!(symbol.kind, SymbolKind::Var | SymbolKind::Param) {
            continue;
        }
        let Some(scheme) = symbol.scheme.as_ref() else {
            continue;
        };
        let Some(token) = name_token_at(snapshot, *range) else {
            continue;
        };
        let owner = annotatable_owner(&token);
        if owner.as_ref().is_some_and(has_annotation) {
            continue;
        }
        // The scheme's own rendering, so an unbound variable arrives as `?T`
        // and a generalized one as `T` — the spelling every other surface uses.
        let rendered = db.render_scheme(scheme);
        let at = u32::from(range.end());
        out.push((
            at,
            InlayHint {
                position: snapshot.positions().position(at, enc),
                label: InlayHintLabel::String(format!(": {rendered}")),
                kind: Some(InlayHintKind::TYPE),
                text_edits: owner.filter(|_| is_spellable(&rendered)).map(|_| {
                    vec![TextEdit {
                        range: Range {
                            start: snapshot.positions().position(at, enc),
                            end: snapshot.positions().position(at, enc),
                        },
                        new_text: format!(": {rendered}"),
                    }]
                }),
                tooltip: None,
                padding_left: Some(false),
                padding_right: Some(false),
                data: None,
            },
        ));
    }
    out
}

/// A hint for a `read`/`parse` body whose result type is not already shown by
/// the binding it initializes.
fn parser_root_hints(snapshot: &Snapshot, enc: Encoding) -> Vec<(u32, InlayHint)> {
    let analysis = snapshot.analyze();
    let db = &analysis.db;
    let mut out = Vec::new();

    for index in &analysis.parser_exprs {
        let start = u32::from(index.expr_range.start());
        let Some(ty) = index.type_at(start) else {
            continue;
        };
        let Some(expr) = enclosing_read(snapshot, index.expr_range) else {
            continue;
        };
        // The binding's own hint already says this, one column to the left.
        if expr
            .parent()
            .is_some_and(|p| p.kind() == SyntaxKind::VAR_STMT)
        {
            continue;
        }
        // After the whole `read …`/`parse(…)` expression, which is where the
        // value it produces appears — not after the parser body, where
        // `parse("1", int)` would read as though `int` had a named argument.
        let at = u32::from(expr.text_range().end());
        out.push((
            at,
            InlayHint {
                position: snapshot.positions().position(at, enc),
                label: InlayHintLabel::String(format!(": {}", db.render(db.follow(ty)))),
                kind: Some(InlayHintKind::TYPE),
                text_edits: None,
                tooltip: None,
                padding_left: Some(true),
                padding_right: Some(false),
                data: None,
            },
        ));
    }
    out
}

/// The `read`/`parse` expression a parser body belongs to.
fn enclosing_read(snapshot: &Snapshot, range: TextRange) -> Option<SyntaxNode> {
    snapshot
        .token_at(u32::from(range.start()))?
        .parent_ancestors()
        .find(|node| matches!(node.kind(), SyntaxKind::READ_EXPR | SyntaxKind::PARSE_EXPR))
}

/// The `Ident` token at a declaration's range.
fn name_token_at(snapshot: &Snapshot, range: TextRange) -> Option<SyntaxToken> {
    let token = snapshot.token_at(u32::from(range.start()))?;
    (token.text_range() == range && token.kind() == SyntaxKind::Ident).then_some(token)
}

/// The node that *could* carry an annotation for this name, if any.
///
/// A `PARAM` and a `VAR_STMT` are the two forms with annotation syntax, and only
/// when the name being declared is their own — the `q` of `var f = |q| q` is the
/// closure parameter's, not the `var`'s, so the walk stops at the innermost of
/// the two and checks that the name it declares is this one.
fn annotatable_owner(token: &SyntaxToken) -> Option<SyntaxNode> {
    let owner = token
        .parent_ancestors()
        .find(|n| matches!(n.kind(), SyntaxKind::PARAM | SyntaxKind::VAR_STMT))?;
    (declared_name(&owner).as_ref().map(SyntaxToken::text_range) == Some(token.text_range()))
        .then_some(owner)
}

/// The name a `PARAM` or `VAR_STMT` declares: its first identifier that is not
/// part of a type.
///
/// **Not its first child token.** A `fn` parameter writes its name as a bare
/// `Ident`, and a *closure* parameter writes a `PATTERN` around it — so a search
/// of direct children would find the first and miss the second, leaving
/// `|q: Int|` with a hint next to the annotation it already has.
fn declared_name(owner: &SyntaxNode) -> Option<SyntaxToken> {
    owner
        .descendants_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|t| {
            t.kind() == SyntaxKind::Ident && !t.parent_ancestors().any(|n| n.kind().is_type_node())
        })
}

/// Whether the source already writes this declaration's type.
fn has_annotation(owner: &SyntaxNode) -> bool {
    owner.children().any(|c| c.kind().is_type_node())
}

/// Whether a rendered type is one the parser would read back as an annotation.
///
/// Deliberately narrow. `?T` names a variable nothing binds, `{ x: Int }` is a
/// structural record with no annotation syntax, and a function type's spelling
/// is not this module's to guess — so those hints show and offer no edit.
fn is_spellable(rendered: &str) -> bool {
    !rendered.is_empty()
        && !rendered.contains('?')
        && !rendered.contains('{')
        && !rendered.contains('>')
        && !rendered.contains("forall")
}
