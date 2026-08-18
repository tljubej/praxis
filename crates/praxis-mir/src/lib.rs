//! The mid-level IR (§13.5, §14.1).
//!
//! Responsibility (per the design): basic blocks with explicit branches, local
//! slots containing `GcRef` for every language value, calls, allocation
//! instructions, bounds and overflow checks, fault edges, GC safepoints, and
//! debug-local metadata. MIR does not need to be SSA initially — the Cranelift
//! lowering layer creates SSA values and block parameters.
//!
//! The data structures live in [`ir`] (ADR-015); the GC-liveness pass (§12.3)
//! that computes the minimal root set per safepoint lives in [`liveness`]; the
//! HIR→MIR lowering lives in [`build`]; and [`verify`] is what keeps the rooting
//! invariant fixed once a stage has fixed it — every host runs it after
//! `annotate` and refuses to compile MIR that fails.
//!
//! [`forward`] and [`promote`] are the crate's *optimizations*, and both run
//! inside `lower_module` rather than beside it, because each deletes safepoints
//! and so must precede [`annotate`] (ADR-120, ADR-121, ADR-108 §1).
//!
//! They run in that order and the order is load-bearing, not alphabetical.
//! [`forward`] is a peephole over the box/unbox pairs the builder's single
//! return convention emits, and it leaves behind exactly the boxes that cross a
//! block boundary; [`promote`] is the whole-function pass that decides those
//! slots' representation. Running `promote` first would make it price
//! materializations `forward` was about to delete, and decline promotions on the
//! strength of a cost that was never going to be paid.

pub mod annot;
pub mod build;
pub mod forward;
pub mod ir;
pub mod liveness;
pub mod promote;
pub mod provable;
pub mod verify;

/// Shape assertions over lowered MIR, for tests whose gate is a count of
/// emitted instructions.
///
/// `cfg(test)` **and** the feature, because there are two consumers and they
/// need different doors: this crate's own tests get it with no flag to
/// remember, and another crate's tests get it by asking for
/// `features = ["test-support"]` in their `[dev-dependencies]`. Neither door is
/// open to `cargo build`, which is the point — the front end this module needs
/// is not a dependency of the compiler's MIR crate.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use annot::{DebugSlots, RootSlots, roots_of, slot_sets};
pub use build::lower_module;
pub use forward::forward_boxes;
pub use ir::{
    AllocKind, Block, BlockId, CallTarget, CmpOp, FloatBinOp, Function, GcConst, Inst, IntBinOp,
    Local, LocalId, LocalKind, MirType, Overflow, ScalarKind, Terminator,
};
pub use liveness::{annotate, defs};
pub use promote::promote_scalars;
pub use provable::{DescriptorClass, ProvableDescriptors};
pub use verify::{VerifyError, defines, verify};

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
