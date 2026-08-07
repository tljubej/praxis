//! The crash debugger (§9, §14.1).
//!
//! Responsibility (per the design): capture stable per-frame snapshots on
//! fault propagation (function id, current source span, named local slots,
//! local type descriptors, active input parser path), and serve the interactive
//! crash REPL with `bt`, `frame`, `locals`, `p EXPR`, `type EXPR`, `source`,
//! `input`, `parser`, `heap`, `restart`, `reload`, `quit`, `help`.
//!
//! **M10a:** snapshot rendering + the noninteractive fallback (§9.6) + the
//! interactive REPL navigation/locals (`bt`/`frame`/`up`/`down`/`locals`).
//! **M10b:** the read-only `p EXPR`/`type EXPR` evaluator, the `source`/
//! `input`/`parser`/`heap` context commands, and `restart`/`reload`. The
//! [`session`] module owns the live compile/run state those commands reach.

pub mod evaluate;
pub mod purity;
pub mod render;
pub mod repl;
pub mod session;
pub mod synth;
pub mod tui;
pub mod value;

/// Marker documenting that this crate was a skeleton through Milestone 9 and
/// fills at Milestone 10.
pub const FILLED_AT_MILESTONE: u32 = 10;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_reports_fill_milestone() {
        assert_eq!(FILLED_AT_MILESTONE, 10);
    }
}
