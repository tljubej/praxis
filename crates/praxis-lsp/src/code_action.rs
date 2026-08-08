//! Code actions (§15.2, §19.12 criterion 3, ADR-132).
//!
//! > *"Code actions can fix misspelled parser constructors and add missing
//! > match arms."*
//!
//! **A code action is a diagnostic's machine-applicable suggestion.** Not a
//! second analysis, not a table of "common mistakes" kept in the language
//! server: `praxis_source::Suggestion` carries an optional `replacement`, the
//! pass that detects a mistake is the one that knows how to fix it, and this
//! module is the twenty lines that turn one into a `WorkspaceEdit`.
//!
//! That is why the fixes live where they do: the "did you mean `lines`" for
//! `line` is written where the constructor table is consulted, the missing arms
//! are written where the coverage witnesses are computed. A fix invented here
//! would be a second opinion about a question the compiler has already
//! answered — and, unlike the compiler's, one that no `praxis check` run would
//! ever exercise.
//!
//! # Which diagnostics are offered
//!
//! The server's **own**, recomputed from the current snapshot, filtered to the
//! range the client asked about. The `context.diagnostics` a client echoes back
//! are from whatever version it last received: acting on those risks applying an
//! edit computed against text that has since changed.

use crate::position::Encoding;
use crate::query::Snapshot;
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Range, TextEdit, Uri, WorkspaceEdit,
};

/// Every quick fix available in `range`.
// `WorkspaceEdit::changes` is a `HashMap<Uri, _>` and `lsp_types::Uri` has
// interior mutability in a field this code never touches. The protocol type is
// not ours to change, and the key is a URI string by construction.
#[allow(clippy::mutable_key_type)]
#[must_use]
pub fn actions(
    snapshot: &Snapshot,
    range: Range,
    uri: &Uri,
    enc: Encoding,
) -> Vec<CodeActionOrCommand> {
    let positions = snapshot.positions();
    let asked = positions.span(range, enc);
    let mut out = Vec::new();

    for diag in snapshot.diagnostics() {
        let at = diag.primary().span;
        // Touching counts: a caret *at* the end of a misspelled name is on it as
        // far as a user is concerned, and a zero-width insertion point is inside
        // any range that reaches it.
        if at.end().to_u32() < asked.start().to_u32() || at.start().to_u32() > asked.end().to_u32()
        {
            continue;
        }
        for suggestion in diag.suggestions() {
            let Some(replacement) = suggestion.replacement.as_deref() else {
                // Advice with no rewrite is already in the diagnostic's message
                // (`diagnostics::message_with_advice`); offering it as an action
                // that changes nothing would be a menu entry that does nothing.
                continue;
            };
            let mut changes = std::collections::HashMap::new();
            changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range: positions.range(suggestion.span.span, enc),
                    new_text: replacement.to_string(),
                }],
            );
            out.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: title(&suggestion.label),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![crate::diagnostics::to_lsp(&diag, uri, positions, enc)]),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }
    }
    out
}

/// The action's menu entry.
///
/// The suggestion's own label, which the compiler wrote as a sentence for a
/// `help:` line — `did you mean `lines`?` — and which reads correctly in a menu
/// too. Rewriting it here would be a second wording of the same advice, free to
/// disagree with what `praxis check` prints.
fn title(label: &str) -> String {
    let mut chars = label.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => "Apply fix".to_string(),
    }
}
