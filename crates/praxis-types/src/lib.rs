//! Type interning, inference, and capability resolution (§5, §14.1).
//!
//! Responsibility (per the design): an HM-inspired inference engine with
//! extensions for mutable variables, nominal records and enums, anonymous
//! structural records, built-in collection constructors, closure types, a
//! small internal capability system, and monomorphization.
//!
//! **Milestone 0: skeleton.** Inference lands in Milestone 2.

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
