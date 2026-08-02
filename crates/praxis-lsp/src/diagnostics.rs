//! `praxis_source::Diagnostic` → `lsp_types::Diagnostic` (WS3, §8.2, §15.2).
//!
//! The mapping is field for field, and the fields already exist: severity,
//! a **registered** code, a primary `FileSpan`, `notes` that are secondary spans
//! with messages, and `suggestions` that are advice with an optional
//! machine-applicable replacement.
//!
//! **The code is never a hand-written string.** ADR-051 owns the allocation:
//! a `Diagnostic` stores a `DiagCode`, and `DiagCode::code()` is the one place a
//! `(category, number)` pair exists. Rendering it here is a `to_string`, so a
//! code the editor shows is a code the register allocated.
//!
//! The `replacement`s ride along untouched. They are M12's code actions, and
//! carrying them now costs nothing and keeps the conversion total.

use lsp_types::{
    Diagnostic as LspDiagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, Location,
    NumberOrString, Uri,
};
use praxis_source::{Diagnostic, Severity};

use crate::position::{Encoding, PositionMap};

/// The `source` field every Praxis diagnostic carries, so an editor showing
/// several servers' reports can say which one spoke.
pub const SOURCE: &str = "praxis";

/// Convert one diagnostic.
///
/// `uri` is this document's, and it is the only URI involved: M11 is one file,
/// so a note's span is in the same file as the primary by construction. A
/// multi-file note would need the note's own `FileId` resolved to a URI, which
/// is M12's workspace indexing.
#[must_use]
pub fn to_lsp(
    diag: &Diagnostic,
    uri: &Uri,
    positions: PositionMap<'_>,
    enc: Encoding,
) -> LspDiagnostic {
    let related: Vec<DiagnosticRelatedInformation> = diag
        .notes()
        .iter()
        .map(|n| DiagnosticRelatedInformation {
            location: Location {
                uri: uri.clone(),
                range: positions.range(n.span.span, enc),
            },
            message: n.message.clone(),
        })
        .collect();

    LspDiagnostic {
        range: positions.range(diag.primary().span, enc),
        severity: Some(severity(diag.severity())),
        code: Some(NumberOrString::String(diag.code().to_string())),
        code_description: None,
        source: Some(SOURCE.to_string()),
        message: message_with_advice(diag),
        related_information: (!related.is_empty()).then_some(related),
        tags: None,
        data: None,
    }
}

/// Convert a whole file's diagnostics.
#[must_use]
pub fn all_to_lsp(
    diags: &[Diagnostic],
    uri: &Uri,
    positions: PositionMap<'_>,
    enc: Encoding,
) -> Vec<LspDiagnostic> {
    diags
        .iter()
        .map(|d| to_lsp(d, uri, positions, enc))
        .collect()
}

/// The message, with each advisory suggestion appended as a `help:` line.
///
/// §8.2's rendered form puts advice under the snippet; an LSP diagnostic has one
/// message, so the advice goes in its tail. A suggestion that carries a
/// `replacement` is a *fix*, not advice — it belongs in a code action (M12) and
/// repeating its text in the message would say the same thing twice.
fn message_with_advice(diag: &Diagnostic) -> String {
    let mut message = diag.message().to_string();
    for s in diag.suggestions() {
        if s.replacement.is_none() {
            message.push_str("\nhelp: ");
            message.push_str(&s.label);
        }
    }
    message
}

/// `Severity` is `#[non_exhaustive]`, so this has a fallback arm on purpose: a
/// severity added later shows as a hint rather than failing to compile a crate
/// that has no opinion about it.
fn severity(s: Severity) -> DiagnosticSeverity {
    match s {
        Severity::Error => DiagnosticSeverity::ERROR,
        Severity::Warning => DiagnosticSeverity::WARNING,
        Severity::Note => DiagnosticSeverity::INFORMATION,
        _ => DiagnosticSeverity::HINT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_source::{DiagCode, FileId, FileSpan, LineMap, Span};
    use std::str::FromStr;

    fn uri() -> Uri {
        Uri::from_str("file:///t.px").expect("valid")
    }

    #[test]
    fn the_code_is_the_registered_one() {
        let text = "let x = 1\n";
        let lines = LineMap::new(text);
        let d = Diagnostic::new(
            Severity::Error,
            DiagCode::TypeMismatch,
            "expected Int, found Text",
            FileSpan::new(FileId::SYNTHETIC, Span::new(4u32, 5u32)),
        );
        let lsp = to_lsp(&d, &uri(), PositionMap::new(text, &lines), Encoding::Utf16);
        assert_eq!(
            lsp.code,
            Some(NumberOrString::String("Y001".to_string())),
            "the code comes from DiagCode, not from a string in this crate"
        );
        assert_eq!(lsp.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(lsp.source.as_deref(), Some("praxis"));
        assert_eq!(lsp.range.start.character, 4);
        assert_eq!(lsp.range.end.character, 5);
    }

    #[test]
    fn a_note_becomes_related_information() {
        let text = "let x = 1\nlet y = 2\n";
        let lines = LineMap::new(text);
        let d = Diagnostic::new(
            Severity::Error,
            DiagCode::TypeMismatch,
            "mismatch",
            FileSpan::new(FileId::SYNTHETIC, Span::new(4u32, 5u32)),
        )
        .with_note(
            FileSpan::new(FileId::SYNTHETIC, Span::new(14u32, 15u32)),
            "first inferred here",
        );
        let lsp = to_lsp(&d, &uri(), PositionMap::new(text, &lines), Encoding::Utf16);
        let related = lsp.related_information.expect("a note is related info");
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].message, "first inferred here");
        assert_eq!(related[0].location.range.start.line, 1);
    }
}
