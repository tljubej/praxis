//! CLI-facing diagnostic rendering.
//!
//! Thin wrapper over [`praxis_source::Renderer`] that adds terminal-friendly
//! framing: a blank line between diagnostics and a trailing summary line. The
//! real terminal colour/ styling is deferred to a later milestone; the output
//! is plain text so snapshot tests stay stable.

use std::io::Write;

use praxis_source::style::Palette;
use praxis_source::{Diagnostic, Renderer, Severity, SourceMap};

/// Outcome of rendering a batch of diagnostics: the rendered text plus the
/// counts by severity, so the CLI can produce an accurate summary line.
#[derive(Debug, Default)]
pub struct RenderedDiagnostics {
    pub text: String,
    pub errors: usize,
    pub warnings: usize,
}

impl RenderedDiagnostics {
    /// True if at least one error was reported.
    pub fn has_errors(&self) -> bool {
        self.errors > 0
    }
}

/// Render all `diags` (in order) to one string and tally severities. `palette`
/// controls ANSI styling — pass [`Palette::plain()`] for snapshot-stable output.
pub fn render_all(
    source: &SourceMap,
    diags: &[Diagnostic],
    palette: Palette,
) -> RenderedDiagnostics {
    let renderer = Renderer::new_styled(source, palette);
    let mut out = String::new();
    let mut errors = 0;
    let mut warnings = 0;
    for d in diags {
        if !out.is_empty() {
            out.push('\n');
        }
        match d.severity() {
            Severity::Error => errors += 1,
            Severity::Warning => warnings += 1,
            _ => {}
        }
        renderer.render(d, &mut out);
    }
    RenderedDiagnostics {
        text: out,
        errors,
        warnings,
    }
}

/// Write `rendered` to `writer` followed by a one-line summary. Returns Ok(())
/// on success; the caller decides the exit code from `rendered.has_errors()`.
pub fn write_to(writer: &mut impl Write, rendered: &RenderedDiagnostics) -> std::io::Result<()> {
    if !rendered.text.is_empty() {
        writeln!(writer, "{}", rendered.text.trim_end())?;
    }
    let mut parts = Vec::new();
    if rendered.errors > 0 {
        parts.push(format!("{} error(s)", rendered.errors));
    }
    if rendered.warnings > 0 {
        parts.push(format!("{} warning(s)", rendered.warnings));
    }
    if !parts.is_empty() {
        writeln!(writer, "\npraxis: {}", parts.join(", "))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_source::{DiagnosticCategory, DiagnosticCode, FileSpan, Severity, Span};
    use praxis_test_support::single_file;

    #[test]
    fn tally_counts_by_severity() {
        let (map, id) = single_file("f.px", "ab\ncd\n");
        let e = Diagnostic::new(
            Severity::Error,
            DiagnosticCode::new(DiagnosticCategory::Lex, 3),
            "boom",
            FileSpan::new(id, Span::new(0, 1)),
        );
        let w = Diagnostic::new(
            Severity::Warning,
            DiagnosticCode::new(DiagnosticCategory::Lex, 4),
            "careful",
            FileSpan::new(id, Span::new(3, 4)),
        );
        let r = render_all(&map, &[e, w], praxis_source::style::Palette::plain());
        assert_eq!(r.errors, 1);
        assert_eq!(r.warnings, 1);
        assert!(r.has_errors());
    }

    #[test]
    fn empty_batch_renders_nothing() {
        let (map, _id) = single_file("f.px", "abc");
        let r = render_all(&map, &[], praxis_source::style::Palette::plain());
        assert!(r.text.is_empty());
        assert!(!r.has_errors());
    }
}
