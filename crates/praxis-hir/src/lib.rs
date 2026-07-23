//! Name resolution and the high-level intermediate representation (§13.3, §14.1).
//!
//! Responsibility (per the design): resolve names, remove surface sugar (top
//! level becomes a generated `main`, method calls become resolved intrinsic or
//! function calls, `for` becomes explicit iteration, `read`/`parse` parser
//! expressions become typed parser plans, string interpolation becomes
//! formatting nodes).
//!
//! **Milestone 0: skeleton.** Name resolution lands in Milestone 2.

/// Marker documenting that this crate is a deliberate skeleton.
pub const FILLED_AT_MILESTONE: u32 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_reports_fill_milestone() {
        assert_eq!(FILLED_AT_MILESTONE, 2);
    }
}
