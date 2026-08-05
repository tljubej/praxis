//! Rename (§15.2, §19.12's first acceptance criterion, ADR-131).
//!
//! > *"Rename updates all valid references and rejects unsafe collisions."*
//!
//! The first half is [`navigation::reference_ranges`](crate::navigation::reference_ranges):
//! a symbol's declaration and every reference that resolved to **it**, which is
//! already the set of places that must change together.
//!
//! The second half is the hard one, and this module's whole subject. A rename is
//! unsafe when the new spelling makes some name mean something else — and the
//! ways that happens are not a list anybody can be sure they finished:
//!
//! - the new name is already declared where the old one was, so one binding
//!   shadows the other;
//! - a reference to the renamed binding now resolves to an *outer* binding that
//!   was there all along (`var n = 1` renamed to `out`);
//! - a reference to some *other* binding of that name now resolves to the
//!   renamed one, because the rename moved a shadowing declaration into its way;
//! - the new name is not an identifier at all, or is a keyword.
//!
//! So the check is not a list of collisions. **The edited text is analyzed, and
//! the rename is accepted only when name resolution comes out the same** — every
//! reference resolving to the symbol it resolved to before, and no new
//! diagnostic. That is the property "rejects unsafe collisions" is trying to
//! describe, asked directly of the resolver instead of re-derived from a scope
//! tree that cannot answer "which scope is this offset in" (M11 handover §5.2).
//!
//! It costs one extra analysis per rename — about 4 ms on a puzzle-sized file,
//! for an operation a user performs by hand and waits for.

use std::collections::HashMap;

use lsp_types::{TextEdit, Uri, WorkspaceEdit};
use praxis_hir::SymbolId;
use rowan::TextRange;

use crate::navigation::{reference_ranges, symbol_at};
use crate::position::Encoding;
use crate::query::Snapshot;
use crate::Revision;

/// Why a rename was refused. Each variant is a message the editor shows, so the
/// user learns which rename would have been safe rather than that "rename
/// failed".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The cursor is not on a name this file declares. A prelude name (`out`,
    /// `Vec`, `Int`) lands here: it has no declaration site to edit.
    NotRenameable,
    /// The new spelling is not an identifier (§4.1).
    NotAnIdentifier(String),
    /// The new spelling is one of the language's keywords.
    IsAKeyword(String),
    /// The new spelling would change what some name refers to, or would
    /// introduce a diagnostic the file does not have. The detail names the
    /// change, because "unsafe" on its own is not actionable.
    Unsafe(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NotRenameable => {
                write!(f, "there is no binding to rename here")
            }
            Refusal::NotAnIdentifier(name) => {
                write!(f, "`{name}` is not a valid Praxis identifier")
            }
            Refusal::IsAKeyword(name) => write!(f, "`{name}` is a keyword"),
            Refusal::Unsafe(detail) => write!(f, "{detail}"),
        }
    }
}

/// `textDocument/prepareRename`: the range the editor should pre-select, and
/// the word to seed the input box with.
///
/// Answering `None` is how a client is told the position cannot be renamed —
/// before the user has typed a new name, rather than after.
#[must_use]
pub fn prepare(
    snapshot: &Snapshot,
    offset: u32,
    enc: Encoding,
) -> Option<lsp_types::PrepareRenameResponse> {
    let symbol = renameable_symbol(snapshot, offset)?;
    let token = snapshot.token_at(offset)?;
    // The token under the cursor must be the *name*, not a neighbouring
    // bracket that happens to touch the same offset.
    let ranges = reference_ranges(snapshot, symbol);
    if !ranges.contains(&token.text_range()) {
        return None;
    }
    Some(lsp_types::PrepareRenameResponse::RangeWithPlaceholder {
        range: snapshot.positions().text_range(token.text_range(), enc),
        placeholder: token.text().to_string(),
    })
}

/// `textDocument/rename`: every edit, or the reason there are none.
///
/// # Errors
/// Returns a [`Refusal`] when the position names nothing renameable, the new
/// spelling is not a usable identifier, or the rename would change what some
/// name in the file refers to.
// See `code_action::actions` for why the `HashMap<Uri, _>` lint is allowed: the
// map is the protocol's own type.
#[allow(clippy::mutable_key_type)]
pub fn rename(
    snapshot: &Snapshot,
    offset: u32,
    new_name: &str,
    uri: &Uri,
    enc: Encoding,
) -> Result<WorkspaceEdit, Refusal> {
    let symbol = renameable_symbol(snapshot, offset).ok_or(Refusal::NotRenameable)?;
    check_spelling(new_name)?;

    let ranges = reference_ranges(snapshot, symbol);
    if ranges.is_empty() {
        return Err(Refusal::NotRenameable);
    }
    let edited = apply(snapshot.source_text(), &ranges, new_name);
    let after = Snapshot::new("rename-probe", edited, Revision(snapshot.revision().0));
    if let Some(detail) = resolution_changed(snapshot, &after, new_name) {
        return Err(Refusal::Unsafe(detail));
    }

    let positions = snapshot.positions();
    let edits: Vec<TextEdit> = ranges
        .iter()
        .map(|range| TextEdit {
            range: positions.text_range(*range, enc),
            new_text: new_name.to_string(),
        })
        .collect();
    let mut changes = HashMap::new();
    changes.insert(uri.clone(), edits);
    Ok(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

/// The symbol at `offset`, if it is one this file can rewrite.
///
/// A symbol with no declaration span is a prelude name: it is declared in the
/// compiler, and renaming it in one file would rename nothing.
fn renameable_symbol(snapshot: &Snapshot, offset: u32) -> Option<SymbolId> {
    let symbol = symbol_at(snapshot, offset)?;
    snapshot.analyze().names.get(symbol)?.decl?;
    Some(symbol)
}

fn check_spelling(new_name: &str) -> Result<(), Refusal> {
    if !praxis_syntax::ident::is_ident(new_name) {
        return Err(Refusal::NotAnIdentifier(new_name.to_string()));
    }
    // **From the lexer's own table** — a keyword added later is refused here
    // without anybody remembering to add it to a list in the language server.
    if praxis_syntax::SyntaxKind::from_keyword(new_name).is_some() {
        return Err(Refusal::IsAKeyword(new_name.to_string()));
    }
    Ok(())
}

/// `text` with every range replaced by `replacement`, right to left so the
/// earlier ranges are still valid as each edit is applied.
fn apply(text: &str, ranges: &[TextRange], replacement: &str) -> String {
    let mut out = text.to_string();
    for range in ranges.iter().rev() {
        let (start, end) = (usize::from(range.start()), usize::from(range.end()));
        out.replace_range(start..end, replacement);
    }
    out
}

/// What the rename changed besides the spelling, or `None` when it changed
/// nothing.
///
/// Two comparisons, and neither needs the edited ranges to be mapped back onto
/// the original — which is what makes this exact rather than approximate:
///
/// 1. **The resolution sequence.** A rename does not add, remove or reorder
///    tokens, so the file's references and declarations come out in the same
///    order with the same symbols — *if* nothing was captured. An entry whose
///    symbol changed is a name that now means something else, and one appearing
///    or vanishing is a name that started or stopped resolving.
/// 2. **The diagnostics.** A collision that resolution alone cannot see —
///    `fn g` renamed to `f` beside an existing `fn f` — arrives as a new
///    `N004`. Counting per code, rather than comparing messages, keeps a
///    message that merely mentions the new spelling from reading as a new
///    problem.
fn resolution_changed(before: &Snapshot, after: &Snapshot, new_name: &str) -> Option<String> {
    let (old_refs, old_decls) = sequence(before);
    let (new_refs, new_decls) = sequence(after);

    if old_refs.len() != new_refs.len() || old_decls.len() != new_decls.len() {
        return Some(format!(
            "renaming to `{new_name}` would change which names resolve in this file"
        ));
    }
    for (i, (old, new)) in old_refs.iter().zip(new_refs.iter()).enumerate() {
        if old.1 != new.1 {
            let line = after
                .line_map()
                .offset_to_linecol(praxis_source::BytePos::from(u32::from(new.0.start())));
            let word = &after.source_text()[usize::from(new.0.start())..usize::from(new.0.end())];
            let _ = i;
            return Some(format!(
                "renaming to `{new_name}` would change what `{word}` on line {} refers to",
                line.line
            ));
        }
    }

    let old_counts = code_counts(before);
    let new_counts = code_counts(after);
    for (code, count) in &new_counts {
        if old_counts.get(code).copied().unwrap_or(0) < *count {
            let example = after
                .diagnostics()
                .into_iter()
                .find(|d| d.code().to_string() == *code)
                .map(|d| d.message().to_string())
                .unwrap_or_default();
            return Some(format!(
                "renaming to `{new_name}` would introduce {code}: {example}"
            ));
        }
    }
    None
}

/// Names in source order, each with the symbol it resolved to — the thing a
/// rename must leave alone.
type Named = Vec<(TextRange, SymbolId)>;

/// The file's references and declarations in source order, each with the symbol
/// it names.
fn sequence(snapshot: &Snapshot) -> (Named, Named) {
    let analysis = snapshot.analyze();
    let mut refs: Named = analysis.refs.iter().map(|(r, v)| (*r, v.symbol)).collect();
    let mut decls: Named = analysis.decls.iter().map(|(r, v)| (*r, *v)).collect();
    refs.sort_by_key(|(r, _)| (r.start(), r.end()));
    decls.sort_by_key(|(r, _)| (r.start(), r.end()));
    (refs, decls)
}

fn code_counts(snapshot: &Snapshot) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for d in snapshot.diagnostics() {
        *counts.entry(d.code().to_string()).or_default() += 1;
    }
    counts
}
