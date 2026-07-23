//! The Praxis language server (§15, §14.1).
//!
//! Responsibility (per the design): speak JSON-RPC LSP over stdio, maintain
//! open-document overlays and source revisions, and answer diagnostics, hover,
//! completion, signature help, go-to-definition, references, semantic tokens,
//! and formatting queries by reusing the compiler front end. The same query API
//! is shared between the CLI and the LSP (§14.2).
//!
//! **Milestone 0: skeleton.** The LSP MVP lands in Milestone 11.

/// Marker documenting that this crate is a deliberate skeleton.
pub const FILLED_AT_MILESTONE: u32 = 11;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_reports_fill_milestone() {
        assert_eq!(FILLED_AT_MILESTONE, 11);
    }
}
