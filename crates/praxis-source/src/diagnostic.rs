//! Diagnostics: structured problems reported against source spans.
//!
//! A [`Diagnostic`] always carries a [`Severity`], a structured [`DiagnosticCode`],
//! a message, and a primary [`FileSpan`]. There is no such thing as a diagnostic
//! without a code or a primary location: those fields are non-optional, so the
//! thing you most want to know about an error (where, what kind, what message)
//! can never be missing.
//!
//! The [`Renderer`] produces the §8.2/§8.3 layout:
//!
//! ```text
//! error[T012]: expected Int, found Text
//!
//!   day03.px:18:14
//!   18 | total += line
//!      |          ^^^^ this value is Text
//!
//! hint: parse it with the input parser or call line.int()
//! ```

use std::fmt::Write;

use crate::file::SourceMap;
use crate::line_map::LineCol;
use crate::span::{BytePos, FileSpan};

/// How serious a diagnostic is. Non-exhaustive so future severities (e.g. an
/// "advice" level for inlay context) don't break match exhaustiveness downstream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Severity {
    Error,
    Warning,
    Note,
    Hint,
}

impl Severity {
    /// The lowercase label used in the rendered header (`error`, `warning`...).
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
            Severity::Hint => "hint",
        }
    }
}

/// The broad category a diagnostic belongs to. The category + a per-category
/// number together form the user-facing code (`T012`, `P003`, ...). Categories
/// are closed and compiler-owned, matching the design's "closed tables"
/// philosophy (§4.8).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiagnosticCategory {
    /// Lexical errors (`T0xx`). `T` for Token.
    Lex,
    /// Syntax / parse errors (`P0xx`).
    Parse,
    /// Name-resolution errors (`N0xx`).
    Name,
    /// Type-inference errors (`Y0xx`). `Y` for tYpe.
    Type,
    /// Input-parser errors (`I0xx`).
    Input,
    /// Runtime faults surfaced as compile-time-relevant diagnostics (`R0xx`).
    Runtime,
}

impl DiagnosticCategory {
    /// The single-letter prefix used in the rendered code.
    pub fn prefix(self) -> char {
        match self {
            DiagnosticCategory::Lex => 'T',
            DiagnosticCategory::Parse => 'P',
            DiagnosticCategory::Name => 'N',
            DiagnosticCategory::Type => 'Y',
            DiagnosticCategory::Input => 'I',
            DiagnosticCategory::Runtime => 'R',
        }
    }
}

/// A structured diagnostic code: a category plus a per-category number.
///
/// Because the category is a closed enum and the number is a `u32`, arbitrary
/// free-text codes are unrepresentable. The `Display` impl renders the §8.2
/// `T012`-style form (prefix + zero-padded three-digit number; numbers ≥ 1000
/// are not zero-padded so they stay readable).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DiagnosticCode {
    category: DiagnosticCategory,
    number: u32,
}

impl DiagnosticCode {
    /// Create a code. The number is per-category: `Lex`/1 and `Parse`/1 are two
    /// distinct codes and both are valid.
    #[inline]
    pub const fn new(category: DiagnosticCategory, number: u32) -> DiagnosticCode {
        DiagnosticCode { category, number }
    }

    #[inline]
    pub const fn category(self) -> DiagnosticCategory {
        self.category
    }

    #[inline]
    pub const fn number(self) -> u32 {
        self.number
    }
}

impl std::fmt::Display for DiagnosticCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.number;
        if n < 1000 {
            write!(f, "{}{:03}", self.category.prefix(), n)
        } else {
            write!(f, "{}{}", self.category.prefix(), n)
        }
    }
}

/// A secondary span attached to a diagnostic, with its own message.
///
/// Used for the "related spans when inference connects distant expressions"
/// case in §8.2: a type error's primary span is the failing expression, and a
/// note can point at where the conflicting type was first inferred.
#[derive(Clone, Debug)]
pub struct DiagnosticNote {
    pub span: FileSpan,
    pub message: String,
}

/// A machine-applicable fix: replace `span` with `replacement`, labelled `label`.
#[derive(Clone, Debug)]
pub struct Suggestion {
    pub span: FileSpan,
    pub replacement: String,
    pub label: String,
}

/// A structured diagnostic.
///
/// Construction goes through [`Diagnostic::new`] (required fields only) or the
/// [`DiagnosticBuilder`] (fluent, for the optional notes/suggestions). This
/// keeps the "a diagnostic always has severity + code + message + primary span"
/// invariant structural rather than conventional.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    severity: Severity,
    code: DiagnosticCode,
    message: String,
    primary: FileSpan,
    notes: Vec<DiagnosticNote>,
    suggestions: Vec<Suggestion>,
}

impl Diagnostic {
    /// The minimal complete diagnostic: severity, code, message, primary span.
    #[inline]
    pub fn new(
        severity: Severity,
        code: DiagnosticCode,
        message: impl Into<String>,
        primary: FileSpan,
    ) -> Diagnostic {
        Diagnostic {
            severity,
            code,
            message: message.into(),
            primary,
            notes: Vec::new(),
            suggestions: Vec::new(),
        }
    }

    /// Begin a fluent build, starting from `new`'s required fields.
    #[inline]
    pub fn build(
        severity: Severity,
        code: DiagnosticCode,
        message: impl Into<String>,
        primary: FileSpan,
    ) -> DiagnosticBuilder {
        DiagnosticBuilder {
            diag: Diagnostic::new(severity, code, message, primary),
        }
    }

    #[inline]
    pub fn severity(&self) -> Severity {
        self.severity
    }

    #[inline]
    pub fn code(&self) -> DiagnosticCode {
        self.code
    }

    #[inline]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[inline]
    pub fn primary(&self) -> FileSpan {
        self.primary
    }

    #[inline]
    pub fn notes(&self) -> &[DiagnosticNote] {
        &self.notes
    }

    #[inline]
    pub fn suggestions(&self) -> &[Suggestion] {
        &self.suggestions
    }
}

/// Fluent builder for the optional parts of a [`Diagnostic`].
pub struct DiagnosticBuilder {
    diag: Diagnostic,
}

impl DiagnosticBuilder {
    /// Attach a secondary span with a message.
    pub fn note(mut self, span: FileSpan, message: impl Into<String>) -> Self {
        self.diag.notes.push(DiagnosticNote {
            span,
            message: message.into(),
        });
        self
    }

    /// Attach a machine-applicable suggestion.
    pub fn suggestion(
        mut self,
        span: FileSpan,
        replacement: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        self.diag.suggestions.push(Suggestion {
            span,
            replacement: replacement.into(),
            label: label.into(),
        });
        self
    }

    /// Finish building.
    #[inline]
    pub fn finish(self) -> Diagnostic {
        self.diag
    }
}

// ---------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------

/// Renders diagnostics in the §8.2 layout.
///
/// The renderer borrows a [`SourceMap`] for source snippets and line/column
/// conversion; it has no state of its own and is cheap to construct per render.
pub struct Renderer<'a> {
    source: &'a SourceMap,
}

impl<'a> Renderer<'a> {
    pub fn new(source: &'a SourceMap) -> Renderer<'a> {
        Renderer { source }
    }

    /// Render one diagnostic into `out`.
    pub fn render(&self, diag: &Diagnostic, out: &mut String) {
        self.render_header(diag, out);
        out.push('\n');

        // Primary location + source snippet. The header already carries the
        // diagnostic message, so the caret line shows just the caret (matching
        // how note spans render).
        self.render_location_and_snippet(diag.primary, "", out);

        // Related notes.
        for note in &diag.notes {
            out.push('\n');
            out.push_str(note.message.as_str());
            out.push('\n');
            self.render_location_and_snippet(note.span, "", out);
        }

        // Suggestions as hint lines.
        for sugg in &diag.suggestions {
            out.push('\n');
            let _ = writeln!(
                out,
                "hint: {} (suggestion: {})",
                sugg.label, sugg.replacement
            );
        }
    }

    /// Render `error[code]: message`.
    fn render_header(&self, diag: &Diagnostic, out: &mut String) {
        out.push_str(diag.severity.label());
        let _ = write!(out, "[{}]: {}", diag.code, diag.message);
    }

    /// Render the `path:line:col` header followed by the source line with a
    /// caret underline pointing at `span`. If `message` is non-empty it is
    /// written on the caret line as a trailing note.
    fn render_location_and_snippet(&self, span: FileSpan, message: &str, out: &mut String) {
        let Some(file) = self.source.get(span.file) else {
            // Synthetic / unknown file: fall back to a location-only line.
            let _ = writeln!(out, "  <unknown file> [{:?}]", span);
            return;
        };
        let line_map = file.line_map();
        let text = file.text();
        let start = span.span.start();
        let end = span.span.end();
        let LineCol { line, col } = line_map.offset_to_linecol(start);

        out.push('\n');
        let _ = writeln!(out, "  {}:{}:{}", file.path().display(), line, col);

        // Source line + gutter. The line text runs to the line's real end, not
        // just the span end, so the reader sees the surrounding context.
        let (line_start, line_end) = line_map
            .line_range(line)
            .unwrap_or((BytePos::ZERO, BytePos::ZERO));
        let line_bytes = text.as_bytes();
        let s = line_start.to_usize();
        let e = (line_end.to_u32() as usize).min(line_bytes.len());
        let line_text = std::str::from_utf8(&line_bytes[s..e])
            .unwrap_or("<invalid utf-8>")
            .trim_end_matches(['\n', '\r']);
        let _ = writeln!(out, "  {line} | {line_text}");

        // Caret line: `   | ^^^^ ` aligned under the start, with the message.
        let gutter_width = line.to_string().len();
        let pad: String = " ".repeat(gutter_width);
        let carets = "^".repeat((end.to_u32().saturating_sub(start.to_u32()) as usize).max(1));
        let _ = write!(out, "  {pad} | ");
        for _ in 0..col {
            out.push(' ');
        }
        let _ = write!(out, "{carets}");
        if !message.is_empty() {
            let _ = write!(out, " {message}");
        }
        out.push('\n');
    }
}

/// Helper used by tests and the CLI to render a single diagnostic to a string.
pub fn render_one(source: &SourceMap, diag: &Diagnostic) -> String {
    let mut out = String::new();
    Renderer::new(source).render(diag, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::FileId;
    use crate::span::Span;

    fn span(file: FileId, start: u32, end: u32) -> FileSpan {
        FileSpan::new(file, Span::new(start, end))
    }

    #[test]
    fn code_renders_zero_padded() {
        let code = DiagnosticCode::new(DiagnosticCategory::Lex, 12);
        assert_eq!(code.to_string(), "T012");
    }

    #[test]
    fn code_distinguishes_categories() {
        let lex = DiagnosticCode::new(DiagnosticCategory::Lex, 3);
        let parse = DiagnosticCode::new(DiagnosticCategory::Parse, 3);
        assert_eq!(lex.to_string(), "T003");
        assert_eq!(parse.to_string(), "P003");
        assert_ne!(lex, parse);
    }

    #[test]
    fn code_large_number_not_padded() {
        let code = DiagnosticCode::new(DiagnosticCategory::Type, 1234);
        assert_eq!(code.to_string(), "Y1234");
    }

    #[test]
    fn diagnostic_carries_required_fields() {
        let d = Diagnostic::new(
            Severity::Error,
            DiagnosticCode::new(DiagnosticCategory::Type, 12),
            "expected Int, found Text",
            span(FileId::SYNTHETIC, 0, 1),
        );
        assert_eq!(d.severity(), Severity::Error);
        // `Type` category renders as `Y`, matching the prefix table.
        assert_eq!(d.code().to_string(), "Y012");
        assert_eq!(d.message(), "expected Int, found Text");
        assert!(d.notes().is_empty());
        assert!(d.suggestions().is_empty());
    }

    #[test]
    fn builder_adds_notes_and_suggestions() {
        let d = Diagnostic::build(
            Severity::Error,
            DiagnosticCode::new(DiagnosticCategory::Name, 1),
            "undefined name",
            span(FileId::SYNTHETIC, 0, 1),
        )
        .note(span(FileId::SYNTHETIC, 5, 6), "defined here")
        .suggestion(span(FileId::SYNTHETIC, 0, 1), "value", "did you mean")
        .finish();
        assert_eq!(d.notes().len(), 1);
        assert_eq!(d.suggestions().len(), 1);
        assert_eq!(d.suggestions()[0].replacement, "value");
    }

    #[test]
    fn render_snapshot_single_line() {
        let map = SourceMap::new();
        let id = map.intern("day03.px", "total += line\n");
        // "line" starts at byte 9, length 4.
        let d = Diagnostic::build(
            Severity::Error,
            DiagnosticCode::new(DiagnosticCategory::Type, 12),
            "expected Int, found Text",
            span(id, 9, 13),
        )
        .suggestion(
            span(id, 9, 13),
            "line.int()",
            "parse it with the input parser",
        )
        .finish();
        let rendered = render_one(&map, &d);
        insta::assert_snapshot!(rendered, @r#"
        error[Y012]: expected Int, found Text

          day03.px:1:9
          1 | total += line
            |          ^^^^

        hint: parse it with the input parser (suggestion: line.int())
        "#);
    }

    #[test]
    fn render_snapshot_two_lines_with_note() {
        let map = SourceMap::new();
        let id = map.intern("f.px", "let a = value\nlet b = a + 1\n");
        // Primary: "value" at 8..13 on line 1.
        let primary = span(id, 8, 13);
        let d = Diagnostic::build(
            Severity::Error,
            DiagnosticCode::new(DiagnosticCategory::Name, 1),
            "undefined name `value`",
            primary,
        )
        .note(span(id, 23, 24), "the name `a` is defined here")
        .finish();
        let rendered = render_one(&map, &d);
        insta::assert_snapshot!(rendered, @r#"
        error[N001]: undefined name `value`

          f.px:1:8
          1 | let a = value
            |         ^^^^^

        the name `a` is defined here

          f.px:2:9
          2 | let b = a + 1
            |          ^
        "#);
    }
}
