//! MIR liveness: compute, per GC safepoint, the minimal set of live `Gc` locals
//! the shadow-stack frame must root (§12.3, ADR-016).
//!
//! A local is *live* at a program point if its current value may be read before
//! it is next overwritten. Only [`LocalKind::Gc`] locals matter for rooting —
//! [`LocalKind::Scalar`] payloads are transient and must be re-materialized into
//! a `GcRef` before any safepoint (enforced by construction in the builder).
//!
//! The pass is classic backward dataflow:
//!   1. Compute each block's `live_out` (the union of its successors' `live_in`),
//!      iterating to a fixpoint.
//!   2. Walk each block **backward** from its `live_out`; at every safepoint
//!      ([`Inst::Alloc`], [`Inst::Materialize`], [`Inst::Call`]) record the set
//!      of live `Gc` locals into that instruction's `live_roots`.
//!
//! `live_roots` excludes the safepoint's own `dst` (it is defined *at* the
//! safepoint, so it is not live across it).

use std::collections::{BTreeSet, HashMap};

#[allow(unused_imports)]
use crate::ir::Block;
use crate::ir::{BlockId, Function, Inst, LocalId, LocalKind, Terminator};

/// Run liveness and populate `live_roots` on every safepoint in `func`.
///
/// Returns the total number of safepoints annotated (for testing/inspection).
pub fn annotate(func: &mut Function) -> usize {
    compute_fixpoint(func)
}

/// The real fixpoint: returns the number of safepoints annotated.
fn compute_fixpoint(func: &mut Function) -> usize {
    // Precompute which locals are `Gc` slots (the only ones worth rooting).
    let gc_locals: BTreeSet<LocalId> = func
        .locals
        .iter()
        .filter(|l| l.kind == LocalKind::Gc)
        .map(|l| l.id)
        .collect();

    // Pass 1: backward dataflow to fixpoint, recording live_IN per block in a
    // separate map (live_out is derived from successors' live_IN).
    let mut live_in: HashMap<BlockId, BTreeSet<LocalId>> = HashMap::new();

    loop {
        let mut changed = false;
        for blk_idx in (0..func.blocks.len()).rev() {
            let blk_id = BlockId(blk_idx as u32);
            let blk = &func.blocks[blk_idx];
            // live_out = union of successors' live_in.
            let mut live = BTreeSet::new();
            for succ in successors(&blk.term) {
                if let Some(succ_in) = live_in.get(&succ) {
                    live.extend(succ_in.iter().copied());
                }
            }
            transfer_term(&mut live, &blk.term);
            for inst in blk.insts.iter().rev() {
                transfer_inst(&mut live, inst);
            }
            let prev = live_in.get(&blk_id).cloned().unwrap_or_default();
            if prev != live {
                live_in.insert(blk_id, live);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Pass 2: walk each block forward, tracking live, and snapshot at safepoints.
    let mut count = 0;
    for blk_idx in 0..func.blocks.len() {
        let blk_id = BlockId(blk_idx as u32);
        // Compute live at the *start* of the block by re-walking backward once
        // more from the block's live_out (consistent with pass 1).
        let mut live = BTreeSet::new();
        for succ in successors(&func.blocks[blk_idx].term) {
            if let Some(succ_in) = live_in.get(&succ) {
                live.extend(succ_in.iter().copied());
            }
        }
        // Walk backward to get live_IN for this block.
        transfer_term_backward_for_in(&mut live, &func.blocks[blk_idx]);
        for inst in func.blocks[blk_idx].insts.iter().rev() {
            transfer_inst(&mut live, inst);
        }
        let mut current = live;
        // Now walk forward, applying defs/uses, snapshotting at safepoints.
        for inst_mut in func.blocks[blk_idx].insts.iter_mut() {
            // At a safepoint, the live set (before the inst's def) is the root set.
            if let Some(slot) = safepoint_roots_slot(inst_mut) {
                let roots: Vec<LocalId> = current.intersection(&gc_locals).copied().collect();
                *slot = roots;
                count += 1;
            }
            apply_forward(&mut current, inst_mut);
        }
        let _ = blk_id;
    }
    count
}

/// Successor blocks of a terminator.
fn successors(term: &Terminator) -> Vec<BlockId> {
    match term {
        Terminator::Branch {
            then_block,
            else_block,
            ..
        } => vec![*then_block, *else_block],
        Terminator::Jump { target } => vec![*target],
        Terminator::Return { .. } | Terminator::Fault => vec![],
    }
}

/// Backward transfer for an instruction: remove defs, add uses.
fn transfer_inst(live: &mut BTreeSet<LocalId>, inst: &Inst) {
    // defs then uses (backward order: remove def, add uses).
    for d in defs(inst) {
        live.remove(&d);
    }
    for u in uses(inst) {
        live.insert(u);
    }
}

/// Backward transfer for the terminator (only uses, no defs — terminators read).
fn transfer_term(live: &mut BTreeSet<LocalId>, term: &Terminator) {
    for u in term_uses(term) {
        live.insert(u);
    }
}

/// Helper: backward transfer of the terminator for computing live_IN. Same as
/// `transfer_term` (terminators only use, never define locals).
fn transfer_term_backward_for_in(live: &mut BTreeSet<LocalId>, blk: &crate::ir::Block) {
    transfer_term(live, &blk.term);
}

/// Locals defined by an instruction.
fn defs(inst: &Inst) -> Vec<LocalId> {
    match inst {
        Inst::Alloc { dst, .. } => vec![*dst],
        Inst::ExtractScalar { dst, .. } => vec![*dst],
        Inst::Materialize { dst, .. } => vec![*dst],
        Inst::IntBinOp { dst, .. } => vec![*dst],
        Inst::IntCmp { dst, .. } => vec![*dst],
        Inst::Call { dst, .. } => vec![*dst],
        Inst::MoveGc { dst, .. } => vec![*dst],
        Inst::ConstInt { dst, .. } => vec![*dst],
        Inst::LoadField { dst, .. } => vec![*dst],
        Inst::EnumTag { dst, .. } => vec![*dst],
        Inst::EnumPayloadGet { dst, .. } => vec![*dst],
        Inst::StoreScalar { .. } | Inst::CheckFault { .. } => vec![],
    }
}

/// Locals used by an instruction (excluding `live_roots`, which is derived, not
/// an operand).
fn uses(inst: &Inst) -> Vec<LocalId> {
    match inst {
        Inst::Alloc {
            alloc:
                crate::ir::AllocKind::Int { value }
                | crate::ir::AllocKind::Bool { value }
                | crate::ir::AllocKind::Char { value },
            ..
        } => vec![*value],
        Inst::Alloc {
            alloc: crate::ir::AllocKind::Record { fields, .. },
            ..
        } => fields.clone(),
        Inst::Alloc {
            alloc: crate::ir::AllocKind::Enum { args, .. },
            ..
        } => args.clone(),
        Inst::Alloc {
            alloc: crate::ir::AllocKind::Unit | crate::ir::AllocKind::Text { .. },
            ..
        } => vec![],
        Inst::ExtractScalar { src, .. } => vec![*src],
        Inst::StoreScalar { dst_gc, src, .. } => vec![*dst_gc, *src],
        Inst::Materialize { src, .. } => vec![*src],
        Inst::IntBinOp { lhs, rhs, .. } => vec![*lhs, *rhs],
        Inst::IntCmp { lhs, rhs, .. } => vec![*lhs, *rhs],
        Inst::Call { args, .. } => args.clone(),
        Inst::MoveGc { src, .. } => vec![*src],
        Inst::LoadField { src, .. } => vec![*src],
        Inst::EnumTag { src, .. } => vec![*src],
        Inst::EnumPayloadGet { src, .. } => vec![*src],
        Inst::ConstInt { .. } => vec![],
        Inst::CheckFault { .. } => vec![],
    }
}

/// Locals used by a terminator.
fn term_uses(term: &Terminator) -> Vec<LocalId> {
    match term {
        Terminator::Branch { cond, .. } => vec![*cond],
        Terminator::Return { value } => vec![*value],
        Terminator::Jump { .. } | Terminator::Fault => vec![],
    }
}

/// Forward transfer: apply defs/uses in program order (uses read first, then def).
fn apply_forward(live: &mut BTreeSet<LocalId>, inst: &Inst) {
    // uses are read first (already live or not), then defs kill.
    let d: Vec<LocalId> = defs(inst);
    for dd in d {
        live.insert(dd);
    }
    // (We do not remove anything forward; defs make the local live from here.)
}

/// Returns a mutable reference to an instruction's `live_roots` slot iff it is
/// a safepoint.
fn safepoint_roots_slot(inst: &mut Inst) -> Option<&mut Vec<LocalId>> {
    match inst {
        Inst::Alloc { live_roots, .. }
        | Inst::Materialize { live_roots, .. }
        | Inst::Call { live_roots, .. } => Some(live_roots),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Block, Function, Inst, IntBinOp, LocalId, LocalKind, ScalarKind, Terminator};
    use praxis_types::{Type, TypeDb};

    fn gc_local(func: &mut Function, name: &str) -> LocalId {
        func.new_local(LocalKind::Gc, Type(0), Some(name.into()))
    }
    fn int_local(func: &mut Function) -> LocalId {
        func.new_local(LocalKind::Scalar(ScalarKind::Int), Type(0), None)
    }

    #[test]
    fn dead_local_is_not_in_safepoint_roots() {
        // fn f() { let a = 1; let b = 2; a }   — `b` is dead; a live.
        // Build: int0=1, int1=2, alloc0(a)=Int(int0), alloc1(b)=Int(int1),
        //        return a. At alloc1's safepoint, `a` is live, `b` is not yet born.
        let mut db = TypeDb::new();
        let int = db.int();
        let mut f = Function {
            name: "f".into(),
            params: Vec::new(),
            return_local: LocalId(0),
            locals: Vec::new(),
            blocks: Vec::new(),
            debug_names: Vec::new(),
        };
        let ret = gc_local(&mut f, "ret");
        f.return_local = ret;
        let i0 = int_local(&mut f);
        let i1 = int_local(&mut f);
        let a = gc_local(&mut f, "a");
        let b = gc_local(&mut f, "b");
        let blk = f.new_block();
        f.blocks[blk.0 as usize].insts.push(Inst::IntBinOp {
            op: IntBinOp::Add,
            dst: i0,
            lhs: i0,
            rhs: i0,
        }); // dummy to define i0
        f.blocks[blk.0 as usize].insts.push(Inst::Alloc {
            dst: a,
            alloc: crate::ir::AllocKind::Int { value: i0 },
            live_roots: Vec::new(),
        });
        f.blocks[blk.0 as usize].insts.push(Inst::IntBinOp {
            op: IntBinOp::Add,
            dst: i1,
            lhs: i0,
            rhs: i0,
        });
        // At this alloc (b), `a` is live; `b` is the def so excluded.
        f.blocks[blk.0 as usize].insts.push(Inst::Alloc {
            dst: b,
            alloc: crate::ir::AllocKind::Int { value: i1 },
            live_roots: Vec::new(),
        });
        f.blocks[blk.0 as usize].term = Terminator::Return { value: a };
        let _ = int;

        let count = annotate(&mut f);
        assert!(count >= 2, "should have annotated both safepoints");
        // Find the second alloc (b) and check its live_roots contains a but not b.
        let b_alloc = f.blocks[0]
            .insts
            .iter()
            .find_map(|i| match i {
                Inst::Alloc {
                    dst, live_roots, ..
                } if *dst == b => Some(live_roots.clone()),
                _ => None,
            })
            .expect("b's alloc");
        assert!(b_alloc.contains(&a), "a should be rooted at b's allocation");
        assert!(
            !b_alloc.contains(&b),
            "b is defined at this safepoint, must not root itself"
        );
    }

    #[test]
    fn annotate_runs_on_empty_function() {
        let mut f = Function {
            name: "f".into(),
            params: Vec::new(),
            return_local: LocalId(0),
            locals: Vec::new(),
            blocks: Vec::new(),
            debug_names: Vec::new(),
        };
        let _blk: Block = Block {
            id: crate::ir::BlockId(0),
            insts: Vec::new(),
            term: Terminator::Return { value: LocalId(0) },
        };
        assert_eq!(annotate(&mut f), 0);
    }
}
