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
    ///
    /// `pub(crate)` on purpose: the only way to reach a code from outside is
    /// [`DiagCode::code`], so a number nobody registered in [`DiagCode`] has no
    /// way into a diagnostic. Before that, five crates wrote
    /// `DiagnosticCode::new(Type, 110)` at the site, and there was no place to
    /// look up which numbers were spent.
    #[inline]
    pub(crate) const fn new(category: DiagnosticCategory, number: u32) -> DiagnosticCode {
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

/// The closed set of diagnostics the compiler can emit.
///
/// Every `(category, number)` pair is written in exactly one place —
/// [`DiagCode::code`]'s exhaustive match — so allocating a code is a
/// compile-time act with a name rather than an integer literal at a call site.
/// [`DiagnosticCode::new`] is `pub(crate)` for the same reason: an unregistered
/// number has no route into a [`Diagnostic`].
///
/// **The allocation is ADR-051.** Adding a variant means amending it first;
/// `every_code_is_distinct` is what catches a collision if you do not.
///
/// The numbers are not contiguous and are not meant to be. `Y09x` is internal
/// errors, `Y11x` member errors, `Y12x` match errors — a split the codes that
/// shipped before this registry already implied, and renumbering them would
/// change identifiers users have already seen.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DiagCode {
    // --- Lex (`T0xx`) ---
    /// `T001` — a `/*` with no matching `*/`.
    UnterminatedBlockComment,
    /// `T002` — a backtick template with no closing backtick.
    UnterminatedTemplate,
    /// `T003` — a character the lexer cannot classify.
    UnexpectedCharacter,
    /// `T004` — a text literal with no closing quote.
    UnterminatedTextLiteral,
    /// `T005` — a `\` escape the lexer does not recognize.
    InvalidEscape,

    // --- Parse (`P0xx`) ---
    /// `P001` — a token that cannot appear here.
    UnexpectedToken,
    /// `P002` — two statements with no `;` and no line break between them
    /// (FE-04).
    ExpectedStatementSeparator,

    // --- Name (`N0xx`) ---
    /// `N000` — internal: the parse tree's root is not a `SOURCE_FILE`.
    InternalNotASourceFile,
    /// `N001` — a name that is not in scope.
    UnknownName,
    /// `N002` — a type annotation naming a type that does not exist.
    UnknownType,
    /// `N003` — a name used in type position that names a value (TY-11).
    NameIsNotAType,
    /// `N004` — one name declared twice in one scope (TY-24).
    DuplicateDeclaration,
    /// `N005` — a function declared inside a function (TY-23).
    NestedFunction,

    // --- Type (`Y0xx`), the user block ---
    /// `Y001` — two types that could not be unified.
    TypeMismatch,
    /// `Y002` — an occurs-check failure.
    InfiniteType,
    /// `Y003` — an annotation that conflicts with what inference derived.
    AnnotationConflict,
    /// `Y004` — a type whose values cannot be compared with `==`.
    NotEquatable,
    /// `Y005` — a type that cannot be iterated.
    NotIterable,
    /// `Y006` — a type that has no ordering.
    NotOrderable,
    /// `Y007` — a type constructor given the wrong number of type arguments.
    /// Also TY-12's `Option[Int, Text]`, which is the same mistake.
    WrongTypeArgumentCount,
    /// `Y008` — a `struct`/`enum` declaring one field or variant twice.
    DuplicateMember,
    /// `Y009` — assignment to something that is not a `var` (TY-14).
    AssignToImmutable,
    /// `Y010` — a compound assignment whose operands are not numeric (TY-15).
    CompoundAssignNonNumeric,
    /// `Y011` — `return` outside a function (TY-20).
    ReturnOutsideFunction,
    /// `Y012` — `break`/`continue` outside a loop (TY-20).
    BreakOutsideLoop,
    /// `Y013` — an integer literal outside the representable range (TY-28).
    IntLiteralOutOfRange,
    /// `Y014` — a `Map`/`Set` key type that cannot be hashed (TY-32).
    NotHashable,
    /// `Y015` — a non-numeric type where a numeric one is required (TY-31).
    NotNumeric,
    /// `Y016` — an operator not defined for these operand types (TY-26, TY-27).
    OperatorNotDefined,
    /// `Y017` — a `break` carrying a value out of a `while`/`for` (TY-21).
    ValueBreakOutsideLoopExpression,

    // --- Type (`Y09x`), internal ---
    /// `Y099` — internal: a type the compiler expected was absent.
    InternalMissingType,

    // --- Type (`Y11x`), member errors ---
    /// `Y110` — no such method on this type at this arity.
    NoMethodOnType,
    /// `Y112` — no such field on this type.
    NoFieldOnType,
    /// `Y113` — a record literal missing one or more fields (HIR-04).
    MissingRecordFields,
    /// `Y114` — a record literal naming a field the type does not have (HIR-04).
    UnknownRecordField,
    /// `Y115` — a record literal naming one field twice (HIR-04).
    DuplicateRecordField,

    // --- Type (`Y12x`), match errors ---
    /// `Y120` — a `match` that does not cover every value.
    NonExhaustiveMatch,
    /// `Y121` — a `match` arm an earlier arm already covers.
    UnreachableArm,
    /// `Y122` — a pattern naming a variant the scrutinee's type has not
    /// (HIR-07).
    UnknownEnumVariant,
    /// `Y123` — a pattern whose shape cannot match the scrutinee (HIR-06).
    NotAPatternForType,

    // --- Input (`I0xx`) ---
    /// `I000` — a parser expression the lowerer cannot read at all.
    MalformedParserExpression,
    /// `I001` — a parser AST that could not be converted to a type or plan.
    ParserConversion,
    /// `I010` — an atomic parser name that does not exist.
    UnknownAtomic,
    /// `I011` — an invalid capture name in a template (IP-04).
    InvalidCaptureName,
    /// `I012` — a capture kind that does not exist (IP-06).
    UnknownCaptureKind,
    /// `I013` — a parser constructor that does not exist (IP-07).
    UnknownConstructor,
    /// `I014` — a constructor argument that is invalid or in excess (IP-07).
    InvalidConstructorArgument,
    /// `I020` — named and anonymous captures mixed in one template (§7.3).
    MixedCaptureNaming,
    /// `I021` — one capture name used twice in a template.
    DuplicateCaptureName,
    /// `I022` — a constructor called with the wrong number of arguments.
    ConstructorArity,
    /// `I023` — an empty separator, which cannot advance a cursor (IP-10).
    EmptySeparator,
    /// `I024` — a section or block field declared twice.
    DuplicateSectionField,
    /// `I025` — a `sections`/`choice` with no field or case at all.
    EmptyFieldList,
    /// `I026` — a positional `block` item returning a scalar with no name.
    UnnamedScalarBlockItem,
    /// `I027` — a `choice` case declared twice.
    DuplicateChoiceCase,
    /// `I028` — a misplaced or repeated `repeated(...)` tail (IP-09).
    MisplacedRepeatedTail,
    /// `I030` — a backtick template the scanner could not read.
    TemplateScan,
}

impl DiagCode {
    /// The rendered code. **The one place a `(category, number)` pair exists.**
    #[must_use]
    pub const fn code(self) -> DiagnosticCode {
        use DiagCode::*;
        use DiagnosticCategory::{Input, Lex, Name, Parse, Type};
        match self {
            UnterminatedBlockComment => DiagnosticCode::new(Lex, 1),
            UnterminatedTemplate => DiagnosticCode::new(Lex, 2),
            UnexpectedCharacter => DiagnosticCode::new(Lex, 3),
            UnterminatedTextLiteral => DiagnosticCode::new(Lex, 4),
            InvalidEscape => DiagnosticCode::new(Lex, 5),

            UnexpectedToken => DiagnosticCode::new(Parse, 1),
            ExpectedStatementSeparator => DiagnosticCode::new(Parse, 2),

            InternalNotASourceFile => DiagnosticCode::new(Name, 0),
            UnknownName => DiagnosticCode::new(Name, 1),
            UnknownType => DiagnosticCode::new(Name, 2),
            NameIsNotAType => DiagnosticCode::new(Name, 3),
            DuplicateDeclaration => DiagnosticCode::new(Name, 4),
            NestedFunction => DiagnosticCode::new(Name, 5),

            TypeMismatch => DiagnosticCode::new(Type, 1),
            InfiniteType => DiagnosticCode::new(Type, 2),
            AnnotationConflict => DiagnosticCode::new(Type, 3),
            NotEquatable => DiagnosticCode::new(Type, 4),
            NotIterable => DiagnosticCode::new(Type, 5),
            NotOrderable => DiagnosticCode::new(Type, 6),
            WrongTypeArgumentCount => DiagnosticCode::new(Type, 7),
            DuplicateMember => DiagnosticCode::new(Type, 8),
            AssignToImmutable => DiagnosticCode::new(Type, 9),
            CompoundAssignNonNumeric => DiagnosticCode::new(Type, 10),
            ReturnOutsideFunction => DiagnosticCode::new(Type, 11),
            BreakOutsideLoop => DiagnosticCode::new(Type, 12),
            IntLiteralOutOfRange => DiagnosticCode::new(Type, 13),
            NotHashable => DiagnosticCode::new(Type, 14),
            NotNumeric => DiagnosticCode::new(Type, 15),
            OperatorNotDefined => DiagnosticCode::new(Type, 16),
            ValueBreakOutsideLoopExpression => DiagnosticCode::new(Type, 17),

            InternalMissingType => DiagnosticCode::new(Type, 99),

            NoMethodOnType => DiagnosticCode::new(Type, 110),
            NoFieldOnType => DiagnosticCode::new(Type, 112),
            MissingRecordFields => DiagnosticCode::new(Type, 113),
            UnknownRecordField => DiagnosticCode::new(Type, 114),
            DuplicateRecordField => DiagnosticCode::new(Type, 115),

            NonExhaustiveMatch => DiagnosticCode::new(Type, 120),
            UnreachableArm => DiagnosticCode::new(Type, 121),
            UnknownEnumVariant => DiagnosticCode::new(Type, 122),
            NotAPatternForType => DiagnosticCode::new(Type, 123),

            MalformedParserExpression => DiagnosticCode::new(Input, 0),
            ParserConversion => DiagnosticCode::new(Input, 1),
            UnknownAtomic => DiagnosticCode::new(Input, 10),
            InvalidCaptureName => DiagnosticCode::new(Input, 11),
            UnknownCaptureKind => DiagnosticCode::new(Input, 12),
            UnknownConstructor => DiagnosticCode::new(Input, 13),
            InvalidConstructorArgument => DiagnosticCode::new(Input, 14),
            MixedCaptureNaming => DiagnosticCode::new(Input, 20),
            DuplicateCaptureName => DiagnosticCode::new(Input, 21),
            ConstructorArity => DiagnosticCode::new(Input, 22),
            EmptySeparator => DiagnosticCode::new(Input, 23),
            DuplicateSectionField => DiagnosticCode::new(Input, 24),
            EmptyFieldList => DiagnosticCode::new(Input, 25),
            UnnamedScalarBlockItem => DiagnosticCode::new(Input, 26),
            DuplicateChoiceCase => DiagnosticCode::new(Input, 27),
            MisplacedRepeatedTail => DiagnosticCode::new(Input, 28),
            TemplateScan => DiagnosticCode::new(Input, 30),
        }
    }

    /// Every code, so a test can assert the allocation is injective.
    pub const ALL: &'static [DiagCode] = {
        use DiagCode::*;
        &[
            UnterminatedBlockComment,
            UnterminatedTemplate,
            UnexpectedCharacter,
            UnterminatedTextLiteral,
            InvalidEscape,
            UnexpectedToken,
            ExpectedStatementSeparator,
            InternalNotASourceFile,
            UnknownName,
            UnknownType,
            NameIsNotAType,
            DuplicateDeclaration,
            NestedFunction,
            TypeMismatch,
            InfiniteType,
            AnnotationConflict,
            NotEquatable,
            NotIterable,
            NotOrderable,
            WrongTypeArgumentCount,
            DuplicateMember,
            AssignToImmutable,
            CompoundAssignNonNumeric,
            ReturnOutsideFunction,
            BreakOutsideLoop,
            IntLiteralOutOfRange,
            NotHashable,
            NotNumeric,
            OperatorNotDefined,
            ValueBreakOutsideLoopExpression,
            InternalMissingType,
            NoMethodOnType,
            NoFieldOnType,
            MissingRecordFields,
            UnknownRecordField,
            DuplicateRecordField,
            NonExhaustiveMatch,
            UnreachableArm,
            UnknownEnumVariant,
            NotAPatternForType,
            MalformedParserExpression,
            ParserConversion,
            UnknownAtomic,
            InvalidCaptureName,
            UnknownCaptureKind,
            UnknownConstructor,
            InvalidConstructorArgument,
            MixedCaptureNaming,
            DuplicateCaptureName,
            ConstructorArity,
            EmptySeparator,
            DuplicateSectionField,
            EmptyFieldList,
            UnnamedScalarBlockItem,
            DuplicateChoiceCase,
            MisplacedRepeatedTail,
            TemplateScan,
        ]
    };
}

impl std::fmt::Display for DiagCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.code().fmt(f)
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
    /// The registered code. Stored as a [`DiagCode`] rather than a rendered
    /// pair so that a diagnostic cannot exist for a number nobody allocated;
    /// [`Diagnostic::code`] renders it on demand.
    code: DiagCode,
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
        code: DiagCode,
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
        code: DiagCode,
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

    /// The rendered `T012`-style code.
    #[inline]
    pub fn code(&self) -> DiagnosticCode {
        self.code.code()
    }

    /// Which diagnostic this is, as the registered name.
    #[inline]
    pub fn kind(&self) -> DiagCode {
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

    /// F2's whole point: two diagnostics must never render the same code. The
    /// numbers used to be integer literals at five crates' call sites, with no
    /// place to look up what was spent — so this could not be asked.
    #[test]
    fn every_code_is_distinct() {
        let mut seen = std::collections::HashMap::new();
        for &code in DiagCode::ALL {
            if let Some(other) = seen.insert(code.to_string(), code) {
                panic!("{other:?} and {code:?} both render {code}");
            }
        }
    }

    /// …and `ALL` really is all of them. A variant left out of the list is a
    /// variant the injectivity test never checks.
    #[test]
    fn all_lists_every_variant() {
        // `ALL` holds each variant once, so its length is the variant count.
        // Update both together; the exhaustive match in `code()` is what makes
        // adding a variant a compile error in the first place.
        assert_eq!(DiagCode::ALL.len(), 57);
        let unique: std::collections::HashSet<_> = DiagCode::ALL.iter().collect();
        assert_eq!(
            unique.len(),
            DiagCode::ALL.len(),
            "a variant is listed twice"
        );
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
            DiagCode::BreakOutsideLoop,
            "expected Int, found Text",
            span(FileId::SYNTHETIC, 0, 1),
        );
        assert_eq!(d.severity(), Severity::Error);
        // `Type` category renders as `Y`, matching the prefix table.
        assert_eq!(d.code().to_string(), "Y012");
        assert_eq!(d.kind(), DiagCode::BreakOutsideLoop);
        assert_eq!(d.message(), "expected Int, found Text");
        assert!(d.notes().is_empty());
        assert!(d.suggestions().is_empty());
    }

    #[test]
    fn builder_adds_notes_and_suggestions() {
        let d = Diagnostic::build(
            Severity::Error,
            DiagCode::UnknownName,
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
            DiagCode::BreakOutsideLoop,
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
            DiagCode::UnknownName,
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
            DiagCode::TypeMismatch,
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
