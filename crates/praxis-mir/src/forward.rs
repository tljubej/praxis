//! Block-local box/unbox forwarding (ADR-120): the pass that deletes the box a
//! value is put into so that the next instruction can take it straight back out.
//!
//! [`crate::build`] materializes **every** intermediate node of an expression
//! tree, because [`lower_expr_gc`](crate::build) has one return convention — a
//! `Gc` local — and an arithmetic operator has one operand convention, a
//! `Scalar`. So `acc + i * 3` emits `IntBinOp` → `Materialize` →
//! `ExtractScalar` → `IntBinOp` and the middle two cancel. The same cancelling
//! pair is what `while i < limit` emits (`IntCmp` → `Materialize{Bool}` →
//! `ExtractScalar{Bool}` → `Terminator::Branch`), what a float expression tree
//! emits at every interior node, and what an `Int` literal small enough for
//! ADR-100's table emits (`ConstGc` → `ExtractScalar`).
//!
//! # What it does, which is not a copy
//!
//! It rewrites the **consuming instruction's operand field** from the extracted
//! local to the scalar the producer boxed, and then deletes what became dead.
//! Nothing is copied, no local leaves [`Function::locals`], and no [`Inst`]
//! variant is added — which is the whole reason this is 300 lines in
//! `praxis-mir` and not a change to the Cranelift backend. Handover 26 §3 posed
//! the mechanism as a dilemma between adding a `MoveScalar` variant and doing
//! whole-function `LocalId` substitution; the first would edit `lower_inst` and
//! the second is invalid in a non-SSA IR (`ir.rs`'s header: MIR is "deliberately
//! **not** SSA", and `Assign` lowers to a `MoveGc` **into the binding's existing
//! slot**, so a `LocalId` does not name one value).
//!
//! # Where it runs, and why that is the end of `lower_module`
//!
//! It must run **before** [`crate::annotate`], because it deletes safepoints and
//! [`RootSlots`](crate::RootSlots)/[`DebugSlots`](crate::DebugSlots) are
//! computed per safepoint. All five hosts do `lower_module → annotate → verify`
//! separately, so hooking the last line of `lower_module` gives that ordering
//! with no host edited — which is ADR-108 §1's stated reason for refusing a
//! standalone pass. It is safe by construction rather than by convention: every
//! builder site writes `RootSlots::unannotated()` and `RootSlots::set` is
//! `pub(crate)` to `annotate` alone, so this pass deletes safepoints whose slot
//! sets hold no answer yet. It cannot invalidate an answer, because none exists.
//!
//! # Why rewriting is always safe and only *deleting* needs a licence
//!
//! This is the design point, and it is what makes the two operand-rewriting
//! helpers below safe to leave non-exhaustive.
//!
//! Under the gates in [`plan`], the scalar `s` holds, at every use of `e`, the
//! exact word `e` was loaded from. So rewriting a use of `e` to `s` cannot
//! change what the program computes — a helper that misses a field has produced
//! a *smaller* optimization, never a wrong one. What needs a licence is
//! **deleting `e`'s definition**, and the licence is a re-run of the read-only
//! [`uses`]/[`term_uses`] over the whole function: the `ExtractScalar` goes only
//! when no use of `e` survives it. That converts a non-exhaustive match from a
//! correctness hazard into a missed-optimization hazard, and it is what leaves
//! ADR-044's count of exhaustive `Inst` matches at five rather than six.
//!
//! (Handover 27 §3 phrased the guard as "delete, then put the `ExtractScalar`
//! back if a use survived". Asking first is the same guard with no undo path,
//! and an undo path is the thing that would have needed its own correctness
//! argument: reversing an operand rewrite `s → e` would clobber a use of `s`
//! that was there all along.)
//!
//! # What it leaves behind
//!
//! The elided box's `Gc` local **stays in the table**, undefined and unused.
//! [`Function::locals`] is indexed by `LocalId` with three parallel `Vec`s, and
//! the backend's `build_function_debug_meta` assigns each `Gc` local its
//! `symbol_id` by position, so removing an entry renumbers every `<tmp#N>` the
//! crash debugger prints. Nothing requires density. The cost is one shadow slot
//! and one debug slot zeroed in the prologue per elided temp — a per-call cost
//! against a per-iteration win.
//!
//! It also cost the debugger a value, and that was a known, measured regression
//! rather than a surprise: a temp whose producer is gone rendered
//! `<tmp#N: Int> @ "a + b" = <uninit>` where it used to render `= 30`. Two
//! tests said so and both were red on the part-1 branch on purpose:
//! `crates/praxis-cli/tests/run.rs`'s
//! `a_forwarded_binop_temp_still_renders_the_value_it_materialized`, added by
//! this package because handover 26 believed an existing test covered it and
//! handover 27 §1 proved none did, and
//! `crates/praxis-codegen-cranelift/tests/jit.rs`'s
//! `a_temp_that_never_reached_a_shadow_slot_is_still_renderable`, which was
//! already there.
//!
//! **ADR-120 part 2 (W8-S0b) turned both green, unedited**, by giving the
//! elided box's debug slot the *scalar* the box would have held. This pass
//! hands it the two things it needs, and both are here because this is the last
//! point in the compiler that knows the box and the scalar are one value:
//! [`carry_debug_metadata`] moves the box's provenance onto the scalar, and
//! [`carry_debug_slot`] tells the box which scalar feeds its slot.

use std::collections::BTreeSet;

use crate::ir::{AllocKind, BlockId, Function, GcConst, Inst, LocalId, ScalarKind, Terminator};
use crate::liveness::{defs, term_uses, uses};

/// Forward every block-local box/unbox pair in `func`, and answer how many
/// `ExtractScalar`s were removed.
///
/// Idempotent: a second run over the result finds nothing, because the gates
/// are properties of the MIR and not of a worklist.
pub fn forward_boxes(func: &mut Function) -> usize {
    // ADR-120's measurement arm A. The A/B baseline is "this tree with this
    // package's single toggle reverted" (handover 26 §6), and this `if` is that
    // toggle: with the feature on the pass is a no-op and the rest of the branch
    // — the tests, the debug hand-off, the census helpers — is compiled
    // unchanged, so the two binaries differ in exactly this transform.
    if cfg!(feature = "adr120-arm-a") {
        return 0;
    }

    // Sites whose rewrite was not exhaustive: the `ExtractScalar` stayed, so
    // planning would find them again forever. Keyed by the extracted local,
    // which is stable because no local is ever renumbered.
    let mut wedged: BTreeSet<LocalId> = BTreeSet::new();
    let mut forwarded = 0;
    while let Some(site) = plan(func, &wedged) {
        if apply(func, &site) {
            forwarded += 1;
        } else {
            wedged.insert(site.extracted);
        }
    }
    forwarded
}

/// One forwardable pair, located. `producer` and `extract` are instruction
/// indices into `block`, and `producer < extract`.
#[derive(Debug)]
struct Site {
    block: BlockId,
    producer: usize,
    extract: usize,
    /// The producer's `Gc` destination — the box.
    boxed: LocalId,
    /// The `ExtractScalar`'s destination.
    extracted: LocalId,
    how: How,
}

/// The two shapes of forwarding, which are different transforms.
#[derive(Clone, Copy, Debug)]
enum How {
    /// The producer boxed a scalar the function already computed: rewrite the
    /// uses of the extracted local to name that scalar instead.
    Operand { scalar: LocalId },
    /// The producer was a [`Inst::ConstGc`], which boxes nothing — the value is
    /// an immediate in the instruction. There is no scalar to forward, so the
    /// `ExtractScalar` is *replaced in place* by the immediate that produced it
    /// and no operand moves at all.
    Immediate { value: i64 },
}

/// Find the first forwardable pair, or `None`.
fn plan(func: &Function, wedged: &BTreeSet<LocalId>) -> Option<Site> {
    let census = Census::of(func);
    for (idx, block) in func.blocks.iter().enumerate() {
        let bid = BlockId(idx as u32);
        for (extract, inst) in block.insts.iter().enumerate() {
            let Inst::ExtractScalar {
                dst: extracted,
                src: boxed,
                scalar,
            } = inst
            else {
                continue;
            };
            if wedged.contains(extracted) {
                continue;
            }
            let Some(site) = consider(func, &census, bid, extract, *extracted, *boxed, *scalar)
            else {
                continue;
            };
            return Some(site);
        }
    }
    None
}

/// Every gate, in one place, for the `ExtractScalar` at `block.insts[extract]`.
///
/// Each `None` below is a gate, and the order is cheapest-first rather than
/// most-likely-first: the census answers are already computed.
#[allow(clippy::too_many_arguments)]
fn consider(
    func: &Function,
    census: &Census,
    bid: BlockId,
    extract: usize,
    extracted: LocalId,
    boxed: LocalId,
    scalar: ScalarKind,
) -> Option<Site> {
    let block = &func.blocks[bid.0 as usize];

    // Gate 1: the extracted local is defined exactly here. Without it, deleting
    // this instruction leaves the *other* definitions reaching uses this pass
    // rewrote against this one.
    if census.def_count(extracted) != 1 {
        return None;
    }
    // Gate 2: every use of it is in this block, and after this instruction.
    // "In this block" is what makes the pass block-local — MIR is not SSA, so a
    // use in a successor may be reached by an edge that never ran this block.
    // "After" is not implied by "in this block": in a loop body a use at a lower
    // index reads the *previous* iteration's value.
    if census.use_block(extracted) != UseBlock::Only(bid) {
        return None;
    }
    let span = block_use_span(block, extracted);
    if span.first_inst_use.is_some_and(|first| first <= extract) {
        return None;
    }

    // Gate 3: the nearest preceding definition of the box is in this block.
    // Nearest, so no redefinition of the box can sit between it and the
    // extraction — which is what makes the extracted payload *this* producer's.
    let producer = (0..extract)
        .rev()
        .find(|&k| defs(&block.insts[k]).contains(&boxed))?;

    // Gate 4: the producer is one this pass understands, its payload kind is the
    // one being extracted, and it cannot fault.
    //
    // Kind equality does real work beyond the obvious: `verify::operands` checks
    // operand *range* only, so a latent `Materialize{Bool}` / `ExtractScalar{Int}`
    // pun verifies today, and forwarding across it would turn a punned reload
    // into a silent value substitution.
    //
    // `can_fault` is the gate that keeps an `Alloc { Char }` out. `AllocChar`
    // validates its Unicode scalar, so its row is `AllocatesAndFaults` and
    // ADR-088 puts a `CheckFault` immediately after it; deleting the producer
    // would orphan the check and `verify::check_fault_observed` would refuse the
    // function. It is not a conservatism — it is the exact boundary, and it is
    // drawn around the *allocation*, not around `Char`: a `ConstGc { Char }`
    // reads an interned slot, cannot fault, and is folded (ADR-141).
    if block.insts[producer].can_fault() {
        return None;
    }
    let how = match &block.insts[producer] {
        Inst::Materialize {
            src, scalar: kind, ..
        } if *kind == scalar => How::Operand { scalar: *src },
        Inst::Alloc { alloc, .. } => match (alloc, scalar) {
            (AllocKind::Int { value }, ScalarKind::Int)
            | (AllocKind::Bool { value }, ScalarKind::Bool)
            | (AllocKind::Float { value }, ScalarKind::Float) => How::Operand { scalar: *value },
            _ => return None,
        },
        Inst::ConstGc { konst, .. } => match (konst, scalar) {
            (GcConst::SmallInt(n), ScalarKind::Int) => How::Immediate { value: *n },
            (GcConst::Bool(b), ScalarKind::Bool) => How::Immediate {
                value: i64::from(*b),
            },
            // The other half of the character literal's cost (ADR-141). Without
            // this row `'#'` is two loads but `if c == '#'` still re-extracts
            // the literal's payload every iteration; with it the comparison is
            // against an immediate the code point is baked into.
            (GcConst::Char(c), ScalarKind::Char) => How::Immediate {
                value: i64::from(*c),
            },
            _ => return None,
        },
        _ => return None,
    };

    // Gate 5: the forwarded scalar still holds the boxed word at every use.
    //
    // Conservative by one instruction on purpose: a definition *at* `last_use`
    // would be read-before-written and therefore fine, but "no definition in the
    // window" is the sentence a reader can check, and the shape it would buy
    // (`s = f(s)` between a box and the last reload of that box) does not occur
    // in anything the builder emits.
    if let How::Operand { scalar: src } = how {
        let window = producer + 1..=span.last_inst_use.unwrap_or(producer);
        if window
            .into_iter()
            .any(|k| defs(&block.insts[k]).contains(&src))
        {
            return None;
        }
    }

    Some(Site {
        block: bid,
        producer,
        extract,
        boxed,
        extracted,
        how,
    })
}

/// Where in one block a local is read.
///
/// The terminator is deliberately *not* an index: it reads after every
/// instruction, so folding it into `last_inst_use` would widen gate 5's window
/// past the end of the block, and folding it into `first_inst_use` would make
/// gate 2's "after the extraction" test compare an index against a
/// non-position. A terminator-only use — which is the `while` shape, the most
/// common one there is — must read as "no instruction uses it" in both.
struct UseSpan {
    first_inst_use: Option<usize>,
    last_inst_use: Option<usize>,
}

fn block_use_span(block: &crate::ir::Block, local: LocalId) -> UseSpan {
    let mut first_inst_use = None;
    let mut last_inst_use = None;
    for (k, inst) in block.insts.iter().enumerate() {
        if uses(inst).contains(&local) {
            first_inst_use.get_or_insert(k);
            last_inst_use = Some(k);
        }
    }
    UseSpan {
        first_inst_use,
        last_inst_use,
    }
}

/// Perform the rewrite. Answers whether the `ExtractScalar` could be deleted —
/// `false` means an operand field escaped the helpers below, the instruction
/// stays, and the site must not be planned again.
fn apply(func: &mut Function, site: &Site) -> bool {
    let bid = site.block.0 as usize;

    if let How::Immediate { value } = site.how {
        // No operand moves: the extraction *becomes* the immediate.
        func.blocks[bid].insts[site.extract] = Inst::ConstInt {
            dst: site.extracted,
            value,
        };
        drop_producer_if_dead(func, site);
        return true;
    }
    let How::Operand { scalar } = site.how else {
        unreachable!("the immediate case returned above");
    };

    for inst in &mut func.blocks[bid].insts[site.extract + 1..] {
        rewrite_scalar_operand(inst, site.extracted, scalar);
    }
    rewrite_branch_condition(&mut func.blocks[bid].term, site.extracted, scalar);

    // The licence to delete: no use of the extracted local survives anywhere.
    if Census::of(func).use_count(site.extracted) != 0 {
        return false;
    }
    func.blocks[bid].insts.remove(site.extract);
    drop_producer_if_dead(func, site);
    true
}

/// Delete the producer if nothing reads its box any more.
///
/// The producer's index is still valid: it is below the `ExtractScalar`'s, and
/// that is the only instruction removed before this runs.
fn drop_producer_if_dead(func: &mut Function, site: &Site) {
    let census = Census::of(func);
    if census.use_count(site.boxed) != 0 || census.def_count(site.boxed) != 1 {
        return;
    }
    // The word the box would have held is now some `Scalar` local's, and which
    // one depends on the transform: the producer's operand where there was one,
    // and the `ConstInt` that replaced the reload where the producer was a
    // `ConstGc` carrying an immediate. Both are the same statement to the
    // debugger — *this box's value is that scalar's* — so both are recorded the
    // same way.
    let replacement = match site.how {
        How::Operand { scalar } => {
            carry_debug_metadata(func, site.boxed, scalar);
            scalar
        }
        How::Immediate { .. } => site.extracted,
    };
    carry_debug_slot(func, &census, site.boxed, replacement);
    func.blocks[site.block.0 as usize]
        .insts
        .remove(site.producer);
}

/// Move the elided box's debugger provenance onto the scalar that replaced it.
///
/// It renders nothing today — a `Scalar` local has no debug slot, which is why
/// the regression in this module's header exists — and it is the whole of
/// W8-S0b's input: the scalar slot that stage adds needs to know that this word
/// is the value of the expression `@ "a + b"`, and this is the only point in the
/// compiler that still knows it.
///
/// Written once. A scalar reached by two forwardings would otherwise take the
/// second box's span, and the first is the one whose expression produced it.
fn carry_debug_metadata(func: &mut Function, boxed: LocalId, scalar: LocalId) {
    let (b, s) = (boxed.0 as usize, scalar.0 as usize);
    if func.debug_spans[s].is_some() {
        return;
    }
    let (span, name, kind) = (
        func.debug_spans[b],
        func.debug_names[b].clone(),
        func.debug_kinds[b],
    );
    func.debug_spans[s] = span;
    func.debug_names[s] = name;
    func.debug_kinds[s] = kind;
}

/// Point the elided box's *debug slot* at the scalar that now holds its word
/// (ADR-120 part 2, and the whole of what part 2 needs from this pass).
///
/// [`carry_debug_metadata`] above moves the box's provenance onto the scalar;
/// this moves the value channel the other way, and the two are separate because
/// they answer different questions. The provenance says what the scalar *is*.
/// This says which slot a definition of the scalar must write — and it is
/// recorded against the **box**, so `build_function_debug_meta` keeps emitting
/// one `DebugLocalMeta` per `Gc` local, in position order, with the box's own
/// name, span and `type_id`. Nothing is renumbered and no second line appears
/// for one temp: the slot that was there gains a value instead of the debugger
/// gaining a local. ADR-120 decision 7 kept the box in `Function::locals` for
/// the numbering; this is what that decision was worth.
///
/// **The gate is that the scalar is defined exactly once in the function.** A
/// debug slot is never cleared, so it renders whatever the most recently
/// executed store wrote (ADR-104); a scalar with two definitions could
/// therefore leave the second expression's value under the first expression's
/// `@ "…"` provenance, which is a *wrong* rendering rather than a missing one.
/// One definition makes that unrepresentable. It costs nothing today —
/// `build.rs` allocates a fresh `Scalar` local per expression node, so every
/// site this pass forwards passes the gate — and it is what keeps the property
/// true if a later builder stops doing that.
fn carry_debug_slot(func: &mut Function, census: &Census, boxed: LocalId, scalar: LocalId) {
    // ADR-120 part 2's measurement arm A, and the whole of the toggle: with the
    // feature on, no box is ever linked to a scalar, so the backend has nothing
    // to store and nothing to mark. The transform above is unaffected — both
    // arms compute the same thing — and what differs is the debugger's view and
    // the stores that produce it, which is exactly the package.
    if cfg!(feature = "adr120b-arm-a") {
        return;
    }
    if census.def_count(scalar) != 1 {
        return;
    }
    func.debug_scalar_sources[boxed.0 as usize] = Some(scalar);
}

/// Rewrite every `Scalar` operand field of `inst` that names `from` to name
/// `to`.
///
/// **Deliberately non-exhaustive**, and the `_` arm is the reason this file
/// does not add a sixth exhaustive match over [`Inst`] to the five ADR-044
/// fixes. A variant added later and forgotten here costs a forwarding
/// opportunity, not a miscompile — see this module's header.
fn rewrite_scalar_operand(inst: &mut Inst, from: LocalId, to: LocalId) {
    fn swap(slot: &mut LocalId, from: LocalId, to: LocalId) {
        if *slot == from {
            *slot = to;
        }
    }
    match inst {
        Inst::Materialize { src, .. }
        | Inst::StoreScalar { src, .. }
        | Inst::FloatNeg { src, .. } => swap(src, from, to),
        Inst::IntBinOp { lhs, rhs, .. }
        | Inst::IntCmp { lhs, rhs, .. }
        | Inst::FloatBinOp { lhs, rhs, .. }
        | Inst::FloatCmp { lhs, rhs, .. } => {
            swap(lhs, from, to);
            swap(rhs, from, to);
        }
        Inst::Alloc {
            alloc:
                AllocKind::Int { value }
                | AllocKind::Bool { value }
                | AllocKind::Char { value }
                | AllocKind::Float { value },
            ..
        } => swap(value, from, to),
        _ => {}
    }
}

/// The terminator half of [`rewrite_scalar_operand`], and it is **mandatory
/// rather than an extra**.
///
/// `lower_while` emits `Materialize{Bool}` → `ExtractScalar{Bool}` →
/// `Terminator::Branch` in one block, and the extracted `Bool` is consumed
/// *only* by the terminator. A pass that walked `insts` alone would forward
/// nothing in the single most common shape in the language, pass every test, and
/// report a smaller win.
fn rewrite_branch_condition(term: &mut Terminator, from: LocalId, to: LocalId) {
    if let Terminator::Branch { cond, .. } = term {
        if *cond == from {
            *cond = to;
        }
    }
}

// ---------------------------------------------------------------------------
// The census
// ---------------------------------------------------------------------------

/// Where a local is used, to the precision the gates need.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum UseBlock {
    Nowhere,
    Only(BlockId),
    Several,
}

/// Definition and use counts per local, plus which block the uses are in.
///
/// One linear pass over the function, built from [`defs`], [`uses`] and
/// [`term_uses`] — the crate's existing answers — rather than a fresh match over
/// [`Inst`].
struct Census {
    def_count: Vec<u32>,
    use_count: Vec<u32>,
    use_block: Vec<UseBlock>,
}

impl Census {
    fn of(func: &Function) -> Census {
        let n = func.locals.len();
        let mut census = Census {
            def_count: vec![0; n],
            use_count: vec![0; n],
            use_block: vec![UseBlock::Nowhere; n],
        };
        for (idx, block) in func.blocks.iter().enumerate() {
            let bid = BlockId(idx as u32);
            for inst in &block.insts {
                for d in defs(inst) {
                    census.def_count[d.0 as usize] += 1;
                }
                for u in uses(inst) {
                    census.record_use(u, bid);
                }
            }
            for u in term_uses(&block.term) {
                census.record_use(u, bid);
            }
        }
        census
    }

    fn record_use(&mut self, local: LocalId, bid: BlockId) {
        let i = local.0 as usize;
        self.use_count[i] += 1;
        self.use_block[i] = match self.use_block[i] {
            UseBlock::Nowhere => UseBlock::Only(bid),
            UseBlock::Only(b) if b == bid => UseBlock::Only(bid),
            _ => UseBlock::Several,
        };
    }

    fn def_count(&self, local: LocalId) -> u32 {
        self.def_count[local.0 as usize]
    }

    fn use_count(&self, local: LocalId) -> u32 {
        self.use_count[local.0 as usize]
    }

    fn use_block(&self, local: LocalId) -> UseBlock {
        self.use_block[local.0 as usize]
    }
}

#[cfg(test)]
mod tests {
    use praxis_stdlib::abi::RuntimeSymbol;

    use super::*;
    use crate::ir::LocalKind;
    // **The forwarded door, not the finished one** (ADR-121). Every number in
    // this module is a statement about what *this* pass leaves behind, and
    // several of the doc comments below say so in as many words — "the two that
    // remain are `x` and `y`, which are W8-S1's". Promotion then removes them,
    // so read through `lower_src_to_mir` these tests would assert ADR-121's
    // output while claiming to measure ADR-120's, and reverting promotion would
    // present as a failure here. The alias keeps the call sites unedited.
    use crate::test_support::lower_src_to_mir_forwarded as lower_src_to_mir;
    use crate::test_support::{benchmark_source, Census, InstKind, Lowered, BENCHMARK_SUITE};
    use crate::verify::verify;

    const INT_BOX: InstKind = InstKind::Materialize(ScalarKind::Int);
    const BOOL_BOX: InstKind = InstKind::Materialize(ScalarKind::Bool);
    const FLOAT_BOX: InstKind = InstKind::Materialize(ScalarKind::Float);
    const INT_LOAD: InstKind = InstKind::ExtractScalar(ScalarKind::Int);
    const BOOL_LOAD: InstKind = InstKind::ExtractScalar(ScalarKind::Bool);
    const FLOAT_LOAD: InstKind = InstKind::ExtractScalar(ScalarKind::Float);

    /// Annotate and verify every function of a lowered module.
    ///
    /// The gates are stated over MIR, and what they must protect is the
    /// invariant the *verifier* states — every fault observed, every safepoint
    /// annotated, no raw word in a rootable slot. Running the real pipeline over
    /// a real program is the assertion that covers a gate nobody thought to
    /// write a unit test for.
    fn verified(lowered: &mut Lowered) {
        for func in &mut lowered.funcs {
            crate::annotate(func);
            if let Err(errs) = verify(func) {
                panic!("{}", crate::verify::report(&errs));
            }
        }
    }

    #[test]
    fn the_measurement_toggle_decides_whether_the_pass_runs() {
        // Both arms in one test, because a toggle whose off-state is untested is
        // a toggle that can stop toggling. `lower_src_to_mir` has already run
        // the pass, so this second call answers idempotence in arm B and "it did
        // nothing" in arm A.
        let mut lowered = lower_src_to_mir("fn f(a: Int) -> Int { a + a * 3 }");
        assert_eq!(
            forward_boxes(&mut lowered.funcs[0]),
            0,
            "the first run reached a fixpoint"
        );

        let census = Census::of_function(lowered.function("f"));
        if cfg!(feature = "adr120-arm-a") {
            assert_eq!(
                census.count(INT_BOX),
                2,
                "arm A boxes both nodes: {census:?}"
            );
        } else {
            assert_eq!(
                census.count(INT_BOX),
                1,
                "arm B boxes only the returned node: {census:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // The three producers
    // -----------------------------------------------------------------------

    /// The interior node of an expression tree, which is the shape the pass was
    /// built for: `a * 3` is boxed only so that `a + _` can unbox it.
    #[test]
    fn an_interior_arithmetic_node_is_neither_boxed_nor_reloaded() {
        let lowered = lower_src_to_mir("fn f(a: Int) -> Int { a + a * 3 }");
        let census = Census::of_function(lowered.function("f"));
        assert_eq!(census.count(INT_BOX), 1, "only the result: {census:?}");
        assert_eq!(
            census.count(INT_LOAD),
            2,
            "`a` twice, and nothing for the interior node: {census:?}"
        );
    }

    /// The `Alloc` producer. An `Int` outside ADR-100's interned range is an
    /// `Inst::Alloc { AllocKind::Int }` rather than a `Materialize`, and it
    /// forwards for the same reason: the scalar it boxes is right there.
    #[test]
    fn an_out_of_range_int_literal_is_forwarded_through_its_alloc() {
        let lowered = lower_src_to_mir("fn f(a: Int) -> Int { a + 999999 }");
        let census = Census::of_function(lowered.function("f"));
        assert_eq!(
            census.count(InstKind::alloc(RuntimeSymbol::AllocInt)),
            0,
            "the literal's box had one consumer and it took the scalar: {census:?}"
        );
        assert_eq!(census.count(INT_BOX), 1, "the sum: {census:?}");
    }

    /// The `ConstGc` producer, which is the *free* case: the value is an
    /// immediate in the instruction, so the reload is replaced in place by
    /// `ConstInt` and no operand moves at all. Two loads out of the context
    /// become one `iconst`.
    #[test]
    fn a_small_int_literal_becomes_an_immediate_instead_of_a_table_read() {
        let lowered = lower_src_to_mir("fn f(a: Int) -> Int { a + 3 }");
        let census = Census::of_function(lowered.function("f"));
        assert_eq!(census.count(InstKind::ConstGc), 0, "{census:?}");
        assert_eq!(
            census.count(InstKind::ConstInt),
            1,
            "the `3` is an immediate now: {census:?}"
        );
        assert_eq!(census.count(INT_LOAD), 1, "only `a`: {census:?}");
    }

    // -----------------------------------------------------------------------
    // The terminator, which is the most common shape in the language
    // -----------------------------------------------------------------------

    /// **The rewrite that is mandatory rather than optional.** `lower_while`
    /// emits `IntCmp` → `Materialize{Bool}` → `ExtractScalar{Bool}` →
    /// `Terminator::Branch` in one block, and the boxed `Bool` is consumed
    /// *only* by the terminator. A pass that walked `insts` alone would forward
    /// nothing here, pass every other test in this file, and report a smaller
    /// win.
    #[test]
    fn a_while_condition_branches_on_the_comparison_instead_of_boxing_it() {
        let lowered = lower_src_to_mir(
            "fn f(limit: Int) -> Int {\n  var i = 0\n  while i < limit { i = i + 1 }\n  i\n}",
        );
        let census = Census::of_function(lowered.function("f"));
        assert_eq!(census.count(BOOL_BOX), 0, "{census:?}");
        assert_eq!(census.count(BOOL_LOAD), 0, "{census:?}");
        assert_eq!(
            census.count(InstKind::IntCmp),
            1,
            "still compared: {census:?}"
        );
    }

    /// The same for `if`, whose condition is also a terminator operand, and for
    /// `!`, which lowers to a comparison against zero.
    #[test]
    fn an_if_condition_and_a_negation_both_branch_on_their_scalar() {
        let lowered = lower_src_to_mir("fn f(a: Int) -> Int { if !(a == 1) { 2 } else { 3 } }");
        let census = Census::of_function(lowered.function("f"));
        assert_eq!(census.count(BOOL_BOX), 0, "{census:?}");
        assert_eq!(census.count(BOOL_LOAD), 0, "{census:?}");
    }

    // -----------------------------------------------------------------------
    // The gates, each with the program that makes it fire
    // -----------------------------------------------------------------------

    /// Gate 4's `can_fault`, which is the exact boundary rather than a
    /// conservatism. `praxis_alloc_char` validates its Unicode scalar, so its
    /// row is `AllocatesAndFaults` and ADR-088 puts a `CheckFault` immediately
    /// after it; deleting the producer would orphan the check and
    /// `verify::check_fault_observed` would refuse the function.
    ///
    /// **Hand-built, because the shape is narrow.** `build.rs`'s
    /// `AllocKind::Char` site is reached from an `Int.to_char()`, whose scalar
    /// arrives at run time, and from a character literal above U+007F, which is
    /// past `small_char`'s interned range (ADR-141). An in-range literal is a
    /// `ConstGc { Char }` instead — no allocation, no check, and *folded*, which
    /// is what [`a_const_gc_char_reload_becomes_an_immediate`] holds. The two
    /// tests are the two sides of `can_fault`: the boundary is the allocation,
    /// not the type.
    ///
    /// [`a_const_gc_char_reload_becomes_an_immediate`]: self::a_const_gc_char_reload_becomes_an_immediate
    #[test]
    fn a_char_box_is_not_forwarded_because_its_check_fault_would_be_orphaned() {
        use crate::ir::fixtures::{gc_local, scalar_local};

        let mut f = Function::empty("f");
        let code = scalar_local(&mut f, ScalarKind::Char);
        let reloaded = scalar_local(&mut f, ScalarKind::Char);
        let boxed = gc_local(&mut f);
        let blk = f.new_block();
        let fault = f.new_block();
        f.blocks[blk.0 as usize].insts = vec![
            Inst::ConstInt {
                dst: code,
                value: i64::from('a' as u32),
            },
            Inst::Alloc {
                dst: boxed,
                alloc: AllocKind::Char { value: code },
                roots: crate::RootSlots::unannotated(),
                debug: crate::DebugSlots::unannotated(),
            },
            Inst::CheckFault {
                on_fault: fault,
                debug: crate::DebugSlots::unannotated(),
            },
            Inst::ExtractScalar {
                dst: reloaded,
                src: boxed,
                scalar: ScalarKind::Char,
            },
            Inst::Materialize {
                dst: boxed,
                src: reloaded,
                scalar: ScalarKind::Char,
                roots: crate::RootSlots::unannotated(),
                debug: crate::DebugSlots::unannotated(),
            },
            Inst::CheckFault {
                on_fault: fault,
                debug: crate::DebugSlots::unannotated(),
            },
        ];
        f.return_local = boxed;
        f.blocks[blk.0 as usize].term = Terminator::Return { value: boxed };
        f.blocks[fault.0 as usize].term = Terminator::Fault;

        assert_eq!(
            forward_boxes(&mut f),
            0,
            "the `Char` pair is not this pass's"
        );
        crate::annotate(&mut f);
        verify(&f).unwrap_or_else(|errs| panic!("{}", crate::verify::report(&errs)));
    }

    /// The other side of `can_fault`, and the second half of the character
    /// literal's cost (ADR-141).
    ///
    /// An in-range `'#'` is a `ConstGc { Char }` — a table read, which allocates
    /// nothing and cannot fault — so its reload folds to the immediate `35`.
    /// That is what makes `for c in line { if c == '#' { … } }` compare against
    /// a baked-in constant instead of re-reading the boxed payload every
    /// iteration. Without it the literal is cheaper to *build* and no cheaper to
    /// *use*, which is most of what the loop was paying for.
    #[test]
    fn a_const_gc_char_reload_becomes_an_immediate() {
        const CHAR_LOAD: InstKind = InstKind::ExtractScalar(ScalarKind::Char);

        let lowered = lower_src_to_mir("fn f(c: Char) -> Bool { c == '#' }");
        let census = Census::of_function(lowered.function("f"));
        assert_eq!(
            census.count(InstKind::ConstGc),
            0,
            "the literal's box had one consumer and it took the code point: {census:?}"
        );
        assert_eq!(
            census.count(InstKind::ConstInt),
            1,
            "`'#'` is the immediate 35 now: {census:?}"
        );
        assert_eq!(
            census.count(CHAR_LOAD),
            1,
            "only `c`'s payload is still read: {census:?}"
        );
    }

    /// Gate 2. A box produced in one block and unboxed in another is left
    /// alone: MIR is not SSA, so the reaching definition is a question this pass
    /// deliberately does not ask (ADR-108 declined the four analyses that answer
    /// it). ADR-108's preheader hoisting puts a loop-invariant literal's
    /// `ConstGc` in the preheader and its reload in the body, which is exactly
    /// that shape.
    #[test]
    fn a_hoisted_literal_is_not_forwarded_across_the_block_boundary() {
        let lowered = lower_src_to_mir(
            "fn f(n: Int) -> Int {\n  var s = 0\n  var i = 0\n  \
             while i < n {\n    s = s + i * 7\n    i = i + 1\n  }\n  s\n}",
        );
        let func = lowered.function("f");
        let body = &func.blocks[lowered.block_over(func, "i * 7").0 as usize];
        let reloads = body
            .insts
            .iter()
            .filter(|i| matches!(i, Inst::ExtractScalar { .. }))
            .count();
        let from_another_block = body
            .insts
            .iter()
            .enumerate()
            .filter(|(k, inst)| match inst {
                Inst::ExtractScalar { src, .. } => {
                    !body.insts[..*k].iter().any(|p| defs(p).contains(src))
                }
                _ => false,
            })
            .count();
        assert_eq!(
            (reloads, from_another_block),
            (3, 3),
            "every reload the body still pays reads a box no preceding \
             instruction in it defines — the two loop variables' slots and \
             ADR-108's hoisted `7`. Gate 3 declines exactly those and nothing \
             else: {:?}",
            body.insts
        );
    }

    /// Gate 1 and gate 2 together, over the shape that motivates them: a box
    /// whose payload is read from two blocks. Neither reload may take the
    /// scalar, because each is reached by an edge on which the other block never
    /// ran.
    #[test]
    fn a_box_read_from_two_blocks_keeps_both_reloads() {
        let lowered = lower_src_to_mir(
            "fn f(a: Int, c: Bool) -> Int {\n  var t = a * 2\n  \
             if c { t + 1 } else { t + 2 }\n}",
        );
        let census = Census::of_function(lowered.function("f"));
        assert_eq!(
            census.count(INT_BOX),
            3,
            "`a * 2` is still boxed for the two arms, and each arm boxes its \
             own sum: {census:?}"
        );
    }

    /// Gate 5, and the shape that could break it: a loop-carried accumulator
    /// reassigned between the box and the reload. The census cannot tell a
    /// correct forward from a wrong one, so this runs the verifier over the
    /// result — and `collatzs_inner_loop_...` plus the backend's own suite
    /// cover the values.
    #[test]
    fn a_loop_carried_accumulator_survives_the_pass_and_the_verifier() {
        let mut lowered = lower_src_to_mir(
            "fn f() -> Int {\n  var acc = 0\n  var i = 1\n  \
             while i < 5 {\n    acc = acc + i * 3\n    i = i + 1\n  }\n  acc\n}",
        );
        verified(&mut lowered);
        let census = Census::of_function(lowered.function("f"));
        assert_eq!(census.count(BOOL_BOX), 0, "the condition: {census:?}");
        assert_eq!(census.count(INT_BOX), 2, "`acc` and `i`: {census:?}");
    }

    // -----------------------------------------------------------------------
    // The suite: the gate handover 26 §1 stated and handover 27 §9 sent back
    // for measurement
    // -----------------------------------------------------------------------

    /// **The headline.** Handover 26 §1 predicted `mandelbrot`'s inner loop goes
    /// from 10 `Materialize{Float}` to 2, and handover 27 §9 listed the figure
    /// as hand-walked and unverified. Wave 0 measured the 10; this is the 2, and
    /// the prediction was exactly right.
    ///
    /// The two that remain are `x` and `y`, which are loop-carried assignments
    /// and therefore not this pass's shape at all — a `MoveGc` into a binding's
    /// existing slot is what W8-S1 addresses.
    #[test]
    fn mandelbrots_inner_loop_boxes_two_floats_where_it_boxed_ten() {
        let mut lowered = lower_src_to_mir(&benchmark_source("mandelbrot"));
        verified(&mut lowered);
        let func = lowered.entry();
        let census = lowered
            .innermost_loop_over(func, "x * x - y * y + x0")
            .census(func);
        assert_eq!(census.count(FLOAT_BOX), 2, "was 10: {census:?}");
        assert_eq!(census.count(FLOAT_LOAD), 14, "was 22: {census:?}");
        assert_eq!(
            census.count(InstKind::FloatBinOp),
            10,
            "the arithmetic is untouched: {census:?}"
        );
    }

    /// Handover 26 framed this package as a float transform throughout. It is
    /// not: `TypedExpr::Bin` materializes every intermediate node, `Int` as well
    /// as `Float`, and the `Bool` box a condition pays is the most common of the
    /// three. `collatz` allocates no float at all and its inner loop still loses
    /// four boxes and eleven reloads.
    #[test]
    fn collatzs_inner_loop_is_an_int_and_bool_win_with_no_float_in_it() {
        let mut lowered = lower_src_to_mir(&benchmark_source("collatz"));
        verified(&mut lowered);
        let func = lowered.entry();
        let census = lowered.innermost_loop_over(func, "3 * c").census(func);
        assert_eq!(census.count(FLOAT_BOX), 0, "{census:?}");
        assert_eq!(census.count(INT_BOX), 3, "was 5: {census:?}");
        assert_eq!(census.count(BOOL_BOX), 0, "was 2: {census:?}");
        assert_eq!(census.count(INT_LOAD), 5, "was 14: {census:?}");
        assert_eq!(census.count(BOOL_LOAD), 0, "was 2: {census:?}");
    }

    /// Every benchmark, through the real pipeline, annotated and verified. The
    /// per-loop numbers are ADR-120's table; what this asserts is the property
    /// they all share — the pass never emits MIR the verifier refuses, over
    /// eight programs that between them reach every producer, every gate and
    /// every terminator shape.
    #[test]
    fn every_benchmark_still_verifies_after_the_pass() {
        for name in BENCHMARK_SUITE {
            let mut lowered = lower_src_to_mir(&benchmark_source(name));
            verified(&mut lowered);
        }
    }

    /// Decision 7's cost, stated as the number it is: an elided box's `Gc`
    /// local stays in the table, so it keeps a shadow slot and a debug slot that
    /// the prologue zeroes and nothing ever writes. That is a per-call cost
    /// against a per-iteration win, and it is only ever paid once per call —
    /// which is the trade this asserts is still the right way round.
    #[test]
    fn an_elided_box_keeps_its_slot_and_the_count_is_a_fifth_of_them() {
        let lowered = lower_src_to_mir(&benchmark_source("mandelbrot"));
        let func = lowered.entry();
        let defined: BTreeSet<LocalId> = func
            .blocks
            .iter()
            .flat_map(|b| b.insts.iter().flat_map(defs))
            .collect();
        let gc = |l: &&crate::ir::Local| l.kind == crate::ir::LocalKind::Gc;
        let total = func.locals.iter().filter(gc).count();
        let undefined = func
            .locals
            .iter()
            .filter(gc)
            .filter(|l| !defined.contains(&l.id) && !func.params.contains(&l.id))
            .count();
        assert_eq!(
            (undefined, total),
            (18, 69),
            "18 zeroing stores in a prologue that runs once, against 8 float \
             allocations removed from a loop that runs 400 times per sample"
        );
    }

    /// The hand-off to ADR-120 part 2 (W8-S0b). An elided box takes its
    /// debugger provenance onto the scalar that replaced it, because this pass
    /// is the last point in the compiler that knows the two are one value.
    /// Nothing renders it today — a `Scalar` local has no debug slot — and that
    /// is exactly the regression part 2 repairs.
    #[test]
    fn an_elided_box_leaves_its_expression_span_on_the_scalar_that_replaced_it() {
        let src = "fn f(a: Int, b: Int) -> Int { a + b + 1 }";
        let lowered = lower_src_to_mir(src);
        let func = lowered.function("f");
        let start = u32::try_from(src.find("a + b").expect("in the source")).unwrap();
        let span = Some((start, start + 5));
        let carried = func
            .locals
            .iter()
            .filter(|l| matches!(l.kind, crate::ir::LocalKind::Scalar(_)))
            .filter(|l| func.debug_spans[l.id.0 as usize] == span)
            .count();
        assert_eq!(
            carried, 1,
            "exactly one scalar carries `a + b`'s span: {:?}",
            func.debug_spans
        );
    }

    /// The other half of the hand-off, and the one part 2 actually reads: the
    /// **box** learns which scalar now holds its word, so the backend can point
    /// its debug slot at that scalar's definition.
    ///
    /// Recorded against the box rather than the scalar because the box is the
    /// local that owns the debug slot and the `symbol_id`. That is what keeps
    /// `<tmp#7>` `<tmp#7>` — one metadata entry per `Gc` local, in position
    /// order, exactly as ADR-120 decision 7 left it.
    #[test]
    fn an_elided_box_learns_which_scalar_holds_the_word_it_would_have_boxed() {
        let src = "fn f(a: Int, b: Int) -> Int { a + b + 1 }";
        let lowered = lower_src_to_mir(src);
        let func = lowered.function("f");
        let start = u32::try_from(src.find("a + b").expect("in the source")).unwrap();
        let span = Some((start, start + 5));
        let boxed = func
            .locals
            .iter()
            .find(|l| l.kind == LocalKind::Gc && func.debug_spans[l.id.0 as usize] == span)
            .expect("`a + b`'s box is still in the table, undefined");
        let (scalar, kind) = func
            .debug_scalar_source(boxed.id)
            .expect("and it names the scalar that replaced it");
        assert_eq!(kind, ScalarKind::Int, "an `Int` expression's payload");
        assert_eq!(
            func.locals[scalar.0 as usize].kind,
            LocalKind::Scalar(ScalarKind::Int),
            "and the accessor's kind is the local table's, not a second copy"
        );
    }

    /// A box the pass did **not** elide keeps its own definition, so it must
    /// *not* be given a scalar source — its slot is written by the producer
    /// that is still there, and marking it scalar would make the collector's
    /// post-sweep scan skip a live reference.
    ///
    /// The control for the test above, and the one that matters for soundness
    /// rather than for fidelity.
    #[test]
    fn a_box_the_pass_kept_is_never_given_a_scalar_source() {
        let src = "fn f(a: Int, b: Int) -> Int { a + b + 1 }";
        let lowered = lower_src_to_mir(src);
        let func = lowered.function("f");
        let defined: BTreeSet<LocalId> = func
            .blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .flat_map(defs)
            .collect();
        for local in &func.locals {
            if local.kind == LocalKind::Gc && defined.contains(&local.id) {
                assert_eq!(
                    func.debug_scalar_source(local.id),
                    None,
                    "{local:?} still has a producer writing its slot"
                );
            }
        }
    }

    /// The `ConstGc` case (decision 4) hands part 2 the `Inst::ConstInt` that
    /// replaced the reload, which is a `Scalar` local like any other. Without
    /// this the immediate-forwarded literals would be the one shape that stayed
    /// `<uninit>` for no reason a reader could state.
    #[test]
    fn an_elided_small_int_literal_is_fed_by_the_const_int_that_replaced_it() {
        // `a + 7`: the `7` is in ADR-100's intern range, so its box is a
        // `ConstGc` and the reload becomes an immediate rather than a forward.
        let src = "fn f(a: Int) -> Int { a + 7 }";
        let lowered = lower_src_to_mir(src);
        let func = lowered.function("f");
        let start = u32::try_from(src.rfind('7').expect("in the source")).unwrap();
        let boxed = func
            .locals
            .iter()
            .find(|l| {
                l.kind == LocalKind::Gc
                    && func.debug_spans[l.id.0 as usize] == Some((start, start + 1))
            })
            .expect("the literal's box");
        assert!(
            func.debug_scalar_source(boxed.id).is_some(),
            "the `7` temp is renderable again: {:?}",
            func.debug_scalar_sources
        );
    }

    /// Arm A of ADR-120 part 2, asserted in a test rather than left to two
    /// binaries that might turn out to be identical.
    ///
    /// The forwarding still happens in both arms — this toggle changes what the
    /// *debugger* is told, not what the program computes — so the check is that
    /// the same function that has a scalar source in arm B has none in arm A.
    #[test]
    fn the_part_two_toggle_decides_whether_a_box_learns_its_scalar() {
        let lowered = lower_src_to_mir("fn f(a: Int, b: Int) -> Int { a + b + 1 }");
        let func = lowered.function("f");
        let linked = func
            .locals
            .iter()
            .filter(|l| func.debug_scalar_source(l.id).is_some())
            .count();
        if cfg!(feature = "adr120b-arm-a") {
            assert_eq!(linked, 0, "arm A links nothing");
        } else {
            assert!(linked > 0, "arm B links the elided box: {linked}");
        }
    }
}
