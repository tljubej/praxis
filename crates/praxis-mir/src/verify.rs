//! The MIR verifier (F17, MIR-10): the pass that makes a fixed invariant stay
//! fixed.
//!
//! ADR-015 §10.3 states the rooting invariant — the GC root set at a safepoint
//! is exactly the live `Gc` locals, and a `Scalar` slot holds a raw word the
//! collector must never see — but nothing checked it. That is how the integer
//! `1` came to inhabit a rootable slot (P0-03, a closure capture index moved
//! into a `Gc` local) and how sixty-one hand-written root lists could disagree
//! with the liveness pass without anything going red. A fix with no verifier
//! behind it is a fix that regresses quietly.
//!
//! [`verify`] runs after [`crate::annotate`] at every pipeline site. It is
//! cheap — one linear pass — and it is a hard error: a function that fails it
//! is a compiler bug, not a user error, so the diagnostic names the function,
//! block and instruction rather than a source span.
//!
//! # What is checked, and what is deliberately not
//!
//! Checked: every slot-set member is a `Gc` local that exists; every operand
//! and destination is in range; `MoveGc` is `Gc → Gc`; a GC safepoint's root
//! set was actually annotated; a branch target exists; a `Return` yields a `Gc`
//! local; a branch condition is a scalar; a `Bounded` arithmetic site is not a
//! division.
//!
//! **`MissingTerminator` is unrepresentable**, so there is no rule for it:
//! [`Block::term`](crate::ir::Block::term) is not an `Option`, and
//! [`Function::new_block`](crate::ir::Function::new_block) installs a
//! self-jump placeholder that `BadBlockTarget` would not catch anyway.
//!
//! **`ScalarLiveAcrossSafepoint` is not implemented, and that is a decision.**
//! F17 predicted it would fire on the eager `lower_seq_*` accumulators, and it
//! would: a `sum`'s running `i64` is live across every `praxis_vec_get` call in
//! the loop by construction. It is also harmless there. A scalar is a *copy* of
//! a payload, so it cannot dangle when the object it came from is collected;
//! the invariant that actually matters is "no raw word in a slot the collector
//! reads", and `RootIsNotGc` plus `MoveGcFromScalar` are that invariant stated
//! directly. Turning the weaker rule on would mean either moving every
//! accumulator into a `Gc` slot (an allocation per iteration) or weakening the
//! rule until it says nothing.
//!
//! **`OpaqueAtDescriptorSite` is still off, and S15 found out why** (hazard
//! H10). The plan schedules it here, on the grounds that lowering could not
//! supply per-use types until F15. F15 landed, and it supplied them: the
//! `for`-loop item, the parser result, the closure value, the indirect call
//! result and a pipeline's *source* item are all `MirType::Known` now, and
//! every `AllocKind::Collection` a program writes carries real type arguments.
//!
//! Two descriptor sites are left, both in the fused pipeline, and both are
//! `AllocKind::Tuple { ty: Opaque }` — the tuple a fused `enumerate` or `zip`
//! builds. Turning the rule on would refuse to compile every program that uses
//! either. They need **two** things, not one:
//!
//! 1. **MIR-05's per-stage item types** (S21). A fused chain knows what its
//!    source yields; what stage *n* yields is a fact no stage carries.
//! 2. **A method catalog that describes `enumerate` and `zip`.** Their rows
//!    declare `result: Vec[T]` — the receiver's own element type — so
//!    `v.enumerate()` on a `Vec[Int]` types as `Vec[Int]` rather than
//!    `Vec[(Int, Int)]`. F15 makes lowering *believe* the catalog, where it
//!    used to re-derive a harmless fresh variable from the same row, so
//!    deriving the tuple's type from the chain's result type today would
//!    replace an honest null descriptor with a wrong one. See the S15 entry in
//!    the repair progress note; this is a finding the register does not have.
//!
//! `MirType::expect_known` lands with the rule that needs it.

use std::collections::BTreeSet;

use crate::ir::{
    AllocKind, BlockId, Function, Inst, IntBinOp, LocalId, LocalKind, Overflow, Terminator,
};

/// One way a [`Function`] can be malformed.
///
/// Every variant names the function, block and instruction index so a failure
/// is locatable without re-running the pass by hand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// A slot set names a local that is not a [`LocalKind::Gc`] slot. The
    /// collector dereferences everything the shadow frame holds, so a `Scalar`
    /// here is a raw word handed to the marker (P0-03, ADR-015 §10.3).
    RootIsNotGc {
        func: String,
        block: BlockId,
        inst: usize,
        local: LocalId,
        set: SlotSet,
    },
    /// An instruction names a local index past the end of `locals`.
    LocalOutOfRange {
        func: String,
        block: BlockId,
        inst: usize,
        local: LocalId,
    },
    /// A `MoveGc` whose source or destination is a `Scalar` slot.
    /// [`Inst::Materialize`](crate::ir::Inst::Materialize) is the one legal
    /// `Scalar → Gc` transition (P0-03).
    MoveGcFromScalar {
        func: String,
        block: BlockId,
        inst: usize,
        dst: LocalId,
        src: LocalId,
    },
    /// A GC safepoint whose [`RootSlots`](crate::RootSlots) was never filled.
    /// An *annotated empty* set is a real answer; an unannotated one means the
    /// liveness pass did not run, and the backend would spill nothing.
    UnannotatedSafepoint {
        func: String,
        block: BlockId,
        inst: usize,
    },
    /// A `CheckFault` whose [`DebugSlots`](crate::DebugSlots) was never filled:
    /// a crash snapshot taken on that fault path would render `<uninit>`.
    UnannotatedDebugPoint {
        func: String,
        block: BlockId,
        inst: usize,
    },
    /// A safepoint that both spills and nulls the same slot.
    LiveAndDeadOverlap {
        func: String,
        block: BlockId,
        inst: usize,
        local: LocalId,
    },
    /// A branch, jump or fault edge naming a block that does not exist
    /// (MIR-11's class).
    BadBlockTarget {
        func: String,
        block: BlockId,
        target: BlockId,
    },
    /// `Terminator::Return` yielding a `Scalar` slot: the ABI returns a
    /// `GcRef`, so this would hand a raw payload word to the caller.
    ReturnIsNotGc {
        func: String,
        block: BlockId,
        local: LocalId,
    },
    /// `Terminator::Branch` on a `Gc` local. The backend branches on a raw
    /// truth value, so a boxed `Bool` here tests a pointer for zero.
    BranchOnGc {
        func: String,
        block: BlockId,
        local: LocalId,
    },
    /// An [`Overflow::Bounded`] `Div`/`Rem`. A bound on the operands cannot
    /// rule out a zero divisor, and `sdiv`/`srem` *trap* on one — a process
    /// abort, not a fault.
    BoundedDivision {
        func: String,
        block: BlockId,
        inst: usize,
        op: IntBinOp,
    },
}

/// Which slot set a [`VerifyError::RootIsNotGc`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotSet {
    /// The GC roots spilled at a safepoint.
    Live,
    /// The stale slots nulled at a safepoint.
    Dead,
    /// The debugger-visible set.
    Debug,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::RootIsNotGc {
                func,
                block,
                inst,
                local,
                set,
            } => write!(
                f,
                "{func}: block {} inst {inst}: {set:?} slot set names {local:?}, \
                 which is not a Gc local",
                block.0
            ),
            VerifyError::LocalOutOfRange {
                func,
                block,
                inst,
                local,
            } => write!(
                f,
                "{func}: block {} inst {inst}: {local:?} is out of range",
                block.0
            ),
            VerifyError::MoveGcFromScalar {
                func,
                block,
                inst,
                dst,
                src,
            } => write!(
                f,
                "{func}: block {} inst {inst}: MoveGc {dst:?} <- {src:?} crosses \
                 the Scalar/Gc boundary; use Materialize",
                block.0
            ),
            VerifyError::UnannotatedSafepoint { func, block, inst } => write!(
                f,
                "{func}: block {} inst {inst}: safepoint has no annotated root set",
                block.0
            ),
            VerifyError::UnannotatedDebugPoint { func, block, inst } => write!(
                f,
                "{func}: block {} inst {inst}: debug point has no annotated slot set",
                block.0
            ),
            VerifyError::LiveAndDeadOverlap {
                func,
                block,
                inst,
                local,
            } => write!(
                f,
                "{func}: block {} inst {inst}: {local:?} is both spilled and nulled",
                block.0
            ),
            VerifyError::BadBlockTarget {
                func,
                block,
                target,
            } => write!(
                f,
                "{func}: block {} targets block {}, which does not exist",
                block.0, target.0
            ),
            VerifyError::ReturnIsNotGc { func, block, local } => write!(
                f,
                "{func}: block {} returns {local:?}, which is not a Gc local",
                block.0
            ),
            VerifyError::BranchOnGc { func, block, local } => write!(
                f,
                "{func}: block {} branches on {local:?}, which is a Gc local",
                block.0
            ),
            VerifyError::BoundedDivision {
                func,
                block,
                inst,
                op,
            } => write!(
                f,
                "{func}: block {} inst {inst}: {op:?} is marked Bounded, but no \
                 bound rules out a zero divisor",
                block.0
            ),
        }
    }
}

/// Check `f`'s structural and rooting invariants. `Ok(())` or every violation
/// found, in program order.
pub fn verify(f: &Function) -> Result<(), Vec<VerifyError>> {
    let mut errs = Vec::new();
    let n_locals = f.locals.len();
    let n_blocks = f.blocks.len();
    let gc: BTreeSet<LocalId> = f
        .locals
        .iter()
        .filter(|l| l.kind == LocalKind::Gc)
        .map(|l| l.id)
        .collect();

    for (blk_idx, block) in f.blocks.iter().enumerate() {
        let bid = BlockId(blk_idx as u32);
        for (i, inst) in block.insts.iter().enumerate() {
            for local in operands(inst) {
                if local.0 as usize >= n_locals {
                    errs.push(VerifyError::LocalOutOfRange {
                        func: f.name.clone(),
                        block: bid,
                        inst: i,
                        local,
                    });
                }
            }
            check_slot_sets(f, bid, i, inst, &gc, n_locals, &mut errs);

            match inst {
                Inst::MoveGc { dst, src } => {
                    if !gc.contains(dst) || !gc.contains(src) {
                        errs.push(VerifyError::MoveGcFromScalar {
                            func: f.name.clone(),
                            block: bid,
                            inst: i,
                            dst: *dst,
                            src: *src,
                        });
                    }
                }
                Inst::IntBinOp { op, overflow, .. } => {
                    if matches!(overflow, Overflow::Bounded)
                        && matches!(op, IntBinOp::Div | IntBinOp::Rem)
                    {
                        errs.push(VerifyError::BoundedDivision {
                            func: f.name.clone(),
                            block: bid,
                            inst: i,
                            op: *op,
                        });
                    }
                }
                Inst::CheckFault { on_fault, .. } if on_fault.0 as usize >= n_blocks => {
                    errs.push(VerifyError::BadBlockTarget {
                        func: f.name.clone(),
                        block: bid,
                        target: *on_fault,
                    });
                }
                _ => {}
            }
        }

        match &block.term {
            Terminator::Branch {
                cond,
                then_block,
                else_block,
            } => {
                for &t in &[*then_block, *else_block] {
                    if t.0 as usize >= n_blocks {
                        errs.push(VerifyError::BadBlockTarget {
                            func: f.name.clone(),
                            block: bid,
                            target: t,
                        });
                    }
                }
                if cond.0 as usize >= n_locals {
                    errs.push(VerifyError::LocalOutOfRange {
                        func: f.name.clone(),
                        block: bid,
                        inst: block.insts.len(),
                        local: *cond,
                    });
                } else if gc.contains(cond) {
                    errs.push(VerifyError::BranchOnGc {
                        func: f.name.clone(),
                        block: bid,
                        local: *cond,
                    });
                }
            }
            Terminator::Jump { target } => {
                if target.0 as usize >= n_blocks {
                    errs.push(VerifyError::BadBlockTarget {
                        func: f.name.clone(),
                        block: bid,
                        target: *target,
                    });
                }
            }
            Terminator::Return { value } => {
                if value.0 as usize >= n_locals {
                    errs.push(VerifyError::LocalOutOfRange {
                        func: f.name.clone(),
                        block: bid,
                        inst: block.insts.len(),
                        local: *value,
                    });
                } else if !gc.contains(value) {
                    errs.push(VerifyError::ReturnIsNotGc {
                        func: f.name.clone(),
                        block: bid,
                        local: *value,
                    });
                }
            }
            Terminator::Fault => {}
        }
    }

    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

/// Render a verifier failure as one multi-line message, for a host that has no
/// better place to put a list.
#[must_use]
pub fn report(errs: &[VerifyError]) -> String {
    let mut out = format!("MIR verification failed ({} problems):", errs.len());
    for e in errs {
        out.push_str("\n  ");
        out.push_str(&e.to_string());
    }
    out
}

/// The slot sets of one instruction, checked for membership and annotation.
fn check_slot_sets(
    f: &Function,
    bid: BlockId,
    i: usize,
    inst: &Inst,
    gc: &BTreeSet<LocalId>,
    n_locals: usize,
    errs: &mut Vec<VerifyError>,
) {
    let (roots, debug) = match inst {
        Inst::Alloc { roots, debug, .. }
        | Inst::Materialize { roots, debug, .. }
        | Inst::Call { roots, debug, .. }
        | Inst::CallIndirect { roots, debug, .. }
        | Inst::StructEq { roots, debug, .. } => (Some(roots), Some(debug)),
        Inst::CheckFault { debug, .. } => (None, Some(debug)),
        _ => (None, None),
    };

    if let Some(roots) = roots {
        if !roots.is_annotated() {
            errs.push(VerifyError::UnannotatedSafepoint {
                func: f.name.clone(),
                block: bid,
                inst: i,
            });
        }
        for (ids, set) in [(roots.live(), SlotSet::Live), (roots.dead(), SlotSet::Dead)] {
            for &local in ids {
                check_is_gc(f, bid, i, local, set, gc, n_locals, errs);
            }
        }
        for &local in roots.dead() {
            if roots.live().contains(&local) {
                errs.push(VerifyError::LiveAndDeadOverlap {
                    func: f.name.clone(),
                    block: bid,
                    inst: i,
                    local,
                });
            }
        }
    }

    if let Some(debug) = debug {
        // Only `CheckFault` is a debug point in its own right; on a safepoint
        // the set rides along with the roots and is annotated with them.
        if !debug.is_annotated() && matches!(inst, Inst::CheckFault { .. }) {
            errs.push(VerifyError::UnannotatedDebugPoint {
                func: f.name.clone(),
                block: bid,
                inst: i,
            });
        }
        for &local in debug.visible() {
            check_is_gc(f, bid, i, local, SlotSet::Debug, gc, n_locals, errs);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn check_is_gc(
    f: &Function,
    bid: BlockId,
    i: usize,
    local: LocalId,
    set: SlotSet,
    gc: &BTreeSet<LocalId>,
    n_locals: usize,
    errs: &mut Vec<VerifyError>,
) {
    if local.0 as usize >= n_locals {
        errs.push(VerifyError::LocalOutOfRange {
            func: f.name.clone(),
            block: bid,
            inst: i,
            local,
        });
    } else if !gc.contains(&local) {
        errs.push(VerifyError::RootIsNotGc {
            func: f.name.clone(),
            block: bid,
            inst: i,
            local,
            set,
        });
    }
}

/// Every local an instruction reads or writes, for the range check. Slot sets
/// are checked separately (they are derived, not operands).
fn operands(inst: &Inst) -> Vec<LocalId> {
    let mut v = Vec::new();
    match inst {
        Inst::ConstInt { dst, .. } | Inst::ConstFloat { dst, .. } => v.push(*dst),
        Inst::Alloc { dst, alloc, .. } => {
            v.push(*dst);
            match alloc {
                AllocKind::Int { value }
                | AllocKind::Bool { value }
                | AllocKind::Char { value }
                | AllocKind::Float { value } => v.push(*value),
                AllocKind::Record { fields, .. } => v.extend(fields.iter().copied()),
                AllocKind::Tuple { elements, .. } => v.extend(elements.iter().copied()),
                AllocKind::Enum { args, .. } => v.extend(args.iter().copied()),
                AllocKind::Closure { captures, .. } => v.extend(captures.iter().copied()),
                AllocKind::Unit | AllocKind::Text { .. } | AllocKind::Collection { .. } => {}
            }
        }
        Inst::ExtractScalar { dst, src, .. }
        | Inst::Materialize { dst, src, .. }
        | Inst::LoadField { dst, src, .. }
        | Inst::EnumTag { dst, src }
        | Inst::EnumPayloadGet { dst, src, .. }
        | Inst::MoveGc { dst, src } => v.extend([*dst, *src]),
        Inst::StoreScalar { dst_gc, src, .. } => v.extend([*dst_gc, *src]),
        Inst::IntBinOp { dst, lhs, rhs, .. }
        | Inst::FloatBinOp { dst, lhs, rhs, .. }
        | Inst::IntCmp { dst, lhs, rhs, .. }
        | Inst::FloatCmp { dst, lhs, rhs, .. }
        | Inst::StructEq { dst, lhs, rhs, .. }
        | Inst::ValueCmp { dst, lhs, rhs } => v.extend([*dst, *lhs, *rhs]),
        Inst::Call { dst, args, .. } => {
            v.push(*dst);
            v.extend(args.iter().copied());
        }
        Inst::CallIndirect {
            dst, callee, args, ..
        } => {
            v.extend([*dst, *callee]);
            v.extend(args.iter().copied());
        }
        Inst::LoadCapture { dst, closure, .. } => v.extend([*dst, *closure]),
        Inst::CheckFault { .. } => {}
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annot::{DebugSlots, RootSlots};
    use crate::ir::{LocalDebugKind, MirType, ScalarKind};

    fn empty_fn(name: &str) -> Function {
        Function {
            name: name.into(),
            params: Vec::new(),
            return_local: LocalId(0),
            locals: Vec::new(),
            blocks: Vec::new(),
            debug_names: Vec::new(),
            debug_kinds: Vec::new(),
            debug_spans: Vec::new(),
            span: (0, 0),
        }
    }

    fn gc_local(f: &mut Function) -> LocalId {
        f.new_local(
            LocalKind::Gc,
            MirType::Opaque,
            None,
            LocalDebugKind::Temp,
            None,
        )
    }

    fn int_local(f: &mut Function) -> LocalId {
        f.new_local(
            LocalKind::Scalar(ScalarKind::Int),
            MirType::Opaque,
            None,
            LocalDebugKind::Temp,
            None,
        )
    }

    /// A one-block function that allocates an `Int` into `dst` and returns it.
    fn alloc_and_return() -> (Function, LocalId, LocalId) {
        let mut f = empty_fn("f");
        let scalar = int_local(&mut f);
        let dst = gc_local(&mut f);
        f.return_local = dst;
        let blk = f.new_block();
        f.blocks[blk.0 as usize].insts.push(Inst::ConstInt {
            dst: scalar,
            value: 1,
        });
        f.blocks[blk.0 as usize].insts.push(Inst::Alloc {
            dst,
            alloc: AllocKind::Int { value: scalar },
            roots: RootSlots::unannotated(),
            debug: DebugSlots::unannotated(),
        });
        f.blocks[blk.0 as usize].term = Terminator::Return { value: dst };
        (f, scalar, dst)
    }

    #[test]
    fn an_annotated_function_verifies() {
        let (mut f, _, _) = alloc_and_return();
        crate::annotate(&mut f);
        assert_eq!(verify(&f), Ok(()));
    }

    /// The exit criterion, first half: a `Scalar` local in a root set. The
    /// collector dereferences everything the shadow frame holds, so this is
    /// P0-03's shape — a raw word in a rootable slot.
    #[test]
    fn a_scalar_local_in_the_root_set_is_rejected() {
        let (mut f, scalar, _) = alloc_and_return();
        crate::annotate(&mut f);
        // Reach past the seal the way a buggy pass would: replace the whole
        // instruction with one whose set was filled with a scalar.
        let mut roots = RootSlots::unannotated();
        roots.set(vec![scalar], Vec::new());
        f.blocks[0].insts[1] = Inst::Alloc {
            dst: f.return_local,
            alloc: AllocKind::Int { value: scalar },
            roots,
            debug: DebugSlots::unannotated(),
        };

        let errs = verify(&f).expect_err("a scalar root must be rejected");
        assert!(
            errs.iter().any(|e| matches!(
                e,
                VerifyError::RootIsNotGc { local, set: SlotSet::Live, .. } if *local == scalar
            )),
            "{errs:?}"
        );
    }

    /// The exit criterion, second half: a jump to a block that does not exist
    /// (MIR-11's class).
    #[test]
    fn an_out_of_range_jump_target_is_rejected() {
        let (mut f, _, _) = alloc_and_return();
        crate::annotate(&mut f);
        f.blocks[0].term = Terminator::Jump {
            target: BlockId(99),
        };

        let errs = verify(&f).expect_err("a dangling jump must be rejected");
        assert!(
            errs.iter().any(|e| matches!(
                e,
                VerifyError::BadBlockTarget { target, .. } if target.0 == 99
            )),
            "{errs:?}"
        );
    }

    /// P0-03 in its original shape: a raw integer moved into a `Gc` slot. The
    /// closure-capture lowering did exactly this before `LoadCapture` carried
    /// the index as an immediate.
    #[test]
    fn a_move_gc_out_of_a_scalar_is_rejected() {
        let (mut f, scalar, dst) = alloc_and_return();
        crate::annotate(&mut f);
        f.blocks[0].insts.push(Inst::MoveGc { dst, src: scalar });

        let errs = verify(&f).expect_err("Scalar -> Gc must go through Materialize");
        assert!(
            errs.iter()
                .any(|e| matches!(e, VerifyError::MoveGcFromScalar { .. })),
            "{errs:?}"
        );
    }

    /// A safepoint the liveness pass never reached spills nothing, which looks
    /// exactly like "nothing is live here". The seal is what makes the two
    /// distinguishable; this is the rule that reads it.
    #[test]
    fn an_unannotated_safepoint_is_rejected() {
        let (f, _, _) = alloc_and_return(); // deliberately not annotated
        let errs = verify(&f).expect_err("an un-annotated safepoint must be rejected");
        assert!(
            errs.iter()
                .any(|e| matches!(e, VerifyError::UnannotatedSafepoint { .. })),
            "{errs:?}"
        );
    }

    /// The ABI returns a `GcRef`. Returning a `Scalar` slot would hand the
    /// caller a raw payload word typed as a pointer.
    #[test]
    fn returning_a_scalar_is_rejected() {
        let (mut f, scalar, _) = alloc_and_return();
        crate::annotate(&mut f);
        f.blocks[0].term = Terminator::Return { value: scalar };

        let errs = verify(&f).expect_err("a scalar return must be rejected");
        assert!(
            errs.iter()
                .any(|e| matches!(e, VerifyError::ReturnIsNotGc { .. })),
            "{errs:?}"
        );
    }

    /// `Overflow::Bounded` claims the operands cannot overflow. No bound on the
    /// operands rules out a zero divisor, and `sdiv` traps rather than faults.
    #[test]
    fn a_bounded_division_is_rejected() {
        let (mut f, scalar, _) = alloc_and_return();
        crate::annotate(&mut f);
        f.blocks[0].insts.push(Inst::IntBinOp {
            op: IntBinOp::Div,
            dst: scalar,
            lhs: scalar,
            rhs: scalar,
            overflow: Overflow::Bounded,
        });

        let errs = verify(&f).expect_err("a bounded division must be rejected");
        assert!(
            errs.iter()
                .any(|e| matches!(e, VerifyError::BoundedDivision { .. })),
            "{errs:?}"
        );
    }

    /// An operand naming a local that does not exist. Cheap, and the shape a
    /// builder bug takes when a helper returns the wrong slot.
    #[test]
    fn an_out_of_range_operand_is_rejected() {
        let (mut f, _, dst) = alloc_and_return();
        crate::annotate(&mut f);
        f.blocks[0].insts.push(Inst::MoveGc {
            dst,
            src: LocalId(999),
        });

        let errs = verify(&f).expect_err("an out-of-range operand must be rejected");
        assert!(
            errs.iter().any(|e| matches!(
                e,
                VerifyError::LocalOutOfRange { local, .. } if local.0 == 999
            )),
            "{errs:?}"
        );
    }
}
