//! The crash debugger (§9, §14.1).
//!
//! Responsibility (per the design): capture stable per-frame snapshots on
//! fault propagation (function id, current source span, named local slots,
//! local type descriptors, active input parser path), and serve the interactive
//! crash REPL with `bt`, `frame`, `locals`, `p EXPR`, `type EXPR`, `source`,
//! `input`, `parser`, `heap`, `restart`, `reload`, `quit`, `help`.
//!
//! **Milestone 0: skeleton.** The REPL lands in Milestone 10.

/// Marker documenting that this crate is a deliberate skeleton.
pub const FILLED_AT_MILESTONE: u32 = 10;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_reports_fill_milestone() {
        assert_eq!(FILLED_AT_MILESTONE, 10);
    }
}
