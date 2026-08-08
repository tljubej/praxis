//! Shared test helpers for the Praxis compiler.
//!
//! The two things every diagnostic / golden-tree test in the project needs:
//!
//! - A source-map helper for one-file fixtures, so tests do not repeat the
//!   boilerplate of building a [`SourceMap`](praxis_source::SourceMap) and a
//!   [`FileId`](praxis_source::FileId).
//! - A stable, human-reviewable dump of a lossless syntax tree, so golden-tree
//!   assertions compare text rather than tree shape.
//!
//! Rendering diagnostics is deliberately *not* here: the only loop anyone runs
//! is praxis-cli's `render_all`, which styles, tallies by severity and guards
//! the separator, and praxis-source already carries the per-diagnostic
//! `Renderer`.
//!
//! This is the canonical harness for the §17.2 "golden tree + bless mode"
//! testing strategy. To accept snapshot updates locally:
//!
//! ```text
//! INSTA_UPDATE=always cargo test          # auto-accept
//! cargo insta review                       # interactive review
//! ```
//! and commit the resulting `.snap` files (never the `.snap.new` ones, which
//! are gitignored).

// Nothing public: it holds the one test that keeps the `.snap` bless path above
// honest, since every other snapshot in the workspace is an inline one.
mod snapshot;
pub mod source;
pub mod tree;

pub use source::single_file;
pub use tree::format_syntax_tree;

// `insta` re-exported so a test crate *can* depend on this crate alone. Most of
// them name `insta` in their own Cargo.toml and write `insta::` directly, so
// this is an offer rather than the only door in.
pub use insta;
