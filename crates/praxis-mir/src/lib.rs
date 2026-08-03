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
//! safepoint lives in [`liveness`]; the HIR→MIR lowering lives in [`build`];
//! and [`verify`] is what keeps the rooting invariant fixed once a stage has
//! fixed it — every host runs it after `annotate` and refuses to compile MIR
//! that fails.

pub mod annot;
pub mod build;
pub mod ir;
pub mod liveness;
pub mod provable;
pub mod verify;

/// Shape assertions over lowered MIR, for the packages whose gate is a count of
/// emitted instructions (handover 26 §2).
///
/// `cfg(test)` **and** the feature, because there are two consumers and they
/// need different doors: this crate's own tests get it with no flag to
/// remember, and another crate's tests get it by asking for
/// `features = ["test-support"]` in their `[dev-dependencies]`. Neither door is
/// open to `cargo build`, which is the point — the front end this module needs
/// is not a dependency of the compiler's MIR crate.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use annot::{DebugSlots, RootSlots};
pub use build::lower_module;
pub use ir::{
    AllocKind, Block, BlockId, CallTarget, CmpOp, FloatBinOp, Function, GcConst, Inst, IntBinOp,
    Local, LocalId, LocalKind, MirType, Overflow, ScalarKind, Terminator,
};
pub use liveness::{annotate, defs};
pub use provable::{DescriptorClass, ProvableDescriptors};
pub use verify::{defines, verify, VerifyError};

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
