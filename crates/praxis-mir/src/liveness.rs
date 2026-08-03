//! MIR liveness: compute, per GC safepoint, the minimal set of live `Gc` locals
//! the shadow-stack frame must root (§12.3, ADR-016), *and*, separately, the
//! set the crash debugger must be able to render (MIR-16, ADR-021/-035).
//!
//! A local is *live* at a program point if its current value may be read before
//! it is next overwritten. Only [`LocalKind::Gc`] locals matter for rooting —
//! [`LocalKind::Scalar`] payloads are transient and must be re-materialized into
//! a `GcRef` before any safepoint (enforced by construction in the builder).
//!
//! # The three analyses, and why they are three
//!
//! 1. **Live-in per block** — classic backward dataflow to a fixpoint.
//! 2. **Roots** ([`RootSlots`]) — one backward walk per block reusing that
//!    transfer. The root set at an instruction is
//!    `((live_out \ defs) ∪ uses) ∩ gc_locals`: the destination is written
//!    *after* the collection so it is not rooted, and the operands are handed to
//!    the allocating call so they must be. This is exact, which is the point:
//!    the old pass walked *forward* and only ever inserted definitions, so a
//!    root set could never shrink within a block and a value stayed rooted long
//!    past its last use (MIR-02).
//! 3. **Dead slots** ([`RootSlots::dead`]) — a forward *may* dataflow over
//!    "which shadow slots might still hold a value". A slot written at one
//!    safepoint and not live at the next was never cleared, so it kept its
//!    object reachable forever (MIR-01). Nulling `dirty \ roots` at each
//!    safepoint is the minimal repair: the frame starts all-null, only a
//!    safepoint writes it, and after a safepoint the dirty set *is* the root
//!    set.
//! 4. **Debug slots** ([`DebugSlots`]) — deliberately the old, over-approximate
//!    forward walk: `live_in(block) ∪ {defs seen so far}`. The debugger must
//!    show `a` after `let a = 10` whether or not anything reads it again, so
//!    this set is what making the *root* set exact must not shrink (H3).
//!
//! The fourth is a **contract, not an emission plan** (ADR-104). The backend no
//! longer writes `DebugSlots::visible()` at each annotated point; it writes each
//! `Gc` local once, at its definition, which leaves the same value in the same
//! slot at every point a snapshot can be taken and costs `Σ 1 per def` stores
//! instead of `Σ_points |visible|`. [`defs`] is public for exactly that, so the
//! two are driven by one answer to "what does this instruction define".

use std::collections::{BTreeSet, HashMap};

use crate::annot::{DebugSlots, RootSlots};
#[allow(unused_imports)]
use crate::ir::Block;
use crate::ir::{BlockId, Function, Inst, LocalId, LocalKind, Terminator};

/// Run liveness and populate the [`RootSlots`]/[`DebugSlots`] on every
/// safepoint in `func`.
///
/// Returns the number of *GC* safepoints annotated (for testing/inspection);
/// debugger-only points like [`Inst::CheckFault`] are not counted.
pub fn annotate(func: &mut Function) -> usize {
    compute_fixpoint(func)
}

/// The real fixpoint: returns the number of GC safepoints annotated.
fn compute_fixpoint(func: &mut Function) -> usize {
    // Precompute which locals are `Gc` slots (the only ones worth rooting).
    let gc_locals: BTreeSet<LocalId> = func
        .locals
        .iter()
        .filter(|l| l.kind == LocalKind::Gc)
        .map(|l| l.id)
        .collect();

    let live_in = live_in_fixpoint(func);
    let dirty_in = dirty_in_fixpoint(func, &gc_locals, &live_in);

    let mut count = 0;
    for blk_idx in 0..func.blocks.len() {
        let blk_id = BlockId(blk_idx as u32);
        // The root set at each instruction, in program order: one backward walk
        // from the block's live_out, snapshotting live-*before* each
        // instruction (which is exactly `((live_out \ defs) ∪ uses)`).
        let roots_at = block_roots(&func.blocks[blk_idx], &live_in, &gc_locals);

        // The debugger's view and the dirty-slot tracker both run forward.
        let mut visible: BTreeSet<LocalId> = live_in
            .get(&blk_id)
            .map(|s| s.intersection(&gc_locals).copied().collect())
            .unwrap_or_default();
        let mut dirty: BTreeSet<LocalId> = dirty_in.get(&blk_id).cloned().unwrap_or_default();

        for (i, inst) in func.blocks[blk_idx].insts.iter_mut().enumerate() {
            let debug_now: Vec<LocalId> = visible.iter().copied().collect();
            if let Some((roots, debug)) = gc_safepoint_slots(inst) {
                let live: BTreeSet<LocalId> = roots_at[i].clone();
                // MIR-01: a slot that may still hold a value but is not a root
                // here is stale, and stale roots retain objects (and, worse,
                // can outlive the local's type). Null exactly those.
                let dead: Vec<LocalId> = dirty.difference(&live).copied().collect();
                roots.set(live.iter().copied().collect(), dead);
                debug.set(debug_now);
                // After this safepoint the frame holds precisely the roots.
                dirty = live;
                count += 1;
            } else if let Some(debug) = debug_only_slots(inst) {
                debug.set(debug_now);
            }
            for d in defs(inst) {
                if gc_locals.contains(&d) {
                    visible.insert(d);
                }
            }
        }
    }
    count
}

/// Backward dataflow to a fixpoint: the live set at the top of each block.
fn live_in_fixpoint(func: &Function) -> HashMap<BlockId, BTreeSet<LocalId>> {
    let mut live_in: HashMap<BlockId, BTreeSet<LocalId>> = HashMap::new();
    loop {
        let mut changed = false;
        for blk_idx in (0..func.blocks.len()).rev() {
            let blk_id = BlockId(blk_idx as u32);
            let blk = &func.blocks[blk_idx];
            let mut live = live_out_of(blk, &live_in);
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
    live_in
}

/// The live set flowing *out* of a block: its successors' live-in, plus the
/// terminator's own reads.
fn live_out_of(blk: &Block, live_in: &HashMap<BlockId, BTreeSet<LocalId>>) -> BTreeSet<LocalId> {
    let mut live = BTreeSet::new();
    for succ in successors(&blk.term) {
        if let Some(succ_in) = live_in.get(&succ) {
            live.extend(succ_in.iter().copied());
        }
    }
    transfer_term(&mut live, &blk.term);
    live
}

/// One backward walk over a block: the GC root set *at* each instruction, in
/// program order. Non-safepoint entries are computed too (they are the same
/// dataflow) and simply unused.
fn block_roots(
    blk: &Block,
    live_in: &HashMap<BlockId, BTreeSet<LocalId>>,
    gc_locals: &BTreeSet<LocalId>,
) -> Vec<BTreeSet<LocalId>> {
    let mut live = live_out_of(blk, live_in);
    let mut out = vec![BTreeSet::new(); blk.insts.len()];
    for (i, inst) in blk.insts.iter().enumerate().rev() {
        // `transfer_inst` turns live-after into live-before, which for a
        // safepoint is `(live_out \ defs) ∪ uses` — the set that must survive
        // the collection.
        transfer_inst(&mut live, inst);
        out[i] = live.intersection(gc_locals).copied().collect();
    }
    out
}

/// Forward *may* dataflow to a fixpoint: which shadow slots might hold a value
/// at the top of each block. The entry block starts empty — the prologue zeroes
/// every slot it claims (ADR-101) — and only a GC safepoint writes the frame,
/// so the set changes only there.
fn dirty_in_fixpoint(
    func: &Function,
    gc_locals: &BTreeSet<LocalId>,
    live_in: &HashMap<BlockId, BTreeSet<LocalId>>,
) -> HashMap<BlockId, BTreeSet<LocalId>> {
    let mut dirty_in: HashMap<BlockId, BTreeSet<LocalId>> = HashMap::new();
    loop {
        let mut changed = false;
        for blk_idx in 0..func.blocks.len() {
            let blk_id = BlockId(blk_idx as u32);
            let blk = &func.blocks[blk_idx];
            let roots_at = block_roots(blk, live_in, gc_locals);
            // Walk the block: after each safepoint the frame holds its roots.
            let mut dirty = dirty_in.get(&blk_id).cloned().unwrap_or_default();
            for (i, inst) in blk.insts.iter().enumerate() {
                if is_gc_safepoint(inst) {
                    dirty = roots_at[i].clone();
                }
            }
            // Propagate to successors.
            for succ in successors(&blk.term) {
                let entry = dirty_in.entry(succ).or_default();
                let before = entry.len();
                entry.extend(dirty.iter().copied());
                if entry.len() != before {
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    dirty_in
}

/// Successor blocks of a terminator.
///
/// `pub(crate)` for the `test_support` module's CFG walk: one statement of
/// "what edges leave a block" is what keeps a second walker from disagreeing
/// with the liveness fixpoint about whether a fault edge is an edge.
pub(crate) fn successors(term: &Terminator) -> Vec<BlockId> {
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

/// Locals defined by an instruction.
///
/// **Public because the backend writes the debugger's view at definitions.**
/// Since ADR-104 the Cranelift lowering emits one debug store per `Gc`
/// definition instead of re-writing the whole [`DebugSlots`] set at every
/// safepoint, and it drives those stores from *this* function rather than from
/// a sixth exhaustive match over [`Inst`]. ADR-044's Consequences fix the count
/// at five (`ir.rs`, this `defs`, `uses`, `verify.rs`'s `operands`, and
/// `lower_inst`); a private copy in the backend would have made it six, and the
/// two would have to agree for the debugger to show what MIR-16 promises.
///
/// **The match itself now lives in [`crate::verify::defines`]** (ADR-122),
/// which answers the sharper question — *the* local, not a list — because the
/// provable-descriptor analysis needs a definition site and a `Vec` of one
/// element is not one. This wraps it rather than restating it, so the count
/// stays at five: the backend's iteration order over a definition set is
/// unchanged, because there was never more than one member.
pub fn defs(inst: &Inst) -> Vec<LocalId> {
    crate::verify::defines(inst).into_iter().collect()
}

/// Locals used by an instruction (excluding `live_roots`, which is derived, not
/// an operand).
fn uses(inst: &Inst) -> Vec<LocalId> {
    match inst {
        Inst::Alloc {
            alloc:
                crate::ir::AllocKind::Int { value }
                | crate::ir::AllocKind::Bool { value }
                | crate::ir::AllocKind::Char { value }
                | crate::ir::AllocKind::Float { value },
            ..
        } => vec![*value],
        Inst::Alloc {
            alloc: crate::ir::AllocKind::Record { fields, .. },
            ..
        } => fields.clone(),
        Inst::Alloc {
            alloc: crate::ir::AllocKind::Tuple { elements, .. },
            ..
        } => elements.clone(),
        Inst::Alloc {
            alloc: crate::ir::AllocKind::Enum { args, .. },
            ..
        } => args.clone(),
        Inst::Alloc {
            alloc: crate::ir::AllocKind::Unit | crate::ir::AllocKind::Text { .. },
            ..
        } => vec![],
        Inst::Alloc {
            alloc: crate::ir::AllocKind::Closure { captures, .. },
            ..
        } => captures.clone(),
        Inst::Alloc {
            alloc: crate::ir::AllocKind::Collection { .. },
            ..
        } => vec![],
        Inst::ExtractScalar { src, .. } => vec![*src],
        Inst::StoreScalar { dst_gc, src, .. } => vec![*dst_gc, *src],
        Inst::Materialize { src, .. } => vec![*src],
        Inst::IntBinOp { lhs, rhs, .. } => vec![*lhs, *rhs],
        Inst::FloatBinOp { lhs, rhs, .. } => vec![*lhs, *rhs],
        Inst::FloatNeg { src, .. } => vec![*src],
        Inst::IntCmp { lhs, rhs, .. } => vec![*lhs, *rhs],
        Inst::FloatCmp { lhs, rhs, .. } => vec![*lhs, *rhs],
        Inst::Call { args, .. } => args.clone(),
        Inst::CallIndirect { callee, args, .. } => {
            let mut v = vec![*callee];
            v.extend(args.iter().copied());
            v
        }
        Inst::MoveGc { src, .. } => vec![*src],
        Inst::LoadField { src, .. } | Inst::LoadTupleElem { src, .. } => vec![*src],
        Inst::LoadCapture { closure, .. } => vec![*closure],
        Inst::EnumTag { src, .. } => vec![*src],
        Inst::EnumPayloadGet { src, .. } => vec![*src],
        Inst::StructEq { lhs, rhs, .. } => vec![*lhs, *rhs],
        Inst::ValueCmp { lhs, rhs, .. } => vec![*lhs, *rhs],
        Inst::ConstInt { .. } => vec![],
        Inst::ConstFloat { .. } => vec![],
        // The value is an immediate; the table it indexes is reached through
        // `ctx`, which is not a MIR local.
        Inst::ConstGc { .. } => vec![],
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

/// Both slot sets of a **GC safepoint** — an instruction whose lowering may
/// trigger a collection, so the collector must see the frame.
///
/// [`Inst::CheckFault`] is deliberately absent: it allocates nothing, roots
/// nothing, and carries only a [`DebugSlots`]. See [`debug_only_slots`].
///
/// [`Inst::ConstGc`] is absent for a stronger reason: it has no slot sets to
/// annotate at all. It reads a reference the runtime minted at startup out of
/// the context, so no collection can happen at it — and unlike `CheckFault` it
/// is not a place control can divert either, so the debugger needs nothing
/// spilled here. A temp holding an interned literal is still spilled at the next
/// `CheckFault`, whose `DebugSlots` is over-approximate on purpose and includes
/// every `Gc` local defined so far in the block (MIR-16, [`crate::annot`]) — and
/// the verifier guarantees a `CheckFault` immediately precedes every fault
/// diversion, so no crash snapshot loses the value.
fn gc_safepoint_slots(inst: &mut Inst) -> Option<(&mut RootSlots, &mut DebugSlots)> {
    match inst {
        Inst::Alloc { roots, debug, .. }
        | Inst::Materialize { roots, debug, .. }
        | Inst::Call { roots, debug, .. }
        | Inst::CallIndirect { roots, debug, .. }
        | Inst::StructEq { roots, debug, .. } => Some((roots, debug)),
        _ => None,
    }
}

/// Whether `inst` is a GC safepoint (the read-only half of
/// [`gc_safepoint_slots`], for the dirty-slot dataflow).
fn is_gc_safepoint(inst: &Inst) -> bool {
    matches!(
        inst,
        Inst::Alloc { .. }
            | Inst::Materialize { .. }
            | Inst::Call { .. }
            | Inst::CallIndirect { .. }
            | Inst::StructEq { .. }
    )
}

/// The [`DebugSlots`] of a debugger-only point.
///
/// `CheckFault` is the one such point: it is where a fault diverts control, so
/// the debugger needs the current values spilled *here* to render a faithful
/// snapshot (without it, a div-by-zero's operands show as `<uninit>` because no
/// GC safepoint ran between their materialization and the fault).
fn debug_only_slots(inst: &mut Inst) -> Option<&mut DebugSlots> {
    match inst {
        Inst::CheckFault { debug, .. } => Some(debug),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        Block, Function, Inst, IntBinOp, LocalDebugKind, LocalId, LocalKind, ScalarKind, Terminator,
    };
    use praxis_types::TypeDb;

    fn gc_local(func: &mut Function, name: &str) -> LocalId {
        func.new_local(
            LocalKind::Gc,
            crate::ir::MirType::Opaque,
            Some(name.into()),
            LocalDebugKind::User,
            None,
        )
    }
    fn int_local(func: &mut Function) -> LocalId {
        func.new_local(
            LocalKind::Scalar(ScalarKind::Int),
            crate::ir::MirType::Opaque,
            None,
            LocalDebugKind::Temp,
            None,
        )
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
            debug_kinds: Vec::new(),
            debug_spans: Vec::new(),
            span: (0, 0),
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
            overflow: crate::ir::Overflow::Checked,
        }); // dummy to define i0
        f.blocks[blk.0 as usize].insts.push(Inst::Alloc {
            dst: a,
            alloc: crate::ir::AllocKind::Int { value: i0 },
            roots: RootSlots::unannotated(),
            debug: DebugSlots::unannotated(),
        });
        f.blocks[blk.0 as usize].insts.push(Inst::IntBinOp {
            op: IntBinOp::Add,
            dst: i1,
            lhs: i0,
            rhs: i0,
            overflow: crate::ir::Overflow::Checked,
        });
        // At this alloc (b), `a` is live; `b` is the def so excluded.
        f.blocks[blk.0 as usize].insts.push(Inst::Alloc {
            dst: b,
            alloc: crate::ir::AllocKind::Int { value: i1 },
            roots: RootSlots::unannotated(),
            debug: DebugSlots::unannotated(),
        });
        f.blocks[blk.0 as usize].term = Terminator::Return { value: a };
        let _ = int;

        let count = annotate(&mut f);
        assert!(count >= 2, "should have annotated both safepoints");
        // Find the second alloc (b) and check its root set contains a but not b.
        let b_alloc = f.blocks[0]
            .insts
            .iter()
            .find_map(|i| match i {
                Inst::Alloc { dst, roots, .. } if *dst == b => Some(roots.live().to_vec()),
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
            debug_kinds: Vec::new(),
            debug_spans: Vec::new(),
            span: (0, 0),
        };
        let _blk: Block = Block {
            id: crate::ir::BlockId(0),
            insts: Vec::new(),
            term: Terminator::Return { value: LocalId(0) },
        };
        assert_eq!(annotate(&mut f), 0);
    }

    #[test]
    fn local_dead_after_its_last_use_is_not_rooted_at_a_later_safepoint() {
        // The root set is specified to be minimal (ADR-016), not merely sound.
        //
        //     a = alloc(1)
        //     consumed = a
        //     b = alloc(2)   // `a` and `consumed` are both dead here
        //     return b
        //
        // This is deliberately stronger than `dead_local_is_not_in_safepoint_roots`:
        // that test only checks that an allocation does not root its own
        // destination, which does not exercise removal after a last use.
        let mut db = TypeDb::new();
        let int = db.int();
        let mut f = Function {
            name: "last_use".into(),
            params: Vec::new(),
            return_local: LocalId(0),
            locals: Vec::new(),
            blocks: Vec::new(),
            debug_names: Vec::new(),
            debug_kinds: Vec::new(),
            debug_spans: Vec::new(),
            span: (0, 0),
        };
        let ret = gc_local(&mut f, "ret");
        f.return_local = ret;
        let one = int_local(&mut f);
        let two = int_local(&mut f);
        let a = gc_local(&mut f, "a");
        let consumed = gc_local(&mut f, "consumed");
        let b = gc_local(&mut f, "b");
        let blk = f.new_block();
        f.blocks[blk.0 as usize].insts.extend([
            Inst::ConstInt { dst: one, value: 1 },
            Inst::Alloc {
                dst: a,
                alloc: crate::ir::AllocKind::Int { value: one },
                roots: RootSlots::unannotated(),
                debug: DebugSlots::unannotated(),
            },
            Inst::MoveGc {
                dst: consumed,
                src: a,
            },
            Inst::ConstInt { dst: two, value: 2 },
            Inst::Alloc {
                dst: b,
                alloc: crate::ir::AllocKind::Int { value: two },
                roots: RootSlots::unannotated(),
                debug: DebugSlots::unannotated(),
            },
        ]);
        f.blocks[blk.0 as usize].term = Terminator::Return { value: b };
        let _ = int;

        annotate(&mut f);

        let b_roots = f.blocks[blk.0 as usize]
            .insts
            .iter()
            .find_map(|inst| match inst {
                Inst::Alloc { dst, roots, .. } if *dst == b => Some(roots.live().to_vec()),
                _ => None,
            })
            .expect("b allocation");
        assert!(
            !b_roots.contains(&a),
            "a is dead after the MoveGc and must not be retained at b's allocation"
        );
        assert!(
            !b_roots.contains(&consumed),
            "the unused MoveGc destination is also dead at b's allocation"
        );
    }

    #[test]
    fn exact_roots_shrink_between_two_safepoints_in_one_block() {
        // A value can be live at one safepoint and dead at the next. This
        // catches a forward annotation walk that only ever adds definitions
        // and therefore turns the root set into a monotonically growing set.
        let mut db = TypeDb::new();
        let int = db.int();
        let mut f = Function {
            name: "shrinking_roots".into(),
            params: Vec::new(),
            return_local: LocalId(0),
            locals: Vec::new(),
            blocks: Vec::new(),
            debug_names: Vec::new(),
            debug_kinds: Vec::new(),
            debug_spans: Vec::new(),
            span: (0, 0),
        };
        let ret = gc_local(&mut f, "ret");
        f.return_local = ret;
        let seed_scalar = int_local(&mut f);
        let first_scalar = int_local(&mut f);
        let second_scalar = int_local(&mut f);
        let seed = gc_local(&mut f, "seed");
        let first = gc_local(&mut f, "first");
        let consumed = gc_local(&mut f, "consumed");
        let second = gc_local(&mut f, "second");
        let blk = f.new_block();
        f.blocks[blk.0 as usize].insts.extend([
            Inst::ConstInt {
                dst: seed_scalar,
                value: 10,
            },
            Inst::Alloc {
                dst: seed,
                alloc: crate::ir::AllocKind::Int { value: seed_scalar },
                roots: RootSlots::unannotated(),
                debug: DebugSlots::unannotated(),
            },
            Inst::ConstInt {
                dst: first_scalar,
                value: 20,
            },
            Inst::Alloc {
                dst: first,
                alloc: crate::ir::AllocKind::Int {
                    value: first_scalar,
                },
                roots: RootSlots::unannotated(),
                debug: DebugSlots::unannotated(),
            },
            // `seed` is live across `first`'s allocation, then consumed.
            Inst::MoveGc {
                dst: consumed,
                src: seed,
            },
            Inst::ConstInt {
                dst: second_scalar,
                value: 30,
            },
            Inst::Alloc {
                dst: second,
                alloc: crate::ir::AllocKind::Int {
                    value: second_scalar,
                },
                roots: RootSlots::unannotated(),
                debug: DebugSlots::unannotated(),
            },
        ]);
        f.blocks[blk.0 as usize].term = Terminator::Return { value: second };
        let _ = int;

        annotate(&mut f);

        let roots_for = |needle: LocalId| {
            f.blocks[blk.0 as usize]
                .insts
                .iter()
                .find_map(|inst| match inst {
                    Inst::Alloc { dst, roots, .. } if *dst == needle => Some(roots.live().to_vec()),
                    _ => None,
                })
                .expect("allocation roots")
        };
        let first_roots = roots_for(first);
        let second_roots = roots_for(second);
        assert!(
            first_roots.contains(&seed),
            "seed is read after the first allocation and must be rooted there"
        );
        assert!(
            !second_roots.contains(&seed),
            "seed's lifetime ended before the second allocation"
        );
        assert!(
            !second_roots.contains(&consumed),
            "the unused copy must not make the later root set grow"
        );
    }

    /// Build the `exact_roots_shrink…` shape and hand back the function plus
    /// the ids the two MIR-01/MIR-16 gates below interrogate.
    ///
    ///     seed   = alloc(10)
    ///     first  = alloc(20)     // `seed` is live across this — it is spilled
    ///     consumed = seed        // `seed`'s last use
    ///     second = alloc(30)     // `seed` is dead here — its slot is stale
    ///     return second
    fn shrinking_roots_function() -> (Function, LocalId, LocalId, LocalId) {
        let mut f = Function {
            name: "shrinking_roots".into(),
            params: Vec::new(),
            return_local: LocalId(0),
            locals: Vec::new(),
            blocks: Vec::new(),
            debug_names: Vec::new(),
            debug_kinds: Vec::new(),
            debug_spans: Vec::new(),
            span: (0, 0),
        };
        let ret = gc_local(&mut f, "ret");
        f.return_local = ret;
        let seed_scalar = int_local(&mut f);
        let first_scalar = int_local(&mut f);
        let second_scalar = int_local(&mut f);
        let seed = gc_local(&mut f, "seed");
        let first = gc_local(&mut f, "first");
        let consumed = gc_local(&mut f, "consumed");
        let second = gc_local(&mut f, "second");
        let blk = f.new_block();
        f.blocks[blk.0 as usize].insts.extend([
            Inst::ConstInt {
                dst: seed_scalar,
                value: 10,
            },
            Inst::Alloc {
                dst: seed,
                alloc: crate::ir::AllocKind::Int { value: seed_scalar },
                roots: RootSlots::unannotated(),
                debug: DebugSlots::unannotated(),
            },
            Inst::ConstInt {
                dst: first_scalar,
                value: 20,
            },
            Inst::Alloc {
                dst: first,
                alloc: crate::ir::AllocKind::Int {
                    value: first_scalar,
                },
                roots: RootSlots::unannotated(),
                debug: DebugSlots::unannotated(),
            },
            Inst::MoveGc {
                dst: consumed,
                src: seed,
            },
            Inst::ConstInt {
                dst: second_scalar,
                value: 30,
            },
            Inst::Alloc {
                dst: second,
                alloc: crate::ir::AllocKind::Int {
                    value: second_scalar,
                },
                roots: RootSlots::unannotated(),
                debug: DebugSlots::unannotated(),
            },
        ]);
        f.blocks[blk.0 as usize].term = Terminator::Return { value: second };
        (f, seed, first, second)
    }

    /// The slot sets of the `Alloc` whose destination is `needle`.
    fn slots_for(f: &Function, needle: LocalId) -> (Vec<LocalId>, Vec<LocalId>, Vec<LocalId>) {
        f.blocks[0]
            .insts
            .iter()
            .find_map(|inst| match inst {
                Inst::Alloc {
                    dst, roots, debug, ..
                } if *dst == needle => Some((
                    roots.live().to_vec(),
                    roots.dead().to_vec(),
                    debug.visible().to_vec(),
                )),
                _ => None,
            })
            .expect("allocation slot sets")
    }

    /// MIR-01. Making the root set exact is only half the fix: the *frame* is
    /// what the collector reads, and a slot written at one safepoint keeps its
    /// value until something overwrites it. `seed` is spilled at `first`'s
    /// allocation and dead at `second`'s, so `second` must null it — otherwise
    /// the object stays reachable for the rest of the call, and (since RT-01
    /// made swept storage reusable) the slot can end up naming a live object of
    /// a different type.
    #[test]
    fn a_slot_spilled_at_one_safepoint_is_nulled_once_its_local_dies() {
        let (mut f, seed, first, second) = shrinking_roots_function();
        annotate(&mut f);

        let (first_live, first_dead, _) = slots_for(&f, first);
        assert!(
            first_live.contains(&seed),
            "seed is spilled here — that is what makes its slot stale later"
        );
        assert!(
            !first_dead.contains(&seed),
            "seed is live at its own spill point and must not be nulled there"
        );

        let (second_live, second_dead, _) = slots_for(&f, second);
        assert!(!second_live.contains(&seed), "seed is dead by the second");
        assert!(
            second_dead.contains(&seed),
            "seed's stale slot must be nulled at the next safepoint, not left \
             holding the object: dead = {second_dead:?}"
        );
    }

    /// Nothing may be in both sets at one safepoint: a slot is either written
    /// with a value or nulled, never both.
    #[test]
    fn the_live_and_dead_sets_of_a_safepoint_are_disjoint() {
        let (mut f, ..) = shrinking_roots_function();
        annotate(&mut f);
        for inst in &f.blocks[0].insts {
            if let Inst::Alloc { dst, roots, .. } = inst {
                for d in roots.dead() {
                    assert!(
                        !roots.live().contains(d),
                        "{d:?} is both live and dead at {dst:?}'s safepoint"
                    );
                }
            }
        }
    }

    /// MIR-16, and the reason the split had to land first (H3). The debugger's
    /// view of a point must not shrink when the *root* set does: `seed` is not
    /// rooted at `second`'s allocation and is nulled out of the shadow frame
    /// there, yet `locals` must still render it. Two frames, two sets.
    #[test]
    fn the_debug_set_still_shows_what_the_root_set_dropped() {
        let (mut f, seed, _, second) = shrinking_roots_function();
        annotate(&mut f);

        let (live, dead, visible) = slots_for(&f, second);
        assert!(!live.contains(&seed), "the collector no longer roots seed");
        assert!(dead.contains(&seed), "and its shadow slot is cleared");
        assert!(
            visible.contains(&seed),
            "but the debugger must still be able to render it: visible = {visible:?}"
        );
    }

    /// A `CheckFault` is a debugger safepoint and nothing else. It carries a
    /// [`DebugSlots`] — which liveness must actually fill, or a fault-path
    /// snapshot shows `<uninit>` for the operands — and, structurally, has no
    /// [`RootSlots`] field at all to carry.
    #[test]
    fn check_fault_carries_an_annotated_debug_set() {
        let (mut f, seed, _, _) = shrinking_roots_function();
        let blk = crate::ir::BlockId(0);
        let fault_blk = f.new_block();
        f.blocks[fault_blk.0 as usize].term = Terminator::Fault;
        f.blocks[blk.0 as usize].insts.push(Inst::CheckFault {
            on_fault: fault_blk,
            debug: DebugSlots::unannotated(),
        });
        annotate(&mut f);

        let debug = f.blocks[blk.0 as usize]
            .insts
            .iter()
            .find_map(|inst| match inst {
                Inst::CheckFault { debug, .. } => Some(debug),
                _ => None,
            })
            .expect("the check-fault");
        assert!(
            debug.is_annotated(),
            "liveness must fill the debugger's set"
        );
        assert!(
            debug.visible().contains(&seed),
            "a value materialized earlier in the block is renderable at the \
             fault: visible = {:?}",
            debug.visible()
        );
    }
}
