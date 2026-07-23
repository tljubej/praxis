//! The `praxis check` command: load a `.px` file, run the front end (lex +
//! parse as of Milestone 1), render any diagnostics, and return the exit code.
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

    // Front end: lex + parse, producing a lossless tree and any `T0xx` (lex) /
    // `P0xx` (parse) diagnostics. Then name resolution + type inference (M2)
    // add `N0xx` (name) / `Y0xx` (type) diagnostics on top.
    let parsed = praxis_parser::parse(id, &text);
    let mut diagnostics = parsed.diagnostics;

    // Skip semantic analysis if parsing already failed badly enough that the
    // tree would mislead resolution. We still run analysis when there are parse
    // errors (recovery keeps the tree usable), but only if the root is intact.
    let analysis = praxis_hir::analyze_root(id, &parsed.tree);
    diagnostics.extend(analysis.diagnostics);
    diagnostics.sort_by_key(|d| {
        let s = d.primary().span;
        (s.start(), s.end())
    });

    let rendered = diagnostic_render::render_all(&source, &diagnostics);
    diagnostic_render::write_to(&mut std::io::stderr(), &rendered)?;

    Ok(if rendered.has_errors() { 1 } else { 0 })
}
