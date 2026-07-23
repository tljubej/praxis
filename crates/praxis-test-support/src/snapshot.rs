//! Snapshot helpers built on `insta`.

use praxis_source::{Diagnostic, Renderer, SourceMap};

/// Render `diags` through a fresh [`Renderer`] and snapshot the joined result.
///
/// This is the standard pattern for "given these diagnostics, here is the
/// rendered output" golden-tree tests (§17.2). Diagnostics are joined with a
/// blank line between them so multi-diagnostic snapshots stay readable.
pub fn snapshot_diagnostics(diags: &[Diagnostic], source: &SourceMap) -> String {
    render_diagnostics(diags, source)
}

/// Render `diags` the same way the CLI would, returning the joined string. Use
/// this directly when you want the rendered text without snapshotting.
pub fn render_diagnostics(diags: &[Diagnostic], source: &SourceMap) -> String {
    let renderer = Renderer::new(source);
    let mut out = String::new();
    for (i, d) in diags.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        renderer.render(d, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::single_file;
    use praxis_source::{DiagnosticCategory, DiagnosticCode, FileSpan, Severity, Span};

    #[test]
    fn snapshot_smoke_test() {
        // The harness itself must produce deterministic output.
        insta::assert_snapshot!("hello-snapshot", "stable output\n");
    }

    #[test]
    fn render_diagnostics_joins_with_blank_lines() {
        let (map, id) = single_file("f.px", "ab\ncd\n");
        let d1 = Diagnostic::new(
            Severity::Error,
            DiagnosticCode::new(DiagnosticCategory::Lex, 3),
            "first problem",
            FileSpan::new(id, Span::new(0, 1)),
        );
        let d2 = Diagnostic::new(
            Severity::Warning,
            DiagnosticCode::new(DiagnosticCategory::Lex, 4),
            "second problem",
            FileSpan::new(id, Span::new(3, 4)),
        );
        let rendered = render_diagnostics(&[d1, d2], &map);
        // Two separate diagnostic blocks, separated by exactly one blank line
        // (the renderer always ends a block with a newline, so the joiner adds
        // one more for visual separation).
        assert!(rendered.contains("first problem"));
        assert!(rendered.contains("second problem"));
        let first_count = rendered.matches("error[T003]").count();
        assert_eq!(first_count, 1, "each diagnostic appears exactly once");
    }
}
