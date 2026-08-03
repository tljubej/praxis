//! Shape assertions over lowered MIR: source text in, a count of instructions
//! by variant out (handover 26 §2).
//!
//! Several packages in the post-M11 performance plan state their gate as a
//! count of emitted MIR — "`mandelbrot`'s inner loop goes from 10
//! `Materialize{Float}` to 2" — and for those a count is better evidence than a
//! stopwatch, because it does not drift with what else the machine is doing.
//! This module is that count, written once.
//!
//! **Why it is a feature and not a `#[cfg(test)] mod tests` helper.** The
//! consumers are in two crates: a MIR pass tests its own before and after here,
//! and the Cranelift backend's tests want the same numbers for the code they
//! generate from. A `#[cfg(test)]` item is compiled into the test binary of
//! *this* crate only and is invisible to any other, so the second consumer
//! would have to grow a parallel copy — two answers to "how many float boxes
//! are in that loop", drifting from the first day they disagree. The
//! `test-support` feature makes one answer reachable from both. It is off by
//! default and takes the front end as an optional dependency, so a plain
//! `cargo build` of the compiler compiles none of this and puts no lexer or
//! parser in the MIR crate's graph — `praxis-mir`'s production job starts at
//! typed HIR.

use std::collections::{BTreeMap, BTreeSet};

use praxis_ast::AstNode;
use praxis_hir::{analyze_root, lower, mono::monomorphize, Analysis};
use praxis_parser::parse;
use praxis_source::SourceMap;
use praxis_stdlib::abi::RuntimeSymbol;

use crate::build::lower_module;
use crate::ir::{AllocKind, BlockId, Function, Inst, ScalarKind};
use crate::liveness::{defs, successors};

// ---------------------------------------------------------------------------
// Lowering a program
// ---------------------------------------------------------------------------

/// A program lowered all the way to MIR, with the source it came from.
///
/// The source is kept because it is how a block gets a *name* — see
/// [`Lowered::block_over`].
#[derive(Debug)]
pub struct Lowered {
    /// The source text as lowered.
    pub src: String,
    /// Every function the module produced, in `lower_module` order: the source
    /// `fn`s first, then one synthetic function per closure literal, then one
    /// adapter per function value.
    pub funcs: Vec<Function>,
    /// The front end's analysis, whose `db` renders a `Type` in a failure
    /// message.
    pub analysis: Analysis,
}

/// Lower Praxis source text to MIR, refusing anything the front end complains
/// about.
///
/// **The pipeline is the host's, monomorphization included.** `run::run`, the
/// debugger's reload and the backend's own integration tests all call
/// `mono::monomorphize` between `lower` and `lower_module`; a helper that
/// skipped it would count instructions in functions the JIT never compiles and
/// miss the clones it does. That was the one divergence between this and the
/// per-crate copies it replaces.
///
/// It stops **before** [`crate::annotate`], because that is where the passes
/// that read it run: a pass that deletes a safepoint has to run before the root
/// sets are computed per safepoint, so its tests need MIR in the state it sees.
///
/// # Panics
/// On any parse, analysis, or lowering diagnostic — a shape assertion is about
/// a program that compiles, and a helper that lowered a broken one would count
/// whatever the recovery path emitted.
#[must_use]
pub fn lower_src_to_mir(src: &str) -> Lowered {
    let map = SourceMap::new();
    let file = map.intern("shape_test.px", src);
    let parsed = parse(file, src);
    assert!(
        parsed.diagnostics.is_empty(),
        "parse diagnostics: {:?}",
        parsed.diagnostics
    );
    let mut analysis = analyze_root(file, &parsed.tree);
    assert!(
        analysis.diagnostics.is_empty(),
        "analysis diagnostics: {:?}",
        analysis.diagnostics
    );
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
    let module = lower(file, &root, &mut analysis);
    assert!(
        module.diagnostics.is_empty(),
        "lowering diagnostics: {:?}",
        module.diagnostics
    );
    let module = monomorphize(module, &analysis.names, &mut analysis.db);
    let funcs = lower_module(&module, &mut analysis.db);
    Lowered {
        src: src.to_string(),
        funcs,
        analysis,
    }
}

impl Lowered {
    /// The function the host would execute: the synthetic holder of the file's
    /// top-level statements, or a declared `fn main`.
    ///
    /// Asks [`praxis_hir::entry_point`], the same question `run` and the
    /// debugger's reload ask, so a benchmark program's entry point here is the
    /// one that actually runs.
    ///
    /// # Panics
    /// When the module has neither.
    #[must_use]
    pub fn entry(&self) -> &Function {
        let name = praxis_hir::entry_point(|n| self.funcs.iter().any(|f| f.name == n))
            .expect("the module has no entry point: no top-level statements and no `fn main`");
        self.function(name)
    }

    /// The function of that name.
    ///
    /// # Panics
    /// When no function has it, naming the ones that do.
    #[must_use]
    pub fn function(&self, name: &str) -> &Function {
        self.funcs
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| {
                let names: Vec<&str> = self.funcs.iter().map(|f| f.name.as_str()).collect();
                panic!("no function named `{name}`; the module has {names:?}")
            })
    }

    /// The block of `func` that lowers `needle` — a snippet of the source text.
    ///
    /// **This is how a block gets named, and the naming is the whole value of
    /// the helper.** A gate phrased "the inner loop emits two
    /// `Materialize{Float}`" has to survive the transform it is gating, and
    /// every one of those transforms adds or removes blocks: a `blocks[9]`
    /// written against today's numbering asserts about a different block
    /// tomorrow, and it does so *silently*, because an index is in range for
    /// some block whatever the builder does. A source snippet is what the reader
    /// of the test already believes the block is, and it moves only when the
    /// program does.
    ///
    /// **`needle` names an expression**, and the answer is the block holding the
    /// local that materializes it: of every local whose debugger span — the same
    /// provenance the crash debugger prints as `@ "expr"` — encloses the whole
    /// snippet, the one with the *narrowest* span. Narrowest, and per local
    /// rather than per block, because both looser readings pick the wrong block
    /// here. A user binding carries its *declaration*'s span, so the hull of a
    /// block's spans reaches back to the top of the function and a block that
    /// merely follows the loop can enclose it more tightly than the loop body
    /// does; and an enclosing span is not a lowering — `x * x` encloses nothing
    /// of `x * x - y * y`, but `x * x - y * y + x0` encloses both.
    ///
    /// # Panics
    /// When `needle` is absent from the source, occurs more than once, or no
    /// local of `func` spans it.
    #[must_use]
    pub fn block_over(&self, func: &Function, needle: &str) -> BlockId {
        let offset = self.offset_of(needle);
        let end_of_needle = offset + u32::try_from(needle.len()).expect("a u32 offset");
        func.blocks
            .iter()
            .flat_map(|b| b.insts.iter().flat_map(defs).map(|d| (b.id, d)))
            .filter_map(|(id, def)| func.debug_spans[def.0 as usize].map(|span| (id, span)))
            .filter(|(_, (start, end))| *start <= offset && end_of_needle <= *end)
            .min_by_key(|(id, (start, end))| (end - start, id.0))
            .map(|(id, _)| id)
            .unwrap_or_else(|| {
                panic!(
                    "no local of `{}` spans `{needle}`; name an expression the \
                     lowering gives a slot to",
                    func.name
                )
            })
    }

    /// The innermost loop of `func` whose body lowers `needle`.
    ///
    /// [`Lowered::block_over`] names the block; this is the smallest loop
    /// holding it. Naming an inner loop by a line of its body is what lets
    /// "`mandelbrot`'s inner loop materializes ten floats" be written as the
    /// sentence it is.
    ///
    /// # Panics
    /// For [`Lowered::block_over`]'s reasons, or when the named block is in no
    /// loop.
    #[must_use]
    pub fn innermost_loop_over(&self, func: &Function, needle: &str) -> LoopRegion {
        let block = self.block_over(func, needle);
        loops(func)
            .into_iter()
            .filter(|l| l.contains(block))
            .min_by_key(|l| l.blocks.len())
            .unwrap_or_else(|| {
                panic!(
                    "`{needle}` lowers to {block:?} of `{}`, which is in no loop",
                    func.name
                )
            })
    }

    /// The byte offset of `needle`, which must occur exactly once.
    fn offset_of(&self, needle: &str) -> u32 {
        let first = self
            .src
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` does not occur in the source"));
        assert!(
            !self.src[first + needle.len()..].contains(needle),
            "`{needle}` occurs more than once in the source; \
             pick a snippet that names one place"
        );
        u32::try_from(first).expect("a source offset is a u32 in the debug metadata")
    }
}

// ---------------------------------------------------------------------------
// The census
// ---------------------------------------------------------------------------

/// What a census counts: an [`Inst`] variant, plus the payload kind for the
/// variants where the kind *is* the question.
///
/// `Materialize{Float}` and `Materialize{Bool}` are not the same instruction to
/// anyone measuring: the first is a `praxis_alloc_float` call that claims a
/// block, and the second has not allocated since ADR-040 decision 4. Collapsing
/// them into "`Materialize`: 12" answers a question nobody asked. The same goes
/// for the two scalar accessors, and for [`Inst::Alloc`] — which is keyed by
/// [`AllocKind::constructor`], the wrapper it calls, because that is already the
/// crate's one statement of the mapping and so costs no second list to keep in
/// step. (`None` is `Range`/`Seq`, which the backend refuses.)
///
/// Everything else is keyed by variant alone: those variants carry operands
/// rather than kinds.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum InstKind {
    ConstInt,
    ConstFloat,
    ConstGc,
    Alloc(Option<RuntimeSymbol>),
    ExtractScalar(ScalarKind),
    StoreScalar(ScalarKind),
    Materialize(ScalarKind),
    IntBinOp,
    IntCmp,
    FloatBinOp,
    FloatNeg,
    FloatCmp,
    StructEq,
    ValueCmp,
    BitsetContains,
    Call,
    CallIndirect,
    CheckFault,
    MoveGc,
    LoadCapture,
    LoadField,
    LoadTupleElem,
    EnumTag,
    EnumPayloadGet,
}

impl InstKind {
    /// An [`Inst::Alloc`] of whatever `sym` constructs — `InstKind::Alloc`
    /// without a `Some` at every call site.
    #[must_use]
    pub const fn alloc(sym: RuntimeSymbol) -> Self {
        InstKind::Alloc(Some(sym))
    }
}

impl From<&Inst> for InstKind {
    /// **Exhaustive on purpose, with no `_` arm.** A census that quietly answers
    /// zero for an instruction added after it was written is worse than no
    /// census, because the number still looks like a number. Adding a variant to
    /// [`Inst`] has to be a build error here, and this match is what makes it
    /// one.
    fn from(inst: &Inst) -> Self {
        match inst {
            Inst::ConstInt { .. } => InstKind::ConstInt,
            Inst::ConstFloat { .. } => InstKind::ConstFloat,
            Inst::ConstGc { .. } => InstKind::ConstGc,
            Inst::Alloc { alloc, .. } => InstKind::Alloc(AllocKind::constructor(alloc)),
            Inst::ExtractScalar { scalar, .. } => InstKind::ExtractScalar(*scalar),
            Inst::StoreScalar { scalar, .. } => InstKind::StoreScalar(*scalar),
            Inst::Materialize { scalar, .. } => InstKind::Materialize(*scalar),
            Inst::IntBinOp { .. } => InstKind::IntBinOp,
            Inst::IntCmp { .. } => InstKind::IntCmp,
            Inst::FloatBinOp { .. } => InstKind::FloatBinOp,
            Inst::FloatNeg { .. } => InstKind::FloatNeg,
            Inst::FloatCmp { .. } => InstKind::FloatCmp,
            Inst::StructEq { .. } => InstKind::StructEq,
            Inst::ValueCmp { .. } => InstKind::ValueCmp,
            Inst::BitsetContains { .. } => InstKind::BitsetContains,
            Inst::Call { .. } => InstKind::Call,
            Inst::CallIndirect { .. } => InstKind::CallIndirect,
            Inst::CheckFault { .. } => InstKind::CheckFault,
            Inst::MoveGc { .. } => InstKind::MoveGc,
            Inst::LoadCapture { .. } => InstKind::LoadCapture,
            Inst::LoadField { .. } => InstKind::LoadField,
            Inst::LoadTupleElem { .. } => InstKind::LoadTupleElem,
            Inst::EnumTag { .. } => InstKind::EnumTag,
            Inst::EnumPayloadGet { .. } => InstKind::EnumPayloadGet,
        }
    }
}

/// How many instructions of each [`InstKind`] a region of MIR holds.
///
/// Sorted (a `BTreeMap`), so the `Debug` rendering an assertion failure prints
/// is stable between a before and an after and diffs cleanly.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Census {
    counts: BTreeMap<InstKind, usize>,
}

impl Census {
    /// Census a sequence of instructions.
    #[must_use]
    pub fn of<'a>(insts: impl IntoIterator<Item = &'a Inst>) -> Self {
        let mut counts: BTreeMap<InstKind, usize> = BTreeMap::new();
        for inst in insts {
            *counts.entry(InstKind::from(inst)).or_default() += 1;
        }
        Census { counts }
    }

    /// Census a whole function: every block, in block order.
    #[must_use]
    pub fn of_function(func: &Function) -> Self {
        Census::of(func.blocks.iter().flat_map(|b| b.insts.iter()))
    }

    /// Census exactly the named blocks — a loop body, one block, a fault path.
    ///
    /// # Panics
    /// On a [`BlockId`] the function does not have.
    #[must_use]
    pub fn of_blocks(func: &Function, blocks: impl IntoIterator<Item = BlockId>) -> Self {
        let ids: Vec<BlockId> = blocks.into_iter().collect();
        Census::of(ids.iter().flat_map(|id| {
            func.blocks
                .get(id.0 as usize)
                .unwrap_or_else(|| panic!("`{}` has no block {id:?}", func.name))
                .insts
                .iter()
        }))
    }

    /// How many of `kind` there are. Zero when there are none.
    #[must_use]
    pub fn count(&self, kind: InstKind) -> usize {
        self.counts.get(&kind).copied().unwrap_or(0)
    }

    /// How many instructions there are in total.
    #[must_use]
    pub fn total(&self) -> usize {
        self.counts.values().sum()
    }

    /// Every kind present and its count, in [`InstKind`] order. A kind with no
    /// instructions is absent rather than present-and-zero.
    pub fn iter(&self) -> impl Iterator<Item = (InstKind, usize)> + '_ {
        self.counts.iter().map(|(k, n)| (*k, *n))
    }
}

/// A census per block, indexed by [`BlockId`].
///
/// The whole-function count and the per-block one answer different questions and
/// both are wanted: "how much does this function allocate" is the first, and
/// "how much does *one iteration* allocate" is the second, because a hot loop is
/// a handful of a function's blocks and the rest is setup.
#[must_use]
pub fn census_by_block(func: &Function) -> Vec<Census> {
    func.blocks
        .iter()
        .map(|b| Census::of(b.insts.iter()))
        .collect()
}

// ---------------------------------------------------------------------------
// Loops
// ---------------------------------------------------------------------------

/// A natural loop: a back edge's header, and every block that reaches the back
/// edge without leaving through the header.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LoopRegion {
    /// The header — the one block every path into the loop enters through, and
    /// the one that dominates every block in it.
    pub header: BlockId,
    /// Every block of the loop, the header included.
    pub blocks: BTreeSet<BlockId>,
}

impl LoopRegion {
    /// The census of the loop's whole body.
    ///
    /// This is a **static** count over the region. It is the per-iteration count
    /// where every block of the region runs on every iteration, and an upper
    /// bound where the body branches — a short-circuit `&&` in the condition
    /// means the iteration that exits early executes less than this. An
    /// assertion that means "per iteration" should say which it is relying on;
    /// [`census_by_block`] with [`Lowered::block_over`] is how to show it.
    #[must_use]
    pub fn census(&self, func: &Function) -> Census {
        Census::of_blocks(func, self.blocks.iter().copied())
    }

    /// Whether `block` is in the loop.
    #[must_use]
    pub fn contains(&self, block: BlockId) -> bool {
        self.blocks.contains(&block)
    }
}

/// Every natural loop in `func`, one per header, largest first.
///
/// Two properties of this are worth stating, because a census over the answer
/// inherits both.
///
/// **A back edge is verified, not assumed.** The header is checked to dominate
/// the latch rather than inferred from the block numbering. The builder emits
/// reducible CFGs today and a header does get the lower id, but a shape test
/// that silently reported "no loops" on the day that stopped being true would
/// *pass*, which is the wrong way for this to fail.
///
/// **A fault edge is not an edge**, because `liveness::successors` — the
/// crate's one statement of what leaves a block, and the function this asks —
/// does not carry [`Inst::CheckFault`]'s `on_fault`. That is the right answer
/// for a loop
/// census: the fault path leaves the loop for the host and never returns, so it
/// is not part of an iteration. It does mean a fault landing pad is in no loop
/// region and, having no terminator edge into it at all, is unreachable here.
#[must_use]
pub fn loops(func: &Function) -> Vec<LoopRegion> {
    let doms = dominators(func);
    let preds = predecessors(func);

    // One region per header: two `continue`s in one loop are two back edges to
    // one header, and they are one loop.
    let mut by_header: BTreeMap<BlockId, BTreeSet<BlockId>> = BTreeMap::new();
    for (idx, block) in func.blocks.iter().enumerate() {
        let latch = BlockId(idx as u32);
        let Some(latch_doms) = &doms[idx] else {
            continue; // Unreachable, so no edge out of it is a back edge.
        };
        for header in successors(&block.term) {
            if latch_doms.contains(&header) {
                let body = by_header
                    .entry(header)
                    .or_insert_with(|| BTreeSet::from([header]));
                collect_backwards(latch, header, &preds, body);
            }
        }
    }

    let mut regions: Vec<LoopRegion> = by_header
        .into_iter()
        .map(|(header, blocks)| LoopRegion { header, blocks })
        .collect();
    regions.sort_by_key(|l| (std::cmp::Reverse(l.blocks.len()), l.header.0));
    regions
}

/// Add `block` to `body`, and everything that reaches it without going through
/// `header`.
fn collect_backwards(
    block: BlockId,
    header: BlockId,
    preds: &[BTreeSet<BlockId>],
    body: &mut BTreeSet<BlockId>,
) {
    if block == header || !body.insert(block) {
        return;
    }
    for &pred in &preds[block.0 as usize] {
        collect_backwards(pred, header, preds, body);
    }
}

/// The predecessor set of each block, indexed by [`BlockId`].
fn predecessors(func: &Function) -> Vec<BTreeSet<BlockId>> {
    let mut preds = vec![BTreeSet::new(); func.blocks.len()];
    for (idx, block) in func.blocks.iter().enumerate() {
        for succ in successors(&block.term) {
            preds[succ.0 as usize].insert(BlockId(idx as u32));
        }
    }
    preds
}

/// The dominator set of each block, or `None` for one no path reaches.
///
/// The unreachable case is `None` rather than the "dominated by everything" the
/// fixpoint's identity element would otherwise leave there, and the distinction
/// is what a back-edge test needs: every edge out of an unreachable block
/// vacuously satisfies "the target dominates me", so the identity element would
/// report a fault landing pad's self-loop as a loop and put blocks that never
/// execute into a per-iteration count.
fn dominators(func: &Function) -> Vec<Option<BTreeSet<BlockId>>> {
    let n = func.blocks.len();
    if n == 0 {
        return Vec::new();
    }
    let entry = BlockId(0);
    let reachable = reachable_from(func, entry);
    let all: BTreeSet<BlockId> = (0..n as u32)
        .map(BlockId)
        .filter(|b| reachable[b.0 as usize])
        .collect();

    let preds = predecessors(func);
    let mut doms: Vec<Option<BTreeSet<BlockId>>> = (0..n)
        .map(|i| match () {
            () if !reachable[i] => None,
            () if BlockId(i as u32) == entry => Some(BTreeSet::from([entry])),
            () => Some(all.clone()),
        })
        .collect();

    loop {
        let mut changed = false;
        for i in 0..n {
            let block = BlockId(i as u32);
            if block == entry || !reachable[i] {
                continue;
            }
            let mut next: Option<BTreeSet<BlockId>> = None;
            for pred in preds[i].iter().filter(|p| reachable[p.0 as usize]) {
                let pd = doms[pred.0 as usize]
                    .as_ref()
                    .expect("reachable, so the fixpoint gave it a set");
                next = Some(match next {
                    None => pd.clone(),
                    Some(acc) => acc.intersection(pd).copied().collect(),
                });
            }
            let mut next = next.unwrap_or_default();
            next.insert(block);
            if doms[i].as_ref() != Some(&next) {
                doms[i] = Some(next);
                changed = true;
            }
        }
        if !changed {
            return doms;
        }
    }
}

/// Which blocks a walk from `start` reaches, indexed by [`BlockId`].
fn reachable_from(func: &Function, start: BlockId) -> Vec<bool> {
    let mut seen = vec![false; func.blocks.len()];
    let mut stack = vec![start];
    while let Some(block) = stack.pop() {
        if seen[block.0 as usize] {
            continue;
        }
        seen[block.0 as usize] = true;
        stack.extend(successors(&func.blocks[block.0 as usize].term));
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOAT_BOX: InstKind = InstKind::Materialize(ScalarKind::Float);

    /// The benchmark suite's `mandelbrot`, read from the tree rather than
    /// copied: a copy would go on asserting about a program the suite no longer
    /// runs.
    fn mandelbrot_src() -> String {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.pop(); // crates/praxis-mir -> crates
        path.pop(); // crates -> the workspace root
        path.push("benchmarks/praxis/mandelbrot.px");
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    #[test]
    fn a_file_of_top_level_statements_has_the_synthetic_entry_as_its_entry_point() {
        let lowered = lower_src_to_mir("var t = 0\nt = t + 1\nout(t)\n");
        assert_eq!(lowered.entry().name, praxis_hir::ENTRY_NAME);
    }

    #[test]
    fn a_file_of_declarations_alone_has_main_as_its_entry_point() {
        let lowered = lower_src_to_mir("fn main() -> Int { 41 + 1 }");
        assert_eq!(lowered.entry().name, "main");
    }

    /// The divergence the per-crate copies had: monomorphization drops the
    /// generic original and the JIT compiles the clones, so the census has to be
    /// of the clones. Without it there is one `id` here, with the wrong body.
    #[test]
    fn a_generic_function_is_censused_as_its_monomorphic_clones() {
        let lowered = lower_src_to_mir(
            "fn id(x) { x }\nfn main() -> Int {\n  let a = id(1)\n  let b = id(2.0)\n  a\n}",
        );
        let ids: Vec<&String> = lowered
            .funcs
            .iter()
            .map(|f| &f.name)
            .filter(|n| n.starts_with("id"))
            .collect();
        assert!(
            !ids.contains(&&"id".to_string()),
            "the generic original is dropped: {ids:?}"
        );
        assert_eq!(ids.len(), 2, "one clone per instantiation: {ids:?}");
    }

    /// The requirement `Materialize` alone cannot meet: a `Float` box is a
    /// `praxis_alloc_float` call and a `Bool` box is a load, so a census
    /// reporting "`Materialize`: 2" would be reporting two different costs as
    /// one.
    ///
    /// The program moved when ADR-120 landed and the sentence did not. It used
    /// to be `a + b < b`, whose `a + b` box is an interior node the forwarding
    /// deletes and whose `Bool` box is a terminator operand it also deletes —
    /// leaving nothing for a census to tell apart. `f(a + b)` boxes the sum
    /// because a call argument is a `Gc` operand, and `let c = a < b` boxes the
    /// comparison because its consumer is a binding rather than a branch. Both
    /// are shapes the language really emits.
    #[test]
    fn a_census_tells_a_float_materialize_from_a_bool_one() {
        let lowered = lower_src_to_mir(
            "fn g(x: Float) -> Float { x }\n\
             fn f(a: Float, b: Float) -> Bool {\n  let c = a < b\n  g(a + b)\n  c\n}",
        );
        let census = Census::of_function(lowered.function("f"));
        assert_eq!(
            census.count(FLOAT_BOX),
            1,
            "the `a + b` argument: {census:?}"
        );
        assert_eq!(
            census.count(InstKind::Materialize(ScalarKind::Bool)),
            1,
            "the comparison bound to `c`: {census:?}"
        );
    }

    #[test]
    fn a_census_keys_an_allocation_by_the_wrapper_it_calls() {
        let lowered = lower_src_to_mir("fn f() -> Text { \"praxis\" }");
        let census = Census::of_function(lowered.function("f"));
        assert_eq!(
            census.count(InstKind::alloc(RuntimeSymbol::AllocText)),
            1,
            "the literal: {census:?}"
        );
        assert_eq!(census.count(InstKind::alloc(RuntimeSymbol::AllocFloat)), 0);
    }

    #[test]
    fn an_empty_census_is_empty_and_answers_zero_rather_than_refusing() {
        let census = Census::default();
        assert_eq!(census.total(), 0);
        assert_eq!(census.count(FLOAT_BOX), 0);
        assert_eq!(census.iter().count(), 0);
    }

    #[test]
    fn the_per_block_censuses_sum_to_the_whole_functions() {
        let lowered = lower_src_to_mir(mandelbrot_src().as_str());
        let func = lowered.entry();
        let by_block = census_by_block(func);
        assert_eq!(by_block.len(), func.blocks.len(), "one census per block");

        let mut summed: BTreeMap<InstKind, usize> = BTreeMap::new();
        for block in &by_block {
            for (kind, n) in block.iter() {
                *summed.entry(kind).or_default() += n;
            }
        }
        assert_eq!(Census { counts: summed }, Census::of_function(func));
    }

    #[test]
    fn three_nested_while_loops_are_three_regions_nested_by_containment() {
        let lowered = lower_src_to_mir(mandelbrot_src().as_str());
        let regions = loops(lowered.entry());
        assert_eq!(regions.len(), 3, "{regions:?}");
        // Largest first, and each is strictly inside the one before it.
        for pair in regions.windows(2) {
            assert!(
                pair[1].blocks.is_subset(&pair[0].blocks) && pair[1].blocks != pair[0].blocks,
                "{:?} is not nested inside {:?}",
                pair[1],
                pair[0]
            );
        }
    }

    #[test]
    fn a_loop_region_holds_its_body_and_not_the_block_it_exits_to() {
        let lowered = lower_src_to_mir(
            "fn f(n: Int) -> Int {\n  var s = 0\n  var i = 0\n  \
             while i < n { s = s + i\n    i = i + 1 }\n  s + 1000\n}",
        );
        let func = lowered.function("f");
        let region = lowered.innermost_loop_over(func, "s + i");
        assert!(region.contains(lowered.block_over(func, "s + i")));
        assert!(
            !region.contains(lowered.block_over(func, "s + 1000")),
            "the exit block is after the loop, not in it: {region:?}"
        );
    }

    /// The naming contract's refusal, and the one that matters: `s + i` appears
    /// in both loops of this program, so resolving it to the first would name
    /// the wrong loop and the census would be off by whatever the other one
    /// holds — quietly, and in the direction that makes a transform look better
    /// than it is.
    #[test]
    #[should_panic(expected = "occurs more than once")]
    fn a_needle_in_two_places_is_refused_rather_than_resolved_to_the_first() {
        let lowered = lower_src_to_mir(
            "fn f(n: Int) -> Int {\n  var s = 0\n  var i = 0\n  \
             while i < n { s = s + i\n    i = i + 1 }\n  \
             while i > 0 { s = s + i\n    i = i - 1 }\n  s\n}",
        );
        let func = lowered.function("f");
        let _ = lowered.block_over(func, "s + i");
    }

    #[test]
    #[should_panic(expected = "name an expression the lowering gives a slot to")]
    fn a_needle_no_local_spans_is_refused() {
        let lowered = lower_src_to_mir("fn f(n: Int) -> Int {\n  // a comment\n  n + 1\n}");
        let func = lowered.function("f");
        let _ = lowered.block_over(func, "a comment");
    }

    #[test]
    fn a_straight_line_function_has_no_loop_region() {
        let lowered = lower_src_to_mir("fn f(n: Int) -> Int { n + 1 }");
        assert!(loops(lowered.function("f")).is_empty());
    }

    /// The stability claim [`Lowered::block_over`] is written for, tested rather
    /// than asserted: one loop, in two programs whose block numbering differs by
    /// a statement in front of it, censuses the same.
    #[test]
    fn naming_a_loop_by_its_source_survives_a_shift_in_the_block_numbering() {
        let body = "fn f(n: Int) -> Int {\n  var s = 0\n  var i = 0\n  \
                    while i < n { s = s + i * 3\n    i = i + 1 }\n  s\n}";
        let plain = lower_src_to_mir(body);
        let shifted = lower_src_to_mir(&body.replace(
            "var s = 0",
            "var s = 0\n  if n < 0 { s = 1 } else { s = 2 }",
        ));

        let plain_loop = plain.innermost_loop_over(plain.function("f"), "s = s + i * 3");
        let shifted_loop = shifted.innermost_loop_over(shifted.function("f"), "s = s + i * 3");
        assert_ne!(
            plain_loop.header, shifted_loop.header,
            "the `if` moved the numbering, which is what makes this a test"
        );
        assert_eq!(
            plain_loop.census(plain.function("f")),
            shifted_loop.census(shifted.function("f"))
        );
    }

    /// **The worked example, and the number handover 26 §1 asserted without
    /// measuring** (§9: "counted by hand-walking one inner loop"). It was
    /// right: the builder emits **ten** `Float` boxes in `mandelbrot`'s
    /// innermost loop, and W8-S0's gate — 10 to 2 — was priced against a real
    /// count.
    ///
    /// **The ten are now two**, because ADR-120's forwarding runs inside
    /// `lower_module` and this helper lowers through it. The number below moved
    /// with the pass and the loop did not: `forward::tests::mandelbrots_inner_
    /// loop_boxes_two_floats_where_it_boxed_ten` is where the delta is stated,
    /// and the per-block split preserved here is what it is a delta *of* —
    /// three of the ten were in the escape test and seven in the body, and the
    /// two survivors are the loop-carried `x` and `y`, one in each.
    ///
    /// Two is the per-iteration figure as well as the static one. The region's
    /// five blocks are the header `i < max_iter`, the `&&`'s second conjunct,
    /// the short-circuit arm, the join, and the body; a full iteration runs all
    /// of them but the short-circuit arm, which materializes nothing.
    #[test]
    fn mandelbrots_inner_loop_materializes_two_floats_where_the_builder_wrote_ten() {
        let lowered = lower_src_to_mir(mandelbrot_src().as_str());
        let func = lowered.entry();
        let inner = lowered.innermost_loop_over(func, "x * x - y * y + x0");
        let census = inner.census(func);
        assert_eq!(census.count(FLOAT_BOX), 2, "was 10: {census:?}");

        let by_block = census_by_block(func);
        let float_boxes_in =
            |needle: &str| by_block[lowered.block_over(func, needle).0 as usize].count(FLOAT_BOX);
        assert_eq!(
            (
                float_boxes_in("x * x + y * y <= 4.0"),
                float_boxes_in("x * x - y * y + x0")
            ),
            (0, 2),
            "was (3, 7); the escape test's three were all interior nodes and \
             the body keeps only `x` and `y`, which are assignments"
        );
    }

    /// The other half of the same measurement, and the reason `mandelbrot` is
    /// the suite's most allocation-bound benchmark: every box was read straight
    /// back out. The builder writes 22 `Float` reloads into that loop; ADR-120
    /// leaves 14, and the eight it removes are the eight boxes it removed
    /// paired one for one.
    #[test]
    fn mandelbrots_inner_loop_extracts_fourteen_float_payloads_where_the_builder_wrote_22() {
        let lowered = lower_src_to_mir(mandelbrot_src().as_str());
        let func = lowered.entry();
        let census = lowered
            .innermost_loop_over(func, "x * x - y * y + x0")
            .census(func);
        assert_eq!(
            census.count(InstKind::ExtractScalar(ScalarKind::Float)),
            14,
            "was 22: {census:?}"
        );
        assert_eq!(
            census.count(InstKind::FloatBinOp),
            10,
            "the arithmetic is untouched: {census:?}"
        );
    }
}
