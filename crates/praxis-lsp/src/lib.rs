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
//! [`completion`], [`signature`], [`navigation`], [`semantic`], [`rename`],
//! [`inlay`], [`code_action`] and [`workspace`]. [`position`] is the one module
//! in the workspace that knows what a UTF-16 code unit is (ADR-096).
//!
//! The parser sublanguage's editor support — hover on an inner constructor,
//! capture-type completion, the four parser token classes, §15.3's cursor mode —
//! reads the index inference retains on `Analysis` (ADR-098). There is no second
//! scanner over template interiors in this crate, on purpose.
//!
//! # The three rules this crate's editor features are built on
//!
//! Each is stated in full in its own ADR, and each exists so this crate does not
//! come to hold a second opinion about something the compiler decides:
//!
//! - **A quick fix is a diagnostic's machine-applicable suggestion** (ADR-132).
//!   [`code_action`] knows about no particular diagnostic; the fix for a
//!   misspelled constructor is written where the constructor table is consulted.
//! - **A rename is safe when re-resolution is unchanged** (ADR-131). [`rename`]
//!   analyzes the edited text and compares, rather than enumerating collision
//!   kinds against a scope tree that cannot answer where an offset is.
//! - **A name's identity is its symbol, never its spelling.** [`navigation`]'s
//!   references, [`rename`]'s edit set and [`inlay`]'s hints are all keyed on
//!   `SymbolId`, which is what tells two shadowed bindings apart.
//!
//! # What this crate must not reach
//!
//! §19.11's first acceptance criterion is that diagnostics update "without
//! running JIT code". That holds **by construction**: this crate's manifest does
//! not depend on `praxis-mir`, `praxis-codegen-cranelift` or `praxis-runtime`,
//! and a test reads the manifest and says so. Observation would only prove that
//! one path did not reach the JIT today.

pub mod code_action;
pub mod completion;
pub mod diagnostics;
pub mod document;
pub mod hover;
pub mod inlay;
pub mod navigation;
pub mod position;
pub mod query;
pub mod rename;
pub mod semantic;
pub mod server;
pub mod signature;
pub mod workspace;

pub use document::{Document, DocumentStore, Revision};
pub use position::{Encoding, PositionMap};
pub use query::{Analyzer, CompletionContext, Snapshot};
pub use server::{Server, run, serve};

/// The milestone that filled this crate.
pub const FILLED_AT_MILESTONE: u32 = 11;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_crate_reports_the_milestone_that_filled_it() {
        assert_eq!(FILLED_AT_MILESTONE, 11);
    }
}
