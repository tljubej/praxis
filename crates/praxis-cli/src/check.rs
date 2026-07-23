//! The `praxis check` command: load a `.px` file, run the front end (lex for
//! Milestone 0), render any diagnostics, and return the exit code.
//!
//! Exit codes:
//! - `0` — no errors.
//! - `1` — one or more language errors reported.
//! - `2` — usage error (file missing, unreadable, etc.).
//!
//! `check::run` prints its own diagnostics and never returns `Err` for a
//! user-facing problem (a missing file is reported here, not via `anyhow`),
//! so the exit code it returns is the final one.

use std::path::Path;

use crate::diagnostic_render;

/// Run the `check` command against `file`. Returns the process exit code.
pub fn run(file: &str) -> anyhow::Result<i32> {
    let path = Path::new(file);
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(err) => {
            eprintln!("error: failed to read source file `{file}`: {err}");
            return Ok(2);
        }
    };

    // Intern the file into the source map so diagnostics can reference it.
    let source = praxis_source::SourceMap::new();
    let id = source.intern(path, text.clone());

    // Milestone 0 front end: just the lexer stub. The full pipeline
    // (parse + resolve + type-check) is layered in by later milestones.
    let lexed = praxis_parser::lex(id, &text);

    let rendered = diagnostic_render::render_all(&source, &lexed.diagnostics);
    diagnostic_render::write_to(&mut std::io::stderr(), &rendered)?;

    Ok(if rendered.has_errors() { 1 } else { 0 })
}
