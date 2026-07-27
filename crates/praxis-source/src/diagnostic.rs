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
use crate::span::FileSpan;
use crate::style;

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

/// A fix or piece of advice attached to a diagnostic.
///
/// When `replacement` is `Some`, it is a machine-applicable fix: replace `span`
/// with the given text (a "fix-it"). When `None`, the suggestion is advisory —
/// a `help:` line that explains how to resolve the problem without offering an
/// automatic rewrite (§8.2: "a concrete suggestion when available").
#[derive(Clone, Debug)]
pub struct Suggestion {
    pub span: FileSpan,
    /// `None` for advisory hints with no automatic replacement.
    pub replacement: Option<String>,
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

    /// Attach a machine-applicable suggestion: replace `span` with `replacement`.
    pub fn suggestion(
        mut self,
        span: FileSpan,
        replacement: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        self.diag.suggestions.push(Suggestion {
            span,
            replacement: Some(replacement.into()),
            label: label.into(),
        });
        self
    }

    /// Attach an advisory `help:` line (no automatic replacement). Use when the
    /// fix is not mechanical (e.g. "remove this expression" or "change the
    /// return type") — §8.2 names these as explanations rather than fix-its.
    pub fn help(mut self, span: FileSpan, label: impl Into<String>) -> Self {
        self.diag.suggestions.push(Suggestion {
            span,
            replacement: None,
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
/// conversion; it holds a [`style::Palette`] that decides whether the output is
/// plain (the default, for snapshot-stable tests) or ANSI-styled. It is cheap to
/// construct per render.
pub struct Renderer<'a> {
    source: &'a SourceMap,
    palette: style::Palette,
}

impl<'a> Renderer<'a> {
    /// A plain-text renderer (no ANSI). The default for snapshot tests, which
    /// must stay byte-stable regardless of terminal state.
    pub fn new(source: &'a SourceMap) -> Renderer<'a> {
        Renderer {
            source,
            palette: style::Palette::plain(),
        }
    }

    /// A renderer that styles its output when `palette` is [`style::Palette::styled`].
    pub fn new_styled(source: &'a SourceMap, palette: style::Palette) -> Renderer<'a> {
        Renderer { source, palette }
    }

    /// The diagnostic's severity in the [`style`] module's terms.
    fn style_severity(sev: Severity) -> style::Severity {
        match sev {
            Severity::Error => style::Severity::Error,
            Severity::Warning => style::Severity::Warning,
            Severity::Note => style::Severity::Note,
            Severity::Hint => style::Severity::Help,
        }
    }

    /// Render one diagnostic into `out`.
    pub fn render(&self, diag: &Diagnostic, out: &mut String) {
        self.render_header(diag, out);
        // §8.2 puts a blank line between the header and the location snippet.
        out.push('\n');

        // Primary location + source snippet, with the diagnostic message as the
        // caret-line label (§8.2: `^^^^ this value is Text`).
        self.render_location_and_snippet(
            diag.primary,
            Some(diag.message.as_str()),
            diag.severity,
            out,
        );

        // Related notes: each carries its own message + span snippet, set off by
        // a blank line so a multi-span diagnostic reads as distinct blocks.
        for note in &diag.notes {
            out.push('\n');
            let label = self
                .palette
                .paint(style::Style::Severity(style::Severity::Note), "note:");
            let _ = writeln!(out, "{label} {}", note.message);
            self.render_location_and_snippet(note.span, None, Severity::Note, out);
        }

        // Suggestions as rustc-style `help:` lines. A machine-applicable fix
        // shows its replacement on the next indented line; an advisory hint
        // shows only the explanation.
        for sugg in &diag.suggestions {
            out.push('\n');
            let label = self
                .palette
                .paint(style::Style::Severity(style::Severity::Help), "help:");
            let _ = writeln!(out, "{label} {}", sugg.label);
            if let Some(repl) = &sugg.replacement {
                let _ = writeln!(out, "      {repl}");
            }
        }
    }

    /// Render `error[code]: message` (no trailing newline; the caller frames it).
    fn render_header(&self, diag: &Diagnostic, out: &mut String) {
        let sev = Self::style_severity(diag.severity);
        let label = self
            .palette
            .paint(style::Style::Severity(sev), diag.severity.label());
        let code = self
            .palette
            .paint(style::Style::Code, &format!("[{}]", diag.code));
        let _ = write!(out, "{label}{code}: {}", diag.message);
    }

    /// Render the `path:line:col` header followed by the source line(s) the
    /// span touches, with a clamped caret underline. `label` (when `Some`)
    /// trails the carets on the first underlined line. The caret is colored in
    /// the diagnostic's severity color when the palette is styled. Delegates the
    /// actual line/caret drawing to the shared
    /// [`snippet::render_span_snippet_styled`] so the compiler and crash debugger
    /// render spans identically.
    fn render_location_and_snippet(
        &self,
        span: FileSpan,
        label: Option<&str>,
        sev: Severity,
        out: &mut String,
    ) {
        let Some(file) = self.source.get(span.file) else {
            // Synthetic / unknown file: fall back to a location-only line.
            let _ = writeln!(out, "  <unknown file> [{:?}]", span);
            return;
        };
        let caret_label = match label {
            Some(s) if !s.is_empty() => crate::snippet::CaretLabel::Labelled(s),
            _ => crate::snippet::CaretLabel::Plain,
        };
        crate::snippet::render_span_snippet_styled(
            &file,
            span,
            caret_label,
            out,
            crate::snippet::MAX_SNIPPET_LINES,
            &self.palette,
            Some(Self::style_severity(sev)),
        );
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
        assert_eq!(d.suggestions()[0].replacement.as_deref(), Some("value"));
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
        insta::assert_snapshot!(rendered, @r"
error[Y012]: expected Int, found Text

  day03.px:1:9
  1 | total += line
    |          ^^^^ expected Int, found Text

help: parse it with the input parser
      line.int()
");
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
        insta::assert_snapshot!(rendered, @r"
error[N001]: undefined name `value`

  f.px:1:8
  1 | let a = value
    |         ^^^^^ undefined name `value`

note: the name `a` is defined here

  f.px:2:9
  2 | let b = a + 1
    |          ^
");
    }

    #[test]
    fn styled_renderer_emits_ansi() {
        // The styled renderer wraps the severity label, code, carets, location,
        // and help label in ANSI escapes. The plain path (default) emits none.
        let map = SourceMap::new();
        let id = map.intern("f.px", "x = 1\n");
        let d = Diagnostic::build(
            Severity::Error,
            DiagnosticCode::new(DiagnosticCategory::Type, 1),
            "expected Int, found Text",
            span(id, 0, 1),
        )
        .help(span(id, 0, 1), "call .int()")
        .finish();

        let mut plain = String::new();
        Renderer::new(&map).render(&d, &mut plain);
        assert!(
            !plain.contains("\x1b["),
            "plain output has no ANSI: {plain:?}"
        );

        let mut styled = String::new();
        Renderer::new_styled(&map, style::Palette::styled()).render(&d, &mut styled);
        // Header: bold-red `error` + bold `[Y001]`.
        assert!(
            styled.contains("\x1b[1;31merror\x1b[0m"),
            "styled error label: {styled:?}"
        );
        assert!(
            styled.contains("\x1b[1m[Y001]\x1b[0m"),
            "styled code: {styled:?}"
        );
        // Caret in the error color (red, not bold).
        assert!(
            styled.contains("\x1b[31m^\x1b[0m"),
            "styled caret: {styled:?}"
        );
        // help label in cyan.
        assert!(
            styled.contains("\x1b[1;36mhelp:\x1b[0m"),
            "styled help label: {styled:?}"
        );
    }
}
