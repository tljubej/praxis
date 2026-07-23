//! Shared test helpers for the Praxis compiler.
//!
//! The two things every diagnostic / golden-tree test in the project needs:
//!
//! - A thin snapshot layer over [`insta`](https://crates.io/crates/insta), so
//!   the snapshot library is swappable and tests depend on this crate, not on
//!   `insta` directly.
//! - A source-map helper for one-file fixtures, so tests do not repeat the
//!   boilerplate of building a [`SourceMap`](praxis_source::SourceMap) and a
//!   [`FileId`](praxis_source::FileId).
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

pub mod snapshot;
pub mod source;
pub mod tree;

pub use snapshot::{render_diagnostics, snapshot_diagnostics};
pub use source::single_file;
pub use tree::format_syntax_tree;

// Re-export the snapshot macro so test crates depend only on this crate.
pub use insta;
