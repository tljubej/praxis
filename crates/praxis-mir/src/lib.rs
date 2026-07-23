//! The mid-level IR (§13.5, §14.1).
//!
//! Responsibility (per the design): basic blocks with explicit branches, local
//! slots containing `GcRef` for every language value, calls, allocation
//! instructions, bounds and overflow checks, fault edges, GC safepoints, and
//! debug-local metadata. MIR does not need to be SSA initially — the Cranelift
//! lowering layer creates SSA values and block parameters.
//!
//! Milestone 4 fills this crate (ADR-015). The data structures live in [`ir`];
//! the GC-liveness pass (§12.3) that computes the minimal root set per
//! safepoint lives in [`liveness`]; the HIR→MIR lowering lives in [`build`].

pub mod build;
pub mod ir;
pub mod liveness;

pub use build::lower_module;
pub use ir::{
    AllocKind, Block, BlockId, CallTarget, CmpOp, Function, Inst, IntBinOp, Local, LocalId,
    LocalKind, ScalarKind, Terminator,
};
pub use liveness::annotate;

/// Marker documenting the milestone that filled this crate.
pub const FILLED_AT_MILESTONE: u32 = 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filled_at_milestone_is_4() {
        assert_eq!(FILLED_AT_MILESTONE, 4);
    }
}
