//! The crash debugger (§9, §14.1).
//!
//! Responsibility (per the design): capture stable per-frame snapshots on
//! fault propagation (function id, current source span, named local slots,
//! local type descriptors, active input parser path), and serve the interactive
//! crash REPL with `bt`, `frame`, `locals`, `p EXPR`, `type EXPR`, `source`,
//! `input`, `parser`, `heap`, `restart`, `reload`, `quit`, `help`.
//!
//! **M10a:** snapshot rendering + the noninteractive fallback (§9.6). The
//! interactive REPL navigation/locals lands in WS5; `p EXPR`/`type EXPR` and
//! the context commands land in M10b.

pub mod render;
pub mod repl;

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
