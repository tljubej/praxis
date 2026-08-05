//! Go-to-definition, find references, and document symbols (WS7, §15.2).
//!
//! Every query here turns on one thing: a name's **symbol**, not its spelling.
//! Two shadowed bindings share a word and have distinct `SymbolId`s, so a query
//! that matched text would answer plausibly and wrongly — definition would land
//! on whichever declaration came first, and references would return both
//! bindings' uses as though they were one.
//!
//! Definition is a two-step lookup: `refs[range].symbol → Symbol.decl`.
//! References is the same map read the other way, and it is what rename edits.

use lsp_types::{DocumentSymbol, Location, SymbolKind as LspSymbolKind, Uri};
use praxis_hir::SymbolId;
use praxis_syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use rowan::{NodeOrToken, TextRange};

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

/// The symbol the name at `offset` denotes — a use or its declaration.
///
/// The one lookup every symbol-shaped query starts from: references, rename,
/// and definition all ask it, so "which binding is this word" is decided once.
#[must_use]
pub fn symbol_at(snapshot: &Snapshot, offset: u32) -> Option<SymbolId> {
    let analysis = snapshot.analyze();
    let range = snapshot.token_at(offset)?.text_range();
    analysis
        .refs
        .get(&range)
        .map(|r| r.symbol)
        .or_else(|| analysis.decls.get(&range).copied())
}

/// Every source range that names `symbol`: its declaration site, and every
/// reference that resolved to it.
///
/// In source order, and **disjoint from every other binding's** — which is the
/// property a text search cannot have. A shadowed `var a` and the `var a` that
/// shadows it are two symbols, so asking about one never returns the other's
/// uses.
#[must_use]
pub fn reference_ranges(snapshot: &Snapshot, symbol: SymbolId) -> Vec<TextRange> {
    let analysis = snapshot.analyze();
    let mut out: Vec<TextRange> = analysis
        .decls
        .iter()
        .filter(|(_, id)| **id == symbol)
        .map(|(range, _)| *range)
        .chain(
            analysis
                .refs
                .iter()
                .filter(|(_, r)| r.symbol == symbol)
                .map(|(range, _)| *range),
        )
        .collect();
    out.sort_by_key(|r| (r.start(), r.end()));
    out.dedup();
    out
}

/// `textDocument/references` at `offset`.
///
/// `include_declaration` is the client's own flag and is honoured rather than
/// ignored: an editor that asks for uses only should not be told about the
/// `var` line it is standing on.
#[must_use]
pub fn references(
    snapshot: &Snapshot,
    offset: u32,
    uri: &Uri,
    enc: Encoding,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let symbol = symbol_at(snapshot, offset)?;
    let analysis = snapshot.analyze();
    let positions = snapshot.positions();
    let is_decl = |range: &TextRange| analysis.decls.get(range) == Some(&symbol);
    Some(
        reference_ranges(snapshot, symbol)
            .into_iter()
            .filter(|range| include_declaration || !is_decl(range))
            .map(|range| Location {
                uri: uri.clone(),
                range: positions.text_range(range, enc),
            })
            .collect(),
    )
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
