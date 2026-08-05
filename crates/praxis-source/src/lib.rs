//! Source files, spans, line maps, and diagnostics for the Praxis compiler.
//!
//! This is the leaf crate of the workspace: every other compiler crate reads
//! source through `SourceMap` and reports problems as `Diagnostic`s. The types
//! here are designed so that illegal states are unrepresentable — an inverted
//! span, an orphan span without a file, or a diagnostic without a primary span
//! cannot be constructed through the public API.
//!
//! See `praxis_technical_design.md` §14.1 (`praxis-source` responsibilities)
//! and §8 (diagnostic format).

pub mod diagnostic;
pub mod file;
pub mod line_map;
pub mod snippet;
pub mod span;
pub mod style;
pub mod suggest;

pub use diagnostic::{
    render_one, DiagCode, Diagnostic, DiagnosticCategory, DiagnosticCode, DiagnosticNote, Renderer,
    Severity, Suggestion,
};
pub use file::{FileId, SourceFile, SourceMap};
pub use line_map::{LineCol, LineMap};
pub use span::{BytePos, FileSpan, Span};
pub use suggest::nearest;
