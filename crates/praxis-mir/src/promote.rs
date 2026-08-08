//! Whole-function `Gc` → `Scalar` slot promotion (ADR-121): the pass that stops
//! a scalar-typed binding from being a heap object at all.
//!
//! [`crate::forward`] deletes a box whose only reader is in its own block. What
//! it provably cannot touch is the shape handover 28 §1 named as what remains:
//! a **loop-carried assignment**, which lowers to a [`Inst::MoveGc`] into the
//! binding's existing slot. `collatz`'s `c` and `steps`, `mandelbrot`'s `x` and
//! `y`, and the `acc` of `dump.rs`'s canonical loop are all that shape, and each
//! costs one [`Inst::Materialize`] per iteration plus one guarded
//! [`Inst::ExtractScalar`] per *use* per iteration — `c` is read three times in
//! its loop body, in three different blocks.
//!
//! # What it does, which is not substitution
//!
//! ADR-120 decision 1 says loop-carried substitution needs dominance and
//! reaching definitions, which are among the four analyses ADR-108 declined to
//! build. That is true of *substitution* — replacing local `e` with local `s` at
//! the uses `s` happens to reach — and this pass does not do it.
//!
//! Instead it changes **what one slot holds**, uniformly, at every definition
//! and every use in the function. MIR is not SSA, so a `LocalId` names a slot
//! rather than a value; a total rewrite of one slot's representation therefore
//! asks no question about which definition reaches which use, because the answer
//! is the same for all of them. That is the same soundness argument
//! [`crate::provable`]'s header makes for its own flow-insensitivity, and it is
//! why this pass is a scan rather than a dataflow.
//!
//! # Where the kind comes from
//!
//! [`ProvableDescriptors`], unchanged and unextended. It already answers "which
//! [`DescriptorClass`] does *every* definition of this slot produce", over
//! exactly the four shapes where MIR wrote the descriptor down — and a slot
//! every definition of which produces `Int` is an `Int` slot. Three of its
//! properties are load-bearing here rather than convenient:
//!
//! - **A parameter is `Bottom`.** Its trap 1 seeds a definition-less local
//!   `Bottom` rather than letting a universal quantifier bless it vacuously, so
//!   parameters are excluded from promotion by construction rather than by a
//!   check here that could be forgotten. Promoting one would be an ABI change
//!   (a caller passes a `GcRef`), which is not this package.
//! - **A runtime result is `Bottom`.** A `Vec[Int]` element read through
//!   `praxis_vec_get` has no proof, so no `MirType` is consulted and no front-end
//!   guarantee is believed — the premise handover 26 §4 refuted.
//! - **It is a greatest fixpoint**, so a loop variable whose back edge assigns it
//!   from another loop variable resolves to its class instead of collapsing. That
//!   is the shape this pass exists for; a pessimistic analysis would find nothing.
//!
//! # Why `Byte` and `Char` are not promoted
//!
//! `Byte` has no descriptor at all ([`ScalarKind::BYTE_HAS_NO_WRAPPER`]), so
//! [`DescriptorClass::of_scalar`] answers `None` and there is nothing to prove.
//!
//! `Char` is excluded by a gate rather than by name, and the gate is the one
//! [`crate::forward`] draws in the same place: **no definition of a promoted slot
//! may fault.** `praxis_alloc_char` validates its Unicode scalar, so ADR-088 puts
//! a [`Inst::CheckFault`] immediately after every `Alloc { Char }` and
//! `Materialize { Char }`; replacing one with a [`Inst::MoveScalar`] would orphan
//! that check and `verify::check_fault_observed` would refuse the function. The
//! same asymmetry runs the other way at a *use*: re-boxing a promoted `Char`
//! needs a faulting `Materialize` and therefore a check this pass would have to
//! synthesize. Both doors are shut by one rule, which is why the rule is written
//! as "cannot fault" and not as "not `Char`" — a future non-validating `Char`
//! constructor opens both without editing this.
//!
//! # `Float`, and what it costs
//!
//! `Float` is promoted, and that is a decision with an observable consequence
//! recorded in ADR-121 rather than netted away. `DynamicKey::eq` opens with a
//! pointer-identity fast path, which is reflexive where the type's own `equals`
//! is not: NaN. A `Float` that was one shared box and is now materialized afresh
//! at each use therefore stops deduplicating as a `Map`/`Set`/`Counter` key. The
//! language is *already* inconsistent here — two NaNs written as two expressions
//! do not deduplicate today and two reads of one binding do — and this pass moves
//! the second spelling onto the first's answer, which is what `float_equals`
//! says and what IEEE-754 says.
//!
//! **It reaches much less far than that makes it sound, and the reason is the
//! profitability rule.** A `Float` bound and then used as a key is exactly the
//! shape [`worth_it`] declines: one box removed at the definition against one
//! added at every `insert`. It takes a `Float` that is *also* worth promoting —
//! loop-carried arithmetic — for the answer to move at all.
//! `crates/praxis-cli/tests/run.rs`'s
//! `a_nan_key_deduplicates_or_not_depending_on_whether_its_slot_was_promoted`
//! is both sides of that boundary in one program, so the reach is a test rather
//! than a paragraph.
//!
//! **The alternative was considered and declined.** Gating the fast path on
//! whether the descriptor's `equals` is reflexive — true for every type but
//! `Float` — would make the boxing genuinely unobservable rather than
//! unobservable-in-practice, at one load and one predictable branch per key
//! comparison. Declined as out of scope for a performance package: a `Float` map
//! key is a footgun in every language that permits one (Rust does not: `f64`
//! implements neither `Eq` nor `Hash`, for this exact reason), and the narrower
//! fix — refusing `Float` as a `CapKind::HashStable` type, which
//! `praxis_hir::capability` is already shaped for — is a language decision with
//! its own ADR rather than a line in this one.
//!
//! # What it leaves behind
//!
//! The promoted slot's `Gc` local **stays in the table**, undefined and unused,
//! and keeps its name, its `symbol_id` and its static type. That is
//! [`crate::forward`]'s shape exactly, and it is what makes the debugger's half
//! free: [`Function::debug_scalar_sources`] points the `Gc` local's debug slot at
//! the `Scalar` local that now holds its word, the backend's ADR-120-part-2 path
//! stores that word at every definition, and `DebugSlotKind` decodes it on the
//! way out. A promoted `var` renders in a crash snapshot exactly as it did.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::annot::{DebugSlots, RootSlots};
use crate::ir::{
    AllocKind, Function, GcConst, Inst, LocalDebugKind, LocalId, LocalKind, MirType, ScalarKind,
    Terminator,
};
use crate::liveness::{successors, term_uses, uses, uses_mut};
use crate::provable::{DescriptorClass, ProvableDescriptors};
use crate::verify::defines;

/// What one [`Inst::Materialize`] costs, in machine instructions, as the
/// profitability rule prices it.
///
/// Read off the tree rather than guessed: ADR-113's inline intern site is a
/// pacing branch, an unsigned range test, a table read and an out-of-line call
/// on the miss, and `PRAXIS_DUMP_VCODE` over `dump.rs`'s canonical loop puts the
/// whole sequence at roughly this. It does not need to be exact. It needs to be
/// large relative to [`EXTRACT_COST`] and small relative to [`LOOP_WEIGHT`],
/// which is what makes the rule's *decisions* insensitive to it.
const MATERIALIZE_COST: i64 = 20;

/// What one [`Inst::ExtractScalar`] costs. ADR-102 made a payload read a guard
/// rather than a load: the object's descriptor, the context's, a compare, a
/// branch, and then the payload — five, plus the cold arm.
const EXTRACT_COST: i64 = 6;

/// What one [`Inst::ConstGc`] costs against the [`Inst::ConstInt`] that replaces
/// it: two loads (the table base out of the context, then the element) against
/// an immediate.
const CONST_GC_COST: i64 = 2;

/// How much more an instruction inside a cycle is worth than one outside it.
///
/// **The rule is worthless without this term and that is the whole reason it
/// exists.** `var acc` summed in a loop and printed once after it removes one
/// box per iteration and adds one box at the `out(acc)`; counted statically that
/// is one against one, and the pass would decline the single most valuable
/// promotion in the language. Ten is a round number well clear of the static
/// counts any real body produces, not a trip-count estimate — see
/// [`block_weights`] for what "inside a cycle" is allowed to mean.
const LOOP_WEIGHT: i64 = 10;

/// Promote every `Gc` slot that provably holds a promotable scalar and pays for
/// itself, and answer how many were promoted.
///
/// Idempotent: a second run finds nothing, because a promoted slot is no longer
/// a [`LocalKind::Gc`] candidate and the `Gc` local it left behind has no
/// definitions and so no provable class.
pub fn promote_scalars(func: &mut Function) -> usize {
    // ADR-121's measurement arm A, and [`crate::forward`]'s toggle exactly: with
    // the feature on the pass is a no-op and everything else in the package —
    // `Inst::MoveScalar`, the verifier's rule, the backend's arm, the tests —
    // compiles unchanged, so the two binaries differ in this transform alone.
    if cfg!(feature = "adr121-arm-a") {
        return 0;
    }

    let legal = legal_candidates(func);
    if legal.is_empty() {
        return 0;
    }
    let weights = block_weights(func);
    let chosen = worth_it(func, legal, &weights);
    if chosen.is_empty() {
        return 0;
    }
    rewrite(func, &chosen);
    chosen.len()
}

/// The [`ScalarKind`] a promotable class carries, or `None` for a class this
/// pass does not promote.
///
/// `Unit`, `Text` and every composite have no scalar representation at all.
/// `Char` is absent here as well as behind the fault gate, so that the two
/// reasons stay separate: this one is "the payload does not ride the scalar
/// channel usefully", and that one is "its constructor validates". Removing
/// either alone must not promote a `Char`.
fn promotable_kind(class: DescriptorClass) -> Option<ScalarKind> {
    match class {
        DescriptorClass::Int => Some(ScalarKind::Int),
        DescriptorClass::Bool => Some(ScalarKind::Bool),
        DescriptorClass::Float => Some(ScalarKind::Float),
        DescriptorClass::Char
        | DescriptorClass::Unit
        | DescriptorClass::Text
        | DescriptorClass::Record
        | DescriptorClass::Tuple
        | DescriptorClass::Enum
        | DescriptorClass::Closure
        | DescriptorClass::Collection => None,
    }
}

/// Every `Gc` local this pass *may* promote, with the kind its slot holds.
///
/// Legality only — [`worth_it`] decides whether promoting one pays. The gates
/// are independent of which other locals are promoted, so this needs no
/// fixpoint; the profitability rule does.
fn legal_candidates(func: &Function) -> BTreeMap<LocalId, ScalarKind> {
    let provable = ProvableDescriptors::of(func);
    let mut out: BTreeMap<LocalId, ScalarKind> = BTreeMap::new();

    for local in &func.locals {
        if local.kind != LocalKind::Gc {
            continue;
        }
        let Some(kind) = provable.class(local.id).and_then(promotable_kind) else {
            continue;
        };
        // The return slot must stay a reference: `Terminator::Return` yields a
        // `GcRef` by ABI, and `verify`'s `ReturnIsNotGc` says so. A *use* at a
        // `Return` is priced by `worth_it` and materialized by `rewrite`; the
        // return slot itself is excluded, because promoting it would mean
        // rewriting the terminator's own operand at every exit rather than the
        // value flowing into it, for no gain a loop can see.
        if local.id == func.return_local {
            continue;
        }
        // Redundant with `ProvableDescriptors`' trap 1 — a parameter has no
        // defining instruction and is seeded `Bottom` — and kept because the two
        // say different things. That says "there is no proof"; this says "even
        // with a proof, the caller wrote a `GcRef` into this slot and no rewrite
        // here can change that". A future analysis that could prove a parameter's
        // class must not thereby start promoting one.
        if func.params.contains(&local.id) {
            continue;
        }
        out.insert(local.id, kind);
    }

    for block in &func.blocks {
        for inst in &block.insts {
            if let Some(dst) = defines(inst) {
                if out
                    .get(&dst)
                    .is_some_and(|&kind| !def_is_rewritable(inst, kind))
                {
                    out.remove(&dst);
                }
            }
            match inst {
                // `StoreScalar` writes a payload *into* the object named by
                // `dst_gc`. A promoted slot has no object to write into, and
                // materializing one would hand the store a box nothing else can
                // see — a write that silently does nothing. The instruction has
                // no builder site today (ADR-100's module doc records the
                // backend's arm as a documented no-op), so this gate is expected
                // to be dead; it is here because "expected to be dead" and
                // "cannot happen" are different, and the failure mode is a lost
                // write rather than a crash.
                Inst::StoreScalar { dst_gc, .. } => {
                    out.remove(dst_gc);
                }
                // A width pun: `verify`'s ADR-122 rule refuses a *proved*
                // contradiction, so this should be unreachable for a local whose
                // class is proved at all. Refusing rather than trusting keeps
                // `rewrite`'s `MoveScalar` from being handed two slots of
                // different widths, which is what `forward`'s gate 4 refuses for
                // the same reason.
                Inst::ExtractScalar { src, scalar, .. }
                    if out.get(src).is_some_and(|kind| kind != scalar) =>
                {
                    out.remove(src);
                }
                _ => {}
            }
        }
    }

    out
}

/// Whether a definition of a promoted slot can be rewritten to produce the raw
/// word instead of a box.
///
/// Every arm that answers `true` has a case in [`rewrite`] and vice versa; the
/// two are one decision written twice because one is a filter and the other a
/// transform, and a shape allowed here with no case there is a `_` arm that
/// silently drops a definition.
fn def_is_rewritable(inst: &Inst, kind: ScalarKind) -> bool {
    // The fault gate. See the module header: it is what excludes `Char` through
    // both `Alloc` and `Materialize`, and it is drawn around the *fault* so that
    // a constructor which stops validating stops being excluded.
    if inst.can_fault() {
        return false;
    }
    match inst {
        Inst::Materialize { scalar, .. } => *scalar == kind,
        Inst::Alloc { alloc, .. } => matches!(
            (alloc, kind),
            (AllocKind::Int { .. }, ScalarKind::Int)
                | (AllocKind::Bool { .. }, ScalarKind::Bool)
                | (AllocKind::Float { .. }, ScalarKind::Float)
        ),
        Inst::ConstGc { konst, .. } => matches!(
            (konst, kind),
            (GcConst::SmallInt(_), ScalarKind::Int) | (GcConst::Bool(_), ScalarKind::Bool)
        ),
        // Always rewritable, in one of two ways: a copy from another promoted
        // slot is a `MoveScalar`, and a copy from a slot that stayed a reference
        // is the `ExtractScalar` that reference always needed. The second is
        // sound without further proof — `src`'s class must be `kind` too, or the
        // meet over `dst`'s definitions could not have been `Known(kind)`.
        Inst::MoveGc { .. } => true,
        _ => false,
    }
}

/// Per-block weight: [`LOOP_WEIGHT`] for a block on a cycle, 1 otherwise.
///
/// "On a cycle" is computed the blunt way — a search from each block's
/// successors, asking whether the block is reachable from itself — rather than
/// by finding loops. Two consequences are worth stating because the rule is read
/// as if it were a loop-depth analysis and is not:
///
/// - **Nesting is invisible.** `mandelbrot`'s three nested `while`s are one
///   cyclic region, so a block in the innermost loop weighs the same as one in
///   the outermost. That under-values the inner loop and never over-values it,
///   and the decision it feeds — promote or not — is not close enough in any
///   shape the suite contains for the difference to change an answer.
/// - **Irreducible flow is handled by not being special.** Reachability asks
///   nothing about headers or back edges, so a CFG no loop-finder would accept
///   still gets sensible weights.
///
/// O(V·(V+E)) with no worklist. A whole benchmark entry point is a few hundred
/// blocks, so this is microseconds; it is written for a reader to check rather
/// than for a compiler to hurry through.
fn block_weights(func: &Function) -> Vec<i64> {
    let n = func.blocks.len();
    let succs: Vec<Vec<usize>> = func
        .blocks
        .iter()
        .map(|b| {
            successors(&b.term)
                .into_iter()
                .map(|s| s.0 as usize)
                .filter(|&s| s < n)
                .collect()
        })
        .collect();

    (0..n)
        .map(|start| {
            let mut seen = vec![false; n];
            let mut queue: VecDeque<usize> = succs[start].iter().copied().collect();
            while let Some(b) = queue.pop_front() {
                if b == start {
                    return LOOP_WEIGHT;
                }
                if std::mem::replace(&mut seen[b], true) {
                    continue;
                }
                queue.extend(succs[b].iter().copied());
            }
            1
        })
        .collect()
}

/// Narrow the legal set to the slots whose promotion pays for itself, **deciding
/// each copy-connected group as one**.
///
/// # Why a group and not a local
///
/// The builder does not put a value in one slot. `var m = if c > 3 { … } else
/// { … }` lowers to a result temp assigned by a `MoveGc` in each arm and a
/// second `MoveGc` into `m`'s slot, and the value is boxed once, at whatever
/// finally consumes `m`. Scored one local at a time that chain unravels from the
/// far end: `m` alone sees one box added and nothing removed, so it is declined;
/// the temp that fed it then sees a copy to a *non*-promoted slot, so it is
/// declined; and so on backwards until nothing is promoted and the boxes have
/// merely moved. Both of ADR-121's first two failing tests were that, and the
/// symptom was a shape that ended one box short of correct rather than wrong.
///
/// A `MoveGc` between two candidates is a copy of one value, so the two slots
/// are promoted together or not at all. Union them and the group is scored as
/// what it is: the boxes its definitions stop writing, against the boxes its
/// escaping uses start writing.
///
/// # Why this needs no fixpoint, where the per-local rule did
///
/// The groups are independent. A `MoveGc` whose two ends are both candidates has
/// already unioned them, so no edge crosses a group boundary with a candidate on
/// each side — and an edge to a *non*-candidate names a slot whose status this
/// function never changes. So one pass is a fixed point, and the decision for
/// one group cannot be invalidated by the decision for another.
///
/// The kinds agree by construction, which is why the union does not check them:
/// if `MoveGc { dst, src }` has both ends proved, `dst`'s class was met over a
/// `CopyOf(src)`, so `src` proves the same class.
fn worth_it(
    func: &Function,
    set: BTreeMap<LocalId, ScalarKind>,
    weights: &[i64],
) -> BTreeMap<LocalId, ScalarKind> {
    let group = copy_groups(func, &set);
    let per_local = score(func, &set, weights);

    let mut per_group: BTreeMap<LocalId, i64> = BTreeMap::new();
    for local in set.keys() {
        *per_group.entry(group[local]).or_default() += per_local[local];
    }

    set.into_iter()
        .filter(|(local, _)| per_group[&group[local]] > 0)
        .collect()
}

/// Each candidate's copy group, as a map from the local to its group's
/// representative. Union-find over the `MoveGc` edges that join two candidates.
fn copy_groups(func: &Function, set: &BTreeMap<LocalId, ScalarKind>) -> BTreeMap<LocalId, LocalId> {
    let mut parent: BTreeMap<LocalId, LocalId> = set.keys().map(|&l| (l, l)).collect();

    for block in &func.blocks {
        for inst in &block.insts {
            let Inst::MoveGc { dst, src } = inst else {
                continue;
            };
            if !parent.contains_key(dst) || !parent.contains_key(src) {
                continue;
            }
            let (a, b) = (find(&mut parent, *dst), find(&mut parent, *src));
            if a != b {
                // Union by `LocalId` order rather than by rank: the groups are
                // tiny (a copy chain the builder emits is a handful of slots),
                // and a deterministic representative keeps `worth_it`'s answer
                // independent of block order.
                let (lo, hi) = if a < b { (a, b) } else { (b, a) };
                parent.insert(hi, lo);
            }
        }
    }

    set.keys().map(|&l| (l, find(&mut parent, l))).collect()
}

/// The representative of `x`'s group, compressing the path behind it.
fn find(parent: &mut BTreeMap<LocalId, LocalId>, x: LocalId) -> LocalId {
    let mut root = x;
    while parent[&root] != root {
        root = parent[&root];
    }
    let mut cur = x;
    while parent[&cur] != root {
        let next = parent[&cur];
        parent.insert(cur, root);
        cur = next;
    }
    root
}

/// Machine instructions saved, per candidate, if `set` is promoted as a whole.
///
/// Positive is a win. The one deliberate imprecision: an instruction naming the
/// same promoted local twice — `f(x, x)` — is charged two materializations where
/// [`rewrite`] emits one. That over-states the cost and so can only decline a
/// promotion, never accept a bad one, which is the direction an approximation
/// here is allowed to be wrong in.
fn score(
    func: &Function,
    set: &BTreeMap<LocalId, ScalarKind>,
    weights: &[i64],
) -> BTreeMap<LocalId, i64> {
    let mut score: BTreeMap<LocalId, i64> = set.keys().map(|&l| (l, 0)).collect();
    let mut add = |local: LocalId, delta: i64| {
        if let Some(slot) = score.get_mut(&local) {
            *slot += delta;
        }
    };

    for (bi, block) in func.blocks.iter().enumerate() {
        let w = weights[bi];
        for inst in &block.insts {
            if let Some(dst) = defines(inst) {
                if set.contains_key(&dst) {
                    add(
                        dst,
                        w * match inst {
                            // The box this pass exists to delete.
                            Inst::Materialize { .. } | Inst::Alloc { .. } => MATERIALIZE_COST,
                            Inst::ConstGc { .. } => CONST_GC_COST,
                            // A copy stays a copy between two promoted slots,
                            // and becomes a guarded reload when the source
                            // stayed a reference.
                            Inst::MoveGc { src, .. } => {
                                if set.contains_key(src) {
                                    0
                                } else {
                                    -EXTRACT_COST
                                }
                            }
                            _ => 0,
                        },
                    );
                }
            }
            for u in uses(inst) {
                if !set.contains_key(&u) {
                    continue;
                }
                add(
                    u,
                    w * match inst {
                        // The reload disappears: the consumer reads the word.
                        Inst::ExtractScalar { .. } => EXTRACT_COST,
                        // A free reference copy becomes an allocation, unless
                        // the destination is promoted too.
                        Inst::MoveGc { dst, .. } => {
                            if set.contains_key(dst) {
                                0
                            } else {
                                -MATERIALIZE_COST
                            }
                        }
                        // Everything else genuinely wants an object.
                        _ => -MATERIALIZE_COST,
                    },
                );
            }
        }
        // A `Return` wants an object; a `Branch` reads a `Scalar` already and so
        // never names a promoted local.
        for u in term_uses(&block.term) {
            if set.contains_key(&u) {
                add(u, -w * MATERIALIZE_COST);
            }
        }
    }
    score
}

/// Apply the promotion: mint a `Scalar` twin per chosen slot, then rewrite every
/// definition and every use in the function.
fn rewrite(func: &mut Function, set: &BTreeMap<LocalId, ScalarKind>) {
    // The twin, and the debugger hand-off. The `Gc` local stays in the table
    // holding the name, the `symbol_id` and the static type; the `Scalar` local
    // holds the word. `debug_scalar_sources` is the link the backend reads, and
    // this is the only point in the compiler that knows the two are one value.
    let mut twin: BTreeMap<LocalId, LocalId> = BTreeMap::new();
    for (&boxed, &kind) in set {
        let span = func.debug_spans[boxed.0 as usize];
        let scalar = func.new_local(
            LocalKind::Scalar(kind),
            // `ir.rs`'s doctrine: a `Scalar` local's `ScalarKind` is
            // authoritative and its `MirType` is always `Opaque`. The static type
            // stays on the `Gc` local, which is what the debugger reads.
            MirType::Opaque,
            None,
            LocalDebugKind::Temp,
            span,
        );
        twin.insert(boxed, scalar);
        func.debug_scalar_sources[boxed.0 as usize] = Some(scalar);
    }

    for bi in 0..func.blocks.len() {
        // Taken out by value so `func` is free for `new_local` below; the block's
        // terminator stays in place and is patched after.
        let old = std::mem::take(&mut func.blocks[bi].insts);
        let mut out: Vec<Inst> = Vec::with_capacity(old.len());
        // One re-boxing slot per promoted local per block, created on demand and
        // reused. Reuse is safe because every box this pass emits is consumed by
        // the very next instruction, so no two live ranges overlap — and it keeps
        // the slot growth ADR-128 colours proportional to the promoted locals a
        // block re-boxes rather than to how often it re-boxes them.
        let mut reboxed: BTreeMap<LocalId, LocalId> = BTreeMap::new();

        for inst in old {
            rewrite_inst(func, &mut out, inst, set, &twin, &mut reboxed);
        }

        // `Terminator::Return` yields a `GcRef` by ABI, so a promoted value
        // returned here is boxed at the exit — once per call, never in a loop.
        if let Terminator::Return { value } = func.blocks[bi].term {
            if let Some(&kind) = set.get(&value) {
                let boxed = rebox_slot(func, &mut reboxed, value);
                out.push(Inst::Materialize {
                    dst: boxed,
                    src: twin[&value],
                    scalar: kind,
                    roots: RootSlots::unannotated(),
                    debug: DebugSlots::unannotated(),
                });
                func.blocks[bi].term = Terminator::Return { value: boxed };
            }
        }

        func.blocks[bi].insts = out;
    }
}

/// The `Gc` slot this block re-boxes `promoted` into, created on first use.
fn rebox_slot(
    func: &mut Function,
    reboxed: &mut BTreeMap<LocalId, LocalId>,
    promoted: LocalId,
) -> LocalId {
    if let Some(&slot) = reboxed.get(&promoted) {
        return slot;
    }
    // The static type and span are the promoted local's own, so the debugger's
    // type column and `@ "expr"` provenance survive re-boxing.
    let ty = func.locals[promoted.0 as usize].ty;
    let span = func.debug_spans[promoted.0 as usize];
    let slot = func.new_local(LocalKind::Gc, ty, None, LocalDebugKind::Temp, span);
    reboxed.insert(promoted, slot);
    slot
}

/// Rewrite one instruction, pushing whatever it becomes onto `out`.
fn rewrite_inst(
    func: &mut Function,
    out: &mut Vec<Inst>,
    mut inst: Inst,
    set: &BTreeMap<LocalId, ScalarKind>,
    twin: &BTreeMap<LocalId, LocalId>,
    reboxed: &mut BTreeMap<LocalId, LocalId>,
) {
    // --- definitions of a promoted slot: the box is not written at all --------
    //
    // Every arm here mirrors one that answered `true` in `def_is_rewritable`.
    if let Some(dst) = defines(&inst) {
        if let Some(&kind) = set.get(&dst) {
            let scalar = twin[&dst];
            match &inst {
                Inst::Materialize { src, .. } => {
                    out.push(Inst::MoveScalar {
                        dst: scalar,
                        src: *src,
                        kind,
                    });
                    return;
                }
                Inst::Alloc {
                    alloc:
                        AllocKind::Int { value }
                        | AllocKind::Bool { value }
                        | AllocKind::Float { value },
                    ..
                } => {
                    out.push(Inst::MoveScalar {
                        dst: scalar,
                        src: *value,
                        kind,
                    });
                    return;
                }
                Inst::ConstGc { konst, .. } => {
                    // `ConstInt` is the scalar channel's immediate loader for
                    // every width, not only `Int` — `crate::forward`'s
                    // `How::Immediate` already replaces a `Bool` and a `Char`
                    // reload with one. `GcConst` has no `Float` variant, so the
                    // two arms below are exhaustive over what
                    // `def_is_rewritable` admits.
                    let value = match konst {
                        GcConst::SmallInt(n) => *n,
                        GcConst::Bool(b) => i64::from(*b),
                        GcConst::Unit | GcConst::Char(_) => unreachable!(
                            "def_is_rewritable admits only SmallInt and Bool constants"
                        ),
                    };
                    out.push(Inst::ConstInt { dst: scalar, value });
                    return;
                }
                Inst::MoveGc { src, .. } => {
                    match twin.get(src) {
                        // Both sides promoted: a word moves between two slots.
                        Some(&src_scalar) => out.push(Inst::MoveScalar {
                            dst: scalar,
                            src: src_scalar,
                            kind,
                        }),
                        // The source stayed a reference, so this is the reload it
                        // always needed. Sound by the meet: `src`'s class is
                        // `kind`, or `dst`'s could not have been.
                        None => out.push(Inst::ExtractScalar {
                            dst: scalar,
                            src: *src,
                            scalar: kind,
                        }),
                    }
                    return;
                }
                _ => unreachable!("def_is_rewritable admits no other definition shape"),
            }
        }
    }

    // --- uses of a promoted slot ---------------------------------------------
    match &inst {
        // The reload the consumer wanted is now a move between scalar slots,
        // which Cranelift's copy propagation removes outright.
        Inst::ExtractScalar { dst, src, scalar } if set.contains_key(src) => {
            out.push(Inst::MoveScalar {
                dst: *dst,
                src: twin[src],
                kind: *scalar,
            });
            return;
        }
        // A copy *out* of a promoted slot into one that stayed a reference. The
        // destination-promoted case was handled above, so this is the box.
        Inst::MoveGc { dst, src } if set.contains_key(src) => {
            out.push(Inst::Materialize {
                dst: *dst,
                src: twin[src],
                scalar: set[src],
                roots: RootSlots::unannotated(),
                debug: DebugSlots::unannotated(),
            });
            return;
        }
        _ => {}
    }

    // Everything else genuinely wants an object: box each promoted local this
    // instruction still reads, immediately before it, and point the operand at
    // the box. `uses_mut` is exhaustive over `Inst`, which is what makes "point
    // the operand at the box" total rather than best-effort.
    let needed: BTreeSet<LocalId> = uses(&inst)
        .into_iter()
        .filter(|l| set.contains_key(l))
        .collect();
    if !needed.is_empty() {
        let mut boxes: BTreeMap<LocalId, LocalId> = BTreeMap::new();
        for local in needed {
            let slot = rebox_slot(func, reboxed, local);
            out.push(Inst::Materialize {
                dst: slot,
                src: twin[&local],
                scalar: set[&local],
                roots: RootSlots::unannotated(),
                debug: DebugSlots::unannotated(),
            });
            boxes.insert(local, slot);
        }
        for field in uses_mut(&mut inst) {
            if let Some(&slot) = boxes.get(field) {
                *field = slot;
            }
        }
    }
    out.push(inst);
}

#[cfg(test)]
mod tests {
    //! What promotion does, what it refuses, and what it must not cost the
    //! debugger.
    //!
    //! Written against **lowered source** rather than hand-built MIR wherever
    //! the question is "does this shape get promoted", because the shapes this
    //! pass targets are properties of what [`crate::build`] emits — a
    //! loop-carried assignment is a `MoveGc` into a binding's existing slot, and
    //! a test that constructs one by hand asserts about its own MIR rather than
    //! about the compiler's.

    use super::*;
    use crate::test_support::{lower_src_to_mir, Census, InstKind};

    /// Every instruction in a function, as a census.
    fn census(func: &Function) -> Census {
        Census::of(func.blocks.iter().flat_map(|b| &b.insts))
    }

    /// Whether `name`'s binding slot was promoted — it is still a `Gc` local and
    /// it names the `Scalar` local now holding its word.
    fn promoted(func: &Function, name: &str) -> bool {
        func.locals.iter().any(|l| {
            func.debug_name(l.id) == Some(name) && func.debug_scalar_source(l.id).is_some()
        })
    }

    /// The loop `dump.rs` quotes every instruction count in the tree against.
    const CANONICAL_LOOP: &str = "\
var i = 0
var acc = 0
var limit = 10
while i < limit {
    acc = acc + i * 3
    i = i + 1
}
out(acc)
";

    #[test]
    fn the_canonical_loop_keeps_no_int_box_in_its_body() {
        let lowered = lower_src_to_mir(CANONICAL_LOOP);
        let c = census(lowered.entry());
        // `out(acc)` still wants an object, and nothing else does: one
        // materialization, outside the loop. Before this pass the same function
        // materialized once per assignment per iteration.
        assert_eq!(
            c.count(InstKind::Materialize(ScalarKind::Int)),
            1,
            "only `out(acc)`'s argument is boxed: {c:?}"
        );
        // And the guarded reloads go with them: there is no object to read a
        // payload out of.
        assert_eq!(
            c.count(InstKind::ExtractScalar(ScalarKind::Int)),
            0,
            "no guarded payload read survives: {c:?}"
        );
    }

    #[test]
    fn a_promoted_binding_keeps_its_box_in_the_table_for_the_debugger() {
        let lowered = lower_src_to_mir(CANONICAL_LOOP);
        let func = lowered.entry();

        // `acc` is still in the local table, still a `Gc` local, still named —
        // that is what carries the `symbol_id` and the static type into the
        // backend's debug metadata (ADR-120 decision 7's shape, reused).
        let acc = func
            .locals
            .iter()
            .find(|l| func.debug_name(l.id) == Some("acc"))
            .expect("`acc`'s binding slot is still in the table");
        assert_eq!(
            acc.kind,
            LocalKind::Gc,
            "the debug identity stays a Gc local"
        );

        let (scalar, kind) = func
            .debug_scalar_source(acc.id)
            .expect("a promoted binding names the scalar holding its value");
        assert_eq!(kind, ScalarKind::Int);
        assert_eq!(
            func.locals[scalar.0 as usize].kind,
            LocalKind::Scalar(ScalarKind::Int),
            "and the accessor's kind is the local table's, not a second copy"
        );

        let written = func
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(defines)
            .any(|d| d == acc.id);
        assert!(!written, "the promoted box is written by nothing");
    }

    /// `mandelbrot`'s shape, reduced: two loop-carried `Float`s, each assigned
    /// from an expression over both. Handover 26 predicted W8-S1 takes that
    /// inner loop to zero float allocations; this is the claim as a test.
    #[test]
    fn a_loop_carried_float_is_promoted() {
        let src = "\
var x = 0.0
var y = 0.0
var i = 0
while i < 400 {
    var xt = x * x - y * y
    y = 2.0 * x * y
    x = xt
    i = i + 1
}
out(x)
";
        let lowered = lower_src_to_mir(src);
        let c = census(lowered.entry());
        assert_eq!(
            c.count(InstKind::Materialize(ScalarKind::Float)),
            1,
            "only `out(x)`'s argument is boxed: {c:?}"
        );
    }

    #[test]
    fn a_while_conditions_bool_is_promoted_out_of_existence() {
        let lowered = lower_src_to_mir(CANONICAL_LOOP);
        let c = census(lowered.entry());
        assert_eq!(
            c.count(InstKind::Materialize(ScalarKind::Bool)),
            0,
            "a condition is a branch, not an object: {c:?}"
        );
    }

    /// A value crossing a block boundary through an `if`-expression is exactly
    /// what [`crate::forward`] provably cannot reach — and it is not a loop, so
    /// it is the shape that shows this pass is whole-function rather than
    /// loop-scoped.
    #[test]
    fn an_if_expressions_result_is_promoted_across_its_arms() {
        let src = "\
var c = 7
var m = if c > 3 { c * 2 } else { c + 1 }
out(m)
";
        let lowered = lower_src_to_mir(src);
        let c = census(lowered.entry());
        assert_eq!(
            c.count(InstKind::Materialize(ScalarKind::Int)),
            1,
            "one box, for `out(m)`'s argument: {c:?}"
        );
    }

    #[test]
    fn every_move_scalar_joins_two_scalar_slots_of_its_own_width() {
        let lowered = lower_src_to_mir(CANONICAL_LOOP);
        let func = lowered.entry();
        let c = census(func);
        assert!(
            c.count(InstKind::MoveScalar(ScalarKind::Int)) > 0,
            "the loop-carried assignments are scalar moves now: {c:?}"
        );
        // `VerifyError::MoveScalarKindMismatch`'s rule, asserted from outside
        // the verifier as well as inside it.
        for inst in func.blocks.iter().flat_map(|b| &b.insts) {
            if let Inst::MoveScalar { dst, src, kind } = inst {
                assert_eq!(func.locals[dst.0 as usize].kind, LocalKind::Scalar(*kind));
                assert_eq!(func.locals[src.0 as usize].kind, LocalKind::Scalar(*kind));
            }
        }
    }

    /// A `Char` is refused, and by the fault gate rather than by name:
    /// `praxis_alloc_char` validates its Unicode scalar, so ADR-088 puts a
    /// `CheckFault` after every `Alloc`/`Materialize` of one and deleting the
    /// producer would orphan it.
    #[test]
    fn a_char_binding_is_not_promoted() {
        let src = "\
var c = 'a'
var n = 0
while n < 3 {
    c = 'b'
    n = n + 1
}
out(c)
";
        let lowered = lower_src_to_mir(src);
        assert!(
            !promoted(lowered.entry(), "c"),
            "a `Char` binding keeps its box"
        );
        // …and the `Int` beside it in the same loop still is, so the test is
        // pinning the gate rather than a lowering that promotes nothing.
        assert!(promoted(lowered.entry(), "n"), "the `Int` counter is");
    }

    /// The profitability rule's headline case, stated as the decision rather
    /// than as a count: `acc` is boxed once at `out(acc)` and unboxed once per
    /// iteration, so the *static* counts are one against one and only
    /// [`LOOP_WEIGHT`] decides it. Set that constant to 1 and this test fails.
    #[test]
    fn a_loop_accumulator_printed_once_is_still_worth_promoting() {
        let lowered = lower_src_to_mir(CANONICAL_LOOP);
        assert!(
            promoted(lowered.entry(), "acc"),
            "one box outside the loop does not outweigh one per iteration inside it"
        );
    }

    /// A binding that is only ever handed to something wanting an object is
    /// declined: promoting it would add a materialization at every use and
    /// remove one box.
    #[test]
    fn a_binding_with_nothing_but_boxed_uses_is_declined() {
        let src = "\
var n = 41 + 1
out(n)
out(n)
out(n)
";
        let lowered = lower_src_to_mir(src);
        assert!(
            !promoted(lowered.entry(), "n"),
            "three boxes added against one removed is not a win"
        );
    }

    #[test]
    fn the_pass_is_idempotent() {
        let mut lowered = lower_src_to_mir(CANONICAL_LOOP);
        let name = lowered.entry().name.clone();
        let func = lowered
            .funcs
            .iter_mut()
            .find(|f| f.name == name)
            .expect("the entry point");
        let before = census(func);
        let again = promote_scalars(func);
        assert_eq!(again, 0, "a second run finds nothing");
        assert_eq!(census(func), before, "and changes nothing");
    }

    /// Arm A of the measurement toggle, asserted in a test rather than left to
    /// two binaries that might turn out to be identical (ADR-120's precedent).
    #[test]
    fn the_measurement_toggle_decides_whether_the_pass_runs() {
        let lowered = lower_src_to_mir(CANONICAL_LOOP);
        let func = lowered.entry();
        if cfg!(feature = "adr121-arm-a") {
            // `forward_boxes` still runs and still links the boxes *it* elided,
            // so "nothing is linked" is the wrong assertion. The arms differ in
            // the loop-carried slots, which is what this count pins.
            let c = census(func);
            assert!(
                c.count(InstKind::Materialize(ScalarKind::Int)) > 1,
                "arm A keeps a box per assignment per iteration: {c:?}"
            );
        } else {
            assert!(promoted(func, "acc"), "arm B promotes the accumulator");
        }
    }
}
