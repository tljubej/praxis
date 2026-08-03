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
//! It also costs the debugger a value, and that is a known, measured regression
//! rather than a surprise: a temp whose producer is gone renders
//! `<tmp#N: Int> @ "a + b" = <uninit>` where it used to render `= 30`. Two
//! tests say so and both are **red on this branch on purpose**:
//! `crates/praxis-cli/tests/run.rs`'s
//! `a_forwarded_binop_temp_still_renders_the_value_it_materialized`, added by
//! this package because handover 26 believed an existing test covered it and
//! handover 27 §1 proved none did, and
//! `crates/praxis-codegen-cranelift/tests/jit.rs`'s
//! `a_temp_that_never_reached_a_shadow_slot_is_still_renderable`, which was
//! already there. ADR-120 part 2 (W8-S0b, a scalar debug slot) is what turns
//! both green **unedited**. The hand-off is [`carry_debug_metadata`].

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
    // `can_fault` is the gate that keeps `Char` out. `AllocChar` validates its
    // Unicode scalar, so its row is `AllocatesAndFaults` and ADR-088 puts a
    // `CheckFault` immediately after it; deleting the producer would orphan the
    // check and `verify::check_fault_observed` would refuse the function. It is
    // not a conservatism — it is the exact boundary.
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
    if let How::Operand { scalar } = site.how {
        carry_debug_metadata(func, site.boxed, scalar);
    }
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
    use crate::test_support::{lower_src_to_mir, Census, InstKind, Lowered};
    use crate::verify::verify;

    const INT_BOX: InstKind = InstKind::Materialize(ScalarKind::Int);
    const BOOL_BOX: InstKind = InstKind::Materialize(ScalarKind::Bool);
    const FLOAT_BOX: InstKind = InstKind::Materialize(ScalarKind::Float);
    const INT_LOAD: InstKind = InstKind::ExtractScalar(ScalarKind::Int);
    const BOOL_LOAD: InstKind = InstKind::ExtractScalar(ScalarKind::Bool);
    const FLOAT_LOAD: InstKind = InstKind::ExtractScalar(ScalarKind::Float);

    /// A benchmark's source, read from the tree rather than copied — a copy
    /// would go on asserting about a program the suite no longer runs.
    fn benchmark(name: &str) -> String {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.pop(); // crates/praxis-mir -> crates
        path.pop(); // crates -> the workspace root
        path.push(format!("benchmarks/praxis/{name}.px"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

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
    /// **Hand-built, because no source program reaches this shape.** ADR-107
    /// gives the language no char-literal syntax, so `Lit::Char` is synthesized
    /// by the input parser alone and `build.rs`'s comment at the `AllocKind::Char`
    /// site says so. A gate whose only witness is a program that cannot be
    /// written is still a gate the pass has to hold, and this is what holds it.
    #[test]
    fn a_char_box_is_not_forwarded_because_its_check_fault_would_be_orphaned() {
        use crate::ir::{LocalDebugKind, LocalKind, MirType};

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
        let scalar = |f: &mut Function| {
            f.new_local(
                LocalKind::Scalar(ScalarKind::Char),
                MirType::Opaque,
                None,
                LocalDebugKind::Temp,
                None,
            )
        };
        let code = scalar(&mut f);
        let reloaded = scalar(&mut f);
        let boxed = f.new_local(
            LocalKind::Gc,
            MirType::Opaque,
            None,
            LocalDebugKind::Temp,
            None,
        );
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
            "fn f(a: Int, c: Bool) -> Int {\n  let t = a * 2\n  \
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
        let mut lowered = lower_src_to_mir(&benchmark("mandelbrot"));
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
        let mut lowered = lower_src_to_mir(&benchmark("collatz"));
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
        for name in [
            "bfs",
            "collatz",
            "hashwork",
            "mandelbrot",
            "pipeline",
            "primes",
            "tree",
            "vm",
        ] {
            let mut lowered = lower_src_to_mir(&benchmark(name));
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
        let lowered = lower_src_to_mir(&benchmark("mandelbrot"));
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
}
