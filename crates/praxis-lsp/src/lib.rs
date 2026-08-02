//! The Praxis language server (§15, §14.1).
//!
//! Two responsibilities, in two halves that do not depend on each other:
//!
//! - **[`query`]** — the shared front-end query API §14.2 requires the CLI and
//!   the LSP to have in common. `praxis check` routes through it (ADR-097), so a
//!   divergence between what the CLI prints and what the editor underlines is
//!   unrepresentable rather than merely unlikely.
//! - **[`server`]** — JSON-RPC over stdio, one synchronous loop, no async
//!   runtime (ADR-095).
//!
//! Everything between them is a query: [`diagnostics`], [`hover`],
//! [`completion`], [`signature`], [`navigation`], [`semantic`]. [`position`] is
//! the one module in the workspace that knows what a UTF-16 code unit is
//! (ADR-096).
//!
//! The parser sublanguage's editor support — hover on an inner constructor,
//! capture-type completion, the four parser token classes, §15.3's cursor mode —
//! reads the index inference retains on `Analysis` (ADR-098). There is no second
//! scanner over template interiors in this crate, on purpose.
//!
//! # What this crate must not reach
//!
//! §19.11's first acceptance criterion is that diagnostics update "without
//! running JIT code". That holds **by construction**: this crate's manifest does
//! not depend on `praxis-mir`, `praxis-codegen-cranelift` or `praxis-runtime`,
//! and a test reads the manifest and says so. Observation would only prove that
//! one path did not reach the JIT today.

pub mod completion;
pub mod diagnostics;
pub mod document;
pub mod hover;
pub mod navigation;
pub mod position;
pub mod query;
pub mod semantic;
pub mod server;
pub mod signature;

pub use document::{Document, DocumentStore, Revision};
pub use position::{Encoding, PositionMap};
pub use query::{Analyzer, CompletionContext, Snapshot};
pub use server::{run, serve, Server};

/// The milestone that filled this crate. M0's skeleton advertised it and a test
/// asserted it; the number is now a fact rather than a promise.
pub const FILLED_AT_MILESTONE: u32 = 11;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_crate_reports_the_milestone_that_filled_it() {
        assert_eq!(FILLED_AT_MILESTONE, 11);
    }
}
