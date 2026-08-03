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
//! division; **a faulting instruction is immediately followed by a
//! [`Inst::CheckFault`], and a `CheckFault` follows only a faulting
//! instruction**.
//!
//! # The fault rule, and why it is strict in both directions (MIR-10, ADR-088)
//!
//! §10.4 says generated code checks `pending_fault` immediately after calls
//! that can fault. Nothing enforced it, and both halves had rotted: the fused
//! `collect` sink pushed into its result Vec with no check at all (REP-52),
//! while every method call emitted one whether or not its wrapper could fault
//! (REP-53) — including `praxis_vec_len`, which is `Effect::Allocates`. The
//! answer to "can this fault" is [`Inst::can_fault`], which derives from the
//! ABI manifest through the same instruction→symbol mapping the Cranelift
//! backend uses, so the verifier and the emitted call cannot disagree.
//!
//! The **converse** is checked too, and that is the half that does the work.
//! Without it the forward rule is satisfied by checking after everything, which
//! is what lowering did: REP-53's fix would have had no invariant behind it and
//! would have regressed to unconditional the first time a site was copied. With
//! it, `praxis_runtime::abi::panic_fault_is_observable`'s premise — that a
//! `Pure`/`Allocates` symbol is never followed by a `CheckFault`, so its panic
//! path can abort rather than fault — is *enforced* rather than asserted.
//!
//! §10.4 also says "later optimization may combine checks when safe". That
//! relaxes **both** directions together — a combined check observes several
//! faulting instructions, so neither "immediately followed" nor "follows a
//! faulting instruction" survives it — and the pass that introduces one
//! relaxes this rule with it. Until then the one-check-per-faulting-instruction
//! shape is what lowering emits, and a rule that admitted a shape nothing
//! builds would not catch the site that forgot.
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
//! **`OpaqueAtDescriptorSite` is still off, and the reason has changed twice**
//! (hazard H10). The plan schedules it here, on the grounds that lowering could
//! not supply per-use types until F15. F15 landed and supplied them: the
//! `for`-loop item, the parser result, the closure value, the indirect call
//! result and a pipeline's *source* item are all `MirType::Known`, and every
//! `AllocKind::Collection` a program writes carries real type arguments. Two
//! sites were left — the pair a fused `enumerate` or `zip` builds — and they
//! needed two things: the catalog to describe those two methods correctly, and
//! the fused lowering to read what it describes. **Both have landed** (TY-31
//! and S21's MIR-05), so no lowering site emits an unconditional `Opaque` into
//! a descriptor-producing position any more.
//!
//! What blocks the rule now is not a lowering gap at all — it is that `Opaque`
//! is a **legal answer**. A type that is still an inference variable has no
//! descriptor and never will: `let m = Map()` generalizes at the `let`, so a
//! `for kv in m` whose body never opens the pair leaves K and V unresolved, and
//! `let v = Vec()` with no push leaves a chain over it the same way. ADR-066
//! decision 5 answers that with a **null schema slot** and a runtime read of the
//! value's own header, which is never the wrong descriptor. A rule that refused
//! `Opaque` outright would reject those programs.
//!
//! So the rule this file is waiting for is narrower than the one the plan named:
//! not "no `Opaque` at a descriptor site", but "no `Opaque` at a descriptor site
//! *whose type could have been resolved*" — which needs a way to distinguish an
//! unresolved inference variable from a lowering that simply did not look. That
//! distinction does not exist in `MirType` today, and inventing it is a change
//! to the representation, not to this pass. `MirType::expect_known` lands with
//! the rule that needs it.

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
    /// An instruction that [`Inst::can_fault`] is not immediately followed by a
    /// [`Inst::CheckFault`] in the same block (§10.4, MIR-10). The fault is
    /// sticky, so it is not *lost* — it is observed wherever the next check
    /// happens to be, which is a different frame, a different iteration, or
    /// after `main` returns. `why` names the wrapper or operation that can
    /// raise it.
    UnobservedFault {
        func: String,
        block: BlockId,
        inst: usize,
        why: &'static str,
    },
    /// A [`Inst::CheckFault`] whose preceding instruction cannot fault — or
    /// which begins a block, where there is no preceding instruction at all.
    ///
    /// The converse of [`VerifyError::UnobservedFault`], and the half that
    /// keeps the forward rule from being satisfiable by checking after
    /// everything (REP-53). It also costs: a check is a call plus a branch, and
    /// on the fused-pipeline loop header it ran once per element.
    RedundantFaultCheck {
        func: String,
        block: BlockId,
        inst: usize,
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
            VerifyError::UnobservedFault {
                func,
                block,
                inst,
                why,
            } => write!(
                f,
                "{func}: block {} inst {inst}: can fault ({why}) but is not \
                 followed by a CheckFault",
                block.0
            ),
            VerifyError::RedundantFaultCheck { func, block, inst } => write!(
                f,
                "{func}: block {} inst {inst}: CheckFault follows nothing that \
                 can fault",
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
            check_fault_observed(f, bid, i, block, &mut errs);

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

/// Both directions of the fault rule, for the instruction at `block.insts[i]`
/// (MIR-10, ADR-088; see this module's header for why it is strict).
///
/// The pairing is **positional and within one block**: a fault is observed by
/// the instruction that immediately follows the one that can raise it. Looking
/// further — "some check dominates this point" — is the weaker property the
/// defect already satisfied: `v.sum()`'s overflow *was* eventually observed, by
/// the next loop-header check, one iteration later and with a snapshot showing
/// values from after the fault.
fn check_fault_observed(
    f: &Function,
    bid: BlockId,
    i: usize,
    block: &crate::ir::Block,
    errs: &mut Vec<VerifyError>,
) {
    let inst = &block.insts[i];
    let next = block.insts.get(i + 1);

    if let Some(why) = inst.fault_reason() {
        if !matches!(next, Some(Inst::CheckFault { .. })) {
            errs.push(VerifyError::UnobservedFault {
                func: f.name.clone(),
                block: bid,
                inst: i,
                why,
            });
        }
    }

    // The converse. A `CheckFault` at index 0 has no predecessor at all: the
    // faulting instruction it would observe is in another block, and control
    // may reach this one by an edge that never executed it.
    if matches!(inst, Inst::CheckFault { .. })
        && !i
            .checked_sub(1)
            .and_then(|p| block.insts.get(p))
            .is_some_and(Inst::can_fault)
    {
        errs.push(VerifyError::RedundantFaultCheck {
            func: f.name.clone(),
            block: bid,
            inst: i,
        });
    }
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
        Inst::ConstInt { dst, .. } | Inst::ConstFloat { dst, .. } | Inst::ConstGc { dst, .. } => {
            v.push(*dst)
        }
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
        | Inst::LoadTupleElem { dst, src, .. }
        | Inst::EnumTag { dst, src }
        | Inst::EnumPayloadGet { dst, src, .. }
        | Inst::MoveGc { dst, src }
        | Inst::FloatNeg { dst, src } => v.extend([*dst, *src]),
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
            debug_scalar_sources: Vec::new(),
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

    /// A one-block function whose whole body is an [`Inst::ConstGc`] returned.
    fn const_gc_and_return() -> (Function, LocalId) {
        let mut f = empty_fn("f");
        let dst = gc_local(&mut f);
        f.return_local = dst;
        let blk = f.new_block();
        f.blocks[blk.0 as usize].insts.push(Inst::ConstGc {
            dst,
            konst: crate::ir::GcConst::SmallInt(1),
        });
        f.blocks[blk.0 as usize].term = Terminator::Return { value: dst };
        (f, dst)
    }

    /// `Inst::ConstGc` verifies with no slot sets, and a `CheckFault` after it
    /// is rejected — both directions of ADR-088 over the new instruction.
    ///
    /// The first half is what makes it cheap: no `RootSlots`, so nothing is
    /// spilled, and `check_slot_sets` must not demand an annotation it has no
    /// field for. The second is what keeps the rule without a carve-out: the
    /// instruction calls no wrapper, so nothing can fault, so observing it is a
    /// redundant check rather than a harmless one.
    #[test]
    fn a_const_gc_verifies_and_may_not_be_fault_checked() {
        let (mut f, _) = const_gc_and_return();
        crate::annotate(&mut f);
        assert_eq!(verify(&f), Ok(()));

        let (mut f, _) = const_gc_and_return();
        f.blocks[0].insts.push(Inst::CheckFault {
            on_fault: BlockId(0),
            debug: DebugSlots::unannotated(),
        });
        crate::annotate(&mut f);
        let errs = verify(&f).expect_err("a check after a non-faulting instruction is redundant");
        assert!(
            errs.iter()
                .any(|e| matches!(e, VerifyError::RedundantFaultCheck { .. })),
            "{errs:?}"
        );
    }

    /// **MIR has no def-dominates-use rule, and that is deliberate** (ADR-015:
    /// MIR is slot-based, not SSA). An `Alloc` in one block whose result is read
    /// in another verifies — which is exactly what every `let` already produces,
    /// and what a future loop-invariant hoisting pass would need.
    ///
    /// Written down as a test rather than left as an absence, because "the
    /// verifier is silent about X" is only load-bearing while someone can see
    /// that it is true.
    #[test]
    fn an_alloc_hoisted_out_of_its_using_block_still_verifies() {
        let mut f = empty_fn("f");
        let scalar = int_local(&mut f);
        let dst = gc_local(&mut f);
        f.return_local = dst;
        let b0 = f.new_block();
        let b1 = f.new_block();
        // Block 0 defines the value...
        f.blocks[b0.0 as usize].insts.push(Inst::ConstInt {
            dst: scalar,
            value: 1,
        });
        f.blocks[b0.0 as usize].insts.push(Inst::Alloc {
            dst,
            alloc: AllocKind::Int { value: scalar },
            roots: RootSlots::unannotated(),
            debug: DebugSlots::unannotated(),
        });
        f.blocks[b0.0 as usize].term = Terminator::Jump { target: b1 };
        // ...and block 1 is the only place it is read.
        f.blocks[b1.0 as usize].term = Terminator::Return { value: dst };

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
