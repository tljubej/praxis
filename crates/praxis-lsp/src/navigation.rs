//! Go-to-definition and document symbols (WS7, §15.2).
//!
//! **Find references, rename and workspace symbols are M12** (§19.12) and are
//! deliberately absent: they are the multi-file half of navigation, and M11's
//! query layer is one document.
//!
//! Definition is a two-step lookup that already exists —
//! `refs[range].symbol → Symbol.decl` — and the reason it is worth a gate is
//! that a *name match* would also appear to work. Two shadowed bindings share a
//! name and have distinct `SymbolId`s; only the symbol table tells them apart.

use lsp_types::{DocumentSymbol, Location, SymbolKind as LspSymbolKind, Uri};
use praxis_syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use rowan::NodeOrToken;

use crate::position::Encoding;
use crate::query::Snapshot;

/// The definition site of the name at `offset`.
#[must_use]
pub fn goto_definition(
    snapshot: &Snapshot,
    offset: u32,
    uri: &Uri,
    enc: Encoding,
) -> Option<Location> {
    let analysis = snapshot.analyze();
    let token = snapshot.token_at(offset)?;
    let range = token.text_range();

    // A reference resolves through the symbol table, which distinguishes two
    // shadowed bindings of the same name.
    let symbol_id = analysis
        .refs
        .get(&range)
        .map(|r| r.symbol)
        // Already on the declaration: answering it keeps "go to definition" from
        // being a dead key at the one place the user knows the answer.
        .or_else(|| analysis.decls.get(&range).copied())?;
    let decl = analysis.names.get(symbol_id)?.decl?;
    Some(Location {
        uri: uri.clone(),
        range: snapshot.positions().range(decl.span, enc),
    })
}

/// The document's top-level symbols: `fn`, `struct`, `enum`, and bindings.
///
/// Nested `fn`s do not exist (§4.9 forbids them, `N005`), so a two-level tree —
/// items, and a struct's fields or an enum's variants under them — is the whole
/// shape.
#[must_use]
pub fn document_symbols(snapshot: &Snapshot, enc: Encoding) -> Vec<DocumentSymbol> {
    let tree = snapshot.tree().clone();
    let mut out = Vec::new();

    for item in tree.children() {
        let Some(name) = name_token(&item) else {
            continue;
        };
        let (kind, children) = match item.kind() {
            SyntaxKind::FN_ITEM => (LspSymbolKind::FUNCTION, Vec::new()),
            SyntaxKind::STRUCT_ITEM => (
                LspSymbolKind::STRUCT,
                members(
                    &item,
                    SyntaxKind::FIELD,
                    LspSymbolKind::FIELD,
                    snapshot,
                    enc,
                ),
            ),
            SyntaxKind::ENUM_ITEM => (
                LspSymbolKind::ENUM,
                members(
                    &item,
                    SyntaxKind::ENUM_VARIANT,
                    LspSymbolKind::ENUM_MEMBER,
                    snapshot,
                    enc,
                ),
            ),
            SyntaxKind::LET_STMT => (LspSymbolKind::CONSTANT, Vec::new()),
            SyntaxKind::VAR_STMT => (LspSymbolKind::VARIABLE, Vec::new()),
            _ => continue,
        };
        out.push(symbol(
            name.text().to_string(),
            kind,
            item.text_range(),
            name.text_range(),
            children,
            snapshot,
            enc,
        ));
    }
    out
}

fn members(
    item: &SyntaxNode,
    child_kind: SyntaxKind,
    lsp_kind: LspSymbolKind,
    snapshot: &Snapshot,
    enc: Encoding,
) -> Vec<DocumentSymbol> {
    item.descendants()
        .filter(|n| n.kind() == child_kind)
        .filter_map(|n| {
            let name = name_token(&n)?;
            Some(symbol(
                name.text().to_string(),
                lsp_kind,
                n.text_range(),
                name.text_range(),
                Vec::new(),
                snapshot,
                enc,
            ))
        })
        .collect()
}

#[allow(clippy::too_many_arguments, deprecated)]
fn symbol(
    name: String,
    kind: LspSymbolKind,
    full: rowan::TextRange,
    selection: rowan::TextRange,
    children: Vec<DocumentSymbol>,
    snapshot: &Snapshot,
    enc: Encoding,
) -> DocumentSymbol {
    let positions = snapshot.positions();
    DocumentSymbol {
        name,
        detail: None,
        kind,
        tags: None,
        // `deprecated` is deprecated in the protocol and required by the struct.
        deprecated: None,
        range: positions.text_range(full, enc),
        selection_range: positions.text_range(selection, enc),
        children: (!children.is_empty()).then_some(children),
    }
}

/// The declared name of an item: its first `Ident` token, which is what every
/// declaration form in the grammar puts first after its keyword.
fn name_token(node: &SyntaxNode) -> Option<SyntaxToken> {
    node.children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|t| t.kind() == SyntaxKind::Ident)
}
