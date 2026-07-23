//! The mid-level IR (§13.5, §14.1).
//!
//! Responsibility (per the design): basic blocks with explicit branches, local
//! slots containing `GcRef` for every language value, calls, allocation
//! instructions, bounds and overflow checks, fault edges, GC safepoints, and
//! debug-local metadata. MIR does not need to be SSA initially — the Cranelift
//! lowering layer creates SSA values and block parameters.
//!
//! **Milestone 0: skeleton.** MIR for object-based control flow lands in
//! Milestone 4.

/// Marker documenting that this crate is a deliberate skeleton.
pub const FILLED_AT_MILESTONE: u32 = 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_reports_fill_milestone() {
        assert_eq!(FILLED_AT_MILESTONE, 4);
    }
}
