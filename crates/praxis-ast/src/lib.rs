//! Typed wrappers over lossless syntax nodes (§13.2, §14.1).
//!
//! Responsibility (per the design): expose typed node wrappers over the
//! syntax tree produced by `praxis-parser`, avoiding copies of source strings.
//!
//! **Milestone 0: skeleton.** The concrete node types land in Milestone 1
//! alongside the lossless parser. This crate exists now so the workspace DAG is
//! complete and later milestones have a home.

/// Marker documenting that this crate is a deliberate skeleton.
///
/// Real contents arrive with the milestone noted in the module docs. The
/// constant carries that milestone id so the LSP and tooling can tell a stubbed
/// area from a bug.
pub const FILLED_AT_MILESTONE: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_reports_fill_milestone() {
        assert_eq!(FILLED_AT_MILESTONE, 1);
    }
}
