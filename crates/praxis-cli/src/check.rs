//! The `praxis check` command: load a `.px` file, run the front end, render any
//! diagnostics, and return the exit code.
//!
//! Exit codes:
//! - `0` — no errors.
//! - `1` — one or more language errors reported.
//! - `2` — usage error (file missing, unreadable, etc.).
//!
//! `check::run` prints its own diagnostics and never returns `Err` for a
//! user-facing problem (a missing file is reported here, not via `anyhow`),
//! so the exit code it returns is the final one.
//!
//! # The front end is not here (ADR-097)
//!
//! This file used to hold its own parse → analyze → concatenate → sort-by-span
//! sequence. §14.2 requires the CLI and the LSP to share one front-end query
//! API, and M11 built it: `praxis_lsp::query::Snapshot`. The sequence lives
//! there now, stated once, so a divergence between what `praxis check` prints
//! and what the editor underlines is unrepresentable rather than merely
//! unlikely.

use std::path::Path;

use praxis_lsp::query::Snapshot;
use praxis_lsp::Revision;

use crate::color_mode::ColorMode;
use crate::diagnostic_render;

/// Run the `check` command against `file`. Returns the process exit code.
pub fn run(file: &str, color: ColorMode) -> anyhow::Result<i32> {
    let path = Path::new(file);
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(err) => {
            eprintln!("error: failed to read source file `{file}`: {err}");
            return Ok(2);
        }
    };

    // One snapshot, one revision: a process that exits has no revisions to
    // invalidate. The queries it answers are the ones the language server asks.
    let snapshot = Snapshot::new(file, text, Revision(0));
    let diagnostics = snapshot.diagnostics();

    let rendered =
        diagnostic_render::render_all(snapshot.source_map(), &diagnostics, color.palette());
    diagnostic_render::write_to(&mut std::io::stderr(), &rendered)?;

    Ok(if rendered.has_errors() { 1 } else { 0 })
}
